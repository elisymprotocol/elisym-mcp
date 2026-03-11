//! Agent configuration loading, validation, and initialization.
//!
//! Extracted from `main.rs` so that both CLI and MCP server tools
//! (e.g. `create_agent`, `switch_agent`) can reuse the same logic.

use anyhow::{Context, Result};
use elisym_core::{AgentNodeBuilder, SolanaNetwork, SolanaPaymentConfig, SolanaPaymentProvider};
use nostr_sdk::ToBech32;
use serde::{Deserialize, Serialize};
use solana_sdk::signature::Signer as _;
use zeroize::Zeroize;

use crate::crypto;

/// Minimal subset of elisym-client's AgentConfig — just what we need.
#[derive(Deserialize)]
pub(crate) struct AgentConfig {
    pub(crate) name: String,
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) capabilities: Vec<String>,
    #[serde(default)]
    pub(crate) relays: Vec<String>,
    #[serde(default)]
    pub(crate) secret_key: String,
    #[serde(default)]
    pub(crate) payment: Option<PaymentSection>,
    #[serde(default)]
    pub(crate) encryption: Option<crypto::EncryptionSection>,
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
pub(crate) struct PaymentSection {
    #[serde(default = "default_chain")]
    pub(crate) chain: String,
    #[serde(default = "default_network")]
    pub(crate) network: String,
    #[serde(default)]
    pub(crate) rpc_url: Option<String>,
    #[serde(default)]
    pub(crate) solana_secret_key: String,
    #[serde(default = "default_job_price")]
    #[allow(dead_code)]
    pub(crate) job_price: u64,
    #[serde(default = "default_payment_timeout")]
    #[allow(dead_code)]
    pub(crate) payment_timeout_secs: u32,
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

pub(crate) fn default_chain() -> String {
    "solana".into()
}
pub(crate) fn default_network() -> String {
    "devnet".into()
}
pub(crate) fn default_job_price() -> u64 {
    10_000_000
}
pub(crate) fn default_payment_timeout() -> u32 {
    120
}

/// Validate agent name: only ASCII alphanumeric, hyphens, underscores. Max 64 chars.
pub(crate) fn validate_agent_name(name: &str) -> Result<()> {
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

pub(crate) fn load_agent_config(name: &str) -> Result<AgentConfig> {
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

pub(crate) fn builder_from_config(config: &AgentConfig) -> AgentNodeBuilder {
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

pub(crate) fn build_solana_provider(payment: &PaymentSection) -> Option<SolanaPaymentProvider> {
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

pub(crate) fn run_init(
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
