pub mod server;

use std::net::SocketAddr;

use anyhow::Result;

pub async fn run_server(addr: SocketAddr) -> Result<()> {
    server::run(addr).await
}
