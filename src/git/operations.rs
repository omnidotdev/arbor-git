use std::path::Path;

use tracing::{info, instrument};

use super::commits::GitActor;
use super::plumbing::run_git;
use super::{GitError, Result, StorageConfig, open_repo_by_name};

pub struct OperationsService {
    config: StorageConfig,
}

#[derive(Debug, Clone)]
pub struct MergeResult {
    pub commit_oid: Option<String>,
    pub conflicts: Vec<ConflictInfo>,
    pub merged_files: Vec<String>,
    pub status: MergeStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStatus {
    Success,
    Conflict,
    AlreadyUpToDate,
    FastForward,
}

#[derive(Debug, Clone)]
pub struct ConflictInfo {
    pub path: String,
    pub ours_oid: Option<String>,
    pub theirs_oid: Option<String>,
    pub ancestor_oid: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RebaseResult {
    pub new_head_oid: Option<String>,
    pub rebased_commits: Vec<String>,
    pub conflicts: Vec<ConflictInfo>,
    pub status: RebaseStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaseStatus {
    Success,
    Conflict,
    NothingToRebase,
}

#[derive(Debug, Clone)]
pub struct CherryPickResult {
    pub commit_oid: Option<String>,
    pub conflicts: Vec<ConflictInfo>,
    pub status: CherryPickStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CherryPickStatus {
    Success,
    Conflict,
    EmptyCommit,
}

/// Whether a three-way merge records a two-parent merge commit or a single-parent
/// squash commit
#[derive(Debug, Clone, Copy)]
enum MergeMode {
    Merge { allow_fast_forward: bool },
    Squash,
}

/// Outcome of replaying one commit onto another via `pick_onto`
enum PickOutcome {
    Committed(String),
    Empty,
    Conflict(Vec<ConflictInfo>),
}

/// The original identity and message of a commit, preserved when it is replayed
struct CommitMetadata {
    author_name: String,
    author_email: String,
    author_date: String,
    message: String,
}

impl OperationsService {
    pub const fn new(config: StorageConfig) -> Self {
        Self { config }
    }

    /// Perform a merge, writing a real two-parent merge commit on success
    #[instrument(skip(self))]
    pub fn merge(
        &self,
        owner: &str,
        name: &str,
        base_ref: &str,
        head_ref: &str,
        author: &GitActor,
        message: Option<&str>,
        allow_fast_forward: bool,
    ) -> Result<MergeResult> {
        self.merge_or_squash(
            owner,
            name,
            base_ref,
            head_ref,
            author,
            message,
            MergeMode::Merge { allow_fast_forward },
        )
    }

    /// Squash-merge, writing a single-parent commit that carries the merged tree
    #[instrument(skip(self))]
    pub fn squash(
        &self,
        owner: &str,
        name: &str,
        base_ref: &str,
        head_ref: &str,
        author: &GitActor,
        message: Option<&str>,
    ) -> Result<MergeResult> {
        self.merge_or_squash(
            owner,
            name,
            base_ref,
            head_ref,
            author,
            message,
            MergeMode::Squash,
        )
    }

    /// Shared three-way merge core for `merge` and `squash`. git is the authority
    /// on whether the merge resolves and on the resulting tree object; the gix
    /// tree diff only enriches the reported conflicts. Never moves any ref itself
    fn merge_or_squash(
        &self,
        owner: &str,
        name: &str,
        base_ref: &str,
        head_ref: &str,
        author: &GitActor,
        message: Option<&str>,
        mode: MergeMode,
    ) -> Result<MergeResult> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        let base_id = repo
            .rev_parse_single(base_ref)
            .map_err(|_| GitError::RefNotFound {
                reference: base_ref.to_string(),
            })?;
        let head_id = repo
            .rev_parse_single(head_ref)
            .map_err(|_| GitError::RefNotFound {
                reference: head_ref.to_string(),
            })?;

        if base_id == head_id {
            return Ok(MergeResult {
                commit_oid: Some(base_id.to_string()),
                conflicts: Vec::new(),
                merged_files: Vec::new(),
                status: MergeStatus::AlreadyUpToDate,
            });
        }

        let merge_base = repo
            .merge_base(base_id.detach(), head_id.detach())
            .map_err(|e| GitError::Gix(e.to_string()))?;

        // A merge may fast-forward when the base is already an ancestor of the
        // head; a squash always records a fresh single-parent commit
        if matches!(
            mode,
            MergeMode::Merge {
                allow_fast_forward: true
            }
        ) && merge_base == base_id.detach()
        {
            info!("Fast-forward merge possible");
            return Ok(MergeResult {
                commit_oid: Some(head_id.to_string()),
                conflicts: Vec::new(),
                merged_files: Vec::new(),
                status: MergeStatus::FastForward,
            });
        }

        // Load the three trees so gix can enrich the reported conflicts
        let base_commit = repo
            .find_commit(base_id)
            .map_err(|e| GitError::Gix(e.to_string()))?;
        let head_commit = repo
            .find_commit(head_id)
            .map_err(|e| GitError::Gix(e.to_string()))?;
        let ancestor_commit = repo
            .find_commit(merge_base)
            .map_err(|e| GitError::Gix(e.to_string()))?;
        let base_tree = base_commit
            .tree()
            .map_err(|e| GitError::Gix(e.to_string()))?;
        let head_tree = head_commit
            .tree()
            .map_err(|e| GitError::Gix(e.to_string()))?;
        let ancestor_tree = ancestor_commit
            .tree()
            .map_err(|e| GitError::Gix(e.to_string()))?;
        let tree_merge = self.merge_trees(&repo, &ancestor_tree, &base_tree, &head_tree)?;

        let repo_path = self.config.repo_path(owner, name);
        let base_sha = base_id.to_string();
        let head_sha = head_id.to_string();

        let merged = run_git(
            &repo_path,
            &[
                "merge-tree",
                "--write-tree",
                "--name-only",
                &base_sha,
                &head_sha,
            ],
            &[],
        )
        .map_err(|e| GitError::Internal(format!("failed to run git merge-tree: {e}")))?;

        if !merged.status.success() {
            return Ok(MergeResult {
                commit_oid: None,
                conflicts: tree_merge.conflicts,
                merged_files: tree_merge.merged_files,
                status: MergeStatus::Conflict,
            });
        }

        let stdout = String::from_utf8_lossy(&merged.stdout);
        let tree_oid = stdout.lines().next().unwrap_or_default().trim().to_string();

        let default_message = match mode {
            MergeMode::Squash => format!("Squash {head_ref} into {base_ref}"),
            MergeMode::Merge { .. } => format!("Merge {head_ref} into {base_ref}"),
        };
        let message = message.unwrap_or(default_message.as_str());

        let envs = [
            ("GIT_AUTHOR_NAME", author.name.as_str()),
            ("GIT_AUTHOR_EMAIL", author.email.as_str()),
            ("GIT_COMMITTER_NAME", author.name.as_str()),
            ("GIT_COMMITTER_EMAIL", author.email.as_str()),
        ];
        let parents: Vec<&str> = match mode {
            MergeMode::Squash => vec![base_sha.as_str()],
            MergeMode::Merge { .. } => vec![base_sha.as_str(), head_sha.as_str()],
        };
        let commit_sha = self.create_commit(&repo_path, &tree_oid, &parents, &envs, message)?;

        Ok(MergeResult {
            commit_oid: Some(commit_sha),
            conflicts: Vec::new(),
            merged_files: tree_merge.merged_files,
            status: MergeStatus::Success,
        })
    }

    /// Write a commit object from a tree, parents, and identity via `commit-tree`
    fn create_commit(
        &self,
        repo_path: &Path,
        tree_oid: &str,
        parents: &[&str],
        envs: &[(&str, &str)],
        message: &str,
    ) -> Result<String> {
        let mut args: Vec<&str> = vec!["commit-tree", tree_oid];
        for parent in parents {
            args.push("-p");
            args.push(parent);
        }
        args.push("-m");
        args.push(message);

        let out = run_git(repo_path, &args, envs)
            .map_err(|e| GitError::Internal(format!("failed to run git commit-tree: {e}")))?;
        if !out.status.success() {
            return Err(GitError::Internal(format!(
                "git commit-tree failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Read a commit's original author identity, author date, and message so a
    /// replay can preserve them
    fn commit_metadata(&self, repo_path: &Path, commit_sha: &str) -> Result<CommitMetadata> {
        let out = run_git(
            repo_path,
            &["show", "-s", "--format=%an%x00%ae%x00%ad%x00%B", commit_sha],
            &[],
        )
        .map_err(|e| GitError::Internal(format!("failed to run git show: {e}")))?;
        if !out.status.success() {
            return Err(GitError::Internal(format!(
                "git show failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut parts = stdout.splitn(4, '\0');
        let author_name = parts.next().unwrap_or_default().to_string();
        let author_email = parts.next().unwrap_or_default().to_string();
        let author_date = parts.next().unwrap_or_default().to_string();
        let message = parts.next().unwrap_or_default().trim_end().to_string();
        Ok(CommitMetadata {
            author_name,
            author_email,
            author_date,
            message,
        })
    }

    /// Replay `commit_sha` on top of `onto_sha` with a three-way merge whose base
    /// is the commit's first parent, preserving the original author and message.
    /// Reports whether it committed, was empty, or conflicted; never moves a ref
    fn pick_onto(&self, repo_path: &Path, commit_sha: &str, onto_sha: &str) -> Result<PickOutcome> {
        let parent = run_git(
            repo_path,
            &["rev-parse", "--verify", &format!("{commit_sha}^")],
            &[],
        )
        .map_err(|e| GitError::Internal(format!("failed to run git rev-parse: {e}")))?;
        if !parent.status.success() {
            return Err(GitError::Internal(
                "cannot cherry-pick a root commit".to_string(),
            ));
        }
        let parent_sha = String::from_utf8_lossy(&parent.stdout).trim().to_string();

        let merged = run_git(
            repo_path,
            &[
                "merge-tree",
                "--write-tree",
                "--name-only",
                &format!("--merge-base={parent_sha}"),
                onto_sha,
                commit_sha,
            ],
            &[],
        )
        .map_err(|e| GitError::Internal(format!("failed to run git merge-tree: {e}")))?;

        let stdout = String::from_utf8_lossy(&merged.stdout);
        let mut lines = stdout.lines();
        let tree_oid = lines.next().unwrap_or_default().trim().to_string();

        if !merged.status.success() {
            let conflicts = lines
                .take_while(|line| !line.trim().is_empty())
                .map(|line| ConflictInfo {
                    path: line.trim().to_string(),
                    ours_oid: None,
                    theirs_oid: None,
                    ancestor_oid: None,
                })
                .collect();
            return Ok(PickOutcome::Conflict(conflicts));
        }

        // The change already lands on the new base: an empty pick
        let onto_tree = run_git(
            repo_path,
            &["rev-parse", "--verify", &format!("{onto_sha}^{{tree}}")],
            &[],
        )
        .map_err(|e| GitError::Internal(format!("failed to run git rev-parse: {e}")))?;
        let onto_tree_oid = String::from_utf8_lossy(&onto_tree.stdout)
            .trim()
            .to_string();
        if tree_oid == onto_tree_oid {
            return Ok(PickOutcome::Empty);
        }

        let meta = self.commit_metadata(repo_path, commit_sha)?;
        let envs = [
            ("GIT_AUTHOR_NAME", meta.author_name.as_str()),
            ("GIT_AUTHOR_EMAIL", meta.author_email.as_str()),
            ("GIT_AUTHOR_DATE", meta.author_date.as_str()),
            ("GIT_COMMITTER_NAME", meta.author_name.as_str()),
            ("GIT_COMMITTER_EMAIL", meta.author_email.as_str()),
        ];
        let new_sha =
            self.create_commit(repo_path, &tree_oid, &[onto_sha], &envs, &meta.message)?;
        Ok(PickOutcome::Committed(new_sha))
    }

    /// Cherry-pick a commit onto current HEAD
    #[instrument(skip(self))]
    pub fn cherry_pick(
        &self,
        owner: &str,
        name: &str,
        commit_ref: &str,
        onto_ref: &str,
        _author: Option<&GitActor>,
    ) -> Result<CherryPickResult> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        let commit_id = repo
            .rev_parse_single(commit_ref)
            .map_err(|_| GitError::RefNotFound {
                reference: commit_ref.to_string(),
            })?;

        let onto_id = repo
            .rev_parse_single(onto_ref)
            .map_err(|_| GitError::RefNotFound {
                reference: onto_ref.to_string(),
            })?;

        let repo_path = self.config.repo_path(owner, name);
        match self.pick_onto(&repo_path, &commit_id.to_string(), &onto_id.to_string())? {
            PickOutcome::Committed(oid) => Ok(CherryPickResult {
                commit_oid: Some(oid),
                conflicts: Vec::new(),
                status: CherryPickStatus::Success,
            }),
            PickOutcome::Empty => Ok(CherryPickResult {
                commit_oid: None,
                conflicts: Vec::new(),
                status: CherryPickStatus::EmptyCommit,
            }),
            PickOutcome::Conflict(conflicts) => Ok(CherryPickResult {
                commit_oid: None,
                conflicts,
                status: CherryPickStatus::Conflict,
            }),
        }
    }

    /// Rebase a branch onto another
    #[instrument(skip(self))]
    pub fn rebase(
        &self,
        owner: &str,
        name: &str,
        branch_ref: &str,
        onto_ref: &str,
        _author: Option<&GitActor>,
    ) -> Result<RebaseResult> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        let branch_id = repo
            .rev_parse_single(branch_ref)
            .map_err(|_| GitError::RefNotFound {
                reference: branch_ref.to_string(),
            })?;

        let onto_id = repo
            .rev_parse_single(onto_ref)
            .map_err(|_| GitError::RefNotFound {
                reference: onto_ref.to_string(),
            })?;

        let merge_base = repo
            .merge_base(branch_id.detach(), onto_id.detach())
            .map_err(|e| GitError::Gix(e.to_string()))?;

        if merge_base == branch_id.detach() {
            return Ok(RebaseResult {
                new_head_oid: Some(onto_id.to_string()),
                rebased_commits: Vec::new(),
                conflicts: Vec::new(),
                status: RebaseStatus::NothingToRebase,
            });
        }

        let walk = repo
            .rev_walk([branch_id.detach()])
            .all()
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let mut commits_to_rebase = Vec::new();

        for info in walk {
            let info = info.map_err(|e| GitError::Gix(e.to_string()))?;
            if info.id == merge_base {
                break;
            }
            commits_to_rebase.push(info.id.to_string());
        }

        commits_to_rebase.reverse();

        if commits_to_rebase.is_empty() {
            return Ok(RebaseResult {
                new_head_oid: Some(onto_id.to_string()),
                rebased_commits: Vec::new(),
                conflicts: Vec::new(),
                status: RebaseStatus::NothingToRebase,
            });
        }

        // Replay each commit onto the moving head, threading the new parent
        // through. Stop at the first conflict; drop commits that become empty
        let repo_path = self.config.repo_path(owner, name);
        let mut current = onto_id.to_string();
        let mut rebased_commits = Vec::new();

        for commit_sha in &commits_to_rebase {
            match self.pick_onto(&repo_path, commit_sha, &current)? {
                PickOutcome::Committed(oid) => {
                    current.clone_from(&oid);
                    rebased_commits.push(oid);
                }
                PickOutcome::Empty => {}
                PickOutcome::Conflict(conflicts) => {
                    return Ok(RebaseResult {
                        new_head_oid: None,
                        rebased_commits,
                        conflicts,
                        status: RebaseStatus::Conflict,
                    });
                }
            }
        }

        Ok(RebaseResult {
            new_head_oid: Some(current),
            rebased_commits,
            conflicts: Vec::new(),
            status: RebaseStatus::Success,
        })
    }

    /// Check which objects exist in the repository
    #[instrument(skip(self, oids))]
    pub fn check_objects_exist(
        &self,
        owner: &str,
        name: &str,
        oids: &[String],
    ) -> Result<Vec<(String, bool)>> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        let results: Vec<(String, bool)> = oids
            .iter()
            .map(|oid| {
                let exists = gix::ObjectId::from_hex(oid.as_bytes())
                    .ok()
                    .and_then(|id| repo.find_object(id).ok())
                    .is_some();
                (oid.clone(), exists)
            })
            .collect();

        Ok(results)
    }

    /// Internal: Merge three trees
    fn merge_trees(
        &self,
        _repo: &gix::Repository,
        ancestor: &gix::Tree,
        ours: &gix::Tree,
        theirs: &gix::Tree,
    ) -> Result<TreeMergeResult> {
        use std::collections::HashMap;

        let mut conflicts = Vec::new();
        let mut merged_files = Vec::new();

        // Build maps of changes directly in the callbacks
        let mut ours_map: HashMap<String, ChangeInfo> = HashMap::new();
        let mut theirs_map: HashMap<String, ChangeInfo> = HashMap::new();

        // Get changes from ancestor to ours
        ancestor
            .changes()
            .map_err(|e| GitError::Gix(e.to_string()))?
            .for_each_to_obtain_tree(ours, |change| {
                use gix::object::tree::diff::Action;
                use gix::object::tree::diff::Change;

                let info = match change {
                    Change::Addition { location, id, .. } => ChangeInfo {
                        path: location.to_string(),
                        old_oid: None,
                        new_oid: Some(id.to_string()),
                    },
                    Change::Deletion { location, id, .. } => ChangeInfo {
                        path: location.to_string(),
                        old_oid: Some(id.to_string()),
                        new_oid: None,
                    },
                    Change::Modification {
                        location,
                        previous_id,
                        id,
                        ..
                    } => ChangeInfo {
                        path: location.to_string(),
                        old_oid: Some(previous_id.to_string()),
                        new_oid: Some(id.to_string()),
                    },
                    Change::Rewrite { .. } => {
                        return Ok::<_, std::convert::Infallible>(Action::Continue(()));
                    }
                };
                ours_map.insert(info.path.clone(), info);
                Ok::<_, std::convert::Infallible>(Action::Continue(()))
            })
            .map_err(|e| GitError::Gix(e.to_string()))?;

        // Get changes from ancestor to theirs
        ancestor
            .changes()
            .map_err(|e| GitError::Gix(e.to_string()))?
            .for_each_to_obtain_tree(theirs, |change| {
                use gix::object::tree::diff::Action;
                use gix::object::tree::diff::Change;

                let info = match change {
                    Change::Addition { location, id, .. } => ChangeInfo {
                        path: location.to_string(),
                        old_oid: None,
                        new_oid: Some(id.to_string()),
                    },
                    Change::Deletion { location, id, .. } => ChangeInfo {
                        path: location.to_string(),
                        old_oid: Some(id.to_string()),
                        new_oid: None,
                    },
                    Change::Modification {
                        location,
                        previous_id,
                        id,
                        ..
                    } => ChangeInfo {
                        path: location.to_string(),
                        old_oid: Some(previous_id.to_string()),
                        new_oid: Some(id.to_string()),
                    },
                    Change::Rewrite { .. } => {
                        return Ok::<_, std::convert::Infallible>(Action::Continue(()));
                    }
                };
                theirs_map.insert(info.path.clone(), info);
                Ok::<_, std::convert::Infallible>(Action::Continue(()))
            })
            .map_err(|e| GitError::Gix(e.to_string()))?;

        // Detect conflicts
        for (path, our_change) in &ours_map {
            if let Some(their_change) = theirs_map.get(path) {
                if our_change.new_oid == their_change.new_oid {
                    merged_files.push(path.clone());
                } else {
                    conflicts.push(ConflictInfo {
                        path: path.clone(),
                        ours_oid: our_change.new_oid.clone(),
                        theirs_oid: their_change.new_oid.clone(),
                        ancestor_oid: our_change.old_oid.clone(),
                    });
                }
            } else {
                merged_files.push(path.clone());
            }
        }

        for path in theirs_map.keys() {
            if !ours_map.contains_key(path) {
                merged_files.push(path.clone());
            }
        }

        Ok(TreeMergeResult {
            conflicts,
            merged_files,
        })
    }
}

struct TreeMergeResult {
    conflicts: Vec<ConflictInfo>,
    merged_files: Vec<String>,
}

struct ChangeInfo {
    path: String,
    old_oid: Option<String>,
    new_oid: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command as StdCommand;
    use tempfile::{TempDir, tempdir};

    #[test]
    fn test_merge_status() {
        assert_ne!(MergeStatus::Success, MergeStatus::Conflict);
        assert_ne!(MergeStatus::FastForward, MergeStatus::AlreadyUpToDate);
    }

    #[test]
    fn test_conflict_info() {
        let conflict = ConflictInfo {
            path: "test.txt".to_string(),
            ours_oid: Some("abc123".to_string()),
            theirs_oid: Some("def456".to_string()),
            ancestor_oid: Some("000000".to_string()),
        };

        assert_eq!(conflict.path, "test.txt");
    }

    fn git(cwd: Option<&Path>, args: &[&str]) -> std::process::Output {
        let mut cmd = StdCommand::new("git");
        if let Some(dir) = cwd {
            cmd.arg("-C").arg(dir);
        }
        cmd.args(args);
        // Strip any ambient GIT_* vars (e.g. exported by a pre-commit hook) so
        // seeding acts on the tempdir rather than the surrounding repo
        for (key, _) in std::env::vars() {
            if key.starts_with("GIT_") {
                cmd.env_remove(key);
            }
        }
        cmd.env("GIT_AUTHOR_NAME", "a")
            .env("GIT_AUTHOR_EMAIL", "a@e")
            .env("GIT_COMMITTER_NAME", "a")
            .env("GIT_COMMITTER_EMAIL", "a@e")
            .output()
            .unwrap()
    }

    fn ok(cwd: Option<&Path>, args: &[&str]) -> String {
        let out = git(cwd, args);
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn actor() -> GitActor {
        GitActor {
            name: "Merger".to_string(),
            email: "merger@arbor.dev".to_string(),
            timestamp: 0,
            offset_minutes: 0,
        }
    }

    /// Create the storage dir and an empty bare repo at acme/widget.git, plus a
    /// work tree seeded with a `base.txt` commit pushed to `main`
    fn setup() -> (StorageConfig, TempDir, TempDir, std::path::PathBuf) {
        let storage = tempdir().unwrap();
        let config = StorageConfig {
            base_path: storage.path().to_path_buf(),
            ..Default::default()
        };
        let bare = config.repo_path("acme", "widget");
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        git(
            None,
            &["init", "--bare", "-b", "main", bare.to_str().unwrap()],
        );

        let work = tempdir().unwrap();
        let w = work.path();
        git(Some(w), &["init", "-q", "-b", "main"]);
        std::fs::write(w.join("base.txt"), "base").unwrap();
        ok(Some(w), &["add", "."]);
        ok(Some(w), &["commit", "-q", "-m", "base"]);
        ok(
            Some(w),
            &["push", "-q", bare.to_str().unwrap(), "main:main"],
        );
        (config, storage, work, bare)
    }

    /// Number of parents of a commit in a bare repo
    fn parent_count(bare: &Path, oid: &str) -> usize {
        let line = ok(
            None,
            &[
                "--git-dir",
                bare.to_str().unwrap(),
                "rev-list",
                "--parents",
                "-n",
                "1",
                oid,
            ],
        );
        line.split_whitespace().count() - 1
    }

    fn rev_parse(bare: &Path, rev: &str) -> String {
        ok(
            None,
            &["--git-dir", bare.to_str().unwrap(), "rev-parse", rev],
        )
    }

    // 5a: a divergent merge writes a real two-parent merge commit
    #[test]
    fn merge_writes_a_real_merge_commit() {
        let (config, _storage, work, bare) = setup();
        let w = work.path();
        let bare_s = bare.to_str().unwrap();

        // feature diverges: add feature.txt
        ok(Some(w), &["checkout", "-q", "-b", "feature"]);
        std::fs::write(w.join("feature.txt"), "feature").unwrap();
        ok(Some(w), &["add", "."]);
        ok(Some(w), &["commit", "-q", "-m", "feat"]);
        ok(Some(w), &["push", "-q", bare_s, "feature:feature"]);

        // main advances independently: add main.txt
        ok(Some(w), &["checkout", "-q", "main"]);
        std::fs::write(w.join("main.txt"), "main").unwrap();
        ok(Some(w), &["add", "."]);
        ok(Some(w), &["commit", "-q", "-m", "mainadv"]);
        ok(Some(w), &["push", "-q", bare_s, "main:main"]);

        let ops = OperationsService::new(config);
        let result = ops
            .merge(
                "acme",
                "widget",
                "refs/heads/main",
                "refs/heads/feature",
                &actor(),
                Some("merge feature"),
                false,
            )
            .unwrap();

        assert_eq!(result.status, MergeStatus::Success);
        let oid = result.commit_oid.expect("merge commit oid");
        assert_eq!(parent_count(&bare, &oid), 2, "merge commit has two parents");
        assert_eq!(
            rev_parse(&bare, &format!("{oid}^1")),
            rev_parse(&bare, "main")
        );
        assert_eq!(
            rev_parse(&bare, &format!("{oid}^2")),
            rev_parse(&bare, "feature")
        );
    }

    // 5a: a merge over conflicting edits reports a conflict, no commit
    #[test]
    fn merge_reports_conflicts() {
        let (config, _storage, work, bare) = setup();
        let w = work.path();
        let bare_s = bare.to_str().unwrap();

        ok(Some(w), &["checkout", "-q", "-b", "feature"]);
        std::fs::write(w.join("base.txt"), "feature").unwrap();
        ok(Some(w), &["add", "."]);
        ok(Some(w), &["commit", "-q", "-m", "feat"]);
        ok(Some(w), &["push", "-q", bare_s, "feature:feature"]);

        ok(Some(w), &["checkout", "-q", "main"]);
        std::fs::write(w.join("base.txt"), "mainside").unwrap();
        ok(Some(w), &["add", "."]);
        ok(Some(w), &["commit", "-q", "-m", "mainadv"]);
        ok(Some(w), &["push", "-q", bare_s, "main:main"]);

        let ops = OperationsService::new(config);
        let result = ops
            .merge(
                "acme",
                "widget",
                "refs/heads/main",
                "refs/heads/feature",
                &actor(),
                None,
                false,
            )
            .unwrap();

        assert_eq!(result.status, MergeStatus::Conflict);
        assert!(result.commit_oid.is_none());
        assert!(result.conflicts.iter().any(|c| c.path == "base.txt"));
    }

    // 5b: cherry-pick writes a real single-parent commit onto the target
    #[test]
    fn cherry_pick_writes_a_real_commit() {
        let (config, _storage, work, bare) = setup();
        let w = work.path();
        let bare_s = bare.to_str().unwrap();

        // feature: add feature.txt
        ok(Some(w), &["checkout", "-q", "-b", "feature"]);
        std::fs::write(w.join("feature.txt"), "feature").unwrap();
        ok(Some(w), &["add", "."]);
        ok(Some(w), &["commit", "-q", "-m", "feat"]);
        ok(Some(w), &["push", "-q", bare_s, "feature:feature"]);

        // main diverges: add main.txt
        ok(Some(w), &["checkout", "-q", "main"]);
        std::fs::write(w.join("main.txt"), "main").unwrap();
        ok(Some(w), &["add", "."]);
        ok(Some(w), &["commit", "-q", "-m", "mainadv"]);
        ok(Some(w), &["push", "-q", bare_s, "main:main"]);

        let ops = OperationsService::new(config);
        let result = ops
            .cherry_pick(
                "acme",
                "widget",
                "refs/heads/feature",
                "refs/heads/main",
                None,
            )
            .unwrap();

        assert_eq!(result.status, CherryPickStatus::Success);
        let oid = result.commit_oid.expect("cherry-pick oid");
        assert_eq!(parent_count(&bare, &oid), 1, "single parent");
        assert_eq!(
            rev_parse(&bare, &format!("{oid}^1")),
            rev_parse(&bare, "main")
        );
        let tree = ok(
            None,
            &["--git-dir", bare_s, "ls-tree", "-r", "--name-only", &oid],
        );
        assert!(tree.contains("feature.txt"), "picks the change: {tree}");
    }

    // 5b: cherry-picking a change already present yields an empty commit
    #[test]
    fn cherry_pick_detects_empty_commit() {
        let (config, _storage, work, bare) = setup();
        let w = work.path();
        let bare_s = bare.to_str().unwrap();

        // feature: add same.txt="same"
        ok(Some(w), &["checkout", "-q", "-b", "feature"]);
        std::fs::write(w.join("same.txt"), "same").unwrap();
        ok(Some(w), &["add", "."]);
        ok(Some(w), &["commit", "-q", "-m", "feat"]);
        ok(Some(w), &["push", "-q", bare_s, "feature:feature"]);

        // main independently adds the identical same.txt="same"
        ok(Some(w), &["checkout", "-q", "main"]);
        std::fs::write(w.join("same.txt"), "same").unwrap();
        ok(Some(w), &["add", "."]);
        ok(Some(w), &["commit", "-q", "-m", "mainadv"]);
        ok(Some(w), &["push", "-q", bare_s, "main:main"]);

        let ops = OperationsService::new(config);
        let result = ops
            .cherry_pick(
                "acme",
                "widget",
                "refs/heads/feature",
                "refs/heads/main",
                None,
            )
            .unwrap();

        assert_eq!(result.status, CherryPickStatus::EmptyCommit);
        assert!(result.commit_oid.is_none());
    }

    // 5b: cherry-pick over a conflicting edit reports a conflict
    #[test]
    fn cherry_pick_reports_conflicts() {
        let (config, _storage, work, bare) = setup();
        let w = work.path();
        let bare_s = bare.to_str().unwrap();

        ok(Some(w), &["checkout", "-q", "-b", "feature"]);
        std::fs::write(w.join("base.txt"), "feature").unwrap();
        ok(Some(w), &["add", "."]);
        ok(Some(w), &["commit", "-q", "-m", "feat"]);
        ok(Some(w), &["push", "-q", bare_s, "feature:feature"]);

        ok(Some(w), &["checkout", "-q", "main"]);
        std::fs::write(w.join("base.txt"), "mainside").unwrap();
        ok(Some(w), &["add", "."]);
        ok(Some(w), &["commit", "-q", "-m", "mainadv"]);
        ok(Some(w), &["push", "-q", bare_s, "main:main"]);

        let ops = OperationsService::new(config);
        let result = ops
            .cherry_pick(
                "acme",
                "widget",
                "refs/heads/feature",
                "refs/heads/main",
                None,
            )
            .unwrap();

        assert_eq!(result.status, CherryPickStatus::Conflict);
        assert!(result.commit_oid.is_none());
        assert!(result.conflicts.iter().any(|c| c.path == "base.txt"));
    }

    // 5c: rebase replays a two-commit branch onto a moved base
    #[test]
    fn rebase_replays_commits_and_returns_a_real_head() {
        let (config, _storage, work, bare) = setup();
        let w = work.path();
        let bare_s = bare.to_str().unwrap();

        // feature: two commits ahead of base (f1.txt, f2.txt)
        ok(Some(w), &["checkout", "-q", "-b", "feature"]);
        std::fs::write(w.join("f1.txt"), "1").unwrap();
        ok(Some(w), &["add", "."]);
        ok(Some(w), &["commit", "-q", "-m", "f1"]);
        std::fs::write(w.join("f2.txt"), "2").unwrap();
        ok(Some(w), &["add", "."]);
        ok(Some(w), &["commit", "-q", "-m", "f2"]);
        ok(Some(w), &["push", "-q", bare_s, "feature:feature"]);

        // main moves on independently
        ok(Some(w), &["checkout", "-q", "main"]);
        std::fs::write(w.join("onto.txt"), "onto").unwrap();
        ok(Some(w), &["add", "."]);
        ok(Some(w), &["commit", "-q", "-m", "mainadv"]);
        ok(Some(w), &["push", "-q", bare_s, "main:main"]);

        let ops = OperationsService::new(config);
        let result = ops
            .rebase(
                "acme",
                "widget",
                "refs/heads/feature",
                "refs/heads/main",
                None,
            )
            .unwrap();

        assert_eq!(result.status, RebaseStatus::Success);
        assert_eq!(result.rebased_commits.len(), 2);
        let head = result.new_head_oid.expect("new head");
        assert_eq!(head, result.rebased_commits[1]);
        // the chain sits directly on top of the moved main
        assert_eq!(
            rev_parse(&bare, &format!("{head}~2")),
            rev_parse(&bare, "main")
        );
        let tree = ok(
            None,
            &["--git-dir", bare_s, "ls-tree", "-r", "--name-only", &head],
        );
        assert!(
            tree.contains("onto.txt") && tree.contains("f1.txt") && tree.contains("f2.txt"),
            "rebased head carries base + replayed changes: {tree}"
        );
    }

    // 5c: a conflicting rebase returns Conflict, not Success
    #[test]
    fn rebase_reports_conflicts() {
        let (config, _storage, work, bare) = setup();
        let w = work.path();
        let bare_s = bare.to_str().unwrap();

        ok(Some(w), &["checkout", "-q", "-b", "feature"]);
        std::fs::write(w.join("base.txt"), "feature").unwrap();
        ok(Some(w), &["add", "."]);
        ok(Some(w), &["commit", "-q", "-m", "feat"]);
        ok(Some(w), &["push", "-q", bare_s, "feature:feature"]);

        ok(Some(w), &["checkout", "-q", "main"]);
        std::fs::write(w.join("base.txt"), "mainside").unwrap();
        ok(Some(w), &["add", "."]);
        ok(Some(w), &["commit", "-q", "-m", "mainadv"]);
        ok(Some(w), &["push", "-q", bare_s, "main:main"]);

        let ops = OperationsService::new(config);
        let result = ops
            .rebase(
                "acme",
                "widget",
                "refs/heads/feature",
                "refs/heads/main",
                None,
            )
            .unwrap();

        assert_eq!(result.status, RebaseStatus::Conflict);
        assert!(result.new_head_oid.is_none());
        assert!(result.conflicts.iter().any(|c| c.path == "base.txt"));
    }
}
