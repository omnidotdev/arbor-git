use tracing::instrument;

use super::{GitError, Result, StorageConfig, open_repo_by_name};

pub struct RefService {
    config: StorageConfig,
}

#[derive(Debug, Clone)]
pub struct RefInfo {
    pub name: String,
    pub short_name: String,
    pub oid: String,
    pub ref_type: RefType,
    pub is_default: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefType {
    Branch,
    Tag,
    Remote,
}

impl RefService {
    pub const fn new(config: StorageConfig) -> Self {
        Self { config }
    }

    /// List all references in a repository
    #[instrument(skip(self))]
    pub fn list_refs(
        &self,
        owner: &str,
        name: &str,
        filter_type: Option<RefType>,
        pattern: Option<&str>,
    ) -> Result<Vec<RefInfo>> {
        let repo = open_repo_by_name(&self.config, owner, name)?;
        let refs = repo
            .references()
            .map_err(|e| GitError::Gix(e.to_string()))?;

        // Get default branch for is_default flag
        let default_branch = repo
            .head_ref()
            .ok()
            .flatten()
            .map(|r| r.name().shorten().to_string());

        let mut result = Vec::new();

        // Iterate based on filter type
        let iter: Box<dyn Iterator<Item = _>> = match filter_type {
            Some(RefType::Branch) => Box::new(
                refs.local_branches()
                    .map_err(|e| GitError::Gix(e.to_string()))?,
            ),
            Some(RefType::Tag) => Box::new(refs.tags().map_err(|e| GitError::Gix(e.to_string()))?),
            Some(RefType::Remote) => Box::new(
                refs.remote_branches()
                    .map_err(|e| GitError::Gix(e.to_string()))?,
            ),
            None => Box::new(refs.all().map_err(|e| GitError::Gix(e.to_string()))?),
        };

        for reference in iter {
            let reference = reference.map_err(|e| GitError::Gix(e.to_string()))?;
            let full_name = reference.name().as_bstr().to_string();
            let short_name = reference.name().shorten().to_string();

            // Apply pattern filter if provided
            if let Some(pat) = pattern
                && !glob_match(&short_name, pat)
                && !glob_match(&full_name, pat)
            {
                continue;
            }

            let ref_type = determine_ref_type(&full_name);
            let oid = reference
                .into_fully_peeled_id()
                .map_err(|e| GitError::Gix(e.to_string()))?
                .to_string();

            let is_default = default_branch.as_ref().is_some_and(|db| db == &short_name);

            result.push(RefInfo {
                name: full_name,
                short_name,
                oid,
                ref_type,
                is_default,
            });
        }

        Ok(result)
    }

    /// Resolve a reference to its OID
    #[instrument(skip(self))]
    pub fn resolve_ref(
        &self,
        owner: &str,
        name: &str,
        reference: &str,
    ) -> Result<(String, String)> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        // Try to resolve as a revision spec
        let id = repo
            .rev_parse_single(reference)
            .map_err(|_| GitError::RefNotFound {
                reference: reference.to_string(),
            })?;

        let oid = id.detach().to_string();

        // Try to get the full ref name if it's a named reference
        let resolved_ref = repo
            .find_reference(reference)
            .ok()
            .map_or_else(|| reference.to_string(), |r| r.name().as_bstr().to_string());

        Ok((oid, resolved_ref))
    }

    /// Create a new branch
    #[instrument(skip(self))]
    pub fn create_branch(
        &self,
        owner: &str,
        name: &str,
        branch_name: &str,
        start_point: &str,
    ) -> Result<RefInfo> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        // Resolve start point to a commit
        let target_id = repo
            .rev_parse_single(start_point)
            .map_err(|_| GitError::RefNotFound {
                reference: start_point.to_string(),
            })?;

        // Create the branch reference
        let ref_name = format!("refs/heads/{branch_name}");
        repo.reference(
            ref_name.as_str(),
            target_id.detach(),
            gix::refs::transaction::PreviousValue::MustNotExist,
            format!("branch: Created branch {branch_name}"),
        )
        .map_err(|e| GitError::Gix(e.to_string()))?;

        // Get default branch for is_default check
        let default_branch = repo
            .head_ref()
            .ok()
            .flatten()
            .map(|r| r.name().shorten().to_string());

        let is_default = default_branch.as_ref().is_some_and(|db| db == branch_name);

        Ok(RefInfo {
            name: ref_name,
            short_name: branch_name.to_string(),
            oid: target_id.to_string(),
            ref_type: RefType::Branch,
            is_default,
        })
    }

    /// Delete a branch
    #[instrument(skip(self))]
    pub fn delete_branch(
        &self,
        owner: &str,
        name: &str,
        branch_name: &str,
        force: bool,
    ) -> Result<bool> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        let ref_name = if branch_name.starts_with("refs/") {
            branch_name.to_string()
        } else {
            format!("refs/heads/{branch_name}")
        };

        // Check if branch exists
        let reference = repo
            .find_reference(&ref_name)
            .map_err(|_| GitError::RefNotFound {
                reference: branch_name.to_string(),
            })?;

        // Don't allow deleting default branch unless forced
        if !force
            && let Ok(Some(head_ref)) = repo.head_ref()
            && head_ref.name().as_bstr() == ref_name.as_bytes()
        {
            return Err(GitError::InvalidRef {
                reference: "Cannot delete the default branch".to_string(),
            });
        }

        // Delete the reference
        reference
            .delete()
            .map_err(|e| GitError::Gix(e.to_string()))?;

        Ok(true)
    }

    /// Point HEAD at a branch, making it the repository's default branch.
    ///
    /// The branch must already exist. HEAD is written as a symbolic ref
    /// (`ref: refs/heads/<branch>`), the same on-disk form git uses, so a fresh
    /// clone checks out the new default.
    #[instrument(skip(self))]
    pub fn set_default_branch(&self, owner: &str, name: &str, branch: &str) -> Result<()> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        let ref_name = format!("refs/heads/{branch}");
        repo.find_reference(&ref_name)
            .map_err(|_| GitError::RefNotFound {
                reference: branch.to_string(),
            })?;

        let head_path = self.config.repo_path(owner, name).join("HEAD");
        std::fs::write(&head_path, format!("ref: {ref_name}\n"))
            .map_err(|e| GitError::Gix(e.to_string()))?;

        Ok(())
    }

    /// Create a new tag
    #[instrument(skip(self))]
    pub fn create_tag(
        &self,
        owner: &str,
        name: &str,
        tag_name: &str,
        target: &str,
        _message: Option<&str>,
        _tagger: Option<&GitSignature>,
    ) -> Result<RefInfo> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        // Resolve target to a commit
        let target_id = repo
            .rev_parse_single(target)
            .map_err(|_| GitError::RefNotFound {
                reference: target.to_string(),
            })?;

        let ref_name = format!("refs/tags/{tag_name}");

        // For now, create lightweight tags
        // TODO: Support annotated tags with gitoxide when API stabilizes
        repo.reference(
            ref_name.as_str(),
            target_id.detach(),
            gix::refs::transaction::PreviousValue::MustNotExist,
            format!("tag: Created tag {tag_name}"),
        )
        .map_err(|e| GitError::Gix(e.to_string()))?;

        Ok(RefInfo {
            name: ref_name,
            short_name: tag_name.to_string(),
            oid: target_id.to_string(),
            ref_type: RefType::Tag,
            is_default: false,
        })
    }

    /// Delete a tag
    #[instrument(skip(self))]
    pub fn delete_tag(&self, owner: &str, name: &str, tag_name: &str) -> Result<bool> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        let ref_name = if tag_name.starts_with("refs/") {
            tag_name.to_string()
        } else {
            format!("refs/tags/{tag_name}")
        };

        let reference = repo
            .find_reference(&ref_name)
            .map_err(|_| GitError::RefNotFound {
                reference: tag_name.to_string(),
            })?;

        reference
            .delete()
            .map_err(|e| GitError::Gix(e.to_string()))?;

        Ok(true)
    }
}

#[derive(Debug, Clone)]
pub struct GitSignature {
    pub name: String,
    pub email: String,
    pub timestamp: i64,
    pub offset_minutes: i32,
}

fn determine_ref_type(full_name: &str) -> RefType {
    if full_name.starts_with("refs/heads/") {
        RefType::Branch
    } else if full_name.starts_with("refs/tags/") {
        RefType::Tag
    } else if full_name.starts_with("refs/remotes/") {
        RefType::Remote
    } else {
        RefType::Branch // Default fallback
    }
}

fn glob_match(s: &str, pattern: &str) -> bool {
    // Simple glob matching (supports * wildcard)
    if pattern == "*" {
        return true;
    }

    if let Some(prefix) = pattern.strip_suffix('*') {
        return s.starts_with(prefix);
    }

    if let Some(suffix) = pattern.strip_prefix('*') {
        return s.ends_with(suffix);
    }

    s == pattern
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_match() {
        assert!(glob_match("main", "*"));
        assert!(glob_match("feature/test", "feature/*"));
        assert!(glob_match("test-branch", "*-branch"));
        assert!(glob_match("main", "main"));
        assert!(!glob_match("main", "develop"));
    }

    #[test]
    fn test_determine_ref_type() {
        assert_eq!(determine_ref_type("refs/heads/main"), RefType::Branch);
        assert_eq!(determine_ref_type("refs/tags/v1.0.0"), RefType::Tag);
        assert_eq!(
            determine_ref_type("refs/remotes/origin/main"),
            RefType::Remote
        );
    }
}
