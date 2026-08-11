//! The git pre-receive hook: the push credential boundary.
//!
//! When a confined token pushes, the `receive_pack` handler runs `git receive-pack`
//! with `core.hooksPath` pointed at a hook that re-invokes this binary as
//! `arbor-git __pre-receive`. git runs it server-side after the pack is
//! quarantined but before any ref update, so it sees the real ref tuples and can
//! diff the incoming objects rather than trusting the client. A non-zero exit
//! makes git reject the whole push atomically, and this hook's stderr is relayed
//! to the client, so the pusher sees why.
//!
//! The bounds arrive as `ARBOR_REF_PATTERNS` / `ARBOR_PATH_PATTERNS` (JSON arrays;
//! an absent var means that dimension is unconfined), matching the shape the
//! in-process arbor-api hook uses, so the two enforce identically.

use std::io::Read;
use std::process::Command;

use super::scope_match::{RefUpdate, ScopeBounds, evaluate_receive_pack, matches_glob};

/// Parse an injected pattern env var: absent means unconfined (`None`); present
/// means confined to the JSON list (a parse failure fails closed to an empty
/// list, which matches nothing).
fn parse_patterns(var: &str) -> Option<Vec<String>> {
    let raw = std::env::var(var).ok()?;
    Some(serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default())
}

/// A ref update whose new OID is all zeroes is a deletion (nothing to diff).
fn is_zero_oid(oid: &str) -> bool {
    !oid.is_empty() && oid.bytes().all(|b| b == b'0')
}

/// Whether a ref names a protected branch: only `refs/heads/*` refs are subject
/// to branch protection (tags and other refs never are), matched against the
/// rule globs by branch name.
fn is_protected(protected: &[String], reference: &str) -> bool {
    let Some(branch) = reference.strip_prefix("refs/heads/") else {
        return false;
    };
    protected
        .iter()
        .any(|pattern| matches_glob(pattern, branch))
}

/// Whether advancing `old` to `new` rewrites history (a force / non-fast-forward
/// push): true when `old` is NOT an ancestor of `new`. `git merge-base
/// --is-ancestor` exits 0 when it is (a fast-forward, allowed), 1 when it is not.
fn is_force_push(old: &str, new: &str) -> Result<bool, String> {
    let out = Command::new("git")
        .args(["merge-base", "--is-ancestor", old, new])
        .output()
        .map_err(|e| format!("git merge-base failed: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(format!(
            "git merge-base failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )),
    }
}

/// The repo-relative paths a push introduces: the union of file changes across
/// every commit reachable from the new tip but not already present on any ref
/// (`--not --all`), so intermediate commits count even on a force-push and a
/// brand-new repository counts its whole history (`--root`). git runs the hook
/// with `GIT_DIR` set, so a bare `git` resolves to the pushed-to repository.
fn changed_paths_for(new_oid: &str) -> Result<Vec<String>, String> {
    let rev_list = Command::new("git")
        .args(["rev-list", new_oid, "--not", "--all"])
        .output()
        .map_err(|e| format!("git rev-list failed: {e}"))?;
    if !rev_list.status.success() {
        return Err(format!(
            "git rev-list failed: {}",
            String::from_utf8_lossy(&rev_list.stderr)
        ));
    }

    let mut paths = std::collections::BTreeSet::new();
    for commit in String::from_utf8_lossy(&rev_list.stdout)
        .lines()
        .filter(|line| !line.is_empty())
    {
        let diff = Command::new("git")
            .args([
                "diff-tree",
                "--no-commit-id",
                "--name-only",
                "-r",
                "--root",
                commit,
            ])
            .output()
            .map_err(|e| format!("git diff-tree failed: {e}"))?;
        if !diff.status.success() {
            return Err(format!(
                "git diff-tree failed: {}",
                String::from_utf8_lossy(&diff.stderr)
            ));
        }
        for path in String::from_utf8_lossy(&diff.stdout).lines() {
            if !path.is_empty() {
                paths.insert(path.to_string());
            }
        }
    }

    Ok(paths.into_iter().collect())
}

/// Run the pre-receive boundary against the updates on stdin, returning the exit
/// code git should use (0 allow, 1 reject). Split from `run` so it is testable.
pub fn evaluate(bounds: &ScopeBounds, protected: &[String], input: &str) -> Result<i32, String> {
    let confined = bounds.ref_patterns.is_some() || bounds.path_patterns.is_some();

    // Nothing to enforce: no token confinement and no protected branches
    if !confined && protected.is_empty() {
        return Ok(0);
    }

    // Changed paths are only needed when paths are confined; skip the walk for the
    // common ref-only case (e.g. a token confined to refs/heads/agent/*)
    let need_paths = bounds.path_patterns.is_some();

    let mut updates = Vec::new();
    let mut reasons: Vec<String> = Vec::new();

    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split(' ');
        let old = parts.next().unwrap_or("");
        let new = parts.next().unwrap_or("");
        let reference = parts.next().unwrap_or("").to_string();

        // Branch protection applies to EVERY pusher (not just confined tokens): a
        // protected branch cannot be deleted or force-pushed
        if is_protected(protected, &reference) {
            if is_zero_oid(new) {
                reasons.push(format!(
                    "{reference} is a protected branch and cannot be deleted"
                ));
            } else if !is_zero_oid(old) && is_force_push(old, new)? {
                reasons.push(format!(
                    "{reference} is a protected branch and cannot be force-pushed"
                ));
            }
        }

        let changed_paths = if need_paths && !is_zero_oid(new) {
            changed_paths_for(new)?
        } else {
            Vec::new()
        };

        updates.push(RefUpdate {
            reference,
            changed_paths,
        });
    }

    // Token ref/path confinement (no-op when unconfined)
    for rejection in evaluate_receive_pack(bounds, &updates) {
        reasons.push(rejection.reason);
    }

    if reasons.is_empty() {
        return Ok(0);
    }
    for reason in &reasons {
        eprintln!("arbor: {reason}");
    }
    Ok(1)
}

/// Entry point for `arbor-git __pre-receive`. Reads the bounds from the
/// environment and the ref updates from stdin, and returns git's exit code.
pub fn run_pre_receive() -> i32 {
    let bounds = ScopeBounds {
        ref_patterns: parse_patterns("ARBOR_REF_PATTERNS"),
        path_patterns: parse_patterns("ARBOR_PATH_PATTERNS"),
    };
    let protected = parse_patterns("ARBOR_PROTECTED_REF_PATTERNS").unwrap_or_default();

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("arbor: could not read ref updates");
        return 1;
    }

    match evaluate(&bounds, &protected, &input) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("arbor: {message}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(refs: Option<&[&str]>, paths: Option<&[&str]>) -> ScopeBounds {
        ScopeBounds {
            ref_patterns: refs.map(|r| r.iter().map(|s| (*s).into()).collect()),
            path_patterns: paths.map(|p| p.iter().map(|s| (*s).into()).collect()),
        }
    }

    #[test]
    fn unconfined_allows_without_touching_git() {
        // no git repo in cwd, but unconfined + unprotected short-circuits any walk
        let input = "0000000000000000000000000000000000000000 abc123 refs/heads/main\n";
        assert_eq!(evaluate(&bounds(None, None), &[], input).unwrap(), 0);
    }

    #[test]
    fn a_ref_outside_the_globs_is_rejected() {
        let input = "old new refs/heads/main\n";
        // ref-only confinement judges without needing changed paths (no git walk)
        assert_eq!(
            evaluate(&bounds(Some(&["refs/heads/agent/*"]), None), &[], input).unwrap(),
            1
        );
    }

    #[test]
    fn an_in_bounds_ref_is_allowed_when_only_refs_are_confined() {
        let input = "old new refs/heads/agent/feature\n";
        assert_eq!(
            evaluate(&bounds(Some(&["refs/heads/agent/*"]), None), &[], input).unwrap(),
            0
        );
    }

    #[test]
    fn a_deletion_is_judged_on_its_ref_alone() {
        // zero new-oid = deletion; path confinement must not trigger a git walk
        let input = "abc 0000000000000000000000000000000000000000 refs/heads/agent/x\n";
        assert_eq!(
            evaluate(
                &bounds(Some(&["refs/heads/agent/*"]), Some(&["src/**"])),
                &[],
                input
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn a_protected_branch_cannot_be_deleted() {
        // deletion of a protected branch is rejected with no git access needed
        let input = "abc 0000000000000000000000000000000000000000 refs/heads/main\n";
        assert_eq!(
            evaluate(&bounds(None, None), &["main".to_string()], input).unwrap(),
            1
        );
    }

    #[test]
    fn protection_ignores_tags_and_unprotected_branches() {
        // a tag is never a protected branch, even under a `**` rule
        let tag = "abc 0000000000000000000000000000000000000000 refs/tags/v1\n";
        assert_eq!(
            evaluate(&bounds(None, None), &["**".to_string()], tag).unwrap(),
            0
        );
        // a branch that matches no rule is unprotected (no git walk since it is
        // not deleted and not protected)
        let other = "old new refs/heads/feature\n";
        assert_eq!(
            evaluate(&bounds(None, None), &["main".to_string()], other).unwrap(),
            0
        );
    }
}
