//! End-to-end security test for the push credential boundary.
//!
//! Drives a REAL `git push` into a bare repository whose `core.hooksPath` points
//! at the pre-receive hook, with `ARBOR_GIT_BIN` set to the actual compiled
//! binary (so git execs `arbor-git __pre-receive`). This exercises the whole
//! confined-push path the way production does: git's hook mechanism, the binary's
//! `__pre-receive` dispatch, the object walk, and the glob matcher. A token
//! confined to `refs/heads/agent/*` (or paths under `src/**`) must have an
//! out-of-scope push rejected and an in-scope push accepted.

use std::path::Path;
use std::process::Command;

/// The compiled arbor-git binary, provided by Cargo for integration tests.
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

/// Write the pre-receive hook (mirrors `StorageConfig::ensure_pre_receive_hook`) and
/// point the bare repo's config at it. A local `git push` runs receive-pack as a
/// child that inherits this process's env and reads this config, so the hook fires.
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

/// Push `HEAD` to `ref`, confined by the given env, from `work`. Returns whether
/// git accepted the push.
fn push(work: &Path, bare: &Path, target_ref: &str, confine: &[(&str, &str)]) -> bool {
    let mut envs = vec![("ARBOR_GIT_BIN", BIN)];
    envs.extend_from_slice(confine);
    run(
        &[
            "push",
            bare.to_str().unwrap(),
            &format!("HEAD:{target_ref}"),
        ],
        work,
        &envs,
    )
    .status
    .success()
}

#[test]
fn confined_push_enforces_ref_and_path_bounds() {
    let storage = tempfile::tempdir().unwrap();
    let bare = storage.path().join("owner").join("repo.git");
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    ok(
        &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
        Path::new("."),
    );

    let hooks_dir = storage.path().join(".arbor-hooks");
    install_hook(&hooks_dir, &bare);

    // A work tree with an initial commit under src/, pushed to main unconfined
    let work = tempfile::tempdir().unwrap();
    ok(&["init", "-q", "-b", "main", "."], work.path());
    std::fs::create_dir_all(work.path().join("src")).unwrap();
    std::fs::write(work.path().join("src/a.txt"), "hi").unwrap();
    ok(&["add", "."], work.path());
    ok(&["commit", "-q", "-m", "seed"], work.path());
    assert!(
        push(work.path(), &bare, "refs/heads/main", &[]),
        "seed push"
    );

    let agent_only = &[("ARBOR_REF_PATTERNS", r#"["refs/heads/agent/*"]"#)];

    // ref confinement: in-scope ref accepted, out-of-scope ref rejected
    std::fs::write(work.path().join("src/b.txt"), "b").unwrap();
    ok(&["add", "."], work.path());
    ok(&["commit", "-q", "-m", "b"], work.path());
    assert!(
        push(work.path(), &bare, "refs/heads/agent/x", agent_only),
        "a token confined to refs/heads/agent/* may push refs/heads/agent/x"
    );
    assert!(
        !push(work.path(), &bare, "refs/heads/main", agent_only),
        "a token confined to refs/heads/agent/* must NOT push refs/heads/main"
    );

    // path confinement (under an allowed ref): touching src/** is accepted
    let src_only = &[
        ("ARBOR_REF_PATTERNS", r#"["refs/heads/agent/*"]"#),
        ("ARBOR_PATH_PATTERNS", r#"["src/**"]"#),
    ];
    std::fs::write(work.path().join("src/c.txt"), "c").unwrap();
    ok(&["add", "."], work.path());
    ok(&["commit", "-q", "-m", "src change"], work.path());
    assert!(
        push(work.path(), &bare, "refs/heads/agent/src", src_only),
        "a src/**-confined token may push a change under src/"
    );

    // touching a path outside src/** is rejected
    std::fs::write(work.path().join("outside.txt"), "x").unwrap();
    ok(&["add", "."], work.path());
    ok(&["commit", "-q", "-m", "outside change"], work.path());
    assert!(
        !push(work.path(), &bare, "refs/heads/agent/outside", src_only),
        "a src/**-confined token must NOT push a change touching outside.txt"
    );
}
