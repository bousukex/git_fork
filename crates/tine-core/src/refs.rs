//! Branches, tags, remotes, stashes and HEAD.

use anyhow::Result;
use bstr::ByteSlice;

use crate::{
    model::{GitRef, Head, Oid, RefKind, RefsSnapshot, Stash, Tracking},
    runner::GitRunner,
};

const FORMAT: &str = "--format=%(refname)%00%(objectname)%00%(*objectname)%00%(HEAD)%00%(upstream:short)%00%(upstream:track,nobracket)%00%(symref)";

pub fn snapshot(git: &GitRunner) -> Result<RefsSnapshot> {
    let out = git.output(["for-each-ref", FORMAT, "refs/heads", "refs/remotes", "refs/tags"])?;
    let refs: Vec<GitRef> = out.lines().filter_map(parse_ref).collect();
    let head = head(git, &refs)?;
    let stashes = stashes(git)?;
    let remotes = git
        .output(["remote"])?
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_str_lossy().into_owned())
        .collect();
    Ok(RefsSnapshot { head: Some(head), refs, stashes, remotes, submodules: submodules(git)? })
}

fn submodules(git: &GitRunner) -> Result<Vec<bstr::BString>> {
    if !git.workdir().join(".gitmodules").is_file() {
        return Ok(Vec::new());
    }
    // Exit code 1: no matching keys.
    let out = git.output_with(
        ["config", "-z", "--file", ".gitmodules", "--get-regexp", r"^submodule\..*\.path$"],
        &[1],
    )?;
    // Each record is `key\nvalue`.
    let mut paths: Vec<bstr::BString> = out
        .split(|&b| b == 0)
        .filter_map(|rec| rec.split_once_str("\n").map(|(_, v)| bstr::BString::from(v)))
        .collect();
    paths.sort();
    Ok(paths)
}

fn parse_ref(line: &[u8]) -> Option<GitRef> {
    let mut f = line.split(|&b| b == 0);
    let full_name = f.next()?.to_str_lossy().into_owned();
    let object = Oid::from_hex(f.next()?)?;
    let peeled = Oid::from_hex(f.next()?);
    let is_head = f.next()? == b"*";
    let upstream = Some(f.next()?).filter(|u| !u.is_empty()).map(|u| u.to_str_lossy().into_owned());
    let track = parse_track(f.next()?, upstream.is_some());
    // Skip symbolic refs such as `origin/HEAD`.
    if !f.next().unwrap_or_default().is_empty() {
        return None;
    }
    let (kind, short_name) = if let Some(s) = full_name.strip_prefix("refs/heads/") {
        (RefKind::LocalBranch, s.to_owned())
    } else if let Some(s) = full_name.strip_prefix("refs/remotes/") {
        (RefKind::RemoteBranch, s.to_owned())
    } else if let Some(s) = full_name.strip_prefix("refs/tags/") {
        (RefKind::Tag, s.to_owned())
    } else {
        (RefKind::Other, full_name.clone())
    };
    Some(GitRef { full_name, short_name, kind, target: peeled.unwrap_or(object), is_head, upstream, track })
}

fn parse_track(s: &[u8], has_upstream: bool) -> Tracking {
    let s = s.to_str_lossy();
    if s == "gone" {
        return Tracking::Gone;
    }
    if !has_upstream {
        return Tracking::None;
    }
    let (mut ahead, mut behind) = (0, 0);
    for part in s.split(", ") {
        if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n.trim().parse().unwrap_or(0);
        }
    }
    Tracking::Diverged { ahead, behind }
}

fn head(git: &GitRunner, refs: &[GitRef]) -> Result<Head> {
    if let Ok(out) = git.output(["symbolic-ref", "-q", "HEAD"]) {
        let full = out.trim().to_str_lossy().into_owned();
        let name = full.strip_prefix("refs/heads/").unwrap_or(&full).to_owned();
        let target = refs.iter().find(|r| r.full_name == full).map(|r| r.target);
        return Ok(match target {
            Some(t) => Head::Branch { name, target: Some(t) },
            None => Head::Unborn { name },
        });
    }
    let out = git.output(["rev-parse", "--verify", "HEAD"])?;
    let id = Oid::from_hex(out.trim()).ok_or_else(|| anyhow::anyhow!("bad HEAD: {:?}", out.as_bstr()))?;
    Ok(Head::Detached(id))
}

fn stashes(git: &GitRunner) -> Result<Vec<Stash>> {
    let out = git.output(["stash", "list", "-z", "--format=%H%x1f%ct%x1f%gs"])?;
    Ok(out
        .split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .enumerate()
        .filter_map(|(index, rec)| {
            let rec = rec.strip_prefix(b"\n").unwrap_or(rec);
            let mut f = rec.splitn(3, |&b| b == 0x1f);
            let id = Oid::from_hex(f.next()?)?;
            let time = f.next()?.to_str().ok()?.parse().ok()?;
            let message = f.next().unwrap_or_default().to_str_lossy().into_owned();
            Some(Stash { index, id, time, message })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";

    #[test]
    fn parses_branch_with_tracking() {
        let line = format!("refs/heads/feature/x\0{A}\0\0*\0origin/feature/x\0ahead 2, behind 3\0");
        let r = parse_ref(line.as_bytes()).unwrap();
        assert_eq!(r.kind, RefKind::LocalBranch);
        assert_eq!(r.short_name, "feature/x");
        assert!(r.is_head);
        assert_eq!(r.track, Tracking::Diverged { ahead: 2, behind: 3 });
    }

    #[test]
    fn peels_annotated_tags_and_skips_symrefs() {
        let tag = format!("refs/tags/v1\0{A}\0{B}\0 \0\0\0");
        assert_eq!(parse_ref(tag.as_bytes()).unwrap().target.to_hex(), B);
        let sym = format!("refs/remotes/origin/HEAD\0{A}\0\0 \0\0\0refs/remotes/origin/main");
        assert!(parse_ref(sym.as_bytes()).is_none());
    }

    #[test]
    fn tracking_states() {
        assert_eq!(parse_track(b"", true), Tracking::Diverged { ahead: 0, behind: 0 });
        assert_eq!(parse_track(b"", false), Tracking::None);
        assert_eq!(parse_track(b"gone", true), Tracking::Gone);
        assert_eq!(parse_track(b"behind 7", true), Tracking::Diverged { ahead: 0, behind: 7 });
    }
}
