use tracing::instrument;

use super::{open_repo_by_name, GitError, Result, StorageConfig};

pub struct CommitService {
    config: StorageConfig,
}

#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub oid: String,
    pub message: String,
    pub author: GitActor,
    pub committer: GitActor,
    pub parent_oids: Vec<String>,
    pub tree_oid: String,
}

#[derive(Debug, Clone)]
pub struct GitActor {
    pub name: String,
    pub email: String,
    pub timestamp: i64,
    pub offset_minutes: i32,
}

impl CommitService {
    pub const fn new(config: StorageConfig) -> Self {
        Self { config }
    }

    /// Get a single commit by OID
    #[instrument(skip(self))]
    pub fn get_commit(&self, owner: &str, name: &str, oid: &str) -> Result<CommitInfo> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        let id = gix::ObjectId::from_hex(oid.as_bytes()).map_err(|_| GitError::ObjectNotFound {
            oid: oid.to_string(),
        })?;

        let commit = repo
            .find_commit(id)
            .map_err(|_| GitError::ObjectNotFound { oid: oid.to_string() })?;

        Ok(commit_to_info(&commit))
    }

    /// Get commit log starting from a reference
    #[instrument(skip(self))]
    pub fn get_log(
        &self,
        owner: &str,
        name: &str,
        start_ref: &str,
        limit: Option<u32>,
        skip: Option<u32>,
        path_filter: Option<&str>,
    ) -> Result<Vec<CommitInfo>> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        // Resolve starting point
        let start_id = repo
            .rev_parse_single(start_ref)
            .map_err(|_| GitError::RefNotFound {
                reference: start_ref.to_string(),
            })?;

        let walk = repo
            .rev_walk([start_id])
            .all()
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let skip_count = skip.unwrap_or(0) as usize;
        let take_count = limit.unwrap_or(100) as usize;

        let mut commits = Vec::new();

        for info in walk.skip(skip_count).take(take_count) {
            let info = info.map_err(|e| GitError::Gix(e.to_string()))?;
            let commit = info.object().map_err(|e| GitError::Gix(e.to_string()))?;

            // If path filter is specified, check if commit touches that path
            if let Some(path) = path_filter {
                if !commit_touches_path(&repo, &commit, path)? {
                    continue;
                }
            }

            commits.push(commit_to_info(&commit));
        }

        Ok(commits)
    }

    /// Get ancestors of a commit
    #[instrument(skip(self))]
    pub fn get_ancestors(
        &self,
        owner: &str,
        name: &str,
        oid: &str,
        limit: Option<u32>,
    ) -> Result<Vec<CommitInfo>> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        let id = gix::ObjectId::from_hex(oid.as_bytes()).map_err(|_| GitError::ObjectNotFound {
            oid: oid.to_string(),
        })?;

        let walk = repo
            .rev_walk([id])
            .all()
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let take_count = limit.unwrap_or(100) as usize;
        let mut commits = Vec::new();

        // Skip the first commit (the one we're starting from)
        for info in walk.skip(1).take(take_count) {
            let info = info.map_err(|e| GitError::Gix(e.to_string()))?;
            let commit = info.object().map_err(|e| GitError::Gix(e.to_string()))?;
            commits.push(commit_to_info(&commit));
        }

        Ok(commits)
    }

    /// Find merge base between two commits
    #[instrument(skip(self))]
    pub fn find_merge_base(&self, owner: &str, name: &str, oid1: &str, oid2: &str) -> Result<Option<String>> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        let id1 = gix::ObjectId::from_hex(oid1.as_bytes()).map_err(|_| GitError::ObjectNotFound {
            oid: oid1.to_string(),
        })?;

        let id2 = gix::ObjectId::from_hex(oid2.as_bytes()).map_err(|_| GitError::ObjectNotFound {
            oid: oid2.to_string(),
        })?;

        // Use gitoxide's merge base functionality
        let base = repo
            .merge_base(id1, id2)
            .map_err(|e| GitError::Gix(e.to_string()))?;

        Ok(Some(base.to_string()))
    }
}

fn commit_to_info(commit: &gix::Commit) -> CommitInfo {
    let author = commit.author().expect("commit has author");
    let committer = commit.committer().expect("commit has committer");

    CommitInfo {
        oid: commit.id().to_string(),
        message: commit
            .message()
            .map(|m| m.title.to_string())
            .unwrap_or_default(),
        author: GitActor {
            name: author.name.to_string(),
            email: author.email.to_string(),
            timestamp: author.time.seconds,
            offset_minutes: author.time.offset,
        },
        committer: GitActor {
            name: committer.name.to_string(),
            email: committer.email.to_string(),
            timestamp: committer.time.seconds,
            offset_minutes: committer.time.offset,
        },
        parent_oids: commit.parent_ids().map(|id| id.to_string()).collect(),
        tree_oid: commit.tree_id().expect("commit has tree").to_string(),
    }
}

fn commit_touches_path(
    repo: &gix::Repository,
    commit: &gix::Commit,
    path: &str,
) -> Result<bool> {
    // Get the commit's tree
    let tree_id = commit.tree_id().map_err(|e| GitError::Gix(e.to_string()))?;
    let tree = repo
        .find_tree(tree_id)
        .map_err(|e| GitError::Gix(e.to_string()))?;

    // Check if the path exists in the tree
    let entry = tree.lookup_entry_by_path(path);
    if entry.is_err() {
        return Ok(false);
    }

    // For more accurate filtering, we'd compare with parent trees
    // For now, just check if the path exists in the commit's tree
    Ok(entry.is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_actor_creation() {
        let actor = GitActor {
            name: "Test User".to_string(),
            email: "test@example.com".to_string(),
            timestamp: 1704067200,
            offset_minutes: 0,
        };

        assert_eq!(actor.name, "Test User");
        assert_eq!(actor.email, "test@example.com");
    }
}
