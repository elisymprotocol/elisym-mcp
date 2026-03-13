//! Read/write `~/.elisym/config.toml` — shared with elisym-client.
//!
//! Only the fields this crate cares about are declared; unknown fields
//! are preserved via `toml::Value` round-tripping.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// Returns `~/.elisym/config.toml`.
fn global_config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot find home directory")?;
    Ok(home.join(".elisym").join("config.toml"))
}

/// Read `default_agent` from `~/.elisym/config.toml`.
/// Returns `None` if the file doesn't exist or the field is absent.
pub(crate) fn get_default_agent() -> Option<String> {
    let path = global_config_path().ok()?;
    let contents = fs::read_to_string(&path).ok()?;
    let table: toml::Table = toml::from_str(&contents).ok()?;
    table
        .get("default_agent")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Persist `default_agent` in `~/.elisym/config.toml`.
/// Creates the file and parent directories if they don't exist.
/// Preserves all other fields in the file.
pub(crate) fn set_default_agent(name: &str) -> Result<()> {
    let path = global_config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Load existing table or start empty — preserves fields we don't know about.
    let mut table: toml::Table = fs::read_to_string(&path)
        .ok()
        .and_then(|c| toml::from_str(&c).ok())
        .unwrap_or_default();

    table.insert(
        "default_agent".to_string(),
        toml::Value::String(name.to_string()),
    );

    let toml_str =
        toml::to_string_pretty(&table).context("failed to serialize global config")?;
    fs::write(&path, toml_str).context("failed to write ~/.elisym/config.toml")?;
    Ok(())
}
