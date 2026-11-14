//! The test MCP server on stdin and stdout. The stdio tests in `tests/mcp_connector.rs` start it.

#[path = "../tests/support/server.rs"]
mod server;

use rmcp::ServiceExt;
use rmcp::transport::stdio;

#[tokio::main]
async fn main() {
    let running = server::TestServer::default()
        .serve(stdio())
        .await
        .expect("the MCP handshake on stdio succeeds");
    running.waiting().await.expect("the server stops cleanly");
}
