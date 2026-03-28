use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

fn default_online_only() -> bool {
    true
}

/// Input for searching agents by capability.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchAgentsInput {
    /// Capabilities to search for (e.g. ["summarization", "translation"]).
    /// At least one capability must match (OR semantics with relevance ranking — more matches rank higher).
    /// Fuzzy matching: "stock" matches "stocks".
    #[schemars(description = "List of capability tags to search for (OR semantics — at least 1 must match, more matches rank higher). Supports fuzzy matching.")]
    pub capabilities: Vec<String>,

    /// Optional NIP-90 job kind offset to filter by (default: 100 for kind:5100).
    #[schemars(description = "NIP-90 job kind offset to filter by (e.g. 100 for kind:5100)")]
    pub job_kind: Option<u16>,

    /// Optional free-text query to search agent names, descriptions, and capabilities.
    /// Case-insensitive substring match. Use this when you don't know the exact capability tags.
    #[schemars(description = "Free-text search query to match against agent name, description, and capabilities (case-insensitive substring)")]
    pub query: Option<String>,

    /// Maximum price in lamports. Agents with a job_price higher than this are excluded.
    #[schemars(description = "Maximum price in lamports to filter agents by. Agents more expensive than this are excluded. 1 SOL = 1,000,000,000 lamports.")]
    pub max_price_lamports: Option<u64>,

    /// Only show agents active in the last 11 minutes. Default: true.
    /// Set to false to see all agents including offline ones.
    #[serde(default = "default_online_only")]
    #[schemars(description = "Only show agents active in the last 11 minutes. Default: true. Set false to include offline agents.", default = "default_online_only")]
    pub online_only: bool,
}

/// Input for listing all capabilities on the network.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListCapabilitiesInput {}

/// Summary of a single capability card published by an agent.
#[derive(Debug, Serialize)]
pub struct CardSummary {
    pub name: String,
    pub description: String,
    pub capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_price_lamports: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// A discovered agent returned by search.
#[derive(Debug, Serialize)]
pub struct AgentInfo {
    pub npub: String,
    pub supported_kinds: Vec<u16>,
    pub cards: Vec<CardSummary>,
}
