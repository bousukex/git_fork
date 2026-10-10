//! Plain data types shared between the git layer and the UI.

use std::{fmt, sync::Arc};

use bstr::BString;

/// A git object id (SHA-1 or SHA-256), stored inline.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid {
    len: u8,
    bytes: [u8; 32],
}

impl Oid {
    /// Parses 40 (SHA-1) or 64 (SHA-256) hex digits.
    pub fn from_hex(hex: &[u8]) -> Option<Oid> {
        let n = hex.len();
        if n != 40 && n != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (i, pair) in hex.chunks_exact(2).enumerate() {
            bytes[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Some(Oid { len: (n / 2) as u8, bytes })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    pub fn to_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(self.len as usize * 2);
        for b in self.as_bytes() {
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 15) as usize] as char);
        }
        s
    }

    pub fn short(&self) -> String {
        let mut s = self.to_hex();
        s.truncate(8);
        s
    }

    /// All-zero ids mean "no object" in git plumbing output.
    pub fn is_zero(&self) -> bool {
        self.as_bytes().iter().all(|&b| b == 0)
    }
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", self.short())
    }
}

/// One row of the history list. Names are interned because large
/// repositories repeat the same authors hundreds of thousands of times.
#[derive(Debug, Clone)]
pub struct CommitSummary {
    pub id: Oid,
    pub parents: Vec<Oid>,
    pub author_name: Arc<str>,
    pub author_email: Arc<str>,
    /// Unix seconds.
    pub author_time: i64,
    pub commit_time: i64,
    pub subject: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    /// Unix seconds.
    pub time: i64,
}

/// Everything shown in the commit detail header.
#[derive(Debug, Clone)]
pub struct CommitDetails {
    pub id: Oid,
    pub parents: Vec<Oid>,
    pub author: Signature,
    pub committer: Signature,
    /// Full message including subject.
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RefKind {
    LocalBranch,
    RemoteBranch,
    Tag,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRef {
    /// e.g. `refs/heads/main`.
    pub full_name: String,
    /// e.g. `main`, `origin/main`, `v1.0`.
    pub short_name: String,
    pub kind: RefKind,
    /// Commit the ref points at (tags are peeled).
    pub target: Oid,
    /// Checked-out branch.
    pub is_head: bool,
    /// e.g. `origin/main` for a local branch with an upstream.
    pub upstream: Option<String>,
    pub track: Tracking,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tracking {
    #[default]
    None,
    /// Upstream is configured but no longer exists.
    Gone,
    Diverged {
        ahead: u32,
        behind: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stash {
    /// Position in `git stash list` (`stash@{index}`).
    pub index: usize,
    pub id: Oid,
    pub time: i64,
    pub message: String,
}

/// Where HEAD points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    Branch { name: String, target: Option<Oid> },
    Detached(Oid),
    /// No commits yet.
    Unborn { name: String },
}

#[derive(Debug, Clone, Default)]
pub struct RefsSnapshot {
    pub head: Option<Head>,
    pub refs: Vec<GitRef>,
    pub stashes: Vec<Stash>,
    pub remotes: Vec<String>,
    /// Submodule paths from `.gitmodules`, relative to the work tree.
    pub submodules: Vec<BString>,
}

impl RefsSnapshot {
    pub fn head_target(&self) -> Option<Oid> {
        match self.head.as_ref()? {
            Head::Branch { target, .. } => *target,
            Head::Detached(id) => Some(*id),
            Head::Unborn { .. } => None,
        }
    }
}

/// Kind of change for one side (index or work tree) of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflicted,
}

impl ChangeKind {
    pub fn letter(self) -> char {
        match self {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Deleted => 'D',
            ChangeKind::Renamed => 'R',
            ChangeKind::Copied => 'C',
            ChangeKind::TypeChanged => 'T',
            ChangeKind::Untracked => '?',
            ChangeKind::Conflicted => 'U',
        }
    }
}

/// A file with changes, as listed in a commit or in local changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: BString,
    /// Source path for renames and copies.
    pub orig_path: Option<BString>,
    pub kind: ChangeKind,
    pub is_submodule: bool,
}

/// In-progress multi-step operation (from files in the git dir).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepoState {
    #[default]
    Clean,
    Merging,
    Rebasing,
    CherryPicking,
    Reverting,
    Bisecting,
}

#[derive(Debug, Clone, Default)]
pub struct WorkingStatus {
    pub staged: Vec<ChangedFile>,
    pub unstaged: Vec<ChangedFile>,
    pub conflicted: Vec<ChangedFile>,
    pub state: RepoState,
}

impl WorkingStatus {
    pub fn change_count(&self) -> usize {
        // A file can be both staged and unstaged; count it once per list like Fork does.
        self.staged.len() + self.unstaged.len() + self.conflicted.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Deleted,
    /// `\ No newline at end of file`
    NoNewline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    /// Line content without the leading marker or trailing newline.
    pub content: BString,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// Text after the second `@@` (usually the enclosing function).
    pub section: BString,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileDiff {
    /// Extended header lines (`new file mode`, `similarity index`, …).
    pub header: Vec<BString>,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    pub fn is_empty(&self) -> bool {
        !self.binary && self.hunks.is_empty()
    }

    pub fn stats(&self) -> (usize, usize) {
        let mut add = 0;
        let mut del = 0;
        for l in self.hunks.iter().flat_map(|h| &h.lines) {
            match l.kind {
                LineKind::Added => add += 1,
                LineKind::Deleted => del += 1,
                _ => {}
            }
        }
        (add, del)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oid_roundtrip() {
        let hex = b"0123456789abcdef0123456789abcdef01234567";
        let oid = Oid::from_hex(hex).unwrap();
        assert_eq!(oid.to_hex().as_bytes(), hex);
        assert_eq!(oid.short(), "01234567");
        assert!(Oid::from_hex(b"xyz").is_none());
        assert!(Oid::from_hex(&[b'g'; 40]).is_none());
        let sha256 = [b'a'; 64];
        assert_eq!(Oid::from_hex(&sha256).unwrap().as_bytes().len(), 32);
    }
}
