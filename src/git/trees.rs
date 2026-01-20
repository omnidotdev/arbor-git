use gix::object::tree::EntryKind;
use tracing::instrument;

use super::{open_repo_by_name, GitError, Result, StorageConfig};

pub struct TreeService {
    config: StorageConfig,
}

#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub name: String,
    pub path: String,
    pub oid: String,
    pub mode: u32,
    pub entry_type: EntryType,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryType {
    Blob,
    Tree,
    Commit, // Submodule
    Link,
}

impl From<EntryKind> for EntryType {
    fn from(kind: EntryKind) -> Self {
        match kind {
            EntryKind::Blob | EntryKind::BlobExecutable => EntryType::Blob,
            EntryKind::Tree => EntryType::Tree,
            EntryKind::Commit => EntryType::Commit,
            EntryKind::Link => EntryType::Link,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BlobInfo {
    pub oid: String,
    pub size: u64,
    pub is_binary: bool,
}

impl TreeService {
    pub fn new(config: StorageConfig) -> Self {
        Self { config }
    }

    /// Get tree entries at a specific path
    #[instrument(skip(self))]
    pub fn get_tree(
        &self,
        owner: &str,
        name: &str,
        tree_ish: &str,
        path: Option<&str>,
    ) -> Result<Vec<TreeEntry>> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        // Resolve tree-ish to a commit, then get its tree
        let commit_id = repo
            .rev_parse_single(tree_ish)
            .map_err(|_| GitError::RefNotFound {
                reference: tree_ish.to_string(),
            })?;

        let commit = repo
            .find_commit(commit_id)
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let tree_id = commit.tree_id().map_err(|e| GitError::Gix(e.to_string()))?;
        let tree = repo
            .find_tree(tree_id)
            .map_err(|e| GitError::Gix(e.to_string()))?;

        // If path is specified, navigate to that subtree
        let (target_tree, base_path) = if let Some(p) = path {
            if p.is_empty() || p == "/" {
                (tree, String::new())
            } else {
                let entry = tree
                    .lookup_entry_by_path(p)
                    .map_err(|e| GitError::Gix(e.to_string()))?
                    .ok_or_else(|| GitError::ObjectNotFound { oid: p.to_string() })?;

                if entry.mode().is_tree() {
                    let subtree = repo
                        .find_tree(entry.object_id())
                        .map_err(|e| GitError::Gix(e.to_string()))?;
                    (subtree, format!("{}/", p.trim_end_matches('/')))
                } else {
                    return Err(GitError::InvalidRef {
                        reference: format!("{} is not a directory", p),
                    });
                }
            }
        } else {
            (tree, String::new())
        };

        let mut entries = Vec::new();

        for entry in target_tree.iter() {
            let entry = entry.map_err(|e| GitError::Gix(e.to_string()))?;
            let entry_name = entry.filename().to_string();
            let entry_path = format!("{}{}", base_path, entry_name);
            let entry_type = EntryType::from(entry.mode().kind());

            // Get size for blobs
            let size = if entry_type == EntryType::Blob {
                repo.find_blob(entry.object_id())
                    .ok()
                    .map(|b| b.data.len() as u64)
            } else {
                None
            };

            entries.push(TreeEntry {
                name: entry_name,
                path: entry_path,
                oid: entry.object_id().to_string(),
                mode: entry.mode().0 as u32,
                entry_type,
                size,
            });
        }

        // Sort: directories first, then by name
        entries.sort_by(|a, b| {
            match (a.entry_type == EntryType::Tree, b.entry_type == EntryType::Tree) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.name.cmp(&b.name),
            }
        });

        Ok(entries)
    }

    /// Get blob content
    #[instrument(skip(self))]
    pub fn get_blob(
        &self,
        owner: &str,
        name: &str,
        oid: &str,
    ) -> Result<Vec<u8>> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        let id = gix::ObjectId::from_hex(oid.as_bytes()).map_err(|_| GitError::ObjectNotFound {
            oid: oid.to_string(),
        })?;

        let blob = repo
            .find_blob(id)
            .map_err(|_| GitError::ObjectNotFound { oid: oid.to_string() })?;

        Ok(blob.data.to_vec())
    }

    /// Get blob content by path
    #[instrument(skip(self))]
    pub fn get_blob_by_path(
        &self,
        owner: &str,
        name: &str,
        tree_ish: &str,
        path: &str,
    ) -> Result<Vec<u8>> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        // Resolve tree-ish to a commit
        let commit_id = repo
            .rev_parse_single(tree_ish)
            .map_err(|_| GitError::RefNotFound {
                reference: tree_ish.to_string(),
            })?;

        let commit = repo
            .find_commit(commit_id)
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let tree_id = commit.tree_id().map_err(|e| GitError::Gix(e.to_string()))?;
        let tree = repo
            .find_tree(tree_id)
            .map_err(|e| GitError::Gix(e.to_string()))?;

        // Find the entry at the path
        let entry = tree
            .lookup_entry_by_path(path)
            .map_err(|e| GitError::Gix(e.to_string()))?
            .ok_or_else(|| GitError::ObjectNotFound {
                oid: path.to_string(),
            })?;

        if !entry.mode().is_blob() {
            return Err(GitError::InvalidRef {
                reference: format!("{} is not a file", path),
            });
        }

        let blob = repo
            .find_blob(entry.object_id())
            .map_err(|e| GitError::Gix(e.to_string()))?;

        Ok(blob.data.to_vec())
    }

    /// Get blob info without content
    #[instrument(skip(self))]
    pub fn get_blob_info(
        &self,
        owner: &str,
        name: &str,
        oid: &str,
    ) -> Result<BlobInfo> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        let id = gix::ObjectId::from_hex(oid.as_bytes()).map_err(|_| GitError::ObjectNotFound {
            oid: oid.to_string(),
        })?;

        let blob = repo
            .find_blob(id)
            .map_err(|_| GitError::ObjectNotFound { oid: oid.to_string() })?;

        let data = &blob.data;
        let is_binary = is_binary_content(data);

        Ok(BlobInfo {
            oid: oid.to_string(),
            size: data.len() as u64,
            is_binary,
        })
    }

    /// Recursively get all entries in a tree
    #[instrument(skip(self))]
    pub fn get_tree_recursive(
        &self,
        owner: &str,
        name: &str,
        tree_ish: &str,
        path: Option<&str>,
        max_depth: Option<u32>,
    ) -> Result<Vec<TreeEntry>> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        let commit_id = repo
            .rev_parse_single(tree_ish)
            .map_err(|_| GitError::RefNotFound {
                reference: tree_ish.to_string(),
            })?;

        let commit = repo
            .find_commit(commit_id)
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let tree_id = commit.tree_id().map_err(|e| GitError::Gix(e.to_string()))?;
        let tree = repo
            .find_tree(tree_id)
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let (start_tree, base_path) = if let Some(p) = path {
            if p.is_empty() || p == "/" {
                (tree, String::new())
            } else {
                let entry = tree
                    .lookup_entry_by_path(p)
                    .map_err(|e| GitError::Gix(e.to_string()))?
                    .ok_or_else(|| GitError::ObjectNotFound { oid: p.to_string() })?;

                if entry.mode().is_tree() {
                    let subtree = repo
                        .find_tree(entry.object_id())
                        .map_err(|e| GitError::Gix(e.to_string()))?;
                    (subtree, format!("{}/", p.trim_end_matches('/')))
                } else {
                    return Err(GitError::InvalidRef {
                        reference: format!("{} is not a directory", p),
                    });
                }
            }
        } else {
            (tree, String::new())
        };

        let max_depth = max_depth.unwrap_or(10);
        let mut entries = Vec::new();

        self.collect_tree_entries(&repo, &start_tree, &base_path, 0, max_depth, &mut entries)?;

        Ok(entries)
    }

    fn collect_tree_entries(
        &self,
        repo: &gix::Repository,
        tree: &gix::Tree,
        base_path: &str,
        current_depth: u32,
        max_depth: u32,
        entries: &mut Vec<TreeEntry>,
    ) -> Result<()> {
        if current_depth > max_depth {
            return Ok(());
        }

        for entry in tree.iter() {
            let entry = entry.map_err(|e| GitError::Gix(e.to_string()))?;
            let entry_name = entry.filename().to_string();
            let entry_path = format!("{}{}", base_path, entry_name);
            let entry_type = EntryType::from(entry.mode().kind());

            let size = if entry_type == EntryType::Blob {
                repo.find_blob(entry.object_id())
                    .ok()
                    .map(|b| b.data.len() as u64)
            } else {
                None
            };

            entries.push(TreeEntry {
                name: entry_name,
                path: entry_path.clone(),
                oid: entry.object_id().to_string(),
                mode: entry.mode().0 as u32,
                entry_type,
                size,
            });

            // Recurse into subtrees
            if entry_type == EntryType::Tree && current_depth < max_depth {
                if let Ok(subtree) = repo.find_tree(entry.object_id()) {
                    let subtree_base = format!("{}/", entry_path);
                    self.collect_tree_entries(
                        repo,
                        &subtree,
                        &subtree_base,
                        current_depth + 1,
                        max_depth,
                        entries,
                    )?;
                }
            }
        }

        Ok(())
    }
}

/// Check if content appears to be binary
fn is_binary_content(data: &[u8]) -> bool {
    // Check first 8KB for null bytes (common binary indicator)
    let check_len = std::cmp::min(data.len(), 8192);
    data[..check_len].contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entry_type_from_kind() {
        assert_eq!(EntryType::from(EntryKind::Blob), EntryType::Blob);
        assert_eq!(EntryType::from(EntryKind::BlobExecutable), EntryType::Blob);
        assert_eq!(EntryType::from(EntryKind::Tree), EntryType::Tree);
        assert_eq!(EntryType::from(EntryKind::Commit), EntryType::Commit);
        assert_eq!(EntryType::from(EntryKind::Link), EntryType::Link);
    }

    #[test]
    fn test_is_binary_content() {
        assert!(!is_binary_content(b"hello world"));
        assert!(!is_binary_content(b"fn main() {\n    println!(\"test\");\n}"));
        assert!(is_binary_content(b"\x00\x01\x02\x03"));
        assert!(is_binary_content(b"PNG\x00\x00\x00"));
    }
}
