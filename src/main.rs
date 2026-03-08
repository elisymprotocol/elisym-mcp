mod crypto;
mod install;
mod server;
mod tools;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use elisym_core::{
    AgentNodeBuilder, SolanaNetwork, SolanaPaymentConfig, SolanaPaymentProvider,
};
use rmcp::{ServiceExt, transport::stdio};
use nostr_sdk::ToBech32;
use solana_sdk::signature::Signer as _;
use serde::{Deserialize, Serialize};
use tracing_subscriber::{self, EnvFilter};
use zeroize::{Zeroize, Zeroizing};

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

    /// Create a new agent identity (generates Nostr keypair + config).
    Init {
        /// Agent name (used as directory name under ~/.elisym/agents/).
        name: String,

        /// Agent description.
        #[arg(long, default_value = "elisym MCP agent")]
        description: Option<String>,

        /// Capabilities (comma-separated).
        #[arg(long, default_value = "mcp-gateway")]
        capabilities: Option<String>,

        /// Encrypt secret keys with a password (AES-256-GCM + Argon2id).
        #[arg(long)]
        password: Option<String>,

        /// Solana network: devnet, testnet, mainnet (default: devnet).
        #[arg(long, default_value = "devnet")]
        network: Option<String>,

        /// Auto-install into MCP clients after creating the agent.
        #[arg(long)]
        install: bool,
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
    #[serde(default)]
    secret_key: String,
    #[serde(default)]
    payment: Option<PaymentSection>,
    #[serde(default)]
    encryption: Option<crypto::EncryptionSection>,
}

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("name", &self.name)
            .field("secret_key", &"[REDACTED]")
            .field("encryption", &self.encryption.is_some())
            .finish()
    }
}

impl Drop for AgentConfig {
    fn drop(&mut self) {
        self.secret_key.zeroize();
        if let Some(ref mut p) = self.payment {
            p.solana_secret_key.zeroize();
        }
    }
}

#[derive(Deserialize)]
struct PaymentSection {
    #[serde(default = "default_chain")]
    chain: String,
    #[serde(default = "default_network")]
    network: String,
    #[serde(default)]
    rpc_url: Option<String>,
    #[serde(default)]
    solana_secret_key: String,
    #[serde(default = "default_job_price")]
    #[allow(dead_code)]
    job_price: u64,
    #[serde(default = "default_payment_timeout")]
    #[allow(dead_code)]
    payment_timeout_secs: u32,
}

impl std::fmt::Debug for PaymentSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaymentSection")
            .field("chain", &self.chain)
            .field("network", &self.network)
            .field("solana_secret_key", &"[REDACTED]")
            .finish()
    }
}

fn default_chain() -> String {
    "solana".into()
}
fn default_network() -> String {
    "devnet".into()
}
fn default_job_price() -> u64 {
    10_000_000
}
fn default_payment_timeout() -> u32 {
    120
}

/// Validate agent name: only ASCII alphanumeric, hyphens, underscores. Max 64 chars.
fn validate_agent_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        "Invalid agent name: '{name}'. Only [a-zA-Z0-9_-] allowed, max 64 chars."
    );
    Ok(())
}

fn load_agent_config(name: &str) -> Result<AgentConfig> {
    validate_agent_name(name)?;
    let home = dirs::home_dir().context("Cannot find home directory")?;
    let path = home
        .join(".elisym")
        .join("agents")
        .join(name)
        .join("config.toml");

    // Warn if config file is readable by others (contains secret keys)
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = std::fs::metadata(&path) {
            let mode = meta.mode();
            if mode & 0o077 != 0 {
                tracing::warn!(
                    path = %path.display(),
                    mode = format!("{mode:04o}"),
                    "Agent config file has insecure permissions (contains secret keys). \
                     Consider: chmod 600 {}",
                    path.display()
                );
            }
        }
    }

    let mut contents = std::fs::read_to_string(&path)
        .with_context(|| format!("Agent '{}' not found at {}", name, path.display()))?;
    let config_result: Result<AgentConfig, _> = toml::from_str(&contents);
    contents.zeroize();
    let mut config: AgentConfig =
        config_result.with_context(|| format!("Invalid config for agent '{}'", name))?;

    // Decrypt secrets if the config is encrypted
    if let Some(ref enc) = config.encryption {
        let mut password = std::env::var("ELISYM_AGENT_PASSWORD").with_context(|| {
            format!(
                "Agent '{}' has encrypted secrets. Set ELISYM_AGENT_PASSWORD env var to decrypt.",
                name
            )
        })?;
        let result = crypto::decrypt_secrets(enc, &password);
        password.zeroize();
        let bundle = result
            .with_context(|| format!("Failed to decrypt secrets for agent '{}'", name))?;
        config.secret_key = bundle.nostr_secret_key.clone();
        if let Some(ref mut payment) = config.payment {
            payment.solana_secret_key = bundle.solana_secret_key.clone();
        }
        tracing::info!("Decrypted agent secrets");
    }

    Ok(config)
}

fn builder_from_config(config: &AgentConfig) -> AgentNodeBuilder {
    let mut b = AgentNodeBuilder::new(&config.name, &config.description)
        .capabilities(config.capabilities.clone())
        .secret_key(&config.secret_key);

    if !config.relays.is_empty() {
        b = b.relays(config.relays.clone());
    }

    if let Some(ref payment) = config.payment {
        if let Some(provider) = build_solana_provider(payment) {
            b = b.solana_payment_provider(provider);
        }
    }

    b
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

fn run_init(
    name: &str,
    description: Option<&str>,
    capabilities: Option<&str>,
    password: Option<&str>,
    network: &str,
    quiet: bool,
) -> Result<()> {
    validate_agent_name(name)?;

    let home = dirs::home_dir().context("Cannot find home directory")?;
    let agent_dir = home.join(".elisym").join("agents").join(name);
    let config_path = agent_dir.join("config.toml");

    // Generate Nostr keypair
    let keys = nostr_sdk::Keys::generate();
    let secret_hex = keys.secret_key().to_secret_hex();
    let npub = keys.public_key().to_bech32().unwrap_or_default();

    // Generate Solana keypair
    let sol_keypair = solana_sdk::signature::Keypair::new();
    let sol_secret_b58 = bs58::encode(sol_keypair.to_bytes()).into_string();
    let sol_address = sol_keypair.pubkey().to_string();

    let desc = description.unwrap_or("elisym MCP agent");
    let caps: Vec<&str> = capabilities
        .unwrap_or("mcp-gateway")
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    // Build config using proper TOML serialization (prevents injection via user input)
    #[derive(Serialize)]
    struct InitConfig {
        name: String,
        description: String,
        capabilities: Vec<String>,
        relays: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        secret_key: Option<String>,
        payment: InitPayment,
        #[serde(skip_serializing_if = "Option::is_none")]
        encryption: Option<InitEncryption>,
    }
    #[derive(Serialize)]
    struct InitPayment {
        chain: String,
        network: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        solana_secret_key: Option<String>,
    }
    #[derive(Serialize)]
    struct InitEncryption {
        ciphertext: String,
        salt: String,
        nonce: String,
    }

    let caps_vec: Vec<String> = caps.iter().map(|c| c.to_string()).collect();
    let relays = vec![
        "wss://relay.damus.io".into(),
        "wss://nos.lol".into(),
        "wss://relay.nostr.band".into(),
    ];

    let (secret_key_field, sol_key_field, encryption_field, encrypted) = if let Some(pw) = password {
        let bundle = crypto::SecretsBundle {
            nostr_secret_key: secret_hex,
            solana_secret_key: sol_secret_b58,
            llm_api_key: String::new(),
            customer_llm_api_key: None,
        };
        let enc = crypto::encrypt_secrets(&bundle, pw)
            .context("Failed to encrypt secrets")?;
        (None, None, Some(InitEncryption {
            ciphertext: enc.ciphertext,
            salt: enc.salt,
            nonce: enc.nonce,
        }), true)
    } else {
        (Some(secret_hex), Some(sol_secret_b58), None, false)
    };

    let init_config = InitConfig {
        name: name.to_string(),
        description: desc.to_string(),
        capabilities: caps_vec,
        relays,
        secret_key: secret_key_field,
        payment: InitPayment {
            chain: "solana".into(),
            network: network.to_string(),
            solana_secret_key: sol_key_field,
        },
        encryption: encryption_field,
    };

    let mut config_content = toml::to_string_pretty(&init_config)
        .context("Failed to serialize config")?;

    // Zeroize secret key material now that it's been serialized.
    // Note: serde/toml internal buffers are not zeroizable — this is a known limitation.
    // SecretsBundle (encrypted path) handles ZeroizeOnDrop; for the plaintext
    // path, zeroize the fields that held raw secret keys.
    if let Some(mut sk) = init_config.secret_key {
        sk.zeroize();
    }
    if let Some(mut sk) = init_config.payment.solana_secret_key {
        sk.zeroize();
    }

    // Create directory and write config atomically (create_new prevents TOCTOU race)
    std::fs::create_dir_all(&agent_dir)
        .with_context(|| format!("Cannot create {}", agent_dir.display()))?;
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&config_path)
            .with_context(|| format!("Agent '{}' already exists at {}", name, config_path.display()))?;
        file.write_all(config_content.as_bytes())
            .with_context(|| format!("Cannot write {}", config_path.display()))?;
    }
    config_content.zeroize();

    // Set permissions to 600 (owner-only) on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Cannot set permissions on {}", config_path.display()))?;
    }

    if quiet {
        tracing::info!(
            agent = name,
            npub = %npub,
            solana = %sol_address,
            config = %config_path.display(),
            encrypted,
            "Agent created"
        );
    } else {
        println!("Agent '{}' created.", name);
        println!("  npub: {}", npub);
        println!("  solana: {} ({network})", sol_address);
        println!("  config: {}", config_path.display());
        if encrypted {
            println!("  encrypted: yes (AES-256-GCM + Argon2id)");
        }
        println!();
        println!("To use with MCP:");
        if encrypted {
            println!("  elisym-mcp install --agent {name} --password <password>");
        } else {
            println!("  elisym-mcp install --agent {name}");
        }
        println!("  # or: ELISYM_AGENT={name} elisym-mcp");
    }

    Ok(())
}

#[cfg(feature = "transport-http")]
async fn start_http_server(
    agent: elisym_core::AgentNode,
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

    let agent = Arc::new(agent);
    let job_cache = Arc::new(Mutex::new(server::JobEventsCache::new()));

    let ct = tokio_util::sync::CancellationToken::new();
    let config = StreamableHttpServerConfig {
        stateful_mode: true,
        cancellation_token: ct.clone(),
        ..Default::default()
    };

    let agent_clone = Arc::clone(&agent);
    let job_cache_clone = Arc::clone(&job_cache);

    let service: StreamableHttpService<ElisymServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(ElisymServer::from_shared(
                Arc::clone(&agent_clone),
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
            if password.is_some() {
                eprintln!("WARNING: --password is visible in process listings. Use ELISYM_AGENT_PASSWORD env var instead.");
                if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                    eprintln!("HINT: Running non-interactively. Prefer ELISYM_AGENT_PASSWORD env var over --password.");
                }
            }
            let net = network.as_deref().unwrap_or("devnet");
            run_init(
                &name,
                description.as_deref(),
                capabilities.as_deref(),
                password.as_deref(),
                net,
                false,
            )?;
            if auto_install {
                let mut env = vec![("ELISYM_AGENT".to_string(), name)];
                if let Some(ref pw) = password {
                    env.push(("ELISYM_AGENT_PASSWORD".to_string(), pw.clone()));
                }
                install::run_install(None, None, &env)?;
            }
            if let Some(ref mut pw) = password {
                pw.zeroize();
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

    let builder = if let Ok(agent_name) = std::env::var("ELISYM_AGENT") {
        let config = load_agent_config(&agent_name)?;
        tracing::info!(agent = %agent_name, "Loading agent from ~/.elisym/agents/");
        builder_from_config(&config)
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
        b
    } else {
        // No ELISYM_AGENT, no ELISYM_NOSTR_SECRET — auto-persist a default identity
        let agent_name =
            std::env::var("ELISYM_AGENT_NAME").unwrap_or_else(|_| "mcp-agent".into());

        // Try loading existing config first, create if missing
        match load_agent_config(&agent_name) {
            Ok(config) => {
                tracing::info!(agent = %agent_name, "Reusing persisted agent identity");
                builder_from_config(&config)
            }
            Err(_) => {
                tracing::info!(agent = %agent_name, "Creating new agent identity");
                run_init(&agent_name, None, None, None, "devnet", true)?;
                let config = load_agent_config(&agent_name)
                    .context("Failed to load newly created agent config")?;
                builder_from_config(&config)
            }
        }
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
            if cli.http_token.is_some() {
                eprintln!("WARNING: --http-token is visible in process listings. Use ELISYM_HTTP_TOKEN env var instead.");
            }
            let http_token = cli
                .http_token
                .or_else(|| std::env::var("ELISYM_HTTP_TOKEN").ok());
            start_http_server(agent, &cli.host, cli.port, http_token).await?;
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
