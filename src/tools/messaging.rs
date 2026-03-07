use schemars::JsonSchema;
use serde::Deserialize;

/// Input for sending a private message.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendMessageInput {
    /// Recipient npub (bech32 Nostr public key).
    #[schemars(description = "Recipient npub (bech32 Nostr public key)")]
    pub recipient_npub: String,

    /// Message content to send.
    #[schemars(description = "Message text to send")]
    pub message: String,
}
