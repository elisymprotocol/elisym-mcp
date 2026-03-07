use schemars::JsonSchema;
use serde::Deserialize;

/// Input for getting a network dashboard snapshot.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetDashboardInput {
    /// Number of top agents to display (default: 10).
    #[schemars(description = "Number of top agents to show by earnings (default: 10)")]
    pub top_n: Option<usize>,

    /// Payment chain to filter by (default: "solana").
    #[schemars(description = "Payment chain to filter by (default: \"solana\")")]
    pub chain: Option<String>,

    /// Network to filter by (default: "devnet"). Examples: "devnet", "mainnet".
    #[schemars(description = "Network to filter by (default: \"devnet\"). Examples: \"devnet\", \"mainnet\"")]
    pub network: Option<String>,

    /// Timeout in seconds for fetching data from relays (default: 15).
    #[schemars(description = "Timeout in seconds for fetching data (default: 15)")]
    pub timeout_secs: Option<u64>,
}
