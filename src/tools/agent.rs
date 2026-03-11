use schemars::JsonSchema;
use serde::Deserialize;

/// Input for creating a new agent identity.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateAgentInput {
    /// Agent name (a-zA-Z0-9, hyphens, underscores, max 64 chars).
    pub name: String,
    /// Agent description.
    #[serde(default)]
    pub description: Option<String>,
    /// Capabilities (comma-separated string or list).
    #[serde(default)]
    pub capabilities: Option<String>,
    /// Solana network: devnet, testnet, mainnet (default: devnet).
    #[serde(default)]
    pub network: Option<String>,
    /// Activate this agent immediately after creation (default: true).
    #[serde(default = "default_true")]
    pub activate: bool,
}

fn default_true() -> bool {
    true
}

/// Input for switching the active agent.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SwitchAgentInput {
    /// Name of an existing agent in ~/.elisym/agents/.
    pub name: String,
}

/// Input for listing loaded agents.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListAgentsInput {}
