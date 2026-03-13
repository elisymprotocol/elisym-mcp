mod agent_config;
mod crypto;
mod global_config;
mod install;
mod sanitize;
mod server;
mod tools;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use dialoguer::{Confirm, Input, Password, Select, theme::ColorfulTheme};
use elisym_core::AgentNodeBuilder;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::{self, EnvFilter};
use zeroize::{Zeroize, Zeroizing};

use agent_config::{
    builder_from_config, load_agent_config, run_init, validate_agent_name,
};
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

    /// Bearer token for HTTP transport authentication.
    /// Can also be set via ELISYM_HTTP_TOKEN env var.
    #[arg(long)]
    http_token: Option<String>,
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

        /// Password to decrypt encrypted agent configs.
        #[arg(long)]
        password: Option<String>,

        /// Bearer token for HTTP transport authentication.
        #[arg(long)]
        http_token: Option<String>,

        /// Set additional env vars (KEY=VALUE, can be repeated).
        #[arg(long = "env", value_name = "KEY=VALUE")]
        extra_env: Vec<String>,

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

    /// Create a new agent identity (interactive wizard if no name given).
    Init {
        /// Agent name (used as directory name under ~/.elisym/agents/).
        /// If omitted, launches interactive wizard.
        name: Option<String>,

        /// Agent description.
        #[arg(long)]
        description: Option<String>,

        /// Capabilities (comma-separated).
        #[arg(long)]
        capabilities: Option<String>,

        /// Encrypt secret keys with a password (AES-256-GCM + Argon2id).
        #[arg(long)]
        password: Option<String>,

        /// Solana network: devnet, testnet, mainnet (default: devnet).
        #[arg(long)]
        network: Option<String>,

        /// Auto-install into MCP clients after creating the agent.
        #[arg(long)]
        install: bool,
    },
}

/// Interactive CLI wizard for agent setup.
fn run_init_wizard() -> Result<()> {
    let theme = ColorfulTheme::default();

    println!();
    println!("  elisym — agent setup wizard");
    println!("  ───────────────────────────");
    println!();

    // 1. Agent name
    let name: String = Input::with_theme(&theme)
        .with_prompt("Agent name")
        .default("my-agent".into())
        .validate_with(|input: &String| -> Result<(), String> {
            validate_agent_name(input).map_err(|e| e.to_string())
        })
        .interact_text()
        .context("Failed to read agent name")?;

    // Check if agent already exists
    let home = dirs::home_dir().context("Cannot find home directory")?;
    let config_path = home
        .join(".elisym")
        .join("agents")
        .join(&name)
        .join("config.toml");
    if config_path.exists() {
        println!();
        println!("  Agent '{}' already exists at {}", name, config_path.display());
        println!("  To recreate, first delete the existing config.");
        return Ok(());
    }

    // 2. Description
    let description: String = Input::with_theme(&theme)
        .with_prompt("Description")
        .default("Elisym MCP agent".into())
        .interact_text()
        .context("Failed to read description")?;

    // 3. Capabilities
    let capabilities: String = Input::with_theme(&theme)
        .with_prompt("Capabilities (comma-separated)")
        .default("mcp-gateway".into())
        .interact_text()
        .context("Failed to read capabilities")?;

    // 4. Solana network
    let networks = &["devnet", "testnet", "mainnet"];
    let network_idx = Select::with_theme(&theme)
        .with_prompt("Solana network")
        .items(networks)
        .default(0)
        .interact()
        .context("Failed to select network")?;
    let network = networks[network_idx];

    // 5. Password encryption
    let encrypt = Confirm::with_theme(&theme)
        .with_prompt("Encrypt secret keys with a password?")
        .default(false)
        .interact()
        .context("Failed to read encryption preference")?;

    let mut password: Option<Zeroizing<String>> = if encrypt {
        let pw = Password::with_theme(&theme)
            .with_prompt("Password")
            .with_confirmation("Confirm password", "Passwords don't match")
            .interact()
            .context("Failed to read password")?;
        Some(Zeroizing::new(pw))
    } else {
        None
    };

    // 6. Auto-install (skip prompt if already configured)
    let already_installed = install::is_installed();
    let auto_install = if already_installed {
        false
    } else {
        Confirm::with_theme(&theme)
            .with_prompt("Install into MCP clients (Claude Desktop, Cursor, etc.)?")
            .default(true)
            .interact()
            .context("Failed to read install preference")?
    };

    println!();

    // Create the agent
    run_init(
        &name,
        Some(&description),
        Some(&capabilities),
        password.as_deref().map(|s| s.as_str()),
        network,
        false,
    )?;

    // Auto-install if requested
    if auto_install {
        println!();
        let mut env = vec![("ELISYM_AGENT".to_string(), name)];
        if let Some(ref pw) = password {
            env.push(("ELISYM_AGENT_PASSWORD".to_string(), pw.to_string()));
        }
        install::run_install(None, None, &env)?;
    }

    if let Some(ref mut pw) = password {
        pw.zeroize();
    }

    Ok(())
}

#[cfg(feature = "transport-http")]
async fn start_http_server(
    builder: elisym_core::AgentNodeBuilder,
    host: &str,
    port: u16,
    http_token: Option<String>,
) -> Result<()> {
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
        session::local::LocalSessionManager,
    };

    // Build agent eagerly for HTTP transport (shared across sessions)
    let agent = builder.build().await?;
    let agent_name = agent.capability_card.name.clone();
    let agent = Arc::new(agent);
    tracing::info!(
        npub = %agent.identity.npub(),
        payments = agent.payments.is_some(),
        "Agent node started (HTTP transport)"
    );
    let job_cache = Arc::new(Mutex::new(server::JobEventsCache::new()));

    let mut registry = std::collections::HashMap::new();
    registry.insert(agent_name.clone(), server::AgentEntry {
        node: Arc::clone(&agent),
        ping_handle: tokio::spawn(async {}),
        ping_active: false,
    });
    let agent_registry = Arc::new(std::sync::RwLock::new(registry));
    let active_agent_name = Arc::new(std::sync::RwLock::new(agent_name));

    let ct = tokio_util::sync::CancellationToken::new();
    let config = StreamableHttpServerConfig {
        stateful_mode: true,
        cancellation_token: ct.clone(),
        ..Default::default()
    };

    let registry_clone = Arc::clone(&agent_registry);
    let active_clone = Arc::clone(&active_agent_name);
    let job_cache_clone = Arc::clone(&job_cache);

    let service: StreamableHttpService<ElisymServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(ElisymServer::from_shared(
                Arc::clone(&registry_clone),
                Arc::clone(&active_clone),
                Arc::clone(&job_cache_clone),
            )),
            Default::default(),
            config,
        );

    let mut router = axum::Router::new().nest_service("/mcp", service);

    // Add bearer token auth middleware if configured
    if let Some(token) = http_token {
        use axum::http::StatusCode;
        use subtle::ConstantTimeEq;

        let expected = format!("Bearer {token}");
        router = router.layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let expected = expected.clone();
                async move {
                    let auth = req
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default();
                    // Constant-time comparison to prevent timing side-channels.
                    // Explicit length check avoids relying on ct_eq's implicit
                    // early-return for different-length slices.
                    if auth.len() == expected.len()
                        && auth.as_bytes().ct_eq(expected.as_bytes()).into()
                    {
                        Ok(next.run(req).await)
                    } else {
                        Err(StatusCode::UNAUTHORIZED)
                    }
                }
            },
        ));
        tracing::info!("HTTP bearer token authentication enabled");
    } else if host != "127.0.0.1" && host != "localhost" {
        tracing::warn!(
            "HTTP transport exposed on {host} without authentication. \
             Consider using --http-token for security."
        );
    }

    // Deny browser cross-origin requests when exposed on non-localhost
    if host != "127.0.0.1" && host != "localhost" {
        router = router.layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::AllowOrigin::list([])),
        );
    }

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
            mut password,
            http_token: install_http_token,
            extra_env,
            list,
        }) => {
            if list {
                install::run_list();
            } else {
                let mut env = Vec::new();
                if let Some(ref pw) = password {
                    eprintln!("WARNING: --password is visible in process listings. Use ELISYM_AGENT_PASSWORD env var instead.");
                    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                        eprintln!("HINT: Running non-interactively. Prefer ELISYM_AGENT_PASSWORD env var over --password.");
                    }
                    env.push(("ELISYM_AGENT_PASSWORD".to_string(), pw.clone()));
                }
                if let Some(ref tok) = install_http_token {
                    eprintln!("WARNING: --http-token is visible in process listings. Use ELISYM_HTTP_TOKEN env var instead.");
                    env.push(("ELISYM_HTTP_TOKEN".to_string(), tok.clone()));
                }
                for kv in &extra_env {
                    if let Some((k, v)) = kv.split_once('=') {
                        env.push((k.to_string(), v.to_string()));
                    } else {
                        anyhow::bail!("Invalid --env format: '{kv}'. Expected KEY=VALUE.");
                    }
                }
                install::run_install(client.as_deref(), agent.as_deref(), &env)?;
            }
            if let Some(ref mut pw) = password {
                pw.zeroize();
            }
            return Ok(());
        }
        Some(Commands::Uninstall { client }) => {
            install::run_uninstall(client.as_deref())?;
            return Ok(());
        }
        Some(Commands::Init {
            name,
            description,
            capabilities,
            mut password,
            network,
            install: auto_install,
        }) => {
            if let Some(name) = name {
                // Non-interactive mode (backward compatible)
                if password.is_some() {
                    eprintln!("WARNING: --password is visible in process listings. Use ELISYM_AGENT_PASSWORD env var instead.");
                    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                        eprintln!("HINT: Running non-interactively. Prefer ELISYM_AGENT_PASSWORD env var over --password.");
                    }
                }
                let desc = description.as_deref().or(Some("Elisym MCP agent"));
                let caps = capabilities.as_deref().or(Some("mcp-gateway"));
                let net = network.as_deref().unwrap_or("devnet");
                run_init(
                    &name,
                    desc,
                    caps,
                    password.as_deref(),
                    net,
                    false,
                )?;
                if auto_install {
                    if install::is_installed() {
                        println!();
                        println!("  MCP already configured — agent created.");
                        println!("  Use `create_agent` or `switch_agent` tools to manage agents at runtime.");
                    } else {
                        let mut env = vec![("ELISYM_AGENT".to_string(), name)];
                        if let Some(ref pw) = password {
                            env.push(("ELISYM_AGENT_PASSWORD".to_string(), pw.clone()));
                        }
                        install::run_install(None, None, &env)?;
                    }
                }
                if let Some(ref mut pw) = password {
                    pw.zeroize();
                }
            } else {
                // Interactive wizard mode
                run_init_wizard()?;
            }
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

    let (agent_name, builder) = if let Ok(agent_name) = std::env::var("ELISYM_AGENT") {
        let config = load_agent_config(&agent_name)?;
        tracing::info!(agent = %agent_name, "Loading agent from ~/.elisym/agents/");
        (agent_name, builder_from_config(&config))
    } else if std::env::var("ELISYM_NOSTR_SECRET").is_ok() {
        // Explicit secret key — ephemeral mode, no auto-persist
        let agent_name =
            std::env::var("ELISYM_AGENT_NAME").unwrap_or_else(|_| "mcp-agent".into());
        let agent_desc = std::env::var("ELISYM_AGENT_DESCRIPTION")
            .unwrap_or_else(|_| "elisym MCP server agent".into());

        let secret = Zeroizing::new(std::env::var("ELISYM_NOSTR_SECRET").unwrap());
        let mut b = AgentNodeBuilder::new(&agent_name, &agent_desc)
            .capabilities(vec!["mcp-gateway".into()])
            .secret_key(secret.as_str());

        if let Ok(relays) = std::env::var("ELISYM_RELAYS") {
            let relay_list: Vec<String> =
                relays.split(',').map(|s| s.trim().to_string()).collect();
            if !relay_list.is_empty() {
                b = b.relays(relay_list);
            }
        }
        (agent_name, b)
    } else {
        // No ELISYM_AGENT, no ELISYM_NOSTR_SECRET — check default_agent, then fallback
        let agent_name = std::env::var("ELISYM_AGENT_NAME")
            .ok()
            .or_else(global_config::get_default_agent)
            .unwrap_or_else(|| "mcp-agent".into());

        // Try loading existing config first, create if missing
        let config = match load_agent_config(&agent_name) {
            Ok(config) => {
                tracing::info!(agent = %agent_name, "Reusing persisted agent identity");
                config
            }
            Err(_) => {
                tracing::info!(agent = %agent_name, "Creating new agent identity");
                run_init(&agent_name, None, None, None, "devnet", true)?;
                load_agent_config(&agent_name)
                    .context("Failed to load newly created agent config")?
            }
        };
        (agent_name, builder_from_config(&config))
    };

    if cli.http {
        #[cfg(feature = "transport-http")]
        {
            if cli.http_token.is_some() {
                eprintln!("WARNING: --http-token is visible in process listings. Use ELISYM_HTTP_TOKEN env var instead.");
            }
            let http_token = cli
                .http_token
                .or_else(|| std::env::var("ELISYM_HTTP_TOKEN").ok());
            start_http_server(builder, &cli.host, cli.port, http_token).await?;
        }
        #[cfg(not(feature = "transport-http"))]
        {
            anyhow::bail!(
                "HTTP transport not available. Rebuild with: cargo build --features transport-http"
            );
        }
    } else {
        let server = ElisymServer::new(agent_name, builder);
        let service = server
            .serve(stdio())
            .await
            .inspect_err(|e| tracing::error!("Failed to start MCP service: {e}"))?;

        service.waiting().await?;
    }

    tracing::info!("elisym MCP server stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_agent_names() {
        assert!(validate_agent_name("my-agent").is_ok());
        assert!(validate_agent_name("agent_1").is_ok());
        assert!(validate_agent_name("AgentX").is_ok());
        assert!(validate_agent_name("a").is_ok());
    }

    #[test]
    fn empty_name() {
        assert!(validate_agent_name("").is_err());
    }

    #[test]
    fn path_traversal() {
        assert!(validate_agent_name("../evil").is_err());
        assert!(validate_agent_name("foo/bar").is_err());
        assert!(validate_agent_name("foo\\bar").is_err());
    }

    #[test]
    fn hidden_dir() {
        assert!(validate_agent_name(".hidden").is_err());
    }

    #[test]
    fn control_chars() {
        assert!(validate_agent_name("agent\x00").is_err());
        assert!(validate_agent_name("agent\n").is_err());
        assert!(validate_agent_name("agent\t").is_err());
    }

    #[test]
    fn spaces_rejected() {
        assert!(validate_agent_name("my agent").is_err());
    }

    #[test]
    fn too_long_name() {
        let long = "a".repeat(65);
        assert!(validate_agent_name(&long).is_err());
        let ok = "a".repeat(64);
        assert!(validate_agent_name(&ok).is_ok());
    }

    #[test]
    fn shell_metacharacters() {
        assert!(validate_agent_name("agent;rm").is_err());
        assert!(validate_agent_name("agent$(cmd)").is_err());
        assert!(validate_agent_name("agent`cmd`").is_err());
    }
}
