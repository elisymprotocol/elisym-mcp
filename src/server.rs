use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use elisym_core::{
    AgentFilter, AgentNode, PaymentProvider,
    DEFAULT_KIND_OFFSET, KIND_JOB_RESULT_BASE, kind,
};
use nostr_sdk::prelude::*;
use rmcp::{
    ServerHandler,
    handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::*,
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::tools::customer::{GetJobFeedbackInput, PingAgentInput, SubmitAndPayJobInput};
use crate::tools::dashboard::GetDashboardInput;
use crate::tools::discovery::{AgentInfo, SearchAgentsInput};
use crate::tools::marketplace::{CreateJobInput, GetJobResultInput};
use crate::tools::messaging::{ReceiveMessagesInput, SendMessageInput};
use crate::tools::provider::{
    CheckPaymentStatusInput, CreatePaymentRequestInput, PollNextJobInput,
    PublishCapabilitiesInput, SendJobFeedbackInput, SubmitJobResultInput,
};
use crate::tools::wallet::SendPaymentInput;

/// Protocol fee in basis points (300 = 3%).
const PROTOCOL_FEE_BPS: u64 = 300;
/// Solana address of the protocol treasury.
const PROTOCOL_TREASURY: &str = "GY7vnWMkKpftU4nQ16C2ATkj1JwrQpHhknkaBUn67VTy";

/// Parsed Solana payment request for fee validation.
#[derive(Debug, Deserialize)]
struct SolanaPaymentRequestData {
    #[allow(dead_code)]
    recipient: String,
    amount: u64,
    #[allow(dead_code)]
    reference: String,
    fee_address: Option<String>,
    fee_amount: Option<u64>,
}

/// Validate that a payment request has the correct protocol fee params.
/// Returns an error message if invalid, None if OK.
fn validate_payment_fee(request: &str) -> Option<String> {
    let data: SolanaPaymentRequestData = match serde_json::from_str(request) {
        Ok(d) => d,
        Err(e) => return Some(format!("Invalid payment request JSON: {e}")),
    };

    let expected_fee = (data.amount * PROTOCOL_FEE_BPS).div_ceil(10_000);

    match (data.fee_address.as_deref(), data.fee_amount) {
        (Some(addr), Some(amt)) if amt > 0 => {
            if addr != PROTOCOL_TREASURY {
                return Some(format!(
                    "Fee address mismatch: expected {PROTOCOL_TREASURY}, got {addr}. \
                     Provider may be attempting to redirect fees."
                ));
            }
            if amt != expected_fee {
                return Some(format!(
                    "Fee amount mismatch: expected {expected_fee} lamports ({}bps of {}), got {amt}. \
                     Provider may be tampering with fee.",
                    PROTOCOL_FEE_BPS, data.amount
                ));
            }
            None
        }
        (None, None) => {
            Some(format!(
                "Payment request missing protocol fee ({PROTOCOL_FEE_BPS}bps). \
                 Expected fee: {expected_fee} lamports to {PROTOCOL_TREASURY}."
            ))
        }
        _ => Some(format!(
            "Invalid fee params in payment request. \
             Expected fee: {expected_fee} lamports to {PROTOCOL_TREASURY}."
        )),
    }
}

/// Truncate a string to `max` chars, appending "…" if truncated. UTF-8 safe.
fn truncate_str(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() > max {
        let truncated: String = chars[..max.saturating_sub(1)].iter().collect();
        format!("{truncated}…")
    } else {
        s.to_string()
    }
}

/// Heartbeat message for ping/pong liveness checks (NIP-17 encrypted).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HeartbeatMessage {
    #[serde(rename = "type")]
    msg_type: String,
    nonce: String,
}

pub struct ElisymServer {
    agent: Arc<AgentNode>,
    /// Stores raw events for received job requests (provider flow).
    job_events: Arc<Mutex<HashMap<EventId, Event>>>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl ElisymServer {
    pub fn new(agent: AgentNode) -> Self {
        Self {
            agent: Arc::new(agent),
            job_events: Arc::new(Mutex::new(HashMap::new())),
            tool_router: Self::tool_router(),
        }
    }

    /// Create from shared state (used by HTTP transport factory).
    #[cfg(feature = "transport-http")]
    pub fn from_shared(
        agent: Arc<AgentNode>,
        job_events: Arc<Mutex<HashMap<EventId, Event>>>,
    ) -> Self {
        Self {
            agent,
            job_events,
            tool_router: Self::tool_router(),
        }
    }

    // ══════════════════════════════════════════════════════════════
    // Discovery tools
    // ══════════════════════════════════════════════════════════════

    #[tool(description = "Search for AI agents on the elisym network by capability. Returns a list of agents with their name, description, capabilities, and public key (npub).")]
    async fn search_agents(
        &self,
        Parameters(input): Parameters<SearchAgentsInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let filter = AgentFilter {
            capabilities: input.capabilities,
            job_kind: input.job_kind,
            ..Default::default()
        };

        match self.agent.discovery.search_agents(&filter).await {
            Ok(agents) => {
                let infos: Vec<AgentInfo> = agents
                    .iter()
                    .map(|a| AgentInfo {
                        npub: a.pubkey.to_bech32().unwrap_or_default(),
                        name: a.card.name.clone(),
                        description: a.card.description.clone(),
                        capabilities: a.card.capabilities.clone(),
                        supported_kinds: a.supported_kinds.clone(),
                    })
                    .collect();

                if infos.is_empty() {
                    Ok(CallToolResult::success(vec![Content::text(
                        "No agents found matching the specified capabilities.",
                    )]))
                } else {
                    let json = serde_json::to_string_pretty(&infos)
                        .unwrap_or_else(|e| format!("Error serializing results: {e}"));
                    Ok(CallToolResult::success(vec![Content::text(json)]))
                }
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Error searching agents: {e}"
            ))])),
        }
    }

    #[tool(description = "Get this agent's identity — public key (npub), name, description, and capabilities.")]
    fn get_identity(&self) -> String {
        let info = AgentInfo {
            npub: self.agent.identity.npub(),
            name: self.agent.capability_card.name.clone(),
            description: self.agent.capability_card.description.clone(),
            capabilities: self.agent.capability_card.capabilities.clone(),
            supported_kinds: vec![],
        };
        serde_json::to_string_pretty(&info)
            .unwrap_or_else(|e| format!("Error serializing identity: {e}"))
    }

    #[tool(description = "Get a snapshot of the elisym network — top agents ranked by earnings, with total protocol earnings. Shows agent name, capabilities, price, and earned amount.")]
    async fn get_dashboard(
        &self,
        Parameters(input): Parameters<GetDashboardInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let top_n = input.top_n.unwrap_or(10);
        let timeout_secs = input.timeout_secs.unwrap_or(15);
        let filter_chain = input.chain.unwrap_or_else(|| "solana".into());
        let filter_network = input.network.unwrap_or_else(|| "devnet".into());

        // 1. Discover all agents and filter by chain + network
        let filter = elisym_core::AgentFilter::default();
        let all_agents = match self.agent.discovery.search_agents(&filter).await {
            Ok(a) => a,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error fetching agents: {e}"
                ))]))
            }
        };
        let agents: Vec<_> = all_agents
            .into_iter()
            .filter(|a| {
                let chain = a.card.metadata.as_ref()
                    .and_then(|m| m["chain"].as_str())
                    .unwrap_or("solana");
                let network = a.card.metadata.as_ref()
                    .and_then(|m| m["network"].as_str())
                    .unwrap_or("devnet");
                chain.eq_ignore_ascii_case(&filter_chain)
                    && network.eq_ignore_ascii_case(&filter_network)
            })
            .collect();

        // Collect npubs of agents matching the network filter
        let agent_npubs: std::collections::HashSet<String> = agents
            .iter()
            .filter_map(|a| a.pubkey.to_bech32().ok())
            .collect();

        // 2. Fetch job result events (kind 6100) to calculate earnings
        //    Filter by author pubkeys to only get results from agents in this network.
        let result_kind = kind(KIND_JOB_RESULT_BASE + DEFAULT_KIND_OFFSET);
        let author_pks: Vec<PublicKey> = agents.iter().map(|a| a.pubkey).collect();
        let event_filter = if author_pks.is_empty() {
            nostr_sdk::Filter::new().kind(result_kind)
        } else {
            nostr_sdk::Filter::new().kind(result_kind).authors(author_pks)
        };
        let events = self
            .agent
            .client
            .fetch_events(
                vec![event_filter],
                Some(std::time::Duration::from_secs(timeout_secs)),
            )
            .await;

        // 3. Accumulate earnings per provider (only agents in this network)
        let mut earnings: HashMap<String, u64> = HashMap::new();
        let event_list = match &events {
            Ok(ev) => ev.iter().collect::<Vec<_>>(),
            Err(_) => vec![],
        };
        let mut total_job_results = 0usize;
        for event in event_list.iter() {
            let npub = event.pubkey.to_bech32().unwrap_or_default();
            if !agent_npubs.contains(&npub) {
                continue;
            }
            total_job_results += 1;
            let amount = event.tags.iter().find_map(|tag| {
                let s = tag.as_slice();
                if s.first().map(|v| v.as_str()) == Some("amount") {
                    s.get(1).and_then(|v| v.parse::<u64>().ok())
                } else {
                    None
                }
            });
            if let Some(amt) = amount {
                *earnings.entry(npub).or_insert(0) += amt;
            }
        }

        // Total earned across ALL agents in this network
        let total_earned_lamports: u64 = earnings.values().sum();
        let total_earned_sol = total_earned_lamports as f64 / 1_000_000_000.0;

        // 4. Build agent list with earnings, filter out observers
        struct AgentRow {
            name: String,
            npub: String,
            capabilities: String,
            price: String,
            earned: u64,
        }

        let mut rows: Vec<AgentRow> = agents
            .iter()
            .filter(|a| !a.card.capabilities.is_empty())
            .map(|a| {
                let npub = a.pubkey.to_bech32().unwrap_or_default();
                let earned = earnings.get(&npub).copied().unwrap_or(0);
                let (price, token) = a
                    .card
                    .metadata
                    .as_ref()
                    .map(|m| {
                        let p = m["job_price"].as_u64().unwrap_or(0);
                        let t = m["token"].as_str().unwrap_or("sol").to_string();
                        (p, t)
                    })
                    .unwrap_or((0, "sol".into()));
                let price_str = if price == 0 {
                    "—".into()
                } else if token == "usdc" {
                    format!("{:.6} USDC", price as f64 / 1_000_000.0)
                } else {
                    format!("{:.4} SOL", price as f64 / 1_000_000_000.0)
                };
                AgentRow {
                    name: a.card.name.clone(),
                    npub: npub[..npub.len().min(20)].to_string(),
                    capabilities: a.card.capabilities.join(", "),
                    price: price_str,
                    earned,
                }
            })
            .collect();

        // Sort by earned (descending)
        rows.sort_by(|a, b| b.earned.cmp(&a.earned));

        // 5. Format as text table
        let mut output = String::new();
        output.push_str(&format!(
            "elisym Network Dashboard ({}/{})\n\
             Agents: {}  |  Total Earned: {:.4} SOL  |  Job Results: {}\n\n",
            filter_chain,
            filter_network,
            agents.len(),
            total_earned_sol,
            total_job_results,
        ));

        if rows.is_empty() {
            output.push_str("No agents found on the network.\n");
        } else {
            // Header
            output.push_str(&format!(
                "{:<20} {:<20} {:<30} {:>12} {:>12}\n",
                "Name", "Pubkey", "Capabilities", "Price", "Earned"
            ));
            output.push_str(&format!("{}\n", "─".repeat(96)));

            // Rows (top N)
            for row in rows.iter().take(top_n) {
                let caps = truncate_str(&row.capabilities, 28);
                let name = truncate_str(&row.name, 18);
                let earned_str = if row.earned == 0 {
                    "—".into()
                } else {
                    format!("{:.4} SOL", row.earned as f64 / 1_000_000_000.0)
                };
                output.push_str(&format!(
                    "{:<20} {:<20} {:<30} {:>12} {:>12}\n",
                    name, row.npub, caps, row.price, earned_str
                ));
            }

            if rows.len() > top_n {
                output.push_str(&format!(
                    "\n… and {} more agent(s)\n",
                    rows.len() - top_n
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    // ══════════════════════════════════════════════════════════════
    // Marketplace tools (customer)
    // ══════════════════════════════════════════════════════════════

    #[tool(description = "Submit a job request to the elisym agent marketplace (NIP-90). Optionally target a specific provider by npub. Returns the job event ID.")]
    async fn create_job(
        &self,
        Parameters(input): Parameters<CreateJobInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let kind_offset = input.kind_offset.unwrap_or(DEFAULT_KIND_OFFSET);
        let input_type = input.input_type.as_deref().unwrap_or("text");
        let tags = input.tags.unwrap_or_default();

        let provider = match &input.provider_npub {
            Some(npub) => match PublicKey::from_bech32(npub) {
                Ok(pk) => Some(pk),
                Err(e) => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "Invalid provider npub: {e}"
                    ))]))
                }
            },
            None => None,
        };

        match self
            .agent
            .marketplace
            .submit_job_request(
                kind_offset,
                &input.input,
                input_type,
                None,
                input.bid_amount,
                provider.as_ref(),
                tags,
            )
            .await
        {
            Ok(event_id) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Job submitted successfully.\nEvent ID: {event_id}"
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Error submitting job: {e}"
            ))])),
        }
    }

    #[tool(description = "Wait for and retrieve the result of a previously submitted job request. Subscribes to NIP-90 results and waits up to the specified timeout.")]
    async fn get_job_result(
        &self,
        Parameters(input): Parameters<GetJobResultInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let timeout_secs = input.timeout_secs.unwrap_or(60);

        let kind_offset = input.kind_offset.unwrap_or(DEFAULT_KIND_OFFSET);
        let mut rx = match self
            .agent
            .marketplace
            .subscribe_to_results(&[kind_offset], &[])
            .await
        {
            Ok(rx) => rx,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error subscribing to results: {e}"
                ))]))
            }
        };

        let target_id = match EventId::parse(&input.job_event_id) {
            Ok(id) => id,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid event ID: {e}"
                ))]))
            }
        };

        let timeout = tokio::time::Duration::from_secs(timeout_secs);
        match tokio::time::timeout(timeout, async {
            while let Some(result) = rx.recv().await {
                if result.request_id == target_id {
                    return Some(result);
                }
            }
            None
        })
        .await
        {
            Ok(Some(result)) => {
                let amount_info = result
                    .amount
                    .map(|a| format!(" (amount: {a} lamports)"))
                    .unwrap_or_default();
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Job result received{}:\n\n{}",
                    amount_info, result.content
                ))]))
            }
            Ok(None) => Ok(CallToolResult::error(vec![Content::text(
                "Result subscription ended without receiving a matching result.",
            )])),
            Err(_) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Timeout after {timeout_secs}s — no result received. \
                 The provider may still be processing. Try again with a longer timeout."
            ))])),
        }
    }

    #[tool(description = "Wait for job feedback (PaymentRequired, Processing, Error, etc.) on a previously submitted job. Returns the first feedback event matching the job ID.")]
    async fn get_job_feedback(
        &self,
        Parameters(input): Parameters<GetJobFeedbackInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let timeout_secs = input.timeout_secs.unwrap_or(60);

        let mut rx = match self.agent.marketplace.subscribe_to_feedback().await {
            Ok(rx) => rx,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error subscribing to feedback: {e}"
                ))]))
            }
        };

        let target_id = match EventId::parse(&input.job_event_id) {
            Ok(id) => id,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid event ID: {e}"
                ))]))
            }
        };

        let timeout = tokio::time::Duration::from_secs(timeout_secs);
        match tokio::time::timeout(timeout, async {
            while let Some(feedback) = rx.recv().await {
                if feedback.request_id == target_id {
                    return Some(feedback);
                }
            }
            None
        })
        .await
        {
            Ok(Some(fb)) => {
                let mut parts = vec![format!("Status: {}", fb.status)];
                if let Some(info) = &fb.extra_info {
                    parts.push(format!("Info: {info}"));
                }
                if let Some(pr) = &fb.payment_request {
                    parts.push(format!("Payment request: {pr}"));
                }
                if let Some(chain) = &fb.payment_chain {
                    parts.push(format!("Payment chain: {chain}"));
                }
                Ok(CallToolResult::success(vec![Content::text(
                    parts.join("\n"),
                )]))
            }
            Ok(None) => Ok(CallToolResult::error(vec![Content::text(
                "Feedback subscription ended without receiving a matching event.",
            )])),
            Err(_) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Timeout after {timeout_secs}s — no feedback received."
            ))])),
        }
    }

    #[tool(description = "Submit a job, automatically pay when the provider requests payment, and wait for the result. This is the full customer flow in one call. Requires Solana payments to be configured.")]
    async fn submit_and_pay_job(
        &self,
        Parameters(input): Parameters<SubmitAndPayJobInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let provider_pk = match PublicKey::from_bech32(&input.provider_npub) {
            Ok(pk) => pk,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid provider npub: {e}"
                ))]))
            }
        };

        let kind_offset = input.kind_offset.unwrap_or(DEFAULT_KIND_OFFSET);
        let input_type = input.input_type.as_deref().unwrap_or("text");
        let tags = input.tags.unwrap_or_default();
        let total_timeout = input.timeout_secs.unwrap_or(300);

        // 1. Subscribe to feedback and results BEFORE submitting (avoid race)
        let mut feedback_rx = match self.agent.marketplace.subscribe_to_feedback().await {
            Ok(rx) => rx,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Failed to subscribe to feedback: {e}"
                ))]))
            }
        };

        let mut result_rx = match self
            .agent
            .marketplace
            .subscribe_to_results(&[kind_offset], &[provider_pk])
            .await
        {
            Ok(rx) => rx,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Failed to subscribe to results: {e}"
                ))]))
            }
        };

        // 2. Submit the job
        let event_id = match self
            .agent
            .marketplace
            .submit_job_request(
                kind_offset,
                &input.input,
                input_type,
                None,
                input.bid_amount,
                Some(&provider_pk),
                tags,
            )
            .await
        {
            Ok(id) => id,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error submitting job: {e}"
                ))]))
            }
        };

        tracing::info!(event_id = %event_id, "Job submitted, waiting for feedback");

        let deadline =
            tokio::time::Instant::now() + tokio::time::Duration::from_secs(total_timeout);
        let mut status_log = vec![format!("Job submitted. Event ID: {event_id}")];
        let mut paid = false;

        // 3. Event loop: handle feedback and results
        loop {
            tokio::select! {
                Some(fb) = feedback_rx.recv(), if !paid => {
                    if fb.request_id != event_id {
                        continue;
                    }
                    match fb.status.as_str() {
                        "payment-required" => {
                            if let Some(payment_request) = &fb.payment_request {
                                // Validate fee before paying
                                if let Some(err) = validate_payment_fee(payment_request) {
                                    status_log.push(format!("Fee validation failed: {err}"));
                                    return Ok(CallToolResult::error(vec![Content::text(
                                        status_log.join("\n")
                                    )]));
                                }
                                let provider = match self.agent.solana_payments() {
                                    Some(p) => p,
                                    None => {
                                        status_log.push("Payment required but Solana payments not configured.".into());
                                        return Ok(CallToolResult::error(vec![Content::text(
                                            status_log.join("\n")
                                        )]));
                                    }
                                };
                                match provider.pay(payment_request) {
                                    Ok(result) => {
                                        status_log.push(format!(
                                            "Payment sent: {} ({})",
                                            result.payment_id, result.status
                                        ));
                                        paid = true;
                                        tracing::info!("Payment sent, waiting for result");
                                    }
                                    Err(e) => {
                                        status_log.push(format!("Payment failed: {e}"));
                                        return Ok(CallToolResult::error(vec![Content::text(
                                            status_log.join("\n")
                                        )]));
                                    }
                                }
                            } else {
                                status_log.push("Payment required but no payment request provided by provider.".into());
                            }
                        }
                        "processing" => {
                            status_log.push("Provider is processing the job...".into());
                        }
                        "error" => {
                            let info = fb.extra_info.as_deref().unwrap_or("unknown error");
                            status_log.push(format!("Provider error: {info}"));
                            return Ok(CallToolResult::error(vec![Content::text(
                                status_log.join("\n")
                            )]));
                        }
                        other => {
                            status_log.push(format!("Feedback: {other}"));
                        }
                    }
                }
                Some(result) = result_rx.recv() => {
                    if result.request_id != event_id {
                        continue;
                    }
                    let amount_info = result
                        .amount
                        .map(|a| format!(" (amount: {a} lamports)"))
                        .unwrap_or_default();
                    status_log.push(format!("Result received{}:\n\n{}", amount_info, result.content));
                    return Ok(CallToolResult::success(vec![Content::text(
                        status_log.join("\n")
                    )]));
                }
                _ = tokio::time::sleep_until(deadline) => {
                    status_log.push(format!(
                        "Timeout after {total_timeout}s — no result received."
                    ));
                    return Ok(CallToolResult::error(vec![Content::text(
                        status_log.join("\n")
                    )]));
                }
            }
        }
    }

    #[tool(description = "Ping an agent to check if it's online. Sends an encrypted heartbeat message and waits for a pong response.")]
    async fn ping_agent(
        &self,
        Parameters(input): Parameters<PingAgentInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let target = match PublicKey::from_bech32(&input.agent_npub) {
            Ok(pk) => pk,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid npub: {e}"
                ))]))
            }
        };

        let timeout_secs = input.timeout_secs.unwrap_or(15);
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nonce = format!(
            "{:x}{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );

        // Subscribe to messages BEFORE sending ping
        let mut msg_rx = match self.agent.messaging.subscribe_to_messages().await {
            Ok(rx) => rx,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error subscribing to messages: {e}"
                ))]))
            }
        };

        let ping = HeartbeatMessage {
            msg_type: "elisym_ping".into(),
            nonce: nonce.clone(),
        };

        if let Err(e) = self
            .agent
            .messaging
            .send_structured_message(&target, &ping)
            .await
        {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Error sending ping: {e}"
            ))]));
        }

        let timeout = tokio::time::Duration::from_secs(timeout_secs);
        match tokio::time::timeout(timeout, async {
            while let Some(msg) = msg_rx.recv().await {
                if let Ok(hb) = serde_json::from_str::<HeartbeatMessage>(&msg.content) {
                    if hb.msg_type == "elisym_pong" && hb.nonce == nonce {
                        return true;
                    }
                }
            }
            false
        })
        .await
        {
            Ok(true) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Agent {} is online (pong received).",
                input.agent_npub
            ))])),
            Ok(false) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Agent {} did not respond (subscription ended).",
                input.agent_npub
            ))])),
            Err(_) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Agent {} did not respond within {timeout_secs}s — likely offline.",
                input.agent_npub
            ))])),
        }
    }

    // ══════════════════════════════════════════════════════════════
    // Messaging tools
    // ══════════════════════════════════════════════════════════════

    #[tool(description = "Send an encrypted private message (NIP-17 gift wrap) to another agent or user on Nostr.")]
    async fn send_message(
        &self,
        Parameters(input): Parameters<SendMessageInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let recipient = match PublicKey::from_bech32(&input.recipient_npub) {
            Ok(pk) => pk,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid recipient npub: {e}"
                ))]))
            }
        };

        match self
            .agent
            .messaging
            .send_message(&recipient, &input.message)
            .await
        {
            Ok(()) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Message sent to {}",
                input.recipient_npub
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Error sending message: {e}"
            ))])),
        }
    }

    #[tool(description = "Listen for incoming encrypted private messages (NIP-17). Collects messages until timeout or max count is reached, then returns them all.")]
    async fn receive_messages(
        &self,
        Parameters(input): Parameters<ReceiveMessagesInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let timeout_secs = input.timeout_secs.unwrap_or(30);
        let max_messages = input.max_messages.unwrap_or(10);

        let mut rx = match self.agent.messaging.subscribe_to_messages().await {
            Ok(rx) => rx,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error subscribing to messages: {e}"
                ))]))
            }
        };

        let mut messages = Vec::new();
        let deadline =
            tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);

        loop {
            tokio::select! {
                Some(msg) = rx.recv() => {
                    let sender_npub = msg.sender.to_bech32().unwrap_or_default();
                    messages.push(serde_json::json!({
                        "sender_npub": sender_npub,
                        "content": msg.content,
                        "timestamp": msg.timestamp.as_u64(),
                    }));
                    if messages.len() >= max_messages {
                        break;
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    break;
                }
            }
        }

        if messages.is_empty() {
            Ok(CallToolResult::success(vec![Content::text(format!(
                "No messages received within {timeout_secs}s."
            ))]))
        } else {
            let json = serde_json::to_string_pretty(&messages)
                .unwrap_or_else(|e| format!("Error serializing messages: {e}"));
            Ok(CallToolResult::success(vec![Content::text(format!(
                "{} message(s) received:\n\n{json}",
                messages.len()
            ))]))
        }
    }

    // ══════════════════════════════════════════════════════════════
    // Wallet tools
    // ══════════════════════════════════════════════════════════════

    #[tool(description = "Get the Solana wallet balance for this agent. Returns the address and balance in SOL. Requires Solana payments to be configured via ELISYM_AGENT.")]
    fn get_balance(&self) -> String {
        let Some(provider) = self.agent.solana_payments() else {
            return "Solana payments not configured. Set ELISYM_AGENT to an agent with a Solana wallet.".to_string();
        };

        let address = provider.address();
        match provider.balance() {
            Ok(lamports) => {
                let sol = lamports as f64 / 1_000_000_000.0;
                format!("Address: {address}\nBalance: {sol:.9} SOL ({lamports} lamports)")
            }
            Err(e) => format!("Address: {address}\nError fetching balance: {e}"),
        }
    }

    #[tool(description = "Pay a Solana payment request (from a provider's job feedback). Validates protocol fee before sending. Requires Solana payments to be configured via ELISYM_AGENT.")]
    fn send_payment(
        &self,
        Parameters(input): Parameters<SendPaymentInput>,
    ) -> String {
        let Some(provider) = self.agent.solana_payments() else {
            return "Solana payments not configured. Set ELISYM_AGENT to an agent with a Solana wallet.".to_string();
        };

        // Validate fee params before paying — prevent provider from tampering
        if let Some(err) = validate_payment_fee(&input.payment_request) {
            return format!("Fee validation failed: {err}");
        }

        match provider.pay(&input.payment_request) {
            Ok(result) => {
                format!(
                    "Payment sent successfully.\nTransaction: {}\nStatus: {}",
                    result.payment_id, result.status
                )
            }
            Err(e) => format!("Payment failed: {e}"),
        }
    }

    // ══════════════════════════════════════════════════════════════
    // Provider tools
    // ══════════════════════════════════════════════════════════════

    #[tool(description = "Wait for the next incoming job request (provider mode). Subscribes to NIP-90 job requests and returns when one arrives. The job event is stored internally so you can respond with send_job_feedback and submit_job_result.")]
    async fn poll_next_job(
        &self,
        Parameters(input): Parameters<PollNextJobInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let kind_offsets = input.kind_offsets.unwrap_or_else(|| vec![DEFAULT_KIND_OFFSET]);
        let timeout_secs = input.timeout_secs.unwrap_or(60);

        let mut rx = match self
            .agent
            .marketplace
            .subscribe_to_job_requests(&kind_offsets)
            .await
        {
            Ok(rx) => rx,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Error subscribing to job requests: {e}"
                ))]))
            }
        };

        let timeout = tokio::time::Duration::from_secs(timeout_secs);
        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Some(job)) => {
                let event_id = job.event_id;
                let customer_npub = job.customer.to_bech32().unwrap_or_default();

                // Store the raw event for later use in feedback/result.
                // Cap at 100 entries to prevent unbounded growth; evict oldest.
                {
                    let mut map = self.job_events.lock().await;
                    if map.len() >= 100 {
                        // Remove the entry with the lowest (oldest) created_at
                        if let Some(oldest_id) = map
                            .iter()
                            .min_by_key(|(_, ev)| ev.created_at)
                            .map(|(id, _)| *id)
                        {
                            map.remove(&oldest_id);
                        }
                    }
                    map.insert(event_id, job.raw_event);
                }

                let info = serde_json::json!({
                    "event_id": event_id.to_string(),
                    "customer_npub": customer_npub,
                    "kind_offset": job.kind_offset,
                    "input_data": job.input_data,
                    "input_type": job.input_type,
                    "bid_amount": job.bid,
                    "tags": job.tags,
                });
                let json = serde_json::to_string_pretty(&info)
                    .unwrap_or_else(|e| format!("Error serializing job: {e}"));
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            Ok(None) => Ok(CallToolResult::error(vec![Content::text(
                "Job subscription ended without receiving a request.",
            )])),
            Err(_) => Ok(CallToolResult::error(vec![Content::text(format!(
                "No job received within {timeout_secs}s. Try again or increase timeout."
            ))])),
        }
    }

    #[tool(description = "Send a job feedback status update to the customer (provider mode). Use this to send PaymentRequired (with payment request), Processing, Error, etc.")]
    async fn send_job_feedback(
        &self,
        Parameters(input): Parameters<SendJobFeedbackInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let event_id = match EventId::parse(&input.job_event_id) {
            Ok(id) => id,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid event ID: {e}"
                ))]))
            }
        };

        let raw_event = match self.job_events.lock().await.get(&event_id) {
            Some(ev) => ev.clone(),
            None => {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Job event not found. Use poll_next_job first to receive jobs.",
                )]))
            }
        };

        let status = match input.status.as_str() {
            "payment-required" => elisym_core::JobStatus::PaymentRequired,
            "processing" => elisym_core::JobStatus::Processing,
            "error" => elisym_core::JobStatus::Error,
            "success" => elisym_core::JobStatus::Success,
            "partial" => elisym_core::JobStatus::Partial,
            other => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Unknown status: '{other}'. Use: payment-required, processing, error, success, partial"
                ))]))
            }
        };

        let payment_chain = if status == elisym_core::JobStatus::PaymentRequired {
            Some("solana")
        } else {
            None
        };

        match self
            .agent
            .marketplace
            .submit_job_feedback(
                &raw_event,
                status,
                input.extra_info.as_deref(),
                input.amount,
                input.payment_request.as_deref(),
                payment_chain,
            )
            .await
        {
            Ok(feedback_id) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Feedback sent. Event ID: {feedback_id}"
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Error sending feedback: {e}"
            ))])),
        }
    }

    #[tool(description = "Submit a job result back to the customer (provider mode). Delivers the completed work for a previously received job request.")]
    async fn submit_job_result(
        &self,
        Parameters(input): Parameters<SubmitJobResultInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let event_id = match EventId::parse(&input.job_event_id) {
            Ok(id) => id,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid event ID: {e}"
                ))]))
            }
        };

        let raw_event = match self.job_events.lock().await.get(&event_id) {
            Some(ev) => ev.clone(),
            None => {
                return Ok(CallToolResult::error(vec![Content::text(
                    "Job event not found. Use poll_next_job first to receive jobs.",
                )]))
            }
        };

        match self
            .agent
            .marketplace
            .submit_job_result(&raw_event, &input.content, input.amount)
            .await
        {
            Ok(result_id) => {
                // Clean up stored event
                self.job_events.lock().await.remove(&event_id);
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Result delivered. Event ID: {result_id}"
                ))]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Error submitting result: {e}"
            ))])),
        }
    }

    #[tool(description = "Generate a Solana payment request with 3% protocol fee to send to a customer (provider mode). Use the returned request string in send_job_feedback with status 'payment-required'.")]
    fn create_payment_request(
        &self,
        Parameters(input): Parameters<CreatePaymentRequestInput>,
    ) -> String {
        let Some(provider) = self.agent.solana_payments() else {
            return "Solana payments not configured. Set ELISYM_AGENT to an agent with a Solana wallet.".to_string();
        };

        let expiry = input.expiry_secs.unwrap_or(600);
        let fee_amount = (input.amount * PROTOCOL_FEE_BPS).div_ceil(10_000);
        match provider.create_payment_request_with_fee(
            input.amount,
            &input.description,
            expiry,
            PROTOCOL_TREASURY,
            fee_amount,
        ) {
            Ok(req) => {
                let provider_net = input.amount - fee_amount;
                format!(
                    "Payment request created.\nRequest: {}\nAmount: {} lamports (provider net: {}, fee: {})\nChain: {:?}",
                    req.request, req.amount, provider_net, fee_amount, req.chain
                )
            }
            Err(e) => format!("Error creating payment request: {e}"),
        }
    }

    #[tool(description = "Check whether a payment request has been paid (provider mode). Use this after sending a PaymentRequired feedback to verify the customer has paid before processing the job.")]
    fn check_payment_status(
        &self,
        Parameters(input): Parameters<CheckPaymentStatusInput>,
    ) -> String {
        let Some(provider) = self.agent.solana_payments() else {
            return "Solana payments not configured. Set ELISYM_AGENT to an agent with a Solana wallet.".to_string();
        };

        match provider.lookup_payment(&input.payment_request) {
            Ok(status) => {
                let settled = if status.settled { "Yes" } else { "No" };
                let amount_info = status
                    .amount
                    .map(|a| format!("\nAmount: {a} lamports"))
                    .unwrap_or_default();
                format!("Settled: {settled}{amount_info}")
            }
            Err(e) => format!("Error checking payment: {e}"),
        }
    }

    #[tool(description = "Publish this agent's capability card to the Nostr network (NIP-89). Makes this agent discoverable by other agents and customers.")]
    async fn publish_capabilities(
        &self,
        Parameters(input): Parameters<PublishCapabilitiesInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let supported_kinds = input.supported_kinds.unwrap_or_else(|| vec![DEFAULT_KIND_OFFSET]);

        match self
            .agent
            .discovery
            .publish_capability(&self.agent.capability_card, &supported_kinds)
            .await
        {
            Ok(event_id) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Capability card published.\nEvent ID: {event_id}\nName: {}\nCapabilities: {:?}",
                self.agent.capability_card.name, self.agent.capability_card.capabilities
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Error publishing capabilities: {e}"
            ))])),
        }
    }
}

#[tool_handler]
impl ServerHandler for ElisymServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_instructions(
            "elisym protocol MCP server — discover AI agents, submit jobs, \
             send messages, and manage payments on the Nostr-based agent marketplace. \
             Use search_agents to find providers, create_job to submit tasks, \
             get_job_result to retrieve results, and get_balance/send_payment for Solana wallet. \
             For the full automated flow, use submit_and_pay_job. \
             For provider mode, use poll_next_job, send_job_feedback, submit_job_result, \
             and publish_capabilities."
                .to_string(),
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListResourcesResult, rmcp::ErrorData> {
        let mut resources = vec![RawResource::new(
            "elisym://identity",
            "Agent Identity".to_string(),
        )
        .with_description("This agent's Nostr public key, name, description, and capabilities")
        .with_mime_type("application/json")
        .no_annotation()];

        if self.agent.solana_payments().is_some() {
            resources.push(
                RawResource::new("elisym://wallet", "Solana Wallet".to_string())
                    .with_description("Solana wallet address and balance")
                    .with_mime_type("application/json")
                    .no_annotation(),
            );
        }

        Ok(ListResourcesResult {
            resources,
            next_cursor: None,
            meta: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ReadResourceResult, rmcp::ErrorData> {
        let uri = &request.uri;
        match uri.as_str() {
            "elisym://identity" => {
                let identity = serde_json::json!({
                    "npub": self.agent.identity.npub(),
                    "name": self.agent.capability_card.name,
                    "description": self.agent.capability_card.description,
                    "capabilities": self.agent.capability_card.capabilities,
                    "protocol_version": self.agent.capability_card.protocol_version,
                });
                let json = serde_json::to_string_pretty(&identity).unwrap_or_default();
                Ok(ReadResourceResult::new(vec![ResourceContents::text(
                    json,
                    uri.clone(),
                )]))
            }
            "elisym://wallet" => {
                let Some(provider) = self.agent.solana_payments() else {
                    return Err(rmcp::ErrorData::resource_not_found(
                        "Solana payments not configured",
                        None,
                    ));
                };

                let address = provider.address();
                let balance = provider.balance().unwrap_or(0);
                let sol = balance as f64 / 1_000_000_000.0;

                let wallet = serde_json::json!({
                    "address": address,
                    "balance_lamports": balance,
                    "balance_sol": format!("{sol:.9}"),
                    "chain": "solana",
                });
                let json = serde_json::to_string_pretty(&wallet).unwrap_or_default();
                Ok(ReadResourceResult::new(vec![ResourceContents::text(
                    json,
                    uri.clone(),
                )]))
            }
            _ => Err(rmcp::ErrorData::resource_not_found(
                "resource_not_found",
                Some(serde_json::json!({ "uri": uri })),
            )),
        }
    }
}
