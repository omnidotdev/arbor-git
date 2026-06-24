use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use arbor_git::server::{ServerConfig, run};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load environment variables from .env if present
    dotenvy::dotenv().ok();

    // Initialize tracing
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive("arbor_git=info".parse()?))
        .init();

    // Load configuration
    let config = ServerConfig::from_env();

    tracing::info!(
        grpc_addr = %config.grpc_addr,
        http_addr = %config.http_addr,
        storage_path = %config.storage.base_path.display(),
        "Starting arbor-git service"
    );

    // Ensure storage directory exists
    std::fs::create_dir_all(&config.storage.base_path)?;

    // Run the server
    run(config).await
}
