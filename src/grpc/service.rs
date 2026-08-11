use std::pin::Pin;

use futures::{Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{error, info, instrument};

use crate::git::{
    StorageConfig,
    commits::CommitService,
    diff::DiffService,
    operations::OperationsService,
    refs::{RefService, RefType as InternalRefType},
    repository::RepositoryService,
    trees::TreeService,
};
use crate::proto::{
    BlobChunk, BlobInfo, CheckObjectsExistRequest, CheckObjectsExistResponse, CherryPickRequest,
    CherryPickResponse, Commit, CreateBranchRequest, CreateBranchResponse, CreateTagRequest,
    CreateTagResponse, DeleteBranchRequest, DeleteBranchResponse, DeleteRepositoryRequest,
    DeleteRepositoryResponse, DeleteTagRequest, DeleteTagResponse, DiffEntry, DiffLineType,
    DiffStatus, FileDiff, GetBlobInfoRequest, GetBlobRequest, GetCommitAncestorsRequest,
    GetCommitLogRequest, GetCommitRequest, GetDiffRequest, GetFileDiffRequest,
    GetRepositoryInfoRequest, GetTreeRequest, GetTreeResponse, GitSignature, InitRepositoryRequest,
    InitRepositoryResponse, ListRefsRequest, ListRefsResponse, MergeRequest, MergeResponse,
    RebaseRequest, RebaseResponse, ReceivePackRequest, ReceivePackResponse, Ref, RefType,
    RepositoryExistsRequest, RepositoryExistsResponse, RepositoryInfo, RepositoryPath,
    ResolveRefRequest, ResolveRefResponse, SetDefaultBranchRequest, SetDefaultBranchResponse,
    TreeEntry, TreeEntryMode, TreeEntryType, UploadPackRequest, UploadPackResponse,
    git_service_server,
};

pub struct GitServiceImpl {
    repo_service: RepositoryService,
    ref_service: RefService,
    commit_service: CommitService,
    tree_service: TreeService,
    diff_service: DiffService,
    ops_service: OperationsService,
    config: StorageConfig,
}

impl GitServiceImpl {
    pub fn new(config: StorageConfig) -> Self {
        Self {
            repo_service: RepositoryService::new(config.clone()),
            ref_service: RefService::new(config.clone()),
            commit_service: CommitService::new(config.clone()),
            tree_service: TreeService::new(config.clone()),
            diff_service: DiffService::new(config.clone()),
            ops_service: OperationsService::new(config.clone()),
            config,
        }
    }
}

/// Run a git pack service (`upload-pack` or `receive-pack`) in stateless-RPC
/// mode over a repository, wiring a channel of request chunks to the process's
/// stdin and returning a channel of its stdout chunks. This is the same path a
/// smart-HTTP git server uses; the gRPC layer is only a transport for the raw
/// protocol bytes, so the real `git` implements the negotiation and packfile.
fn run_git_pack(
    service: &'static str,
    repo_path: &std::path::Path,
    mut stdin_rx: mpsc::Receiver<Vec<u8>>,
) -> Result<mpsc::Receiver<Result<Vec<u8>, Status>>, Status> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    if !repo_path.exists() {
        return Err(Status::not_found("repository not found"));
    }

    let mut child = tokio::process::Command::new("git")
        .arg(service)
        .arg("--stateless-rpc")
        .arg(repo_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Status::internal(format!("failed to spawn git {service}: {e}")))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| Status::internal("no stdin"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| Status::internal("no stdout"))?;

    // Forward request chunks to the process stdin, then close it (EOF)
    tokio::spawn(async move {
        while let Some(chunk) = stdin_rx.recv().await {
            if stdin.write_all(&chunk).await.is_err() {
                break;
            }
        }
        let _ = stdin.shutdown().await;
    });

    let (tx, rx) = mpsc::channel::<Result<Vec<u8>, Status>>(16);

    // Stream stdout back concurrently (draining it avoids a pipe-buffer deadlock
    // with stdin), then surface a non-zero exit with its stderr
    tokio::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(Ok(buf[..n].to_vec())).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(Status::internal(e.to_string()))).await;
                    return;
                }
            }
        }

        match child.wait().await {
            Ok(status) if !status.success() => {
                let mut err = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_string(&mut err).await;
                }
                let _ = tx
                    .send(Err(Status::internal(format!(
                        "git {service} exited with {status}: {err}"
                    ))))
                    .await;
            }
            Err(e) => {
                let _ = tx.send(Err(Status::internal(e.to_string()))).await;
            }
            _ => {}
        }
    });

    Ok(rx)
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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        let default_branch = if req.default_branch.is_empty() {
            None
        } else {
            Some(req.default_branch.as_str())
        };

        match self
            .repo_service
            .init(&repo.owner, &repo.name, default_branch)
        {
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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        let exists = self.repo_service.exists(&repo.owner, &repo.name);
        Ok(Response::new(RepositoryExistsResponse { exists }))
    }

    #[instrument(skip(self))]
    async fn get_repository_info(
        &self,
        request: Request<GetRepositoryInfoRequest>,
    ) -> Result<Response<RepositoryInfo>, Status> {
        let req = request.into_inner();
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        let filter_type = match req.filter_type {
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

        match self
            .ref_service
            .list_refs(&repo.owner, &repo.name, filter_type, pattern)
        {
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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self
            .ref_service
            .resolve_ref(&repo.owner, &repo.name, &req.r#ref)
        {
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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self
            .ref_service
            .create_branch(&repo.owner, &repo.name, &req.name, &req.start_point)
        {
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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self
            .ref_service
            .delete_branch(&repo.owner, &repo.name, &req.name, req.force)
        {
            Ok(deleted) => Ok(Response::new(DeleteBranchResponse { deleted })),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    #[instrument(skip(self))]
    async fn set_default_branch(
        &self,
        request: Request<SetDefaultBranchRequest>,
    ) -> Result<Response<SetDefaultBranchResponse>, Status> {
        let req = request.into_inner();
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self
            .ref_service
            .set_default_branch(&repo.owner, &repo.name, &req.branch)
        {
            Ok(()) => Ok(Response::new(SetDefaultBranchResponse { success: true })),
            Err(e) => Err(Status::internal(e.to_string())),
        }
    }

    #[instrument(skip(self))]
    async fn create_tag(
        &self,
        request: Request<CreateTagRequest>,
    ) -> Result<Response<CreateTagResponse>, Status> {
        let req = request.into_inner();
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self
            .ref_service
            .delete_tag(&repo.owner, &repo.name, &req.name)
        {
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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self
            .commit_service
            .get_commit(&repo.owner, &repo.name, &req.oid)
        {
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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        let limit = if req.limit == 0 {
            None
        } else {
            Some(req.limit)
        };
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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        let limit = if req.limit == 0 {
            None
        } else {
            Some(req.limit)
        };

        match self
            .commit_service
            .get_ancestors(&repo.owner, &repo.name, &req.oid, limit)
        {
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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        let path = if req.path.is_empty() {
            None
        } else {
            Some(req.path.as_str())
        };

        let entries = if req.recursive {
            self.tree_service.get_tree_recursive(
                &repo.owner,
                &repo.name,
                &req.r#ref,
                path,
                Some(10),
            )
        } else {
            self.tree_service
                .get_tree(&repo.owner, &repo.name, &req.r#ref, path)
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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self
            .tree_service
            .get_blob(&repo.owner, &repo.name, &req.oid)
        {
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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        // First resolve path to blob oid
        let blob_data = self
            .tree_service
            .get_blob_by_path(&repo.owner, &repo.name, &req.r#ref, &req.path)
            .map_err(|e| Status::not_found(e.to_string()))?;

        // Check if binary
        let is_binary = blob_data.get(..8192).is_some_and(|d| d.contains(&0));

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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

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
                        let additions: u32 = file
                            .hunks
                            .iter()
                            .flat_map(|h| &h.lines)
                            .filter(|l| l.line_type == crate::git::diff::LineType::Addition)
                            .count() as u32;

                        let deletions: u32 = file
                            .hunks
                            .iter()
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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        let context = if req.context_lines == 0 {
            3
        } else {
            req.context_lines
        };

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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        let author = req.author.map_or_else(
            || crate::git::commits::GitActor {
                name: "arbor-git".to_string(),
                email: "git@arbor.dev".to_string(),
                timestamp: chrono::Utc::now().timestamp(),
                offset_minutes: 0,
            },
            |a| crate::git::commits::GitActor {
                name: a.name,
                email: a.email,
                timestamp: a.timestamp,
                offset_minutes: a.offset_minutes,
            },
        );

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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self
            .ops_service
            .rebase(&repo.owner, &repo.name, &req.branch, &req.onto, None)
        {
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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

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
    // Pack Protocol
    // ========================================================================

    type UploadPackStream = StreamResult<UploadPackResponse>;

    #[instrument(skip(self, request))]
    async fn upload_pack(
        &self,
        request: Request<Streaming<UploadPackRequest>>,
    ) -> Result<Response<Self::UploadPackStream>, Status> {
        use crate::proto::upload_pack_request::Request as Req;

        let mut stream = request.into_inner();

        // The first message carries the repository to serve
        let init = stream
            .message()
            .await?
            .and_then(|m| m.request)
            .ok_or_else(|| Status::invalid_argument("missing init message"))?;
        let repo = match init {
            Req::Init(init) => init
                .repository
                .ok_or_else(|| Status::invalid_argument("missing repository"))?,
            Req::Data(_) => {
                return Err(Status::invalid_argument("first message must be init"));
            }
        };
        let repo_path = self.config.repo_path(&repo.owner, &repo.name);

        // Feed subsequent request chunks to git's stdin
        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>(16);
        tokio::spawn(async move {
            while let Ok(Some(msg)) = stream.message().await {
                let Some(Req::Data(bytes)) = msg.request else {
                    continue;
                };
                if stdin_tx.send(bytes).await.is_err() {
                    break;
                }
            }
        });

        let out_rx = run_git_pack("upload-pack", &repo_path, stdin_rx)?;
        let out =
            ReceiverStream::new(out_rx).map(|chunk| chunk.map(|data| UploadPackResponse { data }));
        Ok(Response::new(Box::pin(out)))
    }

    type ReceivePackStream = StreamResult<ReceivePackResponse>;

    #[instrument(skip(self, request))]
    async fn receive_pack(
        &self,
        request: Request<Streaming<ReceivePackRequest>>,
    ) -> Result<Response<Self::ReceivePackStream>, Status> {
        use crate::proto::receive_pack_request::Request as Req;

        let mut stream = request.into_inner();

        let init = stream
            .message()
            .await?
            .and_then(|m| m.request)
            .ok_or_else(|| Status::invalid_argument("missing init message"))?;
        let repo = match init {
            Req::Init(init) => init
                .repository
                .ok_or_else(|| Status::invalid_argument("missing repository"))?,
            Req::Data(_) => {
                return Err(Status::invalid_argument("first message must be init"));
            }
        };
        let repo_path = self.config.repo_path(&repo.owner, &repo.name);

        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>(16);
        tokio::spawn(async move {
            while let Ok(Some(msg)) = stream.message().await {
                let Some(Req::Data(bytes)) = msg.request else {
                    continue;
                };
                if stdin_tx.send(bytes).await.is_err() {
                    break;
                }
            }
        });

        let out_rx = run_git_pack("receive-pack", &repo_path, stdin_rx)?;
        let out =
            ReceiverStream::new(out_rx).map(|chunk| chunk.map(|data| ReceivePackResponse { data }));
        Ok(Response::new(Box::pin(out)))
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
        let repo = req
            .repository
            .ok_or_else(|| Status::invalid_argument("repository required"))?;

        match self
            .ops_service
            .check_objects_exist(&repo.owner, &repo.name, &req.oids)
        {
            Ok(results) => {
                let exists: std::collections::HashMap<String, bool> = results.into_iter().collect();
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

const fn entry_mode_to_proto(mode: u32) -> i32 {
    match mode {
        0o10_0644 => TreeEntryMode::File as i32,
        0o10_0755 => TreeEntryMode::Executable as i32,
        0o12_0000 => TreeEntryMode::Symlink as i32,
        0o04_0000 => TreeEntryMode::Tree as i32,
        0o16_0000 => TreeEntryMode::Submodule as i32,
        _ => TreeEntryMode::Unspecified as i32,
    }
}

const fn entry_type_to_proto(entry_type: crate::git::trees::EntryType) -> i32 {
    match entry_type {
        crate::git::trees::EntryType::Blob | crate::git::trees::EntryType::Link => {
            TreeEntryType::Blob as i32 // Symlinks are blob-like
        }
        crate::git::trees::EntryType::Tree => TreeEntryType::Tree as i32,
        crate::git::trees::EntryType::Commit => TreeEntryType::Commit as i32,
    }
}

const fn file_status_to_proto(status: crate::git::diff::FileStatus) -> i32 {
    match status {
        crate::git::diff::FileStatus::Added => DiffStatus::Added as i32,
        crate::git::diff::FileStatus::Deleted => DiffStatus::Deleted as i32,
        crate::git::diff::FileStatus::Modified => DiffStatus::Modified as i32,
        crate::git::diff::FileStatus::Renamed => DiffStatus::Renamed as i32,
        crate::git::diff::FileStatus::Copied => DiffStatus::Copied as i32,
        crate::git::diff::FileStatus::TypeChanged => DiffStatus::TypeChanged as i32,
    }
}

const fn line_type_to_proto(line_type: crate::git::diff::LineType) -> i32 {
    match line_type {
        crate::git::diff::LineType::Context => DiffLineType::Context as i32,
        crate::git::diff::LineType::Addition => DiffLineType::Addition as i32,
        crate::git::diff::LineType::Deletion => DiffLineType::Deletion as i32,
    }
}

#[cfg(test)]
mod pack_tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::tempdir;

    /// A git pkt-line: a 4-hex length prefix (counting itself) then the payload.
    fn pkt_line(s: &str) -> Vec<u8> {
        let mut v = format!("{:04x}", s.len() + 4).into_bytes();
        v.extend_from_slice(s.as_bytes());
        v
    }

    fn git(args: &[&str], cwd: &std::path::Path) {
        let status = StdCommand::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[tokio::test]
    async fn upload_pack_serves_a_real_fetch() {
        let storage = tempdir().unwrap();
        let config = StorageConfig {
            base_path: storage.path().to_path_buf(),
            ..Default::default()
        };
        RepositoryService::new(config.clone())
            .init("owner", "repo", Some("main"))
            .unwrap();
        let bare = config.repo_path("owner", "repo");

        // Create a commit in a work tree and push it into the bare repository
        let work = tempdir().unwrap();
        git(&["init", "-q", "-b", "main", "."], work.path());
        std::fs::write(work.path().join("f.txt"), "hi").unwrap();
        git(&["add", "."], work.path());
        git(&["commit", "-q", "-m", "c"], work.path());
        git(
            &["push", "-q", bare.to_str().unwrap(), "main:main"],
            work.path(),
        );

        let out = StdCommand::new("git")
            .args([
                "--git-dir",
                bare.to_str().unwrap(),
                "rev-parse",
                "refs/heads/main",
            ])
            .output()
            .unwrap();
        let oid = String::from_utf8(out.stdout).unwrap().trim().to_string();

        // A minimal clone request; no side-band, so the packfile is raw in stdout
        let mut req = pkt_line(&format!(
            "want {oid} multi_ack ofs-delta agent=arbor-git-test\n"
        ));
        req.extend_from_slice(b"0000");
        req.extend(pkt_line("done\n"));

        let (tx, rx) = mpsc::channel(4);
        tx.send(req).await.unwrap();
        drop(tx);

        let mut out_rx = run_git_pack("upload-pack", &bare, rx).unwrap();
        let mut response = Vec::new();
        while let Some(chunk) = out_rx.recv().await {
            response.extend(chunk.expect("chunk"));
        }

        assert!(
            response.windows(4).any(|w| w == b"PACK"),
            "upload-pack response should contain a packfile"
        );
    }

    #[tokio::test]
    async fn upload_pack_rejects_a_missing_repository() {
        let storage = tempdir().unwrap();
        let (_tx, rx) = mpsc::channel(1);
        let result = run_git_pack("upload-pack", &storage.path().join("nope.git"), rx);
        assert!(result.is_err());
    }
}
