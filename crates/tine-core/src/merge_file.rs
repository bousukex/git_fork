//! A conflicted file for the merge editor: its index stages read as they
//! are, split by `git merge-file` into what the sides agree on and the
//! conflicts (keeping the bytes and line endings exactly), put back
//! together from the choices made, and saved — only over what was opened.

use std::{
    ffi::OsString,
    os::unix::{
        ffi::{OsStrExt as _, OsStringExt as _},
        fs::PermissionsExt as _,
    },
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, anyhow, bail};
use bstr::{BString, ByteSlice as _};

use crate::{
    CancelToken, GitRunner,
    model::{Oid, RepoState},
    status::repo_state,
};

/// Larger sides aren't opened (an external tool can take them).
pub const MAX_SIDE_BYTES: u64 = 5 << 20;
pub const MAX_SIDE_LINES: usize = 100_000;
/// What `git merge-file` may write, and how many conflicts are shown.
pub const MAX_OUTPUT_BYTES: usize = 16 << 20;
pub const MAX_CONFLICTS: usize = 10_000;
/// git's conflict marker size.
pub const MIN_MARKER: usize = 7;

/// A path's entry in one stage of the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageEntry {
    pub mode: u32,
    pub oid: Oid,
}

/// The stages of a conflicted path: the common ancestor (1), ours (2) and
/// theirs (3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stages {
    pub base: Option<StageEntry>,
    pub ours: Option<StageEntry>,
    pub theirs: Option<StageEntry>,
}

const REGULAR: u32 = 0o100644;
const EXECUTABLE: u32 = 0o100755;

/// Reads the stages of `path` (all of them: an unmerged path has no stage 0).
pub fn read_stages(git: &GitRunner, path: &[u8]) -> Result<Stages> {
    let out = git.output([OsString::from("ls-files"), "-u".into(), "-z".into(), "--".into(), OsString::from_vec(path.to_vec())])?;
    let mut stages = Stages::default();
    for rec in out.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let (meta, name) = rec.split_once_str("\t").ok_or_else(|| anyhow!("unexpected ls-files output"))?;
        if name != path {
            continue;
        }
        let f: Vec<&[u8]> = meta.split(|&b| b == b' ').collect();
        let [mode, oid, stage] = f[..] else { bail!("unexpected ls-files output") };
        let entry = StageEntry {
            mode: u32::from_str_radix(mode.to_str()?, 8)?,
            oid: Oid::from_hex(oid).ok_or_else(|| anyhow!("bad object id in ls-files output"))?,
        };
        match stage {
            b"1" => stages.base = Some(entry),
            b"2" => stages.ours = Some(entry),
            b"3" => stages.theirs = Some(entry),
            _ => bail!("unexpected stage in ls-files output"),
        }
    }
    Ok(stages)
}

/// Why a conflicted file isn't opened in the merge editor (the conflict
/// menu's whole-file actions, or an external merge tool, can take it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unsupported {
    NotConflicted,
    /// Deleted or added on one side only: nothing to merge line by line.
    OneSided,
    Symlink,
    Submodule,
    OtherMode(u32),
    Binary,
    TooLarge,
    /// A clean/smudge filter (Git LFS among them).
    Filter(String),
    WorkingTreeEncoding(String),
    /// A merge driver of its own (or `-merge` / `merge=binary`).
    Driver(String),
    /// `git merge-file` failed, or wrote something that isn't understood.
    MergeFailed(String),
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unsupported::NotConflicted => write!(f, "the file isn't conflicted (any more)"),
            Unsupported::OneSided => write!(f, "one side deleted the file: choose a side, or delete it, from the file's menu"),
            Unsupported::Symlink => write!(f, "it is a symbolic link: choose a side from the file's menu"),
            Unsupported::Submodule => write!(f, "it is a submodule: choose a side from the file's menu"),
            Unsupported::OtherMode(m) => write!(f, "its file mode {m:o} isn't a regular file's"),
            Unsupported::Binary => write!(f, "it is a binary file: choose a side from the file's menu"),
            Unsupported::TooLarge => write!(
                f,
                "a side is larger than {} MiB or {} lines: use an external merge tool",
                MAX_SIDE_BYTES >> 20,
                MAX_SIDE_LINES
            ),
            Unsupported::Filter(v) => write!(f, "it goes through the filter \"{v}\": use an external merge tool"),
            Unsupported::WorkingTreeEncoding(v) => {
                write!(f, "it is stored as {v} in the working tree: use an external merge tool")
            }
            Unsupported::Driver(v) => write!(f, "it has the merge driver \"{v}\": use an external merge tool"),
            Unsupported::MergeFailed(e) => write!(f, "git merge-file couldn't merge it ({e}): use an external merge tool"),
        }
    }
}

impl std::error::Error for Unsupported {}

/// Checks the stages: both sides there, regular files.
fn check_stages(stages: &Stages) -> Result<()> {
    let (Some(ours), Some(theirs)) = (stages.ours, stages.theirs) else {
        return Err(if stages.base.is_none() { Unsupported::NotConflicted } else { Unsupported::OneSided }.into());
    };
    for e in [stages.base, Some(ours), Some(theirs)].into_iter().flatten() {
        match e.mode {
            REGULAR | EXECUTABLE => {}
            0o120000 => return Err(Unsupported::Symlink.into()),
            0o160000 => return Err(Unsupported::Submodule.into()),
            m => return Err(Unsupported::OtherMode(m).into()),
        }
    }
    Ok(())
}

/// A config value (`None`: not set).
fn config(git: &GitRunner, key: &str) -> Result<Option<String>> {
    // Exit code 1: not set.
    let out = git.output_with(["config", "--get", key], &[1])?;
    let v = String::from_utf8_lossy(&out).trim_end_matches('\n').to_owned();
    Ok((!v.is_empty()).then_some(v))
}

/// The attributes, and the merge driver, that make a line-by-line merge
/// here wrong (checked when opening, and again when saving).
fn check_attributes(git: &GitRunner, path: &[u8]) -> Result<()> {
    let out = git.output([
        OsString::from("check-attr"),
        "-z".into(),
        "filter".into(),
        "working-tree-encoding".into(),
        "merge".into(),
        "--".into(),
        OsString::from_vec(path.to_vec()),
    ])?;
    let fields: Vec<&[u8]> = out.split(|&b| b == 0).collect();
    let mut merge = "unspecified".to_owned();
    for t in fields.chunks(3).filter(|t| t.len() == 3) {
        let value = String::from_utf8_lossy(t[2]).into_owned();
        let given = value != "unspecified" && value != "unset";
        match t[1] {
            b"filter" if given => return Err(Unsupported::Filter(value).into()),
            b"working-tree-encoding" if given => return Err(Unsupported::WorkingTreeEncoding(value).into()),
            b"merge" => merge = value,
            _ => {}
        }
    }
    // The driver as git picks it: `merge` set is the text merge, unset
    // the binary one; unspecified is merge.default's (else text); a value
    // names one. A name with a driver configured is that driver (even
    // "text" or "union"); else git's own of that name, or the text merge.
    let name = match merge.as_str() {
        "set" => None,
        "unset" => return Err(Unsupported::Driver("binary".into()).into()),
        "unspecified" => config(git, "merge.default")?,
        v => Some(v.to_owned()),
    };
    if let Some(name) = name
        && (name == "binary" || config(git, &format!("merge.{name}.driver"))?.is_some())
    {
        return Err(Unsupported::Driver(name).into());
    }
    Ok(())
}

/// Reads a stage's blob, refusing it before reading when too large.
fn read_blobs(git: &GitRunner, entries: &[Option<StageEntry>]) -> Result<Vec<Vec<u8>>> {
    let ids: Vec<Oid> = entries.iter().flatten().map(|e| e.oid).collect();
    let input: String = ids.iter().map(|id| format!("{}\n", id.to_hex())).collect();
    let sizes = git.output_with_input(["cat-file", "--batch-check=%(objectsize)"], input.as_bytes(), &[])?;
    for size in String::from_utf8_lossy(&sizes).lines() {
        if size.trim().parse::<u64>().map_err(|_| anyhow!("unexpected cat-file output {size:?}"))? > MAX_SIDE_BYTES {
            return Err(Unsupported::TooLarge.into());
        }
    }
    let mut out = Vec::new();
    for e in entries {
        let bytes = match e {
            Some(e) => git.output(["cat-file", "blob", &e.oid.to_hex()])?,
            None => Vec::new(),
        };
        if bytes.contains(&0) {
            return Err(Unsupported::Binary.into());
        }
        if bytes.lines_with_terminator().count() > MAX_SIDE_LINES {
            return Err(Unsupported::TooLarge.into());
        }
        out.push(bytes);
    }
    Ok(out)
}

/// The marker size for these inputs: one more than any run of a marker
/// character starting one of their lines (at least git's 7), so markers
/// can't be mistaken for content.
pub fn marker_size(inputs: &[&[u8]]) -> usize {
    let mut longest = 0;
    for input in inputs {
        for line in input.split(|&b| b == b'\n') {
            if let Some(&c) = line.first()
                && b"<|=>".contains(&c)
            {
                longest = longest.max(line.iter().take_while(|&&b| b == c).count());
            }
        }
    }
    (longest + 1).max(MIN_MARKER)
}

/// What the sides agree on, or a conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Common(Vec<u8>),
    Conflict(Conflict),
}

/// Each side's lines in a conflict (bytes with their line endings; the
/// last of a side's file may lack one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub ours: Vec<u8>,
    pub base: Vec<u8>,
    pub theirs: Vec<u8>,
    /// The line ending git's markers had here (`\n` or `\r\n`).
    pub eol: Vec<u8>,
}

/// How a conflict is resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Choice {
    #[default]
    Unresolved,
    Ours,
    Theirs,
    OursThenTheirs,
    TheirsThenOurs,
    Base,
}

/// A file split for the merge editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merge {
    /// Marker size (for unresolved conflicts written back, and for telling
    /// markers in an edited result).
    pub marker: usize,
    /// Labels on the markers: ours, base, theirs.
    pub labels: [String; 3],
    pub segments: Vec<Segment>,
    /// The line ending most lines have (for an edited result).
    pub eol: Vec<u8>,
}

impl Merge {
    pub fn conflicts(&self) -> impl Iterator<Item = &Conflict> {
        self.segments.iter().filter_map(|s| match s {
            Segment::Conflict(c) => Some(c),
            Segment::Common(_) => None,
        })
    }

    pub fn conflict_count(&self) -> usize {
        self.conflicts().count()
    }

    /// The file from the segments, with `choices` (one per conflict, in
    /// order; missing ones are unresolved: written with markers as git
    /// would).
    pub fn assemble(&self, choices: &[Choice]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut k = 0;
        for s in &self.segments {
            match s {
                Segment::Common(b) => out.extend_from_slice(b),
                Segment::Conflict(c) => {
                    self.resolve_into(c, choices.get(k).copied().unwrap_or_default(), &mut out);
                    k += 1;
                }
            }
        }
        out
    }

    fn resolve_into(&self, c: &Conflict, choice: Choice, out: &mut Vec<u8>) {
        // Two sides one after the other: the first ends its last line.
        let both = |first: &[u8], second: &[u8], out: &mut Vec<u8>| {
            out.extend_from_slice(first);
            if !first.is_empty() && !first.ends_with(b"\n") && !second.is_empty() {
                out.extend_from_slice(&c.eol);
            }
            out.extend_from_slice(second);
        };
        match choice {
            Choice::Ours => out.extend_from_slice(&c.ours),
            Choice::Theirs => out.extend_from_slice(&c.theirs),
            Choice::Base => out.extend_from_slice(&c.base),
            Choice::OursThenTheirs => both(&c.ours, &c.theirs, out),
            Choice::TheirsThenOurs => both(&c.theirs, &c.ours, out),
            Choice::Unresolved => {
                let marker = |ch: u8, label: Option<&str>, out: &mut Vec<u8>| {
                    out.extend(std::iter::repeat_n(ch, self.marker));
                    if let Some(l) = label {
                        out.push(b' ');
                        out.extend_from_slice(l.as_bytes());
                    }
                    out.extend_from_slice(&c.eol);
                };
                // git ends a side's last line before the marker after it.
                let side = |b: &[u8], out: &mut Vec<u8>| {
                    out.extend_from_slice(b);
                    if !b.is_empty() && !b.ends_with(b"\n") {
                        out.extend_from_slice(&c.eol);
                    }
                };
                marker(b'<', Some(&self.labels[0]), out);
                side(&c.ours, out);
                marker(b'|', Some(&self.labels[1]), out);
                side(&c.base, out);
                marker(b'=', None, out);
                side(&c.theirs, out);
                marker(b'>', Some(&self.labels[2]), out);
            }
        }
    }
}

/// The marker character a line starts with (exactly `size` of them, then a
/// label or the end of the line), and the line's ending.
fn marker_of(line: &[u8], size: usize) -> Option<u8> {
    let &c = line.first()?;
    if !b"<|=>".contains(&c) || line.len() < size || line[..size].iter().any(|&b| b != c) {
        return None;
    }
    match &line[size..] {
        [] | b"\n" | b"\r\n" => Some(c),
        [b' ', ..] => Some(c),
        _ => None,
    }
}

fn line_eol(line: &[u8]) -> Vec<u8> {
    if line.ends_with(b"\r\n") { b"\r\n".to_vec() } else { b"\n".to_vec() }
}

/// Splits text with conflict markers of `size` (as `git merge-file
/// --diff3` writes them; a base section may be missing, as in an edited
/// result). A structure that isn't one is an error.
pub fn parse(text: &[u8], size: usize) -> Result<Vec<Segment>> {
    #[derive(PartialEq)]
    enum At {
        Common,
        Ours,
        Base,
        Theirs,
    }
    let mut segments = Vec::new();
    let mut common = Vec::new();
    let mut c = Conflict { ours: Vec::new(), base: Vec::new(), theirs: Vec::new(), eol: Vec::new() };
    let mut at = At::Common;
    for (n, line) in text.lines_with_terminator().enumerate() {
        let n = n + 1;
        match (marker_of(line, size), &at) {
            (Some(b'<'), At::Common) => {
                if !common.is_empty() {
                    segments.push(Segment::Common(std::mem::take(&mut common)));
                }
                c.eol = line_eol(line);
                at = At::Ours;
            }
            (Some(b'|'), At::Ours) => at = At::Base,
            (Some(b'='), At::Ours | At::Base) => at = At::Theirs,
            (Some(b'>'), At::Theirs) => {
                let eol = std::mem::take(&mut c.eol);
                segments.push(Segment::Conflict(std::mem::replace(
                    &mut c,
                    Conflict { ours: Vec::new(), base: Vec::new(), theirs: Vec::new(), eol: Vec::new() },
                )));
                if let Some(Segment::Conflict(last)) = segments.last_mut() {
                    last.eol = eol;
                }
                at = At::Common;
            }
            (Some(m), _) => bail!("line {n}: a conflict marker ({}) out of place", m as char),
            (None, At::Common) => common.extend_from_slice(line),
            (None, At::Ours) => c.ours.extend_from_slice(line),
            (None, At::Base) => c.base.extend_from_slice(line),
            (None, At::Theirs) => c.theirs.extend_from_slice(line),
        }
    }
    if at != At::Common {
        bail!("a conflict isn't closed");
    }
    if !common.is_empty() {
        segments.push(Segment::Common(common));
    }
    Ok(segments)
}

/// Marker lines of `size` in `text` (an edited result still holding
/// conflicts).
pub fn marker_lines(text: &[u8], size: usize) -> usize {
    text.lines_with_terminator().filter(|l| marker_of(l, size).is_some()).count()
}

/// The line ending most lines of `text` have.
pub fn majority_eol(text: &[u8]) -> Vec<u8> {
    let (mut crlf, mut lf) = (0usize, 0usize);
    for line in text.lines_with_terminator() {
        if line.ends_with(b"\r\n") {
            crlf += 1;
        } else if line.ends_with(b"\n") {
            lf += 1;
        }
    }
    if crlf > lf { b"\r\n".to_vec() } else { b"\n".to_vec() }
}

/// git ends the last line of a side before the marker after it, also when
/// the side's file didn't: in the final conflict (which ends every side's
/// file) that line ending isn't the side's.
/// `eol`: what git adds (the conflict's marker line ending), exactly.
fn restore_tail(section: &mut Vec<u8>, input: &[u8], eol: &[u8]) -> Result<()> {
    if input.ends_with(section) {
        return Ok(());
    }
    if section.ends_with(eol) && input.ends_with(&section[..section.len() - eol.len()]) {
        section.truncate(section.len() - eol.len());
        return Ok(());
    }
    bail!("the last conflict doesn't end the file")
}

/// A scratch directory removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "tine-merge-{}-{}-{nanos:x}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Splits the three versions with `git merge-file --diff3`.
pub fn merge(git: &GitRunner, ours: &[u8], base: &[u8], theirs: &[u8], labels: [&str; 3], cancel: &CancelToken) -> Result<Merge> {
    let marker = marker_size(&[ours, base, theirs]);
    let scratch = Scratch::new()?;
    let files = ["ours", "base", "theirs"].map(|n| scratch.0.join(n));
    for (f, bytes) in files.iter().zip([ours, base, theirs]) {
        std::fs::write(f, bytes)?;
    }
    let mut args: Vec<OsString> = vec!["merge-file".into(), "-p".into(), "--diff3".into(), format!("--marker-size={marker}").into()];
    for l in labels {
        args.extend(["-L".into(), l.into()]);
    }
    args.extend(files.iter().map(|f| f.clone().into_os_string()));
    // Exit code: the number of conflicts (127: as many or more).
    let ok: Vec<i32> = (1..=127).collect();
    let out = match git.output_limited_with(&args, MAX_OUTPUT_BYTES, cancel, &ok) {
        Ok(Some(out)) => out,
        Ok(None) => return Err(Unsupported::TooLarge.into()),
        Err(e) if e.is::<crate::Cancelled>() => return Err(e),
        Err(e) => return Err(Unsupported::MergeFailed(format!("{e:#}")).into()),
    };
    let mut segments = parse(&out, marker).map_err(|e| Unsupported::MergeFailed(format!("{e:#}")))?;
    if let Some(Segment::Conflict(c)) = segments.last_mut() {
        let eol = c.eol.clone();
        for (section, input) in [(&mut c.ours, ours), (&mut c.base, base), (&mut c.theirs, theirs)] {
            restore_tail(section, input, &eol).map_err(|e| Unsupported::MergeFailed(format!("{e:#}")))?;
        }
    }
    let m = Merge { marker, labels: labels.map(str::to_owned), segments, eol: majority_eol(&out) };
    if m.conflict_count() > MAX_CONFLICTS {
        return Err(Unsupported::TooLarge.into());
    }
    Ok(m)
}

/// Working-tree files larger than this aren't what git wrote for a
/// conflict the editor takes (its sides are 5 MiB at most).
pub const MAX_WORK_BYTES: u64 = 64 << 20;

/// The working-tree file as found, to tell later that nothing changed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkFile {
    Missing,
    /// `mode`: the permission bits.
    File { len: u64, hash: u64, mode: u32 },
    /// Over [`MAX_WORK_BYTES`] (not read).
    Large { len: u64 },
    Symlink(BString),
    /// A directory, a socket…
    Other,
}

impl WorkFile {
    /// Something saving may replace (a regular file, or none).
    fn replaceable(&self) -> bool {
        matches!(self, WorkFile::Missing | WorkFile::File { .. })
    }
}

/// The working-tree file at `path`: its kind, and for a regular file its
/// length, permissions and a hash of its content (read in pieces, so a
/// huge file isn't held; stopping when `cancel` fires).
pub fn work_file(path: &Path, cancel: &CancelToken) -> Result<WorkFile> {
    use std::io::Read as _;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(WorkFile::Missing),
        Err(e) => return Err(anyhow::Error::from(e).context(format!("reading {}", path.display()))),
    };
    let kind = meta.file_type();
    if kind.is_symlink() {
        let target = std::fs::read_link(path)?;
        return Ok(WorkFile::Symlink(target.into_os_string().into_vec().into()));
    }
    if !kind.is_file() {
        return Ok(WorkFile::Other);
    }
    if meta.len() > MAX_WORK_BYTES {
        return Ok(WorkFile::Large { len: meta.len() });
    }
    let mut f = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    // FNV-1a: only to notice a change.
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut len = 0u64;
    let mut chunk = vec![0u8; 1 << 16];
    loop {
        if cancel.is_cancelled() {
            return Err(crate::Cancelled.into());
        }
        let n = match f.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(anyhow::Error::from(e).context(format!("reading {}", path.display()))),
        };
        len += n as u64;
        if len > MAX_WORK_BYTES {
            return Ok(WorkFile::Large { len });
        }
        h = chunk[..n].iter().fold(h, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    }
    Ok(WorkFile::File { len, hash: h, mode: meta.permissions().mode() & 0o7777 })
}

/// How the sides are named: titles for the editor (`ours`/`theirs` don't
/// say which is which in a rebase) and labels for the markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sides {
    pub ours: String,
    pub base: String,
    pub theirs: String,
    pub labels: [String; 3],
}

/// Names the sides after the operation in progress (best effort: what
/// can't be read is left out).
pub fn sides(git: &GitRunner, git_dir: &Path) -> Sides {
    let commit = |rev: &str| -> Option<(String, String)> {
        let out = git.output(["log", "-1", "--no-walk", "--format=%h%x00%s", rev, "--"]).ok()?;
        let s = String::from_utf8_lossy(&out).trim_end().to_owned();
        let (short, subject) = s.split_once('\0')?;
        Some((short.to_owned(), subject.to_owned()))
    };
    let branch = git
        .output_with(["symbolic-ref", "-q", "--short", "HEAD"], &[1])
        .ok()
        .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
        .filter(|b| !b.is_empty());
    let head = match &branch {
        Some(b) => format!("HEAD — {b}"),
        None => "HEAD".to_owned(),
    };
    let named = |rev: &str| commit(rev).map_or_else(|| rev.to_owned(), |(h, s)| format!("{h} {s}"));
    // A merge commit picked or reverted: git takes the parent given with
    // -m, which isn't recorded anywhere to read back.
    let is_merge = |rev: &str| {
        git.output(["rev-list", "--no-walk", "--parents", "-n1", rev, "--"])
            .is_ok_and(|o| String::from_utf8_lossy(&o).split_whitespace().count() > 2)
    };
    let label = |rev: &str| commit(rev).map_or_else(|| rev.to_owned(), |(h, s)| format!("{h} ({s})"));
    let (ours, base, theirs, labels) = match repo_state(git_dir) {
        RepoState::Merging => (
            format!("{head} (the branch merged into)"),
            "Common ancestor".to_owned(),
            format!("MERGE_HEAD — {} (being merged in)", named("MERGE_HEAD")),
            ["HEAD".to_owned(), "base".to_owned(), branch_of_merge(git_dir).unwrap_or_else(|| label("MERGE_HEAD"))],
        ),
        RepoState::Rebasing => {
            let onto = std::fs::read_to_string(git_dir.join("rebase-merge/onto"))
                .or_else(|_| std::fs::read_to_string(git_dir.join("rebase-apply/onto")))
                .ok()
                .and_then(|o| commit(o.trim()))
                .map_or_else(String::new, |(h, _)| format!(" onto {h}"));
            (
                format!("Rebased so far{onto} (HEAD)"),
                "Before the commit being applied".to_owned(),
                format!("Being applied — {} (REBASE_HEAD)", named("REBASE_HEAD")),
                ["HEAD".to_owned(), "base".to_owned(), label("REBASE_HEAD")],
            )
        }
        RepoState::CherryPicking => (
            head.clone(),
            if is_merge("CHERRY_PICK_HEAD") {
                "The parent of the merge being picked chosen with -m".to_owned()
            } else {
                "Before the commit being picked (its parent)".to_owned()
            },
            format!("Being picked — {} (CHERRY_PICK_HEAD)", named("CHERRY_PICK_HEAD")),
            ["HEAD".to_owned(), "base".to_owned(), label("CHERRY_PICK_HEAD")],
        ),
        RepoState::Reverting => (
            head.clone(),
            format!("The commit being reverted — {}", named("REVERT_HEAD")),
            if is_merge("REVERT_HEAD") {
                "Before the merge being reverted (its parent chosen with -m)".to_owned()
            } else {
                "Before the commit being reverted (its parent, REVERT_HEAD^)".to_owned()
            },
            ["HEAD".to_owned(), "base".to_owned(), format!("parent of {}", label("REVERT_HEAD"))],
        ),
        // Conflicts outside an operation (a stash applied, `checkout -m`…):
        // stage 2 is what the index had, not necessarily HEAD.
        _ => (
            "What was there before (stage 2)".to_owned(),
            "Common ancestor (stage 1)".to_owned(),
            "The changes being applied (stage 3)".to_owned(),
            ["Updated upstream".to_owned(), "base".to_owned(), "Stashed changes".to_owned()],
        ),
    };
    Sides { ours, base, theirs, labels }
}

/// What the merge message says is merged ("Merge branch 'x'" → x).
fn branch_of_merge(git_dir: &Path) -> Option<String> {
    let msg = std::fs::read_to_string(git_dir.join("MERGE_MSG")).ok()?;
    let first = msg.lines().next()?;
    let rest = first.strip_prefix("Merge ")?;
    let start = rest.find('\'')?;
    let end = rest[start + 1..].find('\'')?;
    Some(rest[start + 1..start + 1 + end].to_owned())
}

/// A conflicted file opened for the merge editor: what it was opened from
/// (saving checks nothing changed since) and its split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub path: BString,
    pub state: RepoState,
    pub heads: Heads,
    pub stages: Stages,
    pub work: WorkFile,
    pub sides: Sides,
    pub merge: Merge,
}

impl Opened {
    /// What saving checks against.
    pub fn target(&self) -> Target {
        Target { path: self.path.clone(), state: self.state, heads: self.heads, stages: self.stages }
    }
}

/// HEAD and the commit the operation in progress is at (MERGE_HEAD,
/// REBASE_HEAD…): the same kind of operation a step further, or another
/// one, has others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Heads {
    pub head: Option<Oid>,
    pub operation: Option<Oid>,
}

fn heads(git: &GitRunner, state: RepoState) -> Result<Heads> {
    let read = |rev: &str| -> Result<Option<Oid>> {
        // Exit code 1: not there.
        let out = git.output_with(["rev-parse", "-q", "--verify", rev], &[1])?;
        Ok(Oid::from_hex(String::from_utf8_lossy(&out).trim().as_bytes()))
    };
    let operation = match state {
        RepoState::Merging => Some("MERGE_HEAD"),
        RepoState::Rebasing => Some("REBASE_HEAD"),
        RepoState::CherryPicking => Some("CHERRY_PICK_HEAD"),
        RepoState::Reverting => Some("REVERT_HEAD"),
        _ => None,
    };
    Ok(Heads { head: read("HEAD")?, operation: operation.map(read).transpose()?.flatten() })
}

/// The conflict a save is for: the path, its stages and the operation as
/// they were when it was opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub path: BString,
    pub state: RepoState,
    pub heads: Heads,
    pub stages: Stages,
}

fn work_path(git: &GitRunner, path: &[u8]) -> PathBuf {
    git.workdir().join(std::ffi::OsStr::from_bytes(path))
}

/// Opens `path` (a conflict of the operation in progress) for the editor.
/// Files it isn't for come back as an [`Unsupported`] error.
pub fn open(git: &GitRunner, git_dir: &Path, path: &[u8], cancel: &CancelToken) -> Result<Opened> {
    let git = git.clone().with_cancel(cancel.clone());
    let state = repo_state(git_dir);
    let heads = heads(&git, state)?;
    let stages = read_stages(&git, path)?;
    check_stages(&stages)?;
    check_attributes(&git, path)?;
    let work = work_file(&work_path(&git, path), cancel)?;
    if matches!(work, WorkFile::Large { .. }) {
        return Err(Unsupported::TooLarge.into());
    }
    let blobs = read_blobs(&git, &[stages.ours, stages.base, stages.theirs])?;
    let sides = sides(&git, git_dir);
    let labels = [sides.labels[0].as_str(), sides.labels[1].as_str(), sides.labels[2].as_str()];
    let merge = merge(&git, &blobs[0], &blobs[1], &blobs[2], labels, cancel)?;
    Ok(Opened { path: path.into(), state, heads, stages, work, sides, merge })
}

/// Saving refused: the file, its stages or the operation changed since
/// it was opened (nothing was written).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed(pub String);

impl std::fmt::Display for Changed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}; nothing was saved — open the file again", self.0)
    }
}

impl std::error::Error for Changed {}

/// What a save did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    /// The working-tree file as written (saving again checks against it).
    pub work: WorkFile,
    /// With "mark resolved": whether `git add` did (the file is saved
    /// either way).
    pub staged: Option<std::result::Result<(), String>>,
}

/// Nothing changed since `target` was opened (or the file last saved as
/// `work`): the operation, its commits, the stages, what applies to the
/// file, and the file itself (which must be a regular file, or none).
fn verify(git: &GitRunner, git_dir: &Path, target: &Target, work: &WorkFile) -> Result<()> {
    let path = target.path.as_slice();
    let state = repo_state(git_dir);
    if state != target.state || heads(git, state)? != target.heads {
        return Err(Changed("the operation in progress changed (ended, or a step further)".into()).into());
    }
    if read_stages(git, path)? != target.stages {
        return Err(Changed("the file's conflict changed (resolved elsewhere, or a step further)".into()).into());
    }
    match check_attributes(git, path) {
        Err(e) if e.is::<Unsupported>() => return Err(Changed(format!("what applies to the file changed: {e}")).into()),
        r => r?,
    }
    let now = work_file(&work_path(git, path), &CancelToken::default())?;
    if !now.replaceable() {
        return Err(Changed("the file isn't a regular file".into()).into());
    }
    if &now != work {
        return Err(Changed("the file was changed outside the editor".into()).into());
    }
    Ok(())
}

/// Writes `result` (as the index has content: line endings are converted
/// for the working tree as git checks files out) over the file of
/// `target` — only if nothing changed since it was opened (`work`: the
/// file as last opened or saved; checked first, and again right before
/// the file is replaced). With `mark_resolved`, adds it to the index after
/// (checked again: still that conflict, the file as written).
///
/// Only Tine's own operations are kept apart from this; a git command
/// run elsewhere at the very same moment can still slip in between.
pub fn save(git: &GitRunner, git_dir: &Path, target: &Target, work: &WorkFile, result: &[u8], mark_resolved: bool) -> Result<Saved> {
    let path = target.path.as_slice();
    let file = work_path(git, path);
    verify(git, git_dir, target, work)?;
    // As git checks it out (end-of-line conversion).
    let blob = git.output_with_input(["hash-object", "-w", "--stdin", "--no-filters"], result, &[])?;
    let blob = String::from_utf8_lossy(&blob).trim().to_owned();
    let mut cat = OsString::from("--path=");
    cat.push(std::ffi::OsStr::from_bytes(path));
    let bytes = git.output([OsString::from("cat-file"), "--filters".into(), cat, blob.into()])?;
    let executable = target.stages.ours.is_some_and(|e| e.mode == EXECUTABLE);
    write_replacing(&file, &bytes, executable, || verify(git, git_dir, target, work))?;
    let written = work_file(&file, &CancelToken::default())?;
    let staged = mark_resolved.then(|| {
        verify(git, git_dir, target, &written)
            .map_err(|e| format!("{e:#}").replace("nothing was saved", "it was saved, not marked resolved"))?;
        git.output([OsString::from("add"), "--".into(), OsString::from_vec(path.to_vec())]).map(drop).map_err(|e| format!("{e:#}"))
    });
    Ok(Saved { work: written, staged })
}

/// Writes `path` by renaming a file written next to it (never half
/// written), with its permissions (the executable bit as given). The file
/// written first is new (an unused random name, created exclusively: an
/// existing file or link is never written through). `ready` runs right
/// before the rename; an error from it leaves `path` alone.
fn write_replacing(path: &Path, bytes: &[u8], executable: bool, ready: impl FnOnce() -> Result<()>) -> Result<()> {
    use std::{io::Write as _, os::unix::fs::OpenOptionsExt as _};
    let dir = path.parent().ok_or_else(|| anyhow!("no directory for {}", path.display()))?;
    let name = path.file_name().ok_or_else(|| anyhow!("no file name in {}", path.display()))?;
    let perm = match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => m.permissions().mode() & 0o7777,
        _ => 0o644,
    };
    let perm = if executable { perm | ((perm & 0o444) >> 2) } else { perm & !0o111 };
    let (tmp, mut f) = {
        let mut tries = 0;
        loop {
            let mut tmp_name = OsString::from(".");
            tmp_name.push(name);
            tmp_name.push(format!(".tine-merge-{}", crate::interactive::random_id()));
            let tmp = dir.join(tmp_name);
            match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp) {
                Ok(f) => break (tmp, f),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 8 => tries += 1,
                Err(e) => return Err(anyhow::Error::from(e).context(format!("creating a file in {}", dir.display()))),
            }
        }
    };
    // Ours from here: removed again unless it became `path`.
    let written = (|| -> Result<()> {
        f.write_all(bytes).with_context(|| format!("writing {}", tmp.display()))?;
        f.sync_all()?;
        drop(f);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(perm))?;
        ready()?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflict(ours: &str, base: &str, theirs: &str) -> Segment {
        Segment::Conflict(Conflict { ours: ours.into(), base: base.into(), theirs: theirs.into(), eol: b"\n".to_vec() })
    }

    #[test]
    fn marker_size_outgrows_marker_like_lines() {
        assert_eq!(marker_size(&[b"a\n", b"", b"b"]), 7);
        assert_eq!(marker_size(&[b"<<<<<<<< not a marker\n", b"x", b"==========\n"]), 11);
        assert_eq!(marker_size(&[b"a <<<<<<<<<<\n"]), 7, "only at the start of a line");
    }

    #[test]
    fn parsing_and_assembling_round_trip() {
        let text = b"a\n<<<<<<< ours\nx\n||||||| base\nb\n=======\ny\n>>>>>>> theirs\nc\n";
        let segments = parse(text, 7).unwrap();
        assert_eq!(segments, [Segment::Common(b"a\n".to_vec()), conflict("x\n", "b\n", "y\n"), Segment::Common(b"c\n".to_vec())]);
        let m = Merge { marker: 7, labels: ["ours".into(), "base".into(), "theirs".into()], segments, eol: b"\n".to_vec() };
        assert_eq!(m.assemble(&[]), text);
        assert_eq!(m.assemble(&[Choice::Theirs]), b"a\ny\nc\n");
        assert_eq!(m.assemble(&[Choice::TheirsThenOurs]), b"a\ny\nx\nc\n");
        // Without a base section (an edited result), and broken structures.
        assert_eq!(parse(b"<<<<<<< o\nx\n=======\ny\n>>>>>>> t\n", 7).unwrap(), [conflict("x\n", "", "y\n")]);
        assert!(parse(b"<<<<<<< o\nx\n", 7).is_err());
        assert!(parse(b"=======\n", 7).is_err());
        assert!(parse(b"<<<<<<< o\n<<<<<<< o\n", 7).is_err());
        // Longer runs are content.
        assert_eq!(parse(b"<<<<<<<< x\n", 7).unwrap(), [Segment::Common(b"<<<<<<<< x\n".to_vec())]);
        assert_eq!(marker_lines(b"a\n=======\n<<<<<<<< x\n>>>>>>> t\n", 7), 2);
    }

    #[test]
    fn a_failed_last_check_leaves_the_file_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("tine-merge-test-{}", crate::interactive::random_id()));
        std::fs::create_dir(&dir).unwrap();
        let f = dir.join("f.txt");
        std::fs::write(&f, "before\n").unwrap();
        assert!(write_replacing(&f, b"after\n", false, || bail!("changed meanwhile")).is_err());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "before\n");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "the file written first is gone");
        write_replacing(&f, b"after\n", true, || Ok(())).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "after\n");
        assert_ne!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o111, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_last_line_ending_is_put_back() {
        let mut s = b"x\n".to_vec();
        restore_tail(&mut s, b"a\nx", b"\n").unwrap();
        assert_eq!(s, b"x");
        let mut s = b"x\r\n".to_vec();
        restore_tail(&mut s, b"a\r\nx", b"\r\n").unwrap();
        assert_eq!(s, b"x");
        let mut s = b"x\n".to_vec();
        restore_tail(&mut s, b"a\nx\n", b"\n").unwrap();
        assert_eq!(s, b"x\n");
        // A lone CR is content: only what git added goes.
        let mut s = b"\r\n".to_vec();
        restore_tail(&mut s, b"\r", b"\n").unwrap();
        assert_eq!(s, b"\r");
        let mut s = b"x\r\r\n".to_vec();
        restore_tail(&mut s, b"a\r\nx\r", b"\r\n").unwrap();
        assert_eq!(s, b"x\r");
        assert!(restore_tail(&mut b"y\n".to_vec(), b"a\nx", b"\n").is_err());
        assert_eq!(majority_eol(b"a\r\nb\r\nc\n"), b"\r\n");
        assert_eq!(majority_eol(b"a"), b"\n");
    }
}
