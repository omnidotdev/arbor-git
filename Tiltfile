v1alpha1.extension_repo(name='omni', url='https://github.com/omnidotdev/tilt-extensions')
v1alpha1.extension(name='dotenv_values', repo_name='omni', repo_path='dotenv_values')
load('ext://dotenv_values', 'dotenv_values')

env_local = dotenv_values(".env.local")
project_name = "arbor-git"
grpc_port = 50051
http_port = 8080

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
    serve_cmd="cargo run",
    resource_deps=["build-%s" % project_name],
    labels=[project_name],
    env=dict(env_local, **{
        "RUST_LOG": "arbor_git=debug,tower_http=debug",
        "GRPC_PORT": str(grpc_port),
        "HTTP_PORT": str(http_port),
    }),
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
