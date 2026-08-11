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

use super::scope_match::{RefUpdate, ScopeBounds, evaluate_receive_pack};

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
pub fn evaluate(bounds: &ScopeBounds, input: &str) -> Result<i32, String> {
    // Unconfined in both dimensions: nothing for the boundary to enforce
    if bounds.ref_patterns.is_none() && bounds.path_patterns.is_none() {
        return Ok(0);
    }

    // Changed paths are only needed when paths are confined; skip the walk for the
    // common ref-only case (e.g. a token confined to refs/heads/agent/*)
    let need_paths = bounds.path_patterns.is_some();

    let mut updates = Vec::new();
    for line in input.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split(' ');
        let _old = parts.next().unwrap_or("");
        let new = parts.next().unwrap_or("");
        let reference = parts.next().unwrap_or("").to_string();

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

    let rejections = evaluate_receive_pack(bounds, &updates);
    if rejections.is_empty() {
        return Ok(0);
    }
    for rejection in &rejections {
        eprintln!("arbor: {}", rejection.reason);
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

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("arbor: could not read ref updates");
        return 1;
    }

    match evaluate(&bounds, &input) {
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
        // no git repo in cwd, but unconfined bounds short-circuit before any walk
        let input = "0000000000000000000000000000000000000000 abc123 refs/heads/main\n";
        assert_eq!(evaluate(&bounds(None, None), input).unwrap(), 0);
    }

    #[test]
    fn a_ref_outside_the_globs_is_rejected() {
        let input = "old new refs/heads/main\n";
        // ref-only confinement judges without needing changed paths (no git walk)
        assert_eq!(
            evaluate(&bounds(Some(&["refs/heads/agent/*"]), None), input).unwrap(),
            1
        );
    }

    #[test]
    fn an_in_bounds_ref_is_allowed_when_only_refs_are_confined() {
        let input = "old new refs/heads/agent/feature\n";
        assert_eq!(
            evaluate(&bounds(Some(&["refs/heads/agent/*"]), None), input).unwrap(),
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
                input
            )
            .unwrap(),
            0
        );
    }
}
