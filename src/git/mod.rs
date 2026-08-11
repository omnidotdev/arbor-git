pub mod commits;
pub mod diff;
pub mod hook;
pub mod operations;
pub mod refs;
pub mod repository;
pub mod scope_match;
pub mod trees;

pub use commits::CommitService;
pub use diff::DiffService;
pub use operations::OperationsService;
pub use refs::RefService;
pub use repository::RepositoryService;
pub use trees::TreeService;

use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum GitError {
    #[error("Repository not found: {owner}/{name}")]
    RepositoryNotFound { owner: String, name: String },

    #[error("Reference not found: {reference}")]
    RefNotFound { reference: String },

    #[error("Object not found: {oid}")]
    ObjectNotFound { oid: String },

    #[error("Invalid reference: {reference}")]
    InvalidRef { reference: String },

    #[error("Merge conflict in {path}")]
    MergeConflict { path: String },

    #[error("Repository already exists: {owner}/{name}")]
    RepositoryExists { owner: String, name: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Git error: {0}")]
    Gix(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl From<gix::open::Error> for GitError {
    fn from(err: gix::open::Error) -> Self {
        Self::Gix(err.to_string())
    }
}

impl From<gix::reference::find::existing::Error> for GitError {
    fn from(err: gix::reference::find::existing::Error) -> Self {
        Self::Gix(err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, GitError>;

/// Configuration for git storage paths
#[derive(Debug, Clone)]
pub struct StorageConfig {
    /// Base path for all repositories
    pub base_path: PathBuf,
    /// Maximum repository size in bytes
    pub max_repo_size: u64,
    /// Default branch name for new repositories
    pub default_branch: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            base_path: PathBuf::from("/var/lib/arbor-git"),
            max_repo_size: 1024 * 1024 * 1024, // 1GB
            default_branch: "main".to_string(),
        }
    }
}

impl StorageConfig {
    pub fn from_env() -> Self {
        Self {
            base_path: std::env::var("STORAGE_PATH")
                .map_or_else(|_| PathBuf::from("/var/lib/arbor-git"), PathBuf::from),
            max_repo_size: std::env::var("GIT_MAX_REPO_SIZE")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1024 * 1024 * 1024),
            default_branch: std::env::var("GIT_DEFAULT_BRANCH")
                .unwrap_or_else(|_| "main".to_string()),
        }
    }

    /// Get the path for a repository
    pub fn repo_path(&self, owner: &str, name: &str) -> PathBuf {
        self.base_path.join(owner).join(format!("{name}.git"))
    }

    /// Directory holding the pre-receive credential-boundary hook. Kept outside
    /// the per-owner repository tree (owners are usernames, never `.arbor-hooks`).
    pub fn hooks_dir(&self) -> PathBuf {
        self.base_path.join(".arbor-hooks")
    }

    /// Write the pre-receive hook that re-invokes this binary as the push
    /// credential boundary, and return its directory. Idempotent; called at boot.
    /// The script execs `$ARBOR_GIT_BIN __pre-receive`, which git runs before
    /// applying any ref update on a confined push (see the `receive_pack` handler).
    pub fn ensure_pre_receive_hook(&self) -> std::io::Result<PathBuf> {
        let dir = self.hooks_dir();
        std::fs::create_dir_all(&dir)?;
        let hook = dir.join("pre-receive");
        std::fs::write(&hook, "#!/bin/sh\nexec \"$ARBOR_GIT_BIN\" __pre-receive\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(dir)
    }
}

/// Open a repository using gitoxide
pub fn open_repo(path: &Path) -> Result<gix::Repository> {
    gix::open(path).map_err(GitError::from)
}

/// Open a repository by owner/name
pub fn open_repo_by_name(
    config: &StorageConfig,
    owner: &str,
    name: &str,
) -> Result<gix::Repository> {
    let path = config.repo_path(owner, name);
    if !path.exists() {
        return Err(GitError::RepositoryNotFound {
            owner: owner.to_string(),
            name: name.to_string(),
        });
    }
    open_repo(&path)
}
