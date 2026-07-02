//! grpc_client — B1 test client: calls GetVersion and prints the result.
//!
//! Usage:
//!     cargo run --example grpc_client -- [--addr <host:port>]
//!
//! Default addr: http://[::1]:50051

use anyhow::{Context, Result};
use muxrd::proto::Empty;
use muxrd::proto::muxr_client::MuxrClient;

#[tokio::main]
async fn main() -> Result<()> {
    let addr = parse_addr();
    println!("connecting to {addr}…");

    let mut client = MuxrClient::connect(addr.clone())
        .await
        .with_context(|| format!("failed to connect to {addr}"))?;

    let response = client
        .get_version(Empty {})
        .await
        .context("GetVersion RPC failed")?;

    let info = response.into_inner();
    println!("GetVersion response:");
    println!("  server_version : {}", info.server_version);
    println!("  zellij_version : {}", info.zellij_version);

    Ok(())
}

fn parse_addr() -> String {
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--addr")
        && let Some(val) = args.get(pos + 1)
    {
        // Ensure it has a scheme prefix for tonic
        if val.starts_with("http") {
            return val.clone();
        }
        return format!("http://{val}");
    }
    "http://[::1]:50051".to_owned()
}
