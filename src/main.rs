use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use arbor_git::server::{ServerConfig, run};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // When git runs this binary as a confined push's pre-receive hook, enforce the
    // credential boundary and exit instead of starting the server (see the
    // receive_pack handler, which points core.hooksPath at a script that execs us
    // as `arbor-git __pre-receive`).
    if std::env::args().nth(1).as_deref() == Some("__pre-receive") {
        std::process::exit(arbor_git::git::hook::run_pre_receive());
    }

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

    // Install the pre-receive credential-boundary hook once, so confined pushes
    // can point core.hooksPath at it
    config.storage.ensure_pre_receive_hook()?;

    // Run the server
    run(config).await
}
