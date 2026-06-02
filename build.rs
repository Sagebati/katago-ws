//! Compile the orchestrator<->worker gRPC contract (`proto/cluster.proto`) into
//! Rust via tonic + prost. The generated code lands in `OUT_DIR` and is pulled
//! in by `tonic::include_proto!("cluster")` inside `src/cluster/mod.rs`.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/cluster.proto");
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/cluster.proto"], &["proto"])?;
    Ok(())
}
