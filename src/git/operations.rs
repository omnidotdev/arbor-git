use tracing::{info, instrument, warn};

use super::commits::GitActor;
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

impl OperationsService {
    pub const fn new(config: StorageConfig) -> Self {
        Self { config }
    }

    /// Perform a merge operation
    #[instrument(skip(self))]
    pub fn merge(
        &self,
        owner: &str,
        name: &str,
        base_ref: &str,
        head_ref: &str,
        _author: &GitActor,
        message: Option<&str>,
        allow_fast_forward: bool,
    ) -> Result<MergeResult> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        // Resolve both refs
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

        // Check if already up to date
        if base_id == head_id {
            return Ok(MergeResult {
                commit_oid: Some(base_id.to_string()),
                conflicts: Vec::new(),
                merged_files: Vec::new(),
                status: MergeStatus::AlreadyUpToDate,
            });
        }

        // Find merge base
        let merge_base = repo
            .merge_base(base_id.detach(), head_id.detach())
            .map_err(|e| GitError::Gix(e.to_string()))?;

        // Check for fast-forward possibility
        if allow_fast_forward && merge_base == base_id.detach() {
            info!("Fast-forward merge possible");
            return Ok(MergeResult {
                commit_oid: Some(head_id.to_string()),
                conflicts: Vec::new(),
                merged_files: Vec::new(),
                status: MergeStatus::FastForward,
            });
        }

        // Perform three-way merge
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

        // Perform tree merge
        let merge_result = self.merge_trees(&repo, &ancestor_tree, &base_tree, &head_tree)?;

        if !merge_result.conflicts.is_empty() {
            return Ok(MergeResult {
                commit_oid: None,
                conflicts: merge_result.conflicts,
                merged_files: merge_result.merged_files,
                status: MergeStatus::Conflict,
            });
        }

        // Create merge commit
        let _msg = message.unwrap_or_else(|| {
            Box::leak(format!("Merge {head_ref} into {base_ref}").into_boxed_str())
        });

        warn!("Merge commit creation not fully implemented - returning theoretical result");

        Ok(MergeResult {
            commit_oid: None,
            conflicts: Vec::new(),
            merged_files: merge_result.merged_files,
            status: MergeStatus::Success,
        })
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

        let commit = repo
            .find_commit(commit_id)
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let onto_commit = repo
            .find_commit(onto_id)
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let parent_ids: Vec<_> = commit.parent_ids().collect();
        if parent_ids.is_empty() {
            return Err(GitError::Internal(
                "Cannot cherry-pick initial commit".to_string(),
            ));
        }

        let parent_commit = repo
            .find_commit(parent_ids[0])
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let parent_tree = parent_commit
            .tree()
            .map_err(|e| GitError::Gix(e.to_string()))?;
        let commit_tree = commit.tree().map_err(|e| GitError::Gix(e.to_string()))?;
        let onto_tree = onto_commit
            .tree()
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let merge_result = self.merge_trees(&repo, &parent_tree, &onto_tree, &commit_tree)?;

        if !merge_result.conflicts.is_empty() {
            return Ok(CherryPickResult {
                commit_oid: None,
                conflicts: merge_result.conflicts,
                status: CherryPickStatus::Conflict,
            });
        }

        if merge_result.merged_files.is_empty() {
            return Ok(CherryPickResult {
                commit_oid: None,
                conflicts: Vec::new(),
                status: CherryPickStatus::EmptyCommit,
            });
        }

        warn!("Cherry-pick commit creation not fully implemented");

        Ok(CherryPickResult {
            commit_oid: None,
            conflicts: Vec::new(),
            status: CherryPickStatus::Success,
        })
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

        warn!(
            "Full rebase not implemented - would rebase {} commits",
            commits_to_rebase.len()
        );

        Ok(RebaseResult {
            new_head_oid: None,
            rebased_commits: commits_to_rebase,
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
}
