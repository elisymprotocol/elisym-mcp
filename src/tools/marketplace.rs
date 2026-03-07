use schemars::JsonSchema;
use serde::Deserialize;

/// Input for creating a job request.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateJobInput {
    /// The task input (prompt, URL, data, etc.).
    #[schemars(description = "The task input text to send to the provider")]
    pub input: String,

    /// Input type (e.g. "text", "url"). Defaults to "text".
    #[schemars(description = "Input type: 'text' or 'url'")]
    pub input_type: Option<String>,

    /// Provider npub to send the job to (optional — broadcast if omitted).
    #[schemars(description = "Provider npub (bech32) to direct the job to. Omit for broadcast.")]
    pub provider_npub: Option<String>,

    /// Bid amount in lamports (for Solana payments).
    #[schemars(description = "Bid amount in lamports (e.g. 10000000 = 0.01 SOL)")]
    pub bid_amount: Option<u64>,

    /// NIP-90 job kind offset (default: 100 for kind:5100 generic compute).
    #[schemars(description = "Job kind offset (default 100 for kind:5100)")]
    pub kind_offset: Option<u16>,

    /// Optional capability tags for the job.
    #[schemars(description = "Capability tags for the job request")]
    pub tags: Option<Vec<String>>,
}

/// Input for getting a job result.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetJobResultInput {
    /// The event ID of the job request to get results for.
    #[schemars(description = "Event ID (hex) of the job request")]
    pub job_event_id: String,

    /// NIP-90 job kind offset (default: 100 for kind:6100 result).
    #[schemars(description = "Job kind offset (default 100 for kind:6100)")]
    pub kind_offset: Option<u16>,

    /// How long to wait for a result in seconds (default: 60).
    #[schemars(description = "Timeout in seconds to wait for a result (default: 60)")]
    pub timeout_secs: Option<u64>,
}
