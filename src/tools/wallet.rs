use schemars::JsonSchema;
use serde::Deserialize;

/// Input for sending a Solana payment.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendPaymentInput {
    /// Payment request JSON string (from job feedback).
    #[schemars(description = "Payment request JSON string received from a provider's job feedback")]
    pub payment_request: String,
}
