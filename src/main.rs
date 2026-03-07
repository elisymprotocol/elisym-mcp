mod server;
mod tools;

use anyhow::Result;
use elisym_core::AgentNodeBuilder;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::{self, EnvFilter};

use server::ElisymServer;

#[tokio::main]
async fn main() -> Result<()> {
    // MCP servers MUST NOT write to stdout (reserved for JSON-RPC).
    // Log to stderr only.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("Starting elisym MCP server");

    // Build the agent node from environment variables
    let secret_key = std::env::var("ELISYM_NOSTR_SECRET").ok();
    let agent_name = std::env::var("ELISYM_AGENT_NAME").unwrap_or_else(|_| "mcp-agent".into());
    let agent_desc = std::env::var("ELISYM_AGENT_DESCRIPTION")
        .unwrap_or_else(|_| "elisym MCP server agent".into());

    let mut builder = AgentNodeBuilder::new(&agent_name, &agent_desc)
        .capabilities(vec!["mcp-gateway".into()]);

    if let Some(key) = secret_key {
        builder = builder.secret_key(key);
    }

    // Optional: custom relays
    if let Ok(relays) = std::env::var("ELISYM_RELAYS") {
        let relay_list: Vec<String> = relays.split(',').map(|s| s.trim().to_string()).collect();
        if !relay_list.is_empty() {
            builder = builder.relays(relay_list);
        }
    }

    let agent = builder.build().await?;
    tracing::info!(npub = %agent.identity.npub(), "Agent node started");

    let server = ElisymServer::new(agent);
    let service = server.serve(stdio()).await
        .inspect_err(|e| tracing::error!("Failed to start MCP service: {e}"))?;

    service.waiting().await?;

    tracing::info!("elisym MCP server stopped");
    Ok(())
}
