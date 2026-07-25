//! muxr-notify binary entry point.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    muxr_notify::run().await
}
