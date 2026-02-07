# Get absolute path to this Tiltfile's directory using pwd
_this_dir = str(local("pwd", quiet=True)).strip()

project_name = "arbor-git"
grpc_port = 50052
http_port = 8081
storage_path = _this_dir + "/data/repositories"

# Build the Rust binary
local_resource(
    "build-%s" % project_name,
    cmd="cargo build",
    deps=["src", "proto", "Cargo.toml", "Cargo.lock", "build.rs"],
    labels=[project_name],
)

# Run the service
local_resource(
    "dev-%s" % project_name,
    serve_cmd="RUST_LOG=arbor_git=debug,tower_http=debug GRPC_PORT=%s HTTP_PORT=%s STORAGE_PATH='%s' cargo run" % (grpc_port, http_port, storage_path),
    resource_deps=["build-%s" % project_name],
    labels=[project_name],
)

# Run tests
local_resource(
    "test-%s" % project_name,
    cmd="cargo test",
    deps=["src", "proto"],
    labels=[project_name],
    auto_init=False,
    trigger_mode=TRIGGER_MODE_MANUAL,
)

# Clippy linting
local_resource(
    "lint-%s" % project_name,
    cmd="cargo clippy -- -D warnings",
    deps=["src"],
    labels=[project_name],
    auto_init=False,
    trigger_mode=TRIGGER_MODE_MANUAL,
)
