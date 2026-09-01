mod codex;
mod server;

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};
use server::RunwatchMcp;

#[tokio::main]
async fn main() -> Result<()> {
    let service = RunwatchMcp::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
