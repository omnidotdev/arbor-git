pub mod git;
pub mod grpc;
pub mod server;

// Re-export proto types (generated at build time)
pub mod proto {
    #![allow(clippy::all, clippy::pedantic, clippy::nursery)]
    tonic::include_proto!("arbor.git.v1");
}

pub use git::StorageConfig;
