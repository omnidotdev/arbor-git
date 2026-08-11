//! End-to-end test for branch protection at the push boundary.
//!
//! Drives real `git push`es into a bare repo whose `core.hooksPath` points at the
//! pre-receive hook, with `ARBOR_PROTECTED_REF_PATTERNS` set (as the `receive_pack`
//! handler does for a repo with protection rules). A protected branch must reject
//! deletion and force-push while still accepting a normal fast-forward, and an
//! unprotected branch is unaffected.

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_arbor-git");

fn run(args: &[&str], cwd: &Path, envs: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t");
    for (key, value) in envs {
        cmd.env(key, value);
    }
    cmd.output().expect("git runs")
}

fn ok(args: &[&str], cwd: &Path) {
    let out = run(args, cwd, &[]);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn install_hook(hooks_dir: &Path, bare: &Path) {
    std::fs::create_dir_all(hooks_dir).unwrap();
    let hook = hooks_dir.join("pre-receive");
    std::fs::write(&hook, "#!/bin/sh\nexec \"$ARBOR_GIT_BIN\" __pre-receive\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    ok(
        &[
            "--git-dir",
            bare.to_str().unwrap(),
            "config",
            "core.hooksPath",
            hooks_dir.to_str().unwrap(),
        ],
        Path::new("."),
    );
}

/// Run a `git push` with `main` protected, returning whether git accepted it.
fn push(work: &Path, bare: &Path, refspec: &str) -> bool {
    run(
        &["push", bare.to_str().unwrap(), refspec],
        work,
        &[
            ("ARBOR_GIT_BIN", BIN),
            ("ARBOR_PROTECTED_REF_PATTERNS", r#"["main"]"#),
        ],
    )
    .status
    .success()
}

#[test]
fn protected_branch_blocks_force_push_and_deletion() {
    let storage = tempfile::tempdir().unwrap();
    let bare = storage.path().join("owner").join("repo.git");
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    ok(
        &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
        Path::new("."),
    );
    let hooks_dir = storage.path().join(".arbor-hooks");
    install_hook(&hooks_dir, &bare);

    let work = tempfile::tempdir().unwrap();
    ok(&["init", "-q", "-b", "main", "."], work.path());
    std::fs::write(work.path().join("a.txt"), "a").unwrap();
    ok(&["add", "."], work.path());
    ok(&["commit", "-q", "-m", "a"], work.path());

    // initial push of the protected branch is fine (a new branch, not a rewrite)
    assert!(
        push(work.path(), &bare, "main:main"),
        "creating the protected branch should be allowed"
    );

    // a fast-forward on the protected branch is allowed
    std::fs::write(work.path().join("b.txt"), "b").unwrap();
    ok(&["add", "."], work.path());
    ok(&["commit", "-q", "-m", "b"], work.path());
    assert!(
        push(work.path(), &bare, "main:main"),
        "a fast-forward on a protected branch should be allowed"
    );

    // amend so the new tip is NOT a descendant of the pushed tip: a force-push
    ok(
        &["commit", "-q", "--amend", "-m", "b (rewritten)"],
        work.path(),
    );
    assert!(
        !push(work.path(), &bare, "+main:main"),
        "a protected branch must reject a force-push"
    );

    // deletion of the protected branch is rejected
    assert!(
        !push(work.path(), &bare, ":main"),
        "a protected branch must reject deletion"
    );

    // an unprotected branch is unaffected, even by a force-push
    ok(&["branch", "-f", "feature"], work.path());
    assert!(
        push(work.path(), &bare, "+feature:feature"),
        "an unprotected branch may be force-pushed"
    );
}
