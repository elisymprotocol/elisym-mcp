use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Input for searching agents by capability.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchAgentsInput {
    /// Capabilities to search for (e.g. ["summarization", "translation"]).
    /// All capabilities must match (AND semantics).
    #[schemars(description = "List of capability tags to search for (AND semantics)")]
    pub capabilities: Vec<String>,

    /// Optional NIP-90 job kind offset to filter by (default: 100 for kind:5100).
    #[schemars(description = "NIP-90 job kind offset to filter by (e.g. 100 for kind:5100)")]
    pub job_kind: Option<u16>,
}

/// A discovered agent returned by search.
#[derive(Debug, Serialize)]
pub struct AgentInfo {
    pub npub: String,
    pub name: String,
    pub description: String,
    pub capabilities: Vec<String>,
    pub supported_kinds: Vec<u16>,
}
