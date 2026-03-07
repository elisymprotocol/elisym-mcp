mod install;
mod server;
mod tools;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use elisym_core::{
    AgentNodeBuilder, SolanaNetwork, SolanaPaymentConfig, SolanaPaymentProvider,
};
use rmcp::{ServiceExt, transport::stdio};
use serde::Deserialize;
use tracing_subscriber::{self, EnvFilter};

use server::ElisymServer;

/// elisym MCP server — AI agent discovery, marketplace, and payments via Nostr.
#[derive(Parser)]
#[command(name = "elisym-mcp", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Start HTTP transport instead of stdio (requires transport-http feature).
    #[arg(long)]
    http: bool,

    /// Host to bind HTTP server to (default: 127.0.0.1).
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Port for HTTP server (default: 8080).
    #[arg(long, default_value = "8080")]
    port: u16,
}

#[derive(Subcommand)]
enum Commands {
    /// Install elisym-mcp into MCP client configurations.
    Install {
        /// Target a specific client (claude-desktop, cursor, windsurf).
        #[arg(long)]
        client: Option<String>,

        /// Bind to an existing elisym agent (reads ~/.elisym/agents/<name>/config.toml).
        #[arg(long)]
        agent: Option<String>,

        /// List detected MCP clients and their status.
        #[arg(long)]
        list: bool,
    },

    /// Remove elisym-mcp from MCP client configurations.
    Uninstall {
        /// Target a specific client.
        #[arg(long)]
        client: Option<String>,
    },
}

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
    #[allow(dead_code)]
    token: String,
    #[serde(default)]
    solana_secret_key: String,
}

fn default_chain() -> String {
    "solana".into()
}
fn default_network() -> String {
    "devnet".into()
}
fn default_token() -> String {
    "sol".into()
}

fn load_agent_config(name: &str) -> Result<AgentConfig> {
    // Reject path traversal attempts (e.g. "../" or "/")
    anyhow::ensure!(
        !name.is_empty()
            && !name.contains('/')
            && !name.contains('\\')
            && name != "."
            && name != "..",
        "Invalid agent name: '{name}'"
    );
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

    let config = SolanaPaymentConfig {
        network,
        rpc_url: payment.rpc_url.clone(),
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

#[cfg(feature = "transport-http")]
async fn start_http_server(
    agent: elisym_core::AgentNode,
    host: &str,
    port: u16,
) -> Result<()> {
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
        session::local::LocalSessionManager,
    };

    let agent = Arc::new(agent);
    let job_events = Arc::new(Mutex::new(HashMap::new()));

    let ct = tokio_util::sync::CancellationToken::new();
    let config = StreamableHttpServerConfig {
        stateful_mode: true,
        cancellation_token: ct.clone(),
        ..Default::default()
    };

    let agent_clone = Arc::clone(&agent);
    let job_events_clone = Arc::clone(&job_events);

    let service: StreamableHttpService<ElisymServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(ElisymServer::from_shared(
                Arc::clone(&agent_clone),
                Arc::clone(&job_events_clone),
            )),
            Default::default(),
            config,
        );

    let router = axum::Router::new().nest_service("/mcp", service);

    let bind_addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("Cannot bind to {bind_addr}"))?;

    tracing::info!(address = %bind_addr, endpoint = "/mcp", "HTTP transport started");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("Shutting down HTTP server");
            ct.cancel();
        })
        .await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Install {
            client,
            agent,
            list,
        }) => {
            if list {
                install::run_list();
            } else {
                install::run_install(client.as_deref(), agent.as_deref())?;
            }
            return Ok(());
        }
        Some(Commands::Uninstall { client }) => {
            install::run_uninstall(client.as_deref())?;
            return Ok(());
        }
        None => {}
    }

    // MCP server mode (default — no subcommand)
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("Starting elisym MCP server");

    let builder = if let Ok(agent_name) = std::env::var("ELISYM_AGENT") {
        let config = load_agent_config(&agent_name)?;
        tracing::info!(agent = %agent_name, "Loading agent from ~/.elisym/agents/");

        let mut b = AgentNodeBuilder::new(&config.name, &config.description)
            .capabilities(config.capabilities)
            .secret_key(&config.secret_key);

        if !config.relays.is_empty() {
            b = b.relays(config.relays);
        }

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

    if cli.http {
        #[cfg(feature = "transport-http")]
        {
            start_http_server(agent, &cli.host, cli.port).await?;
        }
        #[cfg(not(feature = "transport-http"))]
        {
            anyhow::bail!(
                "HTTP transport not available. Rebuild with: cargo build --features transport-http"
            );
        }
    } else {
        let server = ElisymServer::new(agent);
        let service = server
            .serve(stdio())
            .await
            .inspect_err(|e| tracing::error!("Failed to start MCP service: {e}"))?;

        service.waiting().await?;
    }

    tracing::info!("elisym MCP server stopped");
    Ok(())
}
