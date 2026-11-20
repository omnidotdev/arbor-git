use std::fs;
use std::path::Path;
use tracing::{info, instrument};

use super::{GitError, Result, StorageConfig};

pub struct RepositoryService {
    config: StorageConfig,
}

impl RepositoryService {
    pub const fn new(config: StorageConfig) -> Self {
        Self { config }
    }

    /// Initialize a new bare repository
    #[instrument(skip(self))]
    pub fn init(&self, owner: &str, name: &str, default_branch: Option<&str>) -> Result<String> {
        let path = self.config.repo_path(owner, name);

        if path.exists() {
            return Err(GitError::RepositoryExists {
                owner: owner.to_string(),
                name: name.to_string(),
            });
        }

        // Create parent directories
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Initialize bare repository with gitoxide
        let branch = default_branch.unwrap_or(&self.config.default_branch);

        let _repo = gix::init_bare(&path).map_err(|e| GitError::Gix(e.to_string()))?;

        // Set default branch in HEAD
        let head_path = path.join("HEAD");
        fs::write(&head_path, format!("ref: refs/heads/{branch}\n"))?;

        info!(path = %path.display(), branch = branch, "Initialized bare repository");

        Ok(path.to_string_lossy().to_string())
    }

    /// Check if a repository exists
    pub fn exists(&self, owner: &str, name: &str) -> bool {
        let path = self.config.repo_path(owner, name);
        path.exists() && path.join("HEAD").exists()
    }

    /// Delete a repository
    #[instrument(skip(self))]
    /// Rename a repository within an owner by moving its bare directory.
    ///
    /// Guarded: a no-op when the name is unchanged, and refused (Ok(false)) when
    /// the source is missing or the target already exists, so a rename never
    /// clobbers another repository.
    pub fn rename(&self, owner: &str, old_name: &str, new_name: &str) -> Result<bool> {
        self.relocate(owner, old_name, owner, new_name)
    }

    /// Move a repository to another owner path (and name), as when it is
    /// transferred to another organization and a new owner. The bare
    /// directory moves as-is, so every ref, object and its history comes
    /// along. Guarded like [`Self::rename`]: a no-op when nothing changes, and
    /// refused (Ok(false)) when the source is missing or the target exists
    pub fn relocate(
        &self,
        owner: &str,
        name: &str,
        new_owner: &str,
        new_name: &str,
    ) -> Result<bool> {
        if owner == new_owner && name == new_name {
            return Ok(true);
        }

        let old_path = self.config.repo_path(owner, name);
        let new_path = self.config.repo_path(new_owner, new_name);

        if !old_path.exists() || new_path.exists() {
            return Ok(false);
        }

        if let Some(parent) = new_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&old_path, &new_path)?;
        info!(from = %old_path.display(), to = %new_path.display(), "Moved repository");

        // Clean up an owner directory the move emptied
        if owner != new_owner
            && let Some(parent) = old_path.parent()
            && parent.read_dir()?.next().is_none()
        {
            let _ = fs::remove_dir(parent);
        }

        Ok(true)
    }

    /// Delete a repository
    pub fn delete(&self, owner: &str, name: &str) -> Result<bool> {
        let path = self.config.repo_path(owner, name);

        if !path.exists() {
            return Ok(false);
        }

        fs::remove_dir_all(&path)?;
        info!(path = %path.display(), "Deleted repository");

        // Clean up empty owner directory
        if let Some(parent) = path.parent()
            && parent.read_dir()?.next().is_none()
        {
            let _ = fs::remove_dir(parent);
        }

        Ok(true)
    }

    /// Get repository info
    #[instrument(skip(self))]
    pub fn get_info(&self, owner: &str, name: &str) -> Result<RepositoryInfo> {
        let path = self.config.repo_path(owner, name);

        if !path.exists() {
            return Err(GitError::RepositoryNotFound {
                owner: owner.to_string(),
                name: name.to_string(),
            });
        }

        let repo = super::open_repo(&path)?;

        // Get HEAD reference
        let head_oid = repo.head_id().ok().map(|id| id.to_string());

        // Get default branch from HEAD
        let default_branch = repo.head_ref().ok().flatten().map_or_else(
            || self.config.default_branch.clone(),
            |r| r.name().shorten().to_string(),
        );

        // Count branches
        let branch_count = repo.references().ok().map_or(0, |refs| {
            refs.local_branches().ok().map_or(0, Iterator::count)
        }) as u32;

        // Count tags
        let tag_count =
            repo.references()
                .ok()
                .map_or(0, |refs| refs.tags().ok().map_or(0, Iterator::count)) as u32;

        // Calculate size
        let size_bytes = calculate_dir_size(&path)?;

        // Count commits (approximation - count objects)
        let commit_count = count_commits(&repo);

        Ok(RepositoryInfo {
            owner: owner.to_string(),
            name: name.to_string(),
            default_branch,
            size_bytes,
            commit_count,
            branch_count,
            tag_count,
            head_oid,
        })
    }
}

#[derive(Debug, Clone)]
pub struct RepositoryInfo {
    pub owner: String,
    pub name: String,
    pub default_branch: String,
    pub size_bytes: u64,
    pub commit_count: u32,
    pub branch_count: u32,
    pub tag_count: u32,
    pub head_oid: Option<String>,
}

fn calculate_dir_size(path: &Path) -> Result<u64> {
    let mut size = 0;

    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;

        if metadata.is_dir() {
            size += calculate_dir_size(&entry.path())?;
        } else {
            size += metadata.len();
        }
    }

    Ok(size)
}

fn count_commits(repo: &gix::Repository) -> u32 {
    // Try to count commits by traversing from HEAD
    let Ok(head) = repo.head_id() else {
        return 0;
    };

    let walk = repo.rev_walk([head]);

    walk.all().map_or(0, |iter| iter.count() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_init_and_exists() {
        let temp = tempdir().unwrap();
        let config = StorageConfig {
            base_path: temp.path().to_path_buf(),
            ..Default::default()
        };

        let service = RepositoryService::new(config);

        assert!(!service.exists("testowner", "testrepo"));

        let path = service.init("testowner", "testrepo", Some("main")).unwrap();
        assert!(Path::new(&path).exists());
        assert!(service.exists("testowner", "testrepo"));
    }

    #[test]
    fn relocate_moves_a_repository_to_another_owner_with_its_history() {
        let temp = tempdir().unwrap();
        let config = StorageConfig {
            base_path: temp.path().to_path_buf(),
            ..Default::default()
        };
        let service = RepositoryService::new(config);
        service.init("alice", "site", Some("master")).unwrap();
        // A marker standing in for the repository's objects and refs
        let marker = temp.path().join("alice/site.git/refs/heads/master");
        fs::write(&marker, "0123456789abcdef0123456789abcdef01234567\n").unwrap();

        assert!(service.relocate("alice", "site", "bob", "site").unwrap());
        assert!(!service.exists("alice", "site"));
        assert!(service.exists("bob", "site"));
        assert!(temp.path().join("bob/site.git/refs/heads/master").exists());
        // The emptied owner directory is gone
        assert!(!temp.path().join("alice").exists());
    }

    #[test]
    fn relocate_never_clobbers_or_invents_a_repository() {
        let temp = tempdir().unwrap();
        let config = StorageConfig {
            base_path: temp.path().to_path_buf(),
            ..Default::default()
        };
        let service = RepositoryService::new(config);
        service.init("alice", "site", None).unwrap();
        service.init("bob", "site", None).unwrap();

        assert!(!service.relocate("alice", "site", "bob", "site").unwrap());
        assert!(service.exists("alice", "site"));
        assert!(
            !service
                .relocate("carol", "missing", "bob", "other")
                .unwrap()
        );
        // Unchanged is a no-op
        assert!(service.relocate("alice", "site", "alice", "site").unwrap());
    }

    #[test]
    fn test_delete() {
        let temp = tempdir().unwrap();
        let config = StorageConfig {
            base_path: temp.path().to_path_buf(),
            ..Default::default()
        };

        let service = RepositoryService::new(config);
        service.init("testowner", "testrepo", None).unwrap();

        assert!(service.delete("testowner", "testrepo").unwrap());
        assert!(!service.exists("testowner", "testrepo"));
    }
}
