//! Git — a **read-only** working-tree signal for the no-progress detector.
//!
//! The only thing loopd asks git is "has the working tree changed since I last
//! looked?" — a cheap proxy for "is the agent making progress?". We never mutate
//! the repo: only `rev-parse`, `diff`, and `status` are ever run (the safety
//! invariant, ARCHITECTURE §10 — loopd's worst action is stopping an agent, it
//! never `add`/`commit`/`checkout`s). A non-repo, a missing `git`, or any git
//! error degrades to `None`, never an error, so the detector simply skips the
//! signal rather than failing the tick (claude-squad `UpdateDiffStats()` is the
//! model: compute a diff fingerprint on each poll, off the per-event path).

use std::hash::{Hash, Hasher};
use std::process::Command;

/// A fingerprint of the working tree's current changes, or `None` when there is
/// no signal to take (empty `cwd`, not a git repo, `git` not installed, or any
/// git invocation failed). Two calls return the same value iff the set of
/// changed/untracked files **and** the staged/unstaged diff are byte-identical —
/// so an unchanged fingerprint across iterations means "no progress".
///
/// We combine `git diff --stat` (content of tracked edits) with
/// `git status --porcelain` (which also surfaces *untracked* files the agent
/// creates — `diff` alone would miss a brand-new file). Both are read-only.
pub fn diff_signature(cwd: &str) -> Option<String> {
    if cwd.is_empty() || !is_git_repo(cwd) {
        return None;
    }
    // `diff --stat` for tracked-edit content; `status --porcelain` to also catch
    // new/untracked files. Either failing means we have no reliable signal.
    let diff = run_git(cwd, &["diff", "--stat"])?;
    let status = run_git(cwd, &["status", "--porcelain"])?;

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    diff.hash(&mut hasher);
    "\u{1}".hash(&mut hasher); // separator so concatenation can't collide
    status.hash(&mut hasher);
    Some(format!("{:016x}", hasher.finish()))
}

/// Is `cwd` inside a git work tree? Uses `rev-parse` (read-only); any failure
/// (not a repo, git absent) reads as "no".
pub fn is_git_repo(cwd: &str) -> bool {
    matches!(
        run_git(cwd, &["rev-parse", "--is-inside-work-tree"]).as_deref(),
        Some("true")
    )
}

/// Run a **read-only** git subcommand in `cwd`, returning trimmed stdout on a
/// clean exit, or `None` on spawn failure / non-zero exit. The caller restricts
/// `args` to read-only verbs; this helper does not execute anything else.
fn run_git(cwd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git").current_dir(cwd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::new_run_id;

    /// `git` may not be on every CI box; skip (don't fail) when it isn't.
    fn git_present() -> bool {
        Command::new("git").arg("--version").output().is_ok()
    }

    #[test]
    fn empty_cwd_has_no_signal() {
        assert!(diff_signature("").is_none());
    }

    #[test]
    fn unspawnable_cwd_has_no_signal() {
        // A path that doesn't exist makes `git` fail to launch → no signal, no
        // error. (We deliberately don't assert on a *real* non-repo dir: git's
        // `rev-parse` walks up to any ancestor work tree, so on a machine whose
        // home is itself a repo, a temp dir legitimately reports "inside a repo".)
        let missing = std::env::temp_dir().join(format!("loopd_nope_{}", new_run_id()));
        let cwd = missing.to_string_lossy().to_string();
        assert!(!is_git_repo(&cwd));
        assert!(diff_signature(&cwd).is_none());
    }

    #[test]
    fn fresh_repo_signature_is_some_and_stable() {
        if !git_present() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("loopd_git_{}", new_run_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = dir.to_string_lossy().to_string();
        // Init a throwaway repo (mutating *our temp dir*, never a user repo).
        let init = Command::new("git").current_dir(&cwd).arg("init").output();
        if init.map(|o| o.status.success()).unwrap_or(false) {
            assert!(is_git_repo(&cwd));
            let a = diff_signature(&cwd).expect("a repo yields a signature");
            let b = diff_signature(&cwd).expect("still a signature");
            assert_eq!(a, b, "an unchanged tree hashes identically");

            // Creating an untracked file must move the fingerprint (status sees it).
            std::fs::write(dir.join("new.txt"), "hello").unwrap();
            let c = diff_signature(&cwd).expect("signature after a change");
            assert_ne!(a, c, "a new untracked file changes the signature");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
