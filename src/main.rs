mod server;
mod tools;

use anyhow::{Context, Result};
use elisym_core::{AgentNodeBuilder, SolanaPaymentConfig, SolanaPaymentProvider, SolanaNetwork, SolanaToken};
use rmcp::{ServiceExt, transport::stdio};
use serde::Deserialize;
use tracing_subscriber::{self, EnvFilter};

use server::ElisymServer;

/// Minimal subset of elisym-client's AgentConfig — just what we need.
#[derive(Deserialize)]
struct AgentConfig {
    name: String,
    description: String,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    relays: Vec<String>,
    secret_key: String,
    #[serde(default)]
    payment: Option<PaymentSection>,
}

#[derive(Deserialize)]
struct PaymentSection {
    #[serde(default = "default_chain")]
    chain: String,
    #[serde(default = "default_network")]
    network: String,
    #[serde(default)]
    rpc_url: Option<String>,
    #[serde(default = "default_token")]
    token: String,
    #[serde(default)]
    solana_secret_key: String,
}

fn default_chain() -> String { "solana".into() }
fn default_network() -> String { "devnet".into() }
fn default_token() -> String { "sol".into() }

fn load_agent_config(name: &str) -> Result<AgentConfig> {
    let home = dirs::home_dir().context("Cannot find home directory")?;
    let path = home
        .join(".elisym")
        .join("agents")
        .join(name)
        .join("config.toml");
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("Agent '{}' not found at {}", name, path.display()))?;
    let config: AgentConfig =
        toml::from_str(&contents).with_context(|| format!("Invalid config for agent '{}'", name))?;
    Ok(config)
}

fn build_solana_provider(payment: &PaymentSection) -> Option<SolanaPaymentProvider> {
    if payment.chain != "solana" || payment.solana_secret_key.is_empty() {
        return None;
    }

    let network = match payment.network.as_str() {
        "mainnet" => SolanaNetwork::Mainnet,
        "testnet" => SolanaNetwork::Testnet,
        "devnet" => SolanaNetwork::Devnet,
        custom => SolanaNetwork::Custom(custom.to_string()),
    };

    let token = match payment.token.as_str() {
        "sol" => SolanaToken::Sol,
        _ => SolanaToken::Sol, // default to SOL for now
    };

    let config = SolanaPaymentConfig {
        network,
        rpc_url: payment.rpc_url.clone(),
        token,
    };

    match SolanaPaymentProvider::from_secret_key(config, &payment.solana_secret_key) {
        Ok(provider) => {
            tracing::info!(address = %provider.address(), "Solana wallet configured");
            Some(provider)
        }
        Err(e) => {
            tracing::warn!("Failed to initialize Solana wallet: {e}");
            None
        }
    }
}

fn list_agents() -> Vec<String> {
    let Some(home) = dirs::home_dir() else {
        return vec![];
    };
    let root = home.join(".elisym").join("agents");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return vec![];
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        if entry.path().join("config.toml").exists() {
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    names
}

#[tokio::main]
async fn main() -> Result<()> {
    // MCP servers MUST NOT write to stdout (reserved for JSON-RPC).
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("Starting elisym MCP server");

    // Priority: ELISYM_AGENT (reuse elisym-client config) > individual env vars > defaults
    let builder = if let Ok(agent_name) = std::env::var("ELISYM_AGENT") {
        let config = load_agent_config(&agent_name)?;
        tracing::info!(agent = %agent_name, "Loading agent from ~/.elisym/agents/");

        let mut b = AgentNodeBuilder::new(&config.name, &config.description)
            .capabilities(config.capabilities)
            .secret_key(&config.secret_key);

        if !config.relays.is_empty() {
            b = b.relays(config.relays);
        }

        // Configure Solana payments if available
        if let Some(ref payment) = config.payment {
            if let Some(provider) = build_solana_provider(payment) {
                b = b.solana_payment_provider(provider);
            }
        }

        b
    } else {
        let agent_name =
            std::env::var("ELISYM_AGENT_NAME").unwrap_or_else(|_| "mcp-agent".into());
        let agent_desc = std::env::var("ELISYM_AGENT_DESCRIPTION")
            .unwrap_or_else(|_| "elisym MCP server agent".into());

        let mut b = AgentNodeBuilder::new(&agent_name, &agent_desc)
            .capabilities(vec!["mcp-gateway".into()]);

        if let Ok(key) = std::env::var("ELISYM_NOSTR_SECRET") {
            b = b.secret_key(key);
        } else {
            let agents = list_agents();
            if !agents.is_empty() {
                tracing::info!(
                    "Tip: set ELISYM_AGENT to reuse an existing agent identity. Available: {}",
                    agents.join(", ")
                );
            }
        }

        if let Ok(relays) = std::env::var("ELISYM_RELAYS") {
            let relay_list: Vec<String> =
                relays.split(',').map(|s| s.trim().to_string()).collect();
            if !relay_list.is_empty() {
                b = b.relays(relay_list);
            }
        }
        b
    };

    let agent = builder.build().await?;
    tracing::info!(
        npub = %agent.identity.npub(),
        payments = agent.payments.is_some(),
        "Agent node started"
    );

    let server = ElisymServer::new(agent);
    let service = server
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("Failed to start MCP service: {e}"))?;

    service.waiting().await?;

    tracing::info!("elisym MCP server stopped");
    Ok(())
}
