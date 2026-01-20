use std::pin::Pin;

use futures::Stream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{error, info, instrument};

use crate::git::{
    commits::CommitService,
    diff::DiffService,
    operations::OperationsService,
    refs::{RefService, RefType as InternalRefType},
    repository::RepositoryService,
    trees::TreeService,
    StorageConfig,
};
use crate::proto::*;

pub struct GitServiceImpl {
    repo_service: RepositoryService,
    ref_service: RefService,
    commit_service: CommitService,
    tree_service: TreeService,
    diff_service: DiffService,
    ops_service: OperationsService,
}

impl GitServiceImpl {
    pub fn new(config: StorageConfig) -> Self {
        Self {
            repo_service: RepositoryService::new(config.clone()),
            ref_service: RefService::new(config.clone()),
            commit_service: CommitService::new(config.clone()),
            tree_service: TreeService::new(config.clone()),
            diff_service: DiffService::new(config.clone()),
            ops_service: OperationsService::new(config),
        }
    }
}

type StreamResult<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

#[tonic::async_trait]
impl git_service_server::GitService for GitServiceImpl {
    // ========================================================================
    // Repository Operations
    // ========================================================================

    #[instrument(skip(self))]
    async fn init_repository(
        &self,
        request: Request<InitRepositoryRequest>,
    ) -> Result<Response<InitRepositoryResponse>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        let default_branch = if req.default_branch.is_empty() {
            None
        } else {
            Some(req.default_branch.as_str())
        };

        match self.repo_service.init(&repo.owner, &repo.name, default_branch) {
            Ok(path) => {
                info!(owner = %repo.owner, name = %repo.name, "Repository initialized");
                Ok(Response::new(InitRepositoryResponse {
                    created: true,
                    path,
                }))
            }
            Err(e) => {
                error!(error = %e, "Failed to initialize repository");
                Err(Status::internal(e.to_string()))
            }
        }
    }

    #[instrument(skip(self))]
    async fn delete_repository(
        &self,
        request: Request<DeleteRepositoryRequest>,
    ) -> Result<Response<DeleteRepositoryResponse>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self.repo_service.delete(&repo.owner, &repo.name) {
            Ok(deleted) => Ok(Response::new(DeleteRepositoryResponse { deleted })),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    #[instrument(skip(self))]
    async fn repository_exists(
        &self,
        request: Request<RepositoryExistsRequest>,
    ) -> Result<Response<RepositoryExistsResponse>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        let exists = self.repo_service.exists(&repo.owner, &repo.name);
        Ok(Response::new(RepositoryExistsResponse { exists }))
    }

    #[instrument(skip(self))]
    async fn get_repository_info(
        &self,
        request: Request<GetRepositoryInfoRequest>,
    ) -> Result<Response<RepositoryInfo>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self.repo_service.get_info(&repo.owner, &repo.name) {
            Ok(info) => Ok(Response::new(RepositoryInfo {
                repository: Some(RepositoryPath {
                    owner: info.owner,
                    name: info.name,
                }),
                default_branch: info.default_branch,
                size_bytes: info.size_bytes,
                commit_count: info.commit_count,
                branch_count: info.branch_count,
                tag_count: info.tag_count,
                head_oid: info.head_oid.unwrap_or_default(),
            })),
            Err(e) => Err(Status::not_found(e.to_string())),
        }
    }

    // ========================================================================
    // Reference Operations
    // ========================================================================

    #[instrument(skip(self))]
    async fn list_refs(
        &self,
        request: Request<ListRefsRequest>,
    ) -> Result<Response<ListRefsResponse>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        let filter_type = match req.filter_type {
            0 => None,
            1 => Some(InternalRefType::Branch),
            2 => Some(InternalRefType::Tag),
            3 => Some(InternalRefType::Remote),
            _ => None,
        };

        let pattern = if req.pattern.is_empty() {
            None
        } else {
            Some(req.pattern.as_str())
        };

        match self.ref_service.list_refs(&repo.owner, &repo.name, filter_type, pattern) {
            Ok(refs) => {
                let proto_refs: Vec<Ref> = refs
                    .into_iter()
                    .map(|r| Ref {
                        name: r.name,
                        short_name: r.short_name,
                        oid: r.oid,
                        r#type: match r.ref_type {
                            InternalRefType::Branch => RefType::Branch as i32,
                            InternalRefType::Tag => RefType::Tag as i32,
                            InternalRefType::Remote => RefType::Remote as i32,
                        },
                        is_default: r.is_default,
                    })
                    .collect();

                Ok(Response::new(ListRefsResponse { refs: proto_refs }))
            }
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    #[instrument(skip(self))]
    async fn resolve_ref(
        &self,
        request: Request<ResolveRefRequest>,
    ) -> Result<Response<ResolveRefResponse>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self.ref_service.resolve_ref(&repo.owner, &repo.name, &req.r#ref) {
            Ok((oid, resolved_ref)) => Ok(Response::new(ResolveRefResponse { oid, resolved_ref })),
            Err(e) => Err(Status::not_found(e.to_string())),
        }
    }

    #[instrument(skip(self))]
    async fn create_branch(
        &self,
        request: Request<CreateBranchRequest>,
    ) -> Result<Response<CreateBranchResponse>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self.ref_service.create_branch(&repo.owner, &repo.name, &req.name, &req.start_point) {
            Ok(ref_info) => Ok(Response::new(CreateBranchResponse {
                branch: Some(Ref {
                    name: ref_info.name,
                    short_name: ref_info.short_name,
                    oid: ref_info.oid,
                    r#type: RefType::Branch as i32,
                    is_default: ref_info.is_default,
                }),
            })),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    #[instrument(skip(self))]
    async fn delete_branch(
        &self,
        request: Request<DeleteBranchRequest>,
    ) -> Result<Response<DeleteBranchResponse>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self.ref_service.delete_branch(&repo.owner, &repo.name, &req.name, req.force) {
            Ok(deleted) => Ok(Response::new(DeleteBranchResponse { deleted })),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    #[instrument(skip(self))]
    async fn create_tag(
        &self,
        request: Request<CreateTagRequest>,
    ) -> Result<Response<CreateTagResponse>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        // Convert protobuf signature to internal type if provided
        let tagger = req.tagger.as_ref().map(|t| crate::git::refs::GitSignature {
            name: t.name.clone(),
            email: t.email.clone(),
            timestamp: t.timestamp,
            offset_minutes: t.offset_minutes,
        });

        match self.ref_service.create_tag(
            &repo.owner,
            &repo.name,
            &req.name,
            &req.target,
            req.message.as_deref(),
            tagger.as_ref(),
        ) {
            Ok(ref_info) => Ok(Response::new(CreateTagResponse {
                tag: Some(Ref {
                    name: ref_info.name,
                    short_name: ref_info.short_name,
                    oid: ref_info.oid,
                    r#type: RefType::Tag as i32,
                    is_default: false,
                }),
            })),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    #[instrument(skip(self))]
    async fn delete_tag(
        &self,
        request: Request<DeleteTagRequest>,
    ) -> Result<Response<DeleteTagResponse>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self.ref_service.delete_tag(&repo.owner, &repo.name, &req.name) {
            Ok(deleted) => Ok(Response::new(DeleteTagResponse { deleted })),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    // ========================================================================
    // Commit Operations
    // ========================================================================

    #[instrument(skip(self))]
    async fn get_commit(
        &self,
        request: Request<GetCommitRequest>,
    ) -> Result<Response<Commit>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self.commit_service.get_commit(&repo.owner, &repo.name, &req.oid) {
            Ok(commit) => Ok(Response::new(commit_to_proto(commit))),
            Err(e) => Err(Status::not_found(e.to_string())),
        }
    }

    type GetCommitLogStream = StreamResult<Commit>;

    #[instrument(skip(self))]
    async fn get_commit_log(
        &self,
        request: Request<GetCommitLogRequest>,
    ) -> Result<Response<Self::GetCommitLogStream>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        let limit = if req.limit == 0 { None } else { Some(req.limit) };
        let skip = if req.skip == 0 { None } else { Some(req.skip) };
        let path_filter = req.path.as_deref();

        match self.commit_service.get_log(
            &repo.owner,
            &repo.name,
            &req.start_ref,
            limit,
            skip,
            path_filter,
        ) {
            Ok(commits) => {
                let (tx, rx) = mpsc::channel(32);

                tokio::spawn(async move {
                    for commit in commits {
                        if tx.send(Ok(commit_to_proto(commit))).await.is_err() {
                            break;
                        }
                    }
                });

                Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
            }
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    type GetCommitAncestorsStream = StreamResult<Commit>;

    #[instrument(skip(self))]
    async fn get_commit_ancestors(
        &self,
        request: Request<GetCommitAncestorsRequest>,
    ) -> Result<Response<Self::GetCommitAncestorsStream>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        let limit = if req.limit == 0 { None } else { Some(req.limit) };

        match self.commit_service.get_ancestors(&repo.owner, &repo.name, &req.oid, limit) {
            Ok(commits) => {
                let (tx, rx) = mpsc::channel(32);

                tokio::spawn(async move {
                    for commit in commits {
                        if tx.send(Ok(commit_to_proto(commit))).await.is_err() {
                            break;
                        }
                    }
                });

                Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
            }
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    // ========================================================================
    // Tree & Blob Operations
    // ========================================================================

    #[instrument(skip(self))]
    async fn get_tree(
        &self,
        request: Request<GetTreeRequest>,
    ) -> Result<Response<GetTreeResponse>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        let path = if req.path.is_empty() { None } else { Some(req.path.as_str()) };

        let entries = if req.recursive {
            self.tree_service.get_tree_recursive(&repo.owner, &repo.name, &req.r#ref, path, Some(10))
        } else {
            self.tree_service.get_tree(&repo.owner, &repo.name, &req.r#ref, path)
        };

        match entries {
            Ok(entries) => {
                let proto_entries: Vec<TreeEntry> = entries
                    .into_iter()
                    .map(|e| TreeEntry {
                        name: e.name,
                        oid: e.oid,
                        mode: entry_mode_to_proto(e.mode),
                        r#type: entry_type_to_proto(e.entry_type),
                        size: e.size,
                    })
                    .collect();

                Ok(Response::new(GetTreeResponse {
                    tree_oid: String::new(), // TODO: return actual tree oid
                    entries: proto_entries,
                }))
            }
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    type GetBlobStream = StreamResult<BlobChunk>;

    #[instrument(skip(self))]
    async fn get_blob(
        &self,
        request: Request<GetBlobRequest>,
    ) -> Result<Response<Self::GetBlobStream>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self.tree_service.get_blob(&repo.owner, &repo.name, &req.oid) {
            Ok(data) => {
                let (tx, rx) = mpsc::channel(32);

                tokio::spawn(async move {
                    const CHUNK_SIZE: usize = 64 * 1024; // 64KB chunks
                    let chunks = data.chunks(CHUNK_SIZE);
                    let total_chunks = chunks.len();

                    for (i, chunk) in chunks.enumerate() {
                        let is_last = i == total_chunks - 1;
                        if tx
                            .send(Ok(BlobChunk {
                                data: chunk.to_vec(),
                                is_last,
                            }))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });

                Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
            }
            Err(e) => Err(Status::not_found(e.to_string())),
        }
    }

    #[instrument(skip(self))]
    async fn get_blob_info(
        &self,
        request: Request<GetBlobInfoRequest>,
    ) -> Result<Response<BlobInfo>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        // First resolve path to blob oid
        let blob_data = self
            .tree_service
            .get_blob_by_path(&repo.owner, &repo.name, &req.r#ref, &req.path)
            .map_err(|e| Status::not_found(e.to_string()))?;

        // Check if binary
        let is_binary = blob_data.get(..8192).map(|d| d.contains(&0)).unwrap_or(false);

        Ok(Response::new(BlobInfo {
            oid: String::new(), // Would need to compute or retrieve
            size: blob_data.len() as u64,
            is_binary,
            mime_type: None,
        }))
    }

    // ========================================================================
    // Diff Operations
    // ========================================================================

    type GetDiffStream = StreamResult<DiffEntry>;

    #[instrument(skip(self))]
    async fn get_diff(
        &self,
        request: Request<GetDiffRequest>,
    ) -> Result<Response<Self::GetDiffStream>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self.diff_service.diff_commits(
            &repo.owner,
            &repo.name,
            &req.base_ref,
            &req.head_ref,
            req.path.as_deref(),
            Some(3),
        ) {
            Ok(diff) => {
                let (tx, rx) = mpsc::channel(32);

                tokio::spawn(async move {
                    for file in diff.files {
                        let additions: u32 = file.hunks.iter()
                            .flat_map(|h| &h.lines)
                            .filter(|l| l.line_type == crate::git::diff::LineType::Addition)
                            .count() as u32;

                        let deletions: u32 = file.hunks.iter()
                            .flat_map(|h| &h.lines)
                            .filter(|l| l.line_type == crate::git::diff::LineType::Deletion)
                            .count() as u32;

                        let entry = DiffEntry {
                            path: file.path,
                            status: file_status_to_proto(file.status),
                            old_path: file.old_path,
                            old_oid: file.old_oid.unwrap_or_default(),
                            new_oid: file.new_oid.unwrap_or_default(),
                            additions,
                            deletions,
                        };

                        if tx.send(Ok(entry)).await.is_err() {
                            break;
                        }
                    }
                });

                Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
            }
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    #[instrument(skip(self))]
    async fn get_file_diff(
        &self,
        request: Request<GetFileDiffRequest>,
    ) -> Result<Response<FileDiff>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        let context = if req.context_lines == 0 { 3 } else { req.context_lines };

        match self.diff_service.diff_commits(
            &repo.owner,
            &repo.name,
            &req.base_ref,
            &req.head_ref,
            Some(&req.path),
            Some(context),
        ) {
            Ok(diff) => {
                let file = diff
                    .files
                    .into_iter()
                    .find(|f| f.path == req.path)
                    .ok_or_else(|| Status::not_found("File not in diff"))?;

                let proto_hunks: Vec<crate::proto::DiffHunk> = file
                    .hunks
                    .into_iter()
                    .map(|h| crate::proto::DiffHunk {
                        old_start: h.old_start as i32,
                        old_lines: h.old_lines as i32,
                        new_start: h.new_start as i32,
                        new_lines: h.new_lines as i32,
                        header: h.header,
                        lines: h
                            .lines
                            .into_iter()
                            .map(|l| crate::proto::DiffLine {
                                r#type: line_type_to_proto(l.line_type),
                                content: l.content,
                                old_line_number: l.old_line_no.map(|n| n as i32),
                                new_line_number: l.new_line_no.map(|n| n as i32),
                            })
                            .collect(),
                    })
                    .collect();

                Ok(Response::new(FileDiff {
                    path: file.path,
                    status: file_status_to_proto(file.status),
                    hunks: proto_hunks,
                    is_binary: file.is_binary,
                }))
            }
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    // ========================================================================
    // Advanced Operations
    // ========================================================================

    #[instrument(skip(self))]
    async fn merge(
        &self,
        request: Request<MergeRequest>,
    ) -> Result<Response<MergeResponse>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        let author = req.author.map(|a| crate::git::commits::GitActor {
            name: a.name,
            email: a.email,
            timestamp: a.timestamp,
            offset_minutes: a.offset_minutes,
        }).unwrap_or_else(|| crate::git::commits::GitActor {
            name: "arbor-git".to_string(),
            email: "git@arbor.dev".to_string(),
            timestamp: chrono::Utc::now().timestamp(),
            offset_minutes: 0,
        });

        match self.ops_service.merge(
            &repo.owner,
            &repo.name,
            &req.base_ref,
            &req.head_ref,
            &author,
            req.commit_message.as_deref(),
            true, // allow fast-forward
        ) {
            Ok(result) => {
                let conflicts: Vec<crate::proto::ConflictInfo> = result
                    .conflicts
                    .into_iter()
                    .map(|c| crate::proto::ConflictInfo {
                        path: c.path,
                        base_oid: c.ancestor_oid.unwrap_or_default(),
                        ours_oid: c.ours_oid.unwrap_or_default(),
                        theirs_oid: c.theirs_oid.unwrap_or_default(),
                    })
                    .collect();

                Ok(Response::new(MergeResponse {
                    success: result.status == crate::git::operations::MergeStatus::Success
                        || result.status == crate::git::operations::MergeStatus::FastForward
                        || result.status == crate::git::operations::MergeStatus::AlreadyUpToDate,
                    merge_commit_oid: result.commit_oid,
                    conflicts,
                }))
            }
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    #[instrument(skip(self))]
    async fn rebase(
        &self,
        request: Request<RebaseRequest>,
    ) -> Result<Response<RebaseResponse>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self.ops_service.rebase(&repo.owner, &repo.name, &req.branch, &req.onto, None) {
            Ok(result) => {
                let conflicts: Vec<crate::proto::ConflictInfo> = result
                    .conflicts
                    .into_iter()
                    .map(|c| crate::proto::ConflictInfo {
                        path: c.path,
                        base_oid: c.ancestor_oid.unwrap_or_default(),
                        ours_oid: c.ours_oid.unwrap_or_default(),
                        theirs_oid: c.theirs_oid.unwrap_or_default(),
                    })
                    .collect();

                Ok(Response::new(RebaseResponse {
                    success: result.status == crate::git::operations::RebaseStatus::Success
                        || result.status == crate::git::operations::RebaseStatus::NothingToRebase,
                    rewritten_commits: result.rebased_commits,
                    conflicts,
                }))
            }
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    #[instrument(skip(self))]
    async fn cherry_pick(
        &self,
        request: Request<CherryPickRequest>,
    ) -> Result<Response<CherryPickResponse>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        let mut new_commits = Vec::new();
        let mut all_conflicts = Vec::new();
        let mut success = true;

        for commit_oid in &req.commit_oids {
            match self.ops_service.cherry_pick(
                &repo.owner,
                &repo.name,
                commit_oid,
                &req.target_branch,
                None,
            ) {
                Ok(result) => {
                    if let Some(oid) = result.commit_oid {
                        new_commits.push(oid);
                    }
                    if result.status == crate::git::operations::CherryPickStatus::Conflict {
                        success = false;
                        for c in result.conflicts {
                            all_conflicts.push(crate::proto::ConflictInfo {
                                path: c.path,
                                base_oid: c.ancestor_oid.unwrap_or_default(),
                                ours_oid: c.ours_oid.unwrap_or_default(),
                                theirs_oid: c.theirs_oid.unwrap_or_default(),
                            });
                        }
                        break;
                    }
                }
                Err(e) => {
                    return Err(Status::internal(e.to_string()));
                }
            }
        }

        Ok(Response::new(CherryPickResponse {
            success,
            new_commit_oids: new_commits,
            conflicts: all_conflicts,
        }))
    }

    // ========================================================================
    // Pack Protocol (Stubs)
    // ========================================================================

    type UploadPackStream = StreamResult<UploadPackResponse>;

    async fn upload_pack(
        &self,
        _request: Request<Streaming<UploadPackRequest>>,
    ) -> Result<Response<Self::UploadPackStream>, Status> {
        Err(Status::unimplemented("upload_pack not yet implemented"))
    }

    type ReceivePackStream = StreamResult<ReceivePackResponse>;

    async fn receive_pack(
        &self,
        _request: Request<Streaming<ReceivePackRequest>>,
    ) -> Result<Response<Self::ReceivePackStream>, Status> {
        Err(Status::unimplemented("receive_pack not yet implemented"))
    }

    // ========================================================================
    // Batch Operations
    // ========================================================================

    #[instrument(skip(self))]
    async fn check_objects_exist(
        &self,
        request: Request<CheckObjectsExistRequest>,
    ) -> Result<Response<CheckObjectsExistResponse>, Status> {
        let req = request.into_inner();
        let repo = req.repository.ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self.ops_service.check_objects_exist(&repo.owner, &repo.name, &req.oids) {
            Ok(results) => {
                let exists: std::collections::HashMap<String, bool> =
                    results.into_iter().collect();
                Ok(Response::new(CheckObjectsExistResponse { exists }))
            }
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

fn commit_to_proto(commit: crate::git::commits::CommitInfo) -> Commit {
    Commit {
        oid: commit.oid,
        message: commit.message,
        author: Some(GitSignature {
            name: commit.author.name,
            email: commit.author.email,
            timestamp: commit.author.timestamp,
            offset_minutes: commit.author.offset_minutes,
        }),
        committer: Some(GitSignature {
            name: commit.committer.name,
            email: commit.committer.email,
            timestamp: commit.committer.timestamp,
            offset_minutes: commit.committer.offset_minutes,
        }),
        parent_oids: commit.parent_oids,
        tree_oid: commit.tree_oid,
    }
}

fn entry_mode_to_proto(mode: u32) -> i32 {
    match mode {
        0o100644 => TreeEntryMode::File as i32,
        0o100755 => TreeEntryMode::Executable as i32,
        0o120000 => TreeEntryMode::Symlink as i32,
        0o040000 => TreeEntryMode::Tree as i32,
        0o160000 => TreeEntryMode::Submodule as i32,
        _ => TreeEntryMode::Unspecified as i32,
    }
}

fn entry_type_to_proto(entry_type: crate::git::trees::EntryType) -> i32 {
    match entry_type {
        crate::git::trees::EntryType::Blob => TreeEntryType::Blob as i32,
        crate::git::trees::EntryType::Tree => TreeEntryType::Tree as i32,
        crate::git::trees::EntryType::Commit => TreeEntryType::Commit as i32,
        crate::git::trees::EntryType::Link => TreeEntryType::Blob as i32, // Symlinks are blob-like
    }
}

fn file_status_to_proto(status: crate::git::diff::FileStatus) -> i32 {
    match status {
        crate::git::diff::FileStatus::Added => DiffStatus::Added as i32,
        crate::git::diff::FileStatus::Deleted => DiffStatus::Deleted as i32,
        crate::git::diff::FileStatus::Modified => DiffStatus::Modified as i32,
        crate::git::diff::FileStatus::Renamed => DiffStatus::Renamed as i32,
        crate::git::diff::FileStatus::Copied => DiffStatus::Copied as i32,
        crate::git::diff::FileStatus::TypeChanged => DiffStatus::TypeChanged as i32,
    }
}

fn line_type_to_proto(line_type: crate::git::diff::LineType) -> i32 {
    match line_type {
        crate::git::diff::LineType::Context => DiffLineType::Context as i32,
        crate::git::diff::LineType::Addition => DiffLineType::Addition as i32,
        crate::git::diff::LineType::Deletion => DiffLineType::Deletion as i32,
    }
}
