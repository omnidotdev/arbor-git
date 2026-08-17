use std::net::SocketAddr;

use axum::{Router, extract::State, http::StatusCode, response::IntoResponse, routing::get};
use tonic::transport::Server as TonicServer;
use tracing::{error, info};

use crate::git::StorageConfig;
use crate::grpc::GitServiceImpl;
use crate::proto::git_service_server::GitServiceServer;

/// Server configuration
#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub grpc_addr: SocketAddr,
    pub http_addr: SocketAddr,
    pub storage: StorageConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            grpc_addr: "0.0.0.0:50051".parse().unwrap(),
            http_addr: "0.0.0.0:8080".parse().unwrap(),
            storage: StorageConfig::from_env(),
        }
    }
}

impl ServerConfig {
    pub fn from_env() -> Self {
        let grpc_port: u16 = std::env::var("GRPC_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(50051);

        let http_port: u16 = std::env::var("HTTP_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8080);

        Self {
            grpc_addr: SocketAddr::from(([0, 0, 0, 0], grpc_port)),
            http_addr: SocketAddr::from(([0, 0, 0, 0], http_port)),
            storage: StorageConfig::from_env(),
        }
    }
}

/// State shared with HTTP handlers
#[derive(Clone)]
struct AppState {
    storage: StorageConfig,
}

/// Run both gRPC and HTTP servers
pub async fn run(config: ServerConfig) -> Result<(), Box<dyn std::error::Error>> {
    let storage = config.storage.clone();

    // Create gRPC service
    let git_service = GitServiceImpl::new(storage.clone());
    let grpc_server = TonicServer::builder()
        .add_service(GitServiceServer::new(git_service))
        .serve(config.grpc_addr);

    // Create HTTP server. It only serves health and readiness probes; pack
    // transport is gRPC-only. No CORS layer is mounted because no browser calls
    // these endpoints (they are hit by in-cluster orchestration probes)
    let state = AppState { storage };
    let http_router = Router::new()
        .route("/health", get(health_check))
        .route("/ready", get(readiness_check))
        .with_state(state);

    let http_server = axum::serve(
        tokio::net::TcpListener::bind(config.http_addr).await?,
        http_router,
    );

    info!(grpc_addr = %config.grpc_addr, http_addr = %config.http_addr, "Starting arbor-git servers");

    // Run both servers concurrently
    tokio::select! {
        result = grpc_server => {
            if let Err(e) = result {
                error!(error = %e, "gRPC server error");
            }
        }
        result = http_server => {
            if let Err(e) = result {
                error!(error = %e, "HTTP server error");
            }
        }
    }

    Ok(())
}

// ============================================================================
// HTTP Handlers
// ============================================================================

async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

async fn readiness_check(State(state): State<AppState>) -> impl IntoResponse {
    // Check if storage path exists and is writable
    if state.storage.base_path.exists() {
        (StatusCode::OK, "Ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "Storage not available")
    }
}
