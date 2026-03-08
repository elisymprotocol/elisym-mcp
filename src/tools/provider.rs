use schemars::JsonSchema;
use serde::Deserialize;

/// Input for polling for the next incoming job request.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PollNextJobInput {
    /// NIP-90 job kind offsets to listen for (default: [100] for kind:5100).
    #[schemars(description = "Job kind offsets to listen for (default: [100])")]
    pub kind_offsets: Option<Vec<u16>>,

    /// How long to wait for a job in seconds (default: 60).
    #[schemars(description = "Timeout in seconds to wait for an incoming job (default: 60)")]
    pub timeout_secs: Option<u64>,
}

/// Input for sending job feedback (status update) to a customer.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendJobFeedbackInput {
    /// The event ID of the job request to send feedback for.
    #[schemars(description = "Event ID (hex) of the job request")]
    pub job_event_id: String,

    /// Status: "payment-required", "processing", "error", "success", "partial".
    #[schemars(description = "Feedback status: payment-required, processing, error, success, or partial")]
    pub status: String,

    /// Optional extra info (e.g. error message).
    #[schemars(description = "Optional extra info string (e.g. error details)")]
    pub extra_info: Option<String>,

    /// Optional amount in lamports.
    #[schemars(description = "Amount in lamports (required for payment-required status)")]
    pub amount: Option<u64>,

    /// Optional payment request string (for payment-required status).
    #[schemars(description = "Payment request string (for payment-required status)")]
    pub payment_request: Option<String>,
}

/// Input for submitting a job result back to the customer.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SubmitJobResultInput {
    /// The event ID of the job request to respond to.
    #[schemars(description = "Event ID (hex) of the job request")]
    pub job_event_id: String,

    /// The result content to deliver.
    #[schemars(description = "Result content text to deliver to the customer")]
    pub content: String,

    /// Optional amount earned in lamports.
    #[schemars(description = "Amount earned in lamports (provider's net amount)")]
    pub amount: Option<u64>,
}

/// Input for creating a payment request (provider sends to customer).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreatePaymentRequestInput {
    /// Amount in lamports to request.
    #[schemars(description = "Amount in lamports (e.g. 10000000 = 0.01 SOL)")]
    pub amount: u64,

    /// Description for the payment request.
    #[schemars(description = "Human-readable description (e.g. 'Payment for summarization job')")]
    pub description: String,

    /// Expiry time in seconds (default: 600 = 10 min).
    #[schemars(description = "Expiry time in seconds (default: 600)")]
    pub expiry_secs: Option<u32>,
}

/// Input for checking the payment status of a payment request.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckPaymentStatusInput {
    /// The payment request string to check status for.
    #[schemars(description = "Payment request string to check (as returned by create_payment_request)")]
    pub payment_request: String,
}

/// Input for publishing this agent's capability card to the network.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct PublishCapabilitiesInput {
    /// Supported NIP-90 job kind offsets (default: [100]).
    #[schemars(description = "Supported job kind offsets (default: [100] for kind:5100)")]
    pub supported_kinds: Option<Vec<u16>>,

    /// Price per job in lamports (e.g. 10000000 = 0.01 SOL). Published in the capability card so customers can see it before submitting.
    #[schemars(description = "Price per job in lamports (e.g. 10000000 = 0.01 SOL)")]
    pub job_price_lamports: Option<u64>,
}
