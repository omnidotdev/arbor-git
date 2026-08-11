use tracing::instrument;

use super::{GitError, Result, StorageConfig, open_repo_by_name};

pub struct DiffService {
    config: StorageConfig,
}

/// Internal struct to collect change info from tree diff callback
struct DiffChangeInfo {
    path: String,
    old_oid: Option<String>,
    new_oid: Option<String>,
    old_mode: Option<u32>,
    new_mode: Option<u32>,
    status: FileStatus,
}

#[derive(Debug, Clone)]
pub struct DiffResult {
    pub old_oid: Option<String>,
    pub new_oid: Option<String>,
    pub files: Vec<FileDiff>,
    pub stats: DiffStats,
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>, // For renames
    pub old_oid: Option<String>,
    pub new_oid: Option<String>,
    pub old_mode: Option<u32>,
    pub new_mode: Option<u32>,
    pub status: FileStatus,
    pub hunks: Vec<DiffHunk>,
    pub is_binary: bool,
}

#[derive(Debug, Clone)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub content: String,
    pub line_type: LineType,
    pub old_line_no: Option<u32>,
    pub new_line_no: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineType {
    Context,
    Addition,
    Deletion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
    TypeChanged,
}

#[derive(Debug, Clone, Default)]
pub struct DiffStats {
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
}

impl DiffService {
    pub const fn new(config: StorageConfig) -> Self {
        Self { config }
    }

    /// Get diff between two commits
    #[instrument(skip(self))]
    pub fn diff_commits(
        &self,
        owner: &str,
        name: &str,
        old_ref: &str,
        new_ref: &str,
        path_filter: Option<&str>,
        context_lines: Option<u32>,
    ) -> Result<DiffResult> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        let new_id = repo
            .rev_parse_single(new_ref)
            .map_err(|_| GitError::RefNotFound {
                reference: new_ref.to_string(),
            })?;

        let new_commit = repo
            .find_commit(new_id)
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let new_tree = new_commit
            .tree()
            .map_err(|e| GitError::Gix(e.to_string()))?;

        // An empty base ref means "diff against the empty tree" (a root commit,
        // or the initial state), so every file appears as an addition
        if old_ref.is_empty() {
            return self.diff_initial_commit(&repo, &new_tree, path_filter, context_lines);
        }

        let old_id = repo
            .rev_parse_single(old_ref)
            .map_err(|_| GitError::RefNotFound {
                reference: old_ref.to_string(),
            })?;

        let old_commit = repo
            .find_commit(old_id)
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let old_tree = old_commit
            .tree()
            .map_err(|e| GitError::Gix(e.to_string()))?;

        self.diff_trees(&repo, &old_tree, &new_tree, path_filter, context_lines)
    }

    /// Get diff for a single commit (against its parent)
    #[instrument(skip(self))]
    pub fn diff_commit(
        &self,
        owner: &str,
        name: &str,
        commit_ref: &str,
        path_filter: Option<&str>,
        context_lines: Option<u32>,
    ) -> Result<DiffResult> {
        let repo = open_repo_by_name(&self.config, owner, name)?;

        let commit_id = repo
            .rev_parse_single(commit_ref)
            .map_err(|_| GitError::RefNotFound {
                reference: commit_ref.to_string(),
            })?;

        let commit = repo
            .find_commit(commit_id)
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let new_tree = commit.tree().map_err(|e| GitError::Gix(e.to_string()))?;

        // Get parent tree (empty tree if no parent)
        let parent_ids: Vec<_> = commit.parent_ids().collect();

        if parent_ids.is_empty() {
            // Initial commit - diff against empty tree
            return self.diff_initial_commit(&repo, &new_tree, path_filter, context_lines);
        }

        let parent_commit = repo
            .find_commit(parent_ids[0])
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let old_tree = parent_commit
            .tree()
            .map_err(|e| GitError::Gix(e.to_string()))?;

        self.diff_trees(&repo, &old_tree, &new_tree, path_filter, context_lines)
    }

    fn diff_trees(
        &self,
        repo: &gix::Repository,
        old_tree: &gix::Tree,
        new_tree: &gix::Tree,
        path_filter: Option<&str>,
        context_lines: Option<u32>,
    ) -> Result<DiffResult> {
        let context = context_lines.unwrap_or(3);

        // Collect change info directly in the callback
        let mut change_infos: Vec<DiffChangeInfo> = Vec::new();
        old_tree
            .changes()
            .map_err(|e| GitError::Gix(e.to_string()))?
            .for_each_to_obtain_tree(new_tree, |change| {
                use gix::object::tree::diff::Action;
                use gix::object::tree::diff::Change;

                let info = match change {
                    Change::Addition {
                        location,
                        entry_mode,
                        id,
                        ..
                    } => DiffChangeInfo {
                        path: location.to_string(),
                        old_oid: None,
                        new_oid: Some(id.to_string()),
                        old_mode: None,
                        new_mode: Some(u32::from(entry_mode.value())),
                        status: FileStatus::Added,
                    },
                    Change::Deletion {
                        location,
                        entry_mode,
                        id,
                        ..
                    } => DiffChangeInfo {
                        path: location.to_string(),
                        old_oid: Some(id.to_string()),
                        new_oid: None,
                        old_mode: Some(u32::from(entry_mode.value())),
                        new_mode: None,
                        status: FileStatus::Deleted,
                    },
                    Change::Modification {
                        location,
                        previous_entry_mode,
                        previous_id,
                        entry_mode,
                        id,
                        ..
                    } => {
                        let status = if previous_entry_mode == entry_mode {
                            FileStatus::Modified
                        } else {
                            FileStatus::TypeChanged
                        };
                        DiffChangeInfo {
                            path: location.to_string(),
                            old_oid: Some(previous_id.to_string()),
                            new_oid: Some(id.to_string()),
                            old_mode: Some(u32::from(previous_entry_mode.value())),
                            new_mode: Some(u32::from(entry_mode.value())),
                            status,
                        }
                    }
                    Change::Rewrite { .. } => {
                        // Treat rewrite as modification
                        return Ok::<_, std::convert::Infallible>(Action::Continue(()));
                    }
                };
                change_infos.push(info);
                Ok::<_, std::convert::Infallible>(Action::Continue(()))
            })
            .map_err(|e| GitError::Gix(e.to_string()))?;

        let mut files = Vec::new();
        let mut diff_stats = DiffStats::default();

        for change in change_infos {
            let path = change.path;
            let old_oid = change.old_oid;
            let new_oid = change.new_oid;
            let old_mode = change.old_mode;
            let new_mode = change.new_mode;
            let file_status = change.status;

            // Apply path filter if specified
            if let Some(filter) = path_filter
                && !path.starts_with(filter)
            {
                continue;
            }

            // Determine if binary
            let is_binary = self.is_file_binary(repo, old_oid.as_deref(), new_oid.as_deref());

            // Generate hunks for text files
            let hunks = if is_binary {
                Vec::new()
            } else {
                self.generate_hunks(repo, old_oid.as_deref(), new_oid.as_deref(), context)?
            };

            // Calculate stats from hunks
            for hunk in &hunks {
                for line in &hunk.lines {
                    match line.line_type {
                        LineType::Addition => diff_stats.insertions += 1,
                        LineType::Deletion => diff_stats.deletions += 1,
                        LineType::Context => {}
                    }
                }
            }

            files.push(FileDiff {
                path,
                old_path: None,
                old_oid,
                new_oid,
                old_mode,
                new_mode,
                status: file_status,
                hunks,
                is_binary,
            });
        }

        diff_stats.files_changed = files.len() as u32;

        Ok(DiffResult {
            old_oid: Some(old_tree.id().to_string()),
            new_oid: Some(new_tree.id().to_string()),
            files,
            stats: diff_stats,
        })
    }

    fn diff_initial_commit(
        &self,
        repo: &gix::Repository,
        tree: &gix::Tree,
        path_filter: Option<&str>,
        context_lines: Option<u32>,
    ) -> Result<DiffResult> {
        let context = context_lines.unwrap_or(3);
        let mut files = Vec::new();
        let mut stats = DiffStats::default();

        // Collect all entries recursively
        self.collect_initial_entries(repo, tree, "", path_filter, context, &mut files, &mut stats)?;

        stats.files_changed = files.len() as u32;

        Ok(DiffResult {
            old_oid: None,
            new_oid: Some(tree.id().to_string()),
            files,
            stats,
        })
    }

    fn collect_initial_entries(
        &self,
        repo: &gix::Repository,
        tree: &gix::Tree,
        base_path: &str,
        path_filter: Option<&str>,
        context: u32,
        files: &mut Vec<FileDiff>,
        stats: &mut DiffStats,
    ) -> Result<()> {
        for entry in tree.iter() {
            let entry = entry.map_err(|e| GitError::Gix(e.to_string()))?;
            let entry_name = entry.filename().to_string();
            let path = if base_path.is_empty() {
                entry_name
            } else {
                format!("{base_path}/{entry_name}")
            };

            if entry.mode().is_tree() {
                // Recurse into subtree
                if let Ok(subtree) = repo.find_tree(entry.object_id()) {
                    self.collect_initial_entries(
                        repo,
                        &subtree,
                        &path,
                        path_filter,
                        context,
                        files,
                        stats,
                    )?;
                }
            } else if entry.mode().is_blob() {
                // Apply path filter
                if let Some(filter) = path_filter
                    && !path.starts_with(filter)
                {
                    continue;
                }

                let oid = entry.object_id().to_string();
                let is_binary = self.is_file_binary(repo, None, Some(&oid));

                let hunks = if is_binary {
                    Vec::new()
                } else {
                    self.generate_hunks(repo, None, Some(&oid), context)?
                };

                for hunk in &hunks {
                    for line in &hunk.lines {
                        if line.line_type == LineType::Addition {
                            stats.insertions += 1;
                        }
                    }
                }

                files.push(FileDiff {
                    path,
                    old_path: None,
                    old_oid: None,
                    new_oid: Some(oid),
                    old_mode: None,
                    new_mode: Some(u32::from(entry.mode().value())),
                    status: FileStatus::Added,
                    hunks,
                    is_binary,
                });
            }
        }

        Ok(())
    }

    fn is_file_binary(
        &self,
        repo: &gix::Repository,
        old_oid: Option<&str>,
        new_oid: Option<&str>,
    ) -> bool {
        // Check new file first, then old
        for oid_str in [new_oid, old_oid].into_iter().flatten() {
            if let Ok(id) = gix::ObjectId::from_hex(oid_str.as_bytes())
                && let Ok(blob) = repo.find_blob(id)
            {
                let data = &blob.data;
                let check_len = std::cmp::min(data.len(), 8192);
                if data[..check_len].contains(&0) {
                    return true;
                }
            }
        }
        false
    }

    fn generate_hunks(
        &self,
        repo: &gix::Repository,
        old_oid: Option<&str>,
        new_oid: Option<&str>,
        context: u32,
    ) -> Result<Vec<DiffHunk>> {
        let old_content = old_oid
            .and_then(|oid| gix::ObjectId::from_hex(oid.as_bytes()).ok())
            .and_then(|id| repo.find_blob(id).ok())
            .map(|b| String::from_utf8_lossy(&b.data).to_string())
            .unwrap_or_default();

        let new_content = new_oid
            .and_then(|oid| gix::ObjectId::from_hex(oid.as_bytes()).ok())
            .and_then(|id| repo.find_blob(id).ok())
            .map(|b| String::from_utf8_lossy(&b.data).to_string())
            .unwrap_or_default();

        let old_lines: Vec<&str> = old_content.lines().collect();
        let new_lines: Vec<&str> = new_content.lines().collect();

        // Simple Myers diff algorithm implementation
        let hunks = compute_diff_hunks(&old_lines, &new_lines, context as usize);

        Ok(hunks)
    }
}

/// Compute diff hunks using a simplified diff algorithm
fn compute_diff_hunks(old: &[&str], new: &[&str], context: usize) -> Vec<DiffHunk> {
    let m = old.len();
    let n = new.len();

    if m == 0 && n == 0 {
        return Vec::new();
    }

    // For very large files, fall back to a simpler approach
    if m * n > 10_000_000 {
        return simple_diff_hunks(old, new);
    }

    // Standard LCS DP
    let mut dp = vec![vec![0usize; n + 1]; m + 1];

    for i in 1..=m {
        for j in 1..=n {
            if old[i - 1] == new[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = std::cmp::max(dp[i - 1][j], dp[i][j - 1]);
            }
        }
    }

    // Backtrack to find the diff
    let mut edits = Vec::new();
    let mut i = m;
    let mut j = n;

    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old[i - 1] == new[j - 1] {
            edits.push((EditOp::Equal, i - 1, j - 1));
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            edits.push((EditOp::Insert, i, j - 1));
            j -= 1;
        } else {
            edits.push((EditOp::Delete, i - 1, j));
            i -= 1;
        }
    }

    edits.reverse();

    // Group edits into hunks with context
    group_edits_into_hunks(&edits, old, new, context)
}

#[derive(Clone, Copy, PartialEq)]
enum EditOp {
    Equal,
    Insert,
    Delete,
}

fn group_edits_into_hunks(
    edits: &[(EditOp, usize, usize)],
    old: &[&str],
    new: &[&str],
    context: usize,
) -> Vec<DiffHunk> {
    if edits.is_empty() {
        return Vec::new();
    }

    let mut hunks = Vec::new();
    let mut current_hunk: Option<DiffHunk> = None;
    let mut last_change_idx = 0usize;

    for (idx, (op, old_idx, new_idx)) in edits.iter().enumerate() {
        let is_change = *op != EditOp::Equal;

        if is_change {
            // Start a new hunk if needed
            if current_hunk.is_none() {
                let start_context = idx.saturating_sub(context);
                let old_start = if start_context < edits.len() {
                    edits[start_context].1
                } else {
                    *old_idx
                };
                let new_start = if start_context < edits.len() {
                    edits[start_context].2
                } else {
                    *new_idx
                };

                current_hunk = Some(DiffHunk {
                    old_start: (old_start + 1) as u32,
                    old_lines: 0,
                    new_start: (new_start + 1) as u32,
                    new_lines: 0,
                    header: String::new(),
                    lines: Vec::new(),
                });

                // Add leading context
                for ctx_idx in start_context..idx {
                    if ctx_idx < edits.len() {
                        let (ctx_op, ctx_old, ctx_new) = edits[ctx_idx];
                        if ctx_op == EditOp::Equal
                            && ctx_old < old.len()
                            && let Some(ref mut hunk) = current_hunk
                        {
                            hunk.lines.push(DiffLine {
                                content: old[ctx_old].to_string(),
                                line_type: LineType::Context,
                                old_line_no: Some((ctx_old + 1) as u32),
                                new_line_no: Some((ctx_new + 1) as u32),
                            });
                            hunk.old_lines += 1;
                            hunk.new_lines += 1;
                        }
                    }
                }
            }

            last_change_idx = idx;
        }

        // Add line to current hunk
        if let Some(ref mut hunk) = current_hunk {
            match op {
                EditOp::Equal => {
                    // Only add if within context range of a change
                    if idx <= last_change_idx + context {
                        if *old_idx < old.len() {
                            hunk.lines.push(DiffLine {
                                content: old[*old_idx].to_string(),
                                line_type: LineType::Context,
                                old_line_no: Some((*old_idx + 1) as u32),
                                new_line_no: Some((*new_idx + 1) as u32),
                            });
                            hunk.old_lines += 1;
                            hunk.new_lines += 1;
                        }
                    } else if idx > last_change_idx + context {
                        // Finalize hunk
                        hunk.header = format!(
                            "@@ -{},{} +{},{} @@",
                            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
                        );
                        hunks.push(current_hunk.take().unwrap());
                    }
                }
                EditOp::Delete => {
                    if *old_idx < old.len() {
                        hunk.lines.push(DiffLine {
                            content: old[*old_idx].to_string(),
                            line_type: LineType::Deletion,
                            old_line_no: Some((*old_idx + 1) as u32),
                            new_line_no: None,
                        });
                        hunk.old_lines += 1;
                    }
                }
                EditOp::Insert => {
                    if *new_idx < new.len() {
                        hunk.lines.push(DiffLine {
                            content: new[*new_idx].to_string(),
                            line_type: LineType::Addition,
                            old_line_no: None,
                            new_line_no: Some((*new_idx + 1) as u32),
                        });
                        hunk.new_lines += 1;
                    }
                }
            }
        }
    }

    // Finalize last hunk
    if let Some(mut hunk) = current_hunk {
        hunk.header = format!(
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
        );
        hunks.push(hunk);
    }

    hunks
}

/// Fallback for very large files
fn simple_diff_hunks(old: &[&str], new: &[&str]) -> Vec<DiffHunk> {
    // Just show all old lines as deleted and all new lines as added
    let mut lines = Vec::new();

    for (i, line) in old.iter().enumerate() {
        lines.push(DiffLine {
            content: line.to_string(),
            line_type: LineType::Deletion,
            old_line_no: Some((i + 1) as u32),
            new_line_no: None,
        });
    }

    for (i, line) in new.iter().enumerate() {
        lines.push(DiffLine {
            content: line.to_string(),
            line_type: LineType::Addition,
            old_line_no: None,
            new_line_no: Some((i + 1) as u32),
        });
    }

    if lines.is_empty() {
        return Vec::new();
    }

    vec![DiffHunk {
        old_start: 1,
        old_lines: old.len() as u32,
        new_start: 1,
        new_lines: new.len() as u32,
        header: format!("@@ -1,{} +1,{} @@", old.len(), new.len()),
        lines,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_diff() {
        let old = vec!["line1", "line2", "line3"];
        let new = vec!["line1", "modified", "line3"];

        let hunks = compute_diff_hunks(&old, &new, 3);
        assert!(!hunks.is_empty());
    }

    #[test]
    fn test_addition_diff() {
        let old = vec!["line1", "line2"];
        let new = vec!["line1", "line2", "line3"];

        let hunks = compute_diff_hunks(&old, &new, 3);
        assert!(!hunks.is_empty());

        // Should have an addition
        let has_addition = hunks
            .iter()
            .any(|h| h.lines.iter().any(|l| l.line_type == LineType::Addition));
        assert!(has_addition);
    }

    #[test]
    fn test_deletion_diff() {
        let old = vec!["line1", "line2", "line3"];
        let new = vec!["line1", "line3"];

        let hunks = compute_diff_hunks(&old, &new, 3);
        assert!(!hunks.is_empty());

        let has_deletion = hunks
            .iter()
            .any(|h| h.lines.iter().any(|l| l.line_type == LineType::Deletion));
        assert!(has_deletion);
    }

    #[test]
    fn test_empty_diff() {
        let old: Vec<&str> = vec![];
        let new: Vec<&str> = vec![];

        let hunks = compute_diff_hunks(&old, &new, 3);
        assert!(hunks.is_empty());
    }
}
