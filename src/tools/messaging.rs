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

/// Input for receiving incoming private messages.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReceiveMessagesInput {
    /// How long to listen for messages in seconds (default: 30).
    #[schemars(description = "Timeout in seconds to listen for incoming messages (default: 30)")]
    pub timeout_secs: Option<u64>,

    /// Maximum number of messages to collect before returning (default: 10).
    #[schemars(description = "Max number of messages to collect (default: 10)")]
    pub max_messages: Option<usize>,
}
