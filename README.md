<div align="center">
  <h1 align="center">arbor-git</h1>

[Docs](https://docs.omni.dev/armory/arbor) | [Feedback](https://backfeed.omni.dev/workspaces/omni/projects/arbor) | [Discord](https://discord.gg/omnidotdev) | [X](https://x.com/omnidotdev) | [Threads](https://www.threads.com/@omnidotdev)

</div>

**arbor-git** is the Git backend for Arbor: a daemon that exposes [gitoxide](https://github.com/GitoxideLabs/gitoxide)-powered repository operations over gRPC, alongside an HTTP server for health checks.

## Prerequisites

- [Rust](https://rustup.rs) 1.85+ (edition 2024)
- [protobuf](https://protobuf.dev) compiler (`protoc`) for gRPC codegen

## Setup

```sh
cp .env.local.template .env.local
cargo build
```

## Run

```sh
cargo run
```

Configuration is read from the environment (see `.env.local.template`):

| Variable | Default | Description |
| --- | --- | --- |
| `GRPC_PORT` | `50051` | Port for the gRPC service |
| `HTTP_PORT` | `8080` | Port for the HTTP server |
| `STORAGE_PATH` | `./data/repositories` | On-disk root for repository storage |
| `DEFAULT_BRANCH` | `main` | Default branch for newly created repositories |
| `DATABASE_URL` | _unset_ | Postgres URL for repository metadata (reserved, not yet used) |
| `RUST_LOG` | `arbor_git=info` | Log filter (see [tracing](https://docs.rs/tracing-subscriber)) |

## Diagnostics

The HTTP server exposes health endpoints:

```sh
curl http://localhost:8080/health   # liveness, returns "OK"
curl http://localhost:8080/ready    # readiness, checks storage path
```

Probe the gRPC service (requires [grpcurl](https://github.com/fullstorydev/grpcurl)):

```sh
grpcurl -plaintext localhost:50051 list
```

Increase log verbosity for either server:

```sh
RUST_LOG=arbor_git=debug,tower_http=debug cargo run
```

## Development

```sh
cargo build   # build
cargo test    # test
cargo clippy  # lint
cargo fmt     # format
```

## License

The code in this repository is licensed under Apache 2.0, &copy; [Omni LLC](https://omni.dev). See [LICENSE.md](LICENSE.md) for more information.
