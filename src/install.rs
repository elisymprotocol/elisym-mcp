use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;

/// Known MCP client configurations.
struct McpClient {
    name: &'static str,
    config_path: fn() -> Option<PathBuf>,
}

const CLIENTS: &[McpClient] = &[
    McpClient {
        name: "claude-desktop",
        config_path: claude_desktop_config,
    },
    McpClient {
        name: "cursor",
        config_path: cursor_config,
    },
    McpClient {
        name: "windsurf",
        config_path: windsurf_config,
    },
];

fn home() -> Option<PathBuf> {
    dirs::home_dir()
}

/// Check if an MCP client app is installed on this machine.
/// Currently only implemented for macOS (/Applications/*.app check).
/// On other platforms, auto-detection relies solely on config file existence.
fn is_app_installed(client_name: &str) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let app_name = match client_name {
        "claude-desktop" => "Claude",
        "cursor" => "Cursor",
        "windsurf" => "Windsurf",
        _ => return false,
    };
    PathBuf::from(format!("/Applications/{app_name}.app")).exists()
}

fn claude_desktop_config() -> Option<PathBuf> {
    let home = home()?;
    if cfg!(target_os = "macos") {
        Some(
            home.join("Library")
                .join("Application Support")
                .join("Claude")
                .join("claude_desktop_config.json"),
        )
    } else if cfg!(target_os = "windows") {
        dirs::config_dir().map(|d| d.join("Claude").join("claude_desktop_config.json"))
    } else {
        // Linux: ~/.config/Claude/
        Some(
            home.join(".config")
                .join("Claude")
                .join("claude_desktop_config.json"),
        )
    }
}

fn cursor_config() -> Option<PathBuf> {
    Some(home()?.join(".cursor").join("mcp.json"))
}

fn windsurf_config() -> Option<PathBuf> {
    let home = home()?;
    if cfg!(target_os = "macos") {
        Some(
            home.join("Library")
                .join("Application Support")
                .join("Windsurf")
                .join("mcp.json"),
        )
    } else {
        Some(home.join(".windsurf").join("mcp.json"))
    }
}

/// Resolve the elisym-mcp binary path for the config.
fn resolve_command() -> (String, Vec<String>) {
    // If installed via cargo/brew, use the binary name directly
    if let Ok(path) = std::env::current_exe() {
        let path_str = path.to_string_lossy().to_string();
        // If running from a standard install location, use the path
        if path_str.contains(".cargo/bin")
            || path_str.contains("/usr/local/bin")
            || path_str.contains("/opt/homebrew")
        {
            return (path_str, vec![]);
        }
    }
    // Default to npx
    ("npx".to_string(), vec!["-y".to_string(), "elisym-mcp".to_string()])
}

fn build_server_entry(agent: Option<&str>, env: &[(String, String)]) -> Value {
    let (command, args) = resolve_command();

    let mut entry = serde_json::json!({
        "command": command,
        "args": args,
    });

    // Build env object from agent name + extra env vars
    let mut env_map = serde_json::Map::new();
    if let Some(agent_name) = agent {
        env_map.insert("ELISYM_AGENT".to_string(), Value::String(agent_name.to_string()));
    }
    for (k, v) in env {
        env_map.insert(k.clone(), Value::String(v.clone()));
    }
    if !env_map.is_empty() {
        entry["env"] = Value::Object(env_map);
    }

    entry
}

/// Validate install flags for conflicts and security concerns.
fn validate_install_flags(
    agent: Option<&str>,
    env: &[(String, String)],
) -> Result<()> {
    // Reject conflicting --agent and --env ELISYM_AGENT
    if agent.is_some() && env.iter().any(|(k, _)| k == "ELISYM_AGENT") {
        anyhow::bail!(
            "Cannot use both --agent and --env ELISYM_AGENT=... (they conflict)"
        );
    }

    // Warn if password will be written to config file
    let has_password = env.iter().any(|(k, _)| k == "ELISYM_AGENT_PASSWORD");
    if has_password {
        eprintln!(
            "Warning: ELISYM_AGENT_PASSWORD will be stored in plaintext in the MCP client \
             config file. For better security, set it as a system environment variable instead."
        );
    }

    Ok(())
}

fn install_to_config(path: &PathBuf, agent: Option<&str>, env: &[(String, String)]) -> Result<bool> {
    // Read existing config or start fresh
    let mut config: Value = if path.exists() {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Cannot read {}", path.display()))?;
        serde_json::from_str(&contents)
            .with_context(|| format!("Invalid JSON in {}", path.display()))?
    } else {
        serde_json::json!({})
    };

    // Ensure mcpServers is a JSON object
    if !config.get("mcpServers").is_some_and(|v| v.is_object()) {
        config["mcpServers"] = serde_json::json!({});
    }

    let servers = config["mcpServers"]
        .as_object_mut()
        .context("mcpServers is not an object")?;

    // Check if already installed
    if servers.contains_key("elisym") {
        return Ok(false);
    }

    servers.insert("elisym".to_string(), build_server_entry(agent, env));

    // Write back
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create directory {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(&config)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json + "\n")
        .with_context(|| format!("Cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("Cannot rename {} to {}", tmp.display(), path.display()))?;

    Ok(true)
}

pub fn run_list() {
    println!("Detected MCP clients:\n");
    let mut found = false;
    for client in CLIENTS {
        if let Some(path) = (client.config_path)() {
            let status = if path.exists() {
                // Check if already configured
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    if let Ok(config) = serde_json::from_str::<Value>(&contents) {
                        if config
                            .get("mcpServers")
                            .and_then(|s| s.get("elisym"))
                            .is_some()
                        {
                            "installed"
                        } else {
                            "available"
                        }
                    } else {
                        "available (invalid config)"
                    }
                } else {
                    "available"
                }
            } else {
                // Config file doesn't exist — check if the app itself is installed
                if is_app_installed(client.name) {
                    "available (no config file yet)"
                } else {
                    continue; // Skip — app not installed
                }
            };

            println!("  {:<20} {} [{}]", client.name, path.display(), status);
            found = true;
        }
    }

    if !found {
        println!("  No supported MCP clients found.");
        println!("\n  Supported: Claude Desktop, Cursor, Windsurf");
    }

    // Also check Claude Code CLI
    println!();
    println!("CLI clients (use their own install commands):");
    println!("  claude-code         claude mcp add elisym -- npx -y elisym-mcp");
    println!("  codex               codex mcp add elisym -- npx -y elisym-mcp");
}

pub fn run_install(client_filter: Option<&str>, agent: Option<&str>, env: &[(String, String)]) -> Result<()> {
    validate_install_flags(agent, env)?;

    let mut installed = 0;
    let mut skipped = 0;

    for client in CLIENTS {
        if let Some(filter) = client_filter {
            if client.name != filter {
                continue;
            }
        }

        let Some(path) = (client.config_path)() else {
            continue;
        };

        // If no filter, only install to clients that have a config file or app installed
        if client_filter.is_none() && !path.exists() && !is_app_installed(client.name) {
            continue;
        }

        match install_to_config(&path, agent, env) {
            Ok(true) => {
                println!("  Installed to {} ({})", client.name, path.display());
                installed += 1;
            }
            Ok(false) => {
                println!(
                    "  Already installed in {} ({}). To update, run: elisym-mcp uninstall && elisym-mcp install ...",
                    client.name, path.display()
                );
                skipped += 1;
            }
            Err(e) => {
                eprintln!("  Error installing to {}: {e}", client.name);
            }
        }
    }

    if let Some(filter) = client_filter {
        if installed == 0 && skipped == 0 {
            eprintln!(
                "Client '{filter}' not found or not supported. Use --list to see available clients.",
            );
        }
    } else if installed == 0 && skipped == 0 {
        println!("No MCP clients detected. Use --list to see supported clients.");
        println!("You can also specify a client: elisym-mcp install --client claude-desktop");
    } else {
        println!("\nDone. {} installed, {} already configured.", installed, skipped);
        if let Some(name) = agent {
            println!("Agent: {name}");
        }
        println!("\nRestart your MCP client to activate.");
    }

    Ok(())
}

pub fn run_uninstall(client_filter: Option<&str>) -> Result<()> {
    let mut removed = 0;

    for client in CLIENTS {
        if let Some(filter) = client_filter {
            if client.name != filter {
                continue;
            }
        }

        let Some(path) = (client.config_path)() else {
            continue;
        };

        if !path.exists() {
            continue;
        }

        let contents = std::fs::read_to_string(&path)?;
        let mut config: Value = serde_json::from_str(&contents)?;

        if let Some(servers) = config.get_mut("mcpServers").and_then(|s| s.as_object_mut()) {
            if servers.remove("elisym").is_some() {
                let json = serde_json::to_string_pretty(&config)?;
                let tmp = path.with_extension("json.tmp");
                std::fs::write(&tmp, json + "\n")?;
                std::fs::rename(&tmp, &path)?;
                println!("  Removed from {} ({})", client.name, path.display());
                removed += 1;
            }
        }
    }

    if removed == 0 {
        println!("elisym not found in any MCP client config.");
    } else {
        println!("\nDone. Removed from {} client(s). Restart to apply.", removed);
    }

    Ok(())
}
