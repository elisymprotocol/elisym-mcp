use schemars::JsonSchema;
use serde::Deserialize;

/// Input for getting job feedback (PaymentRequired, Processing, Error, etc.).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetJobFeedbackInput {
    /// The event ID of the job request to get feedback for.
    #[schemars(description = "Event ID (hex) of the job request")]
    pub job_event_id: String,

    /// How long to wait for feedback in seconds (default: 60).
    #[schemars(description = "Timeout in seconds to wait for feedback (default: 60)")]
    pub timeout_secs: Option<u64>,
}

/// Input for submitting a job with automatic payment and result retrieval.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubmitAndPayJobInput {
    /// The task input (prompt, URL, data, etc.).
    #[schemars(description = "The task input text to send to the provider")]
    pub input: String,

    /// Provider npub to send the job to.
    #[schemars(description = "Provider npub (bech32) to direct the job to")]
    pub provider_npub: String,

    /// Input type (e.g. "text", "url"). Defaults to "text".
    #[schemars(description = "Input type: 'text' or 'url' (default: 'text')")]
    pub input_type: Option<String>,

    /// Bid amount in lamports (for Solana payments).
    #[schemars(description = "Bid amount in lamports (e.g. 10000000 = 0.01 SOL)")]
    pub bid_amount: Option<u64>,

    /// NIP-90 job kind offset (default: 100 for kind:5100 generic compute).
    #[schemars(description = "Job kind offset (default 100 for kind:5100)")]
    pub kind_offset: Option<u16>,

    /// Optional capability tags for the job.
    #[schemars(description = "Capability tags for the job request")]
    pub tags: Option<Vec<String>>,

    /// Total timeout in seconds for the entire flow (default: 300 = 5 min).
    #[schemars(description = "Total timeout in seconds for the full flow: submit → pay → result (default: 300)")]
    pub timeout_secs: Option<u64>,

    /// Maximum price in lamports the user is willing to pay. If the provider requests more,
    /// the job is NOT paid and the requested price is returned so the user can decide.
    /// IMPORTANT: Always ask the user for their budget before calling this tool.
    #[schemars(description = "Maximum price in lamports the user is willing to pay. If omitted or if provider asks more, returns price for user confirmation instead of auto-paying. Always confirm price with the user first.")]
    pub max_price_lamports: Option<u64>,
}

/// Input for buying a capability — automatically handles free (price=0) and paid (price>0) flows.
/// For free capabilities, submits the job and waits for the result directly.
/// For paid capabilities, handles the full payment flow with budget validation.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BuyCapabilityInput {
    /// Provider npub (bech32) to buy from.
    #[schemars(description = "Provider npub (bech32) to buy from")]
    pub provider_npub: String,

    /// Capability name exactly as shown in the provider's card (e.g. "Landing page").
    /// Will be converted to a dTag for job matching.
    #[schemars(description = "Capability name from the provider's card (e.g. 'Landing page'). Converted to a dTag automatically.")]
    pub capability: String,

    /// Optional free-text input to send with the job (e.g. customization instructions).
    #[schemars(description = "Optional input text to send to the provider (default: empty)")]
    pub input: Option<String>,

    /// Maximum price in lamports the user is willing to pay. Only needed for paid capabilities.
    /// If omitted and the provider requests payment, returns the price for user confirmation.
    #[schemars(description = "Maximum price in lamports (only for paid capabilities). If omitted and provider requests payment, returns price for user confirmation.")]
    pub max_price_lamports: Option<u64>,

    /// Total timeout in seconds (default: 120).
    #[schemars(description = "Total timeout in seconds (default: 120)")]
    pub timeout_secs: Option<u64>,
}

/// Input for listing submitted jobs and their results/feedback.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListMyJobsInput {
    /// Maximum number of jobs to return (default: 20).
    #[schemars(description = "Maximum number of jobs to return (default: 20, max: 50)")]
    pub limit: Option<usize>,

    /// NIP-90 job kind offset (default: 100 for kind:5100).
    #[schemars(description = "Job kind offset to filter by (default: 100 for kind:5100)")]
    pub kind_offset: Option<u16>,

    /// Whether to fetch results and feedback for each job (default: true).
    #[schemars(description = "Fetch results and feedback for each job (default: true). Set to false for a faster listing.")]
    pub include_results: Option<bool>,
}

/// Input for pinging an agent to check if it's online.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PingAgentInput {
    /// The npub of the agent to ping.
    #[schemars(description = "Agent npub (bech32) to check for liveness")]
    pub agent_npub: String,

    /// Timeout in seconds (default: 15).
    #[schemars(description = "Timeout in seconds to wait for pong response (default: 15)")]
    pub timeout_secs: Option<u64>,
}
