//! Git LFS, shown (never run): whether git-lfs is installed, which files go
//! through it, and pointer files read as what they stand for. Nothing here
//! smudges or downloads anything.

use std::{
    collections::HashSet,
    ffi::OsString,
    os::unix::ffi::OsStrExt as _,
};

use anyhow::Result;
use bstr::{BString, ByteSlice as _};

use crate::{
    GitRunner,
    diff::DiffSource,
    model::{ChangeKind, ChangedFile, FileDiff, LineKind, Oid},
};

/// Pointer files are smaller than this (the spec's limit; git-lfs reads
/// nothing larger as one).
pub const POINTER_SIZE_CUTOFF: u64 = 1024;
const VERSION: &str = "version https://git-lfs.github.com/spec/v1";
/// SHA-256 of nothing: what an empty pointer file stands for.
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// git-lfs is there (`git lfs version` works).
pub fn installed(git: &GitRunner) -> bool {
    git.command(["lfs", "version"]).output().is_ok_and(|o| o.status.success())
}

/// What a pointer file stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pointer {
    /// SHA-256 of the content, in hex.
    pub oid: String,
    pub size: u64,
    /// `ext-<n>-<name>` entries (name, oid).
    pub extensions: Vec<(String, String)>,
}

impl Pointer {
    pub fn short_oid(&self) -> &str {
        &self.oid[..12]
    }
}

fn sha256_hex(v: &str) -> Option<String> {
    let hex = v.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then(|| hex.to_owned())
}

/// An extension's key, `ext-<priority>-<name>`: its priority (one digit, as
/// git-lfs reads them) and name (key characters, `[a-z0-9.-]`).
fn extension_key(key: &str) -> Option<(u8, &str)> {
    let rest = key.strip_prefix("ext-")?.as_bytes();
    let (&[digit, b'-'], name) = rest.split_at_checked(2)? else { return None };
    let ok = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'.' || *b == b'-';
    (digit.is_ascii_digit() && name.first().is_some_and(|b| b.is_ascii_alphanumeric()) && name.iter().all(ok))
        .then(|| (digit - b'0', std::str::from_utf8(name).expect("ASCII")))
}

/// Reads `bytes` as a pointer file, by the spec: UTF-8, under 1 KiB, `key
/// value` lines each ending in LF, `version` first and the others in key
/// order — extensions (`ext-<priority>-<name>`, each priority once),
/// `oid sha256:<64 hex>`, `size <digits>`. An empty file is the pointer of
/// an empty one. Anything else isn't one.
pub fn parse_pointer(bytes: &[u8]) -> Option<Pointer> {
    if bytes.is_empty() {
        return Some(Pointer { oid: EMPTY_SHA256.into(), size: 0, extensions: Vec::new() });
    }
    if bytes.len() as u64 >= POINTER_SIZE_CUTOFF || !bytes.ends_with(b"\n") {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.strip_suffix('\n')?.split('\n');
    if lines.next()? != VERSION {
        return None;
    }
    let (mut oid, mut size, mut extensions) = (None, None, Vec::new());
    let mut last_key = "";
    let mut priorities = 0u16;
    for line in lines {
        let (key, value) = line.split_once(' ')?;
        // Sorted, no key twice.
        if key <= last_key {
            return None;
        }
        last_key = key;
        match key {
            "oid" => oid = Some(sha256_hex(value)?),
            "size" => {
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                size = Some(value.parse().ok()?);
            }
            k => {
                let (priority, name) = extension_key(k)?;
                // Each priority once (sorted keys put them in order).
                if priorities & (1 << priority) != 0 {
                    return None;
                }
                priorities |= 1 << priority;
                extensions.push((name.to_owned(), sha256_hex(value)?));
            }
        }
    }
    Some(Pointer { oid: oid?, size: size?, extensions })
}

/// Text that starts like a pointer file (a broken one, if it doesn't
/// parse).
fn looks_like_pointer(bytes: &[u8]) -> bool {
    bytes.starts_with(b"version ") || bytes.find(b"git-lfs.github.com/spec").is_some()
}

/// Where attributes are read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttrSource {
    WorkTree,
    Index,
    Commit(Oid),
}

/// Which of `paths` go through Git LFS (`filter=lfs`), as `.gitattributes`
/// say at `source`.
pub fn tracked(git: &GitRunner, paths: &[&[u8]], source: AttrSource) -> Result<HashSet<BString>> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    let mut args: Vec<OsString> = vec!["check-attr".into(), "-z".into(), "--stdin".into()];
    match source {
        AttrSource::WorkTree => {}
        AttrSource::Index => args.push("--cached".into()),
        AttrSource::Commit(id) => args.push(format!("--source={}", id.to_hex()).into()),
    }
    args.push("filter".into());
    let input: Vec<u8> = paths.iter().flat_map(|p| p.iter().copied().chain([0])).collect();
    let out = git.output_with_input(&args, &input, &[])?;
    let fields: Vec<&[u8]> = out.split(|&b| b == 0).collect();
    Ok(fields.chunks(3).filter(|t| t.len() == 3 && t[1] == b"filter" && t[2] == b"lfs").map(|t| BString::from(t[0])).collect())
}

/// The repository uses Git LFS: some `.gitattributes` in HEAD (at any
/// level) sends files through it.
pub fn repo_uses_lfs(git: &GitRunner) -> Result<bool> {
    // No commit yet: nothing to say.
    if git.output_with(["rev-parse", "-q", "--verify", "HEAD"], &[1])?.trim().is_empty() {
        return Ok(false);
    }
    // These pathspecs are globs (not literal like Tine's others).
    let grep = git.clone().with_env("GIT_LITERAL_PATHSPECS", "0");
    let out = grep.command(["grep", "-q", "-I", "-F", "-e", "filter=lfs", "HEAD", "--", ".gitattributes", ":(glob)**/.gitattributes"]).output()?;
    match out.status.code() {
        Some(0) => Ok(true),
        // Exit code 1: no match.
        Some(1) => Ok(false),
        _ => anyhow::bail!("git grep failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
    }
}

/// One side of a file in a diff, as Git LFS sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LfsSide {
    /// No file on this side (added or deleted).
    Absent,
    Pointer(Pointer),
    /// The working tree has the file's contents (smudged), not its pointer.
    Contents { size: u64 },
}

/// A diff of a Git LFS file: the objects on each side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsView {
    pub old: LfsSide,
    pub new: LfsSide,
}

/// The diff could be of a Git LFS file (worth asking about): binary, with
/// no lines to show (a rename, a mode change, an empty file), or with a
/// changed line that a pointer file could have. Every line of a pointer
/// starts with one of its keys, so this doesn't depend on how much context
/// the diff shows.
pub fn maybe_pointer_diff(diff: &FileDiff) -> bool {
    const KEYS: [&[u8]; 4] = [b"version ", b"oid ", b"size ", b"ext-"];
    diff.binary
        || diff.hunks.is_empty()
        || diff
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| matches!(l.kind, LineKind::Added | LineKind::Deleted))
            .any(|l| KEYS.iter().any(|k| l.content.starts_with(k)))
}

/// The blob `rev:path` (`rev` empty: the index) as an LFS side: `None`
/// when it is there but not a pointer.
fn blob_side(git: &GitRunner, rev: &str, path: &[u8]) -> Result<Option<LfsSide>> {
    let mut spec = OsString::from(format!("{rev}:"));
    spec.push(std::ffi::OsStr::from_bytes(path));
    // Exit code 128: no such object (the side doesn't exist).
    let size = git.output_with([OsString::from("cat-file"), "-s".into(), spec.clone()], &[128])?;
    let Ok(size) = String::from_utf8_lossy(&size).trim().parse::<u64>() else { return Ok(Some(LfsSide::Absent)) };
    if size >= POINTER_SIZE_CUTOFF {
        return Ok(None);
    }
    Ok(parse_pointer(&git.output([OsString::from("cat-file"), "blob".into(), spec])?).map(LfsSide::Pointer))
}

/// The working tree's `path` as an LFS side: its pointer, or its contents
/// (smudged) — `None` for a broken pointer or anything but a regular file,
/// which show as they are.
fn work_tree_side(git: &GitRunner, path: &[u8]) -> Result<Option<LfsSide>> {
    let path = git.workdir().join(std::ffi::OsStr::from_bytes(path));
    let m = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Some(LfsSide::Absent)),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("can't look at {}", path.display()))),
    };
    if !m.is_file() {
        return Ok(None);
    }
    if m.len() >= POINTER_SIZE_CUTOFF {
        return Ok(Some(LfsSide::Contents { size: m.len() }));
    }
    let bytes = std::fs::read(&path).map_err(|e| anyhow::Error::new(e).context(format!("can't read {}", path.display())))?;
    Ok(match parse_pointer(&bytes) {
        Some(p) => Some(LfsSide::Pointer(p)),
        None if looks_like_pointer(&bytes) => None,
        None => Some(LfsSide::Contents { size: bytes.len() as u64 }),
    })
}

/// The two sides of `file`'s diff from `source`, by where each side is read
/// from: `(old source, old path, new source, new path)`, a side `None`
/// when the file isn't there.
type Sides<'a> = (Option<AttrSource>, &'a [u8], Option<AttrSource>, &'a [u8]);

fn sides<'a>(git: &GitRunner, source: DiffSource, file: &'a ChangedFile) -> Result<Sides<'a>> {
    let new_path: &[u8] = &file.path;
    let old_path: &[u8] = file.orig_path.as_deref().map_or(new_path, |p| p.as_ref());
    let added = matches!(file.kind, ChangeKind::Added | ChangeKind::Untracked);
    let deleted = file.kind == ChangeKind::Deleted;
    let (old, new) = match source {
        DiffSource::Commit { id, parent } => (parent.map(AttrSource::Commit), Some(AttrSource::Commit(id))),
        DiffSource::Staged => {
            let head = git.output_with(["rev-parse", "-q", "--verify", "HEAD"], &[1])?;
            (Oid::from_hex(head.trim()).map(AttrSource::Commit), Some(AttrSource::Index))
        }
        DiffSource::Unstaged => (Some(AttrSource::Index), Some(AttrSource::WorkTree)),
    };
    Ok((old.filter(|_| !added), old_path, new.filter(|_| !deleted), new_path))
}

/// What the diff of `file` (from `source`) shows of Git LFS objects: `Some`
/// when it is a Git LFS file on a side (by the attributes where that side
/// is read from) and every committed or staged side there is a valid
/// pointer (the working tree may have the contents).
pub fn view(git: &GitRunner, source: DiffSource, file: &ChangedFile) -> Result<Option<LfsView>> {
    let (old_src, old_path, new_src, new_path) = sides(git, source, file)?;
    let is_lfs = |src: Option<AttrSource>, p: &[u8]| -> Result<bool> {
        Ok(match src {
            Some(s) => tracked(git, &[p], s)?.contains(p),
            None => false,
        })
    };
    if !is_lfs(old_src, old_path)? && !is_lfs(new_src, new_path)? {
        return Ok(None);
    }
    let read = |src: Option<AttrSource>, p: &[u8]| -> Result<Option<LfsSide>> {
        match src {
            None => Ok(Some(LfsSide::Absent)),
            Some(AttrSource::Commit(id)) => blob_side(git, &id.to_hex(), p),
            Some(AttrSource::Index) => blob_side(git, "", p),
            Some(AttrSource::WorkTree) => work_tree_side(git, p),
        }
    };
    // Each side must be a pointer, absent, or (the working tree) the
    // contents; one side at least is a pointer.
    let (Some(old), Some(new)) = (read(old_src, old_path)?, read(new_src, new_path)?) else { return Ok(None) };
    if !matches!(old, LfsSide::Pointer(_)) && !matches!(new, LfsSide::Pointer(_)) {
        return Ok(None);
    }
    Ok(Some(LfsView { old, new }))
}

/// A row of a file list, and where each of its sides is read from (see
/// [`tracked_rows`]).
#[derive(Debug, Clone)]
pub struct LfsRow {
    /// The path the row shows.
    pub path: BString,
    pub sides: Vec<(AttrSource, BString)>,
}

impl LfsRow {
    /// The rows of a list of `files` changed in `source` (`head`: HEAD's
    /// commit, for staged files). A file's old side is looked at only when
    /// it may say otherwise than its new one: the file was deleted or
    /// renamed, or some `.gitattributes` in the list changed (otherwise
    /// both sides have the same attributes — and reading them from a commit
    /// costs).
    pub fn rows(source: DiffSource, head: Option<Oid>, files: &[ChangedFile]) -> Vec<LfsRow> {
        let is_attrs = |p: &BString| p.as_slice() == b".gitattributes" || p.ends_with(b"/.gitattributes");
        let attrs_changed = files.iter().any(|f| is_attrs(&f.path) || f.orig_path.as_ref().is_some_and(is_attrs));
        let (old, new) = match source {
            DiffSource::Commit { id, parent } => (parent.map(AttrSource::Commit), AttrSource::Commit(id)),
            DiffSource::Staged => (head.map(AttrSource::Commit), AttrSource::Index),
            DiffSource::Unstaged => (Some(AttrSource::Index), AttrSource::WorkTree),
        };
        files
            .iter()
            .map(|file| {
                let added = matches!(file.kind, ChangeKind::Added | ChangeKind::Untracked);
                let deleted = file.kind == ChangeKind::Deleted;
                let mut sides = Vec::new();
                if let Some(old) = old.filter(|_| !added && (deleted || file.orig_path.is_some() || attrs_changed)) {
                    sides.push((old, file.orig_path.clone().unwrap_or_else(|| file.path.clone())));
                }
                if !deleted {
                    sides.push((new, file.path.clone()));
                }
                LfsRow { path: file.path.clone(), sides }
            })
            .collect()
    }
}

/// The rows (by the path they show) that go through Git LFS on either
/// side: one `check-attr` for each place the sides are read from.
pub fn tracked_rows(git: &GitRunner, rows: &[LfsRow]) -> Result<HashSet<BString>> {
    let mut by_source: Vec<(AttrSource, Vec<&[u8]>)> = Vec::new();
    for (source, path) in rows.iter().flat_map(|r| &r.sides) {
        match by_source.iter_mut().find(|(s, _)| s == source) {
            Some((_, paths)) => paths.push(&path[..]),
            None => by_source.push((*source, vec![&path[..]])),
        }
    }
    let mut lfs = HashSet::new();
    for (source, paths) in &by_source {
        lfs.extend(tracked(git, paths, *source)?.into_iter().map(|p| (*source, p)));
    }
    Ok(rows.iter().filter(|r| r.sides.iter().any(|(s, p)| lfs.contains(&(*s, p.clone())))).map(|r| r.path.clone()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointer(lines: &[&str]) -> Vec<u8> {
        lines.iter().flat_map(|l| l.bytes().chain(*b"\n")).collect()
    }

    const OID: &str = "oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";

    fn ext(priority: &str, name: &str) -> String {
        format!("ext-{priority}-{name} sha256:{}", "f".repeat(64))
    }

    #[test]
    fn pointers_by_the_spec() {
        let p = parse_pointer(&pointer(&[VERSION, OID, "size 12345"])).unwrap();
        assert_eq!((p.size, p.short_oid()), (12345, "4d7a214614ab"));
        let p = parse_pointer(&pointer(&[VERSION, &ext("0", "foo"), OID, "size 0"])).unwrap();
        assert_eq!(p.extensions, [("foo".to_owned(), "f".repeat(64))]);
        // Priorities needn't start at 0 or follow each other; names may
        // have the other key characters.
        let p = parse_pointer(&pointer(&[VERSION, &ext("1", "foo"), &ext("7", "b.a-r9"), OID, "size 1"])).unwrap();
        assert_eq!(p.extensions.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["foo", "b.a-r9"]);
        // An empty file is the pointer of an empty one.
        let p = parse_pointer(b"").unwrap();
        assert_eq!((p.size, p.short_oid()), (0, "e3b0c44298fc"));
        // Under 1024 bytes: 1023 is a pointer, 1024 isn't.
        let sized = |extra: usize| {
            let names: Vec<String> = (0..10).map(|i| "n".repeat(if i == 0 { 1 + extra } else { 1 })).collect();
            let exts: Vec<String> = names.iter().enumerate().map(|(i, n)| ext(&i.to_string(), n)).collect();
            let mut lines: Vec<&str> = vec![VERSION];
            lines.extend(exts.iter().map(String::as_str));
            lines.extend([OID, "size 1"]);
            pointer(&lines)
        };
        let base = sized(0).len();
        assert!(parse_pointer(&sized(1023 - base)).is_some());
        assert_eq!(sized(1023 - base).len(), 1023);
        assert_eq!(parse_pointer(&sized(1024 - base)), None);
        for bad in [
            pointer(&[OID, VERSION, "size 1"]),
            pointer(&[VERSION, "size 1", OID]),
            pointer(&[VERSION, OID]),
            pointer(&[VERSION, OID, "size -1"]),
            pointer(&[VERSION, OID, "size 1", "zzz x"]),
            pointer(&[VERSION, "oid sha256:ABC", "size 1"]),
            // The same priority twice, two digits, odd names, out of order.
            pointer(&[VERSION, &ext("0", "a"), &ext("0", "b"), OID, "size 1"]),
            pointer(&[VERSION, &ext("10", "a"), OID, "size 1"]),
            pointer(&[VERSION, &ext("0", "Foo"), OID, "size 1"]),
            pointer(&[VERSION, &ext("0", "fo_o"), OID, "size 1"]),
            pointer(&[VERSION, &ext("0", ""), OID, "size 1"]),
            pointer(&[VERSION, &ext("0", "-a"), OID, "size 1"]),
            pointer(&[VERSION, &ext("x", "a"), OID, "size 1"]),
            pointer(&[VERSION, &ext("2", "a"), &ext("1", "b"), OID, "size 1"]),
            pointer(&[VERSION, OID, &ext("0", "a"), "size 1"]),
            pointer(&[VERSION, "ext-0-a sha256:xyz", OID, "size 1"]),
            pointer(&[VERSION, OID, OID, "size 1"]),
            pointer(&["version https://example.com/spec/v1", OID, "size 1"]),
            pointer(&[VERSION, OID, "size 1"]).strip_suffix(b"\n").unwrap().to_vec(),
            [pointer(&[VERSION, OID, "size 1"]), vec![b'x'; 1024]].concat(),
            b"version https://git-lfs.github.com/spec/v1\r\noid x\r\n".to_vec(),
            b"\xff\xfe".to_vec(),
            b"\n".to_vec(),
        ] {
            assert_eq!(parse_pointer(&bad), None, "{}", String::from_utf8_lossy(&bad));
        }
    }
}
