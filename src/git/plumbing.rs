//! Shared helper for running `git` plumbing against a bare repository.
//!
//! Both the async gRPC transport and the synchronous operations service shell out
//! through this, so the `git --git-dir <dir> <args>` invocation lives in one place

use std::path::Path;
use std::process::Output;

/// Run a `git --git-dir <git_dir> <args>` plumbing command and return its output.
///
/// Applies any extra environment variables (e.g. `GIT_AUTHOR_*` for `commit-tree`);
/// the caller inspects `status`/`stdout`/`stderr`
pub fn run_git(git_dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> std::io::Result<Output> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("--git-dir").arg(git_dir).args(args);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output()
}
