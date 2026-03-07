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
