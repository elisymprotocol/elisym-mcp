use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
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

// ── Input length limits ──────────────────────────────────────────────
const MAX_INPUT_LEN: usize = 100_000;
const MAX_MESSAGE_LEN: usize = 50_000;
const MAX_NPUB_LEN: usize = 128;
const MAX_EVENT_ID_LEN: usize = 128;
const MAX_PAYMENT_REQ_LEN: usize = 10_000;
const MAX_DESCRIPTION_LEN: usize = 1_000;
const MAX_CAPABILITIES: usize = 50;

/// Maximum allowed timeout for any user-supplied timeout_secs parameter (10 minutes).
const MAX_TIMEOUT_SECS: u64 = 600;
/// Maximum allowed value for max_messages parameter.
const MAX_MESSAGES: usize = 1000;

/// Validate that a string field does not exceed `max` bytes.
fn check_len(field: &str, value: &str, max: usize) -> Result<(), String> {
    if value.len() > max {
        Err(format!(
            "{field} too long: {} bytes (max {max})",
            value.len()
        ))
    } else {
        Ok(())
    }
}

/// Cache for received job events.
/// Insert and oldest-eviction are O(1). Removal by ID is O(n).
pub struct JobEventsCache {
    map: HashMap<EventId, Event>,
    order: VecDeque<EventId>,
}

const JOB_CACHE_CAP: usize = 1000;

impl JobEventsCache {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn insert(&mut self, id: EventId, event: Event) {
        // If already present, update the event in place without duplicating in deque.
        if let std::collections::hash_map::Entry::Occupied(mut e) = self.map.entry(id) {
            e.insert(event);
            return;
        }
        if self.map.len() >= JOB_CACHE_CAP {
            if let Some(oldest_id) = self.order.pop_front() {
                tracing::warn!(
                    evicted_event = %oldest_id,
                    "Job events cache full ({JOB_CACHE_CAP}), evicting oldest entry"
                );
                self.map.remove(&oldest_id);
            }
        }
        self.map.insert(id, event);
        self.order.push_back(id);
    }

    fn get(&self, id: &EventId) -> Option<&Event> {
        self.map.get(id)
    }

    /// Remove by ID. O(n) due to deque scan.
    fn remove(&mut self, id: &EventId) {
        self.map.remove(id);
        self.order.retain(|eid| eid != id);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
}

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

    let expected_fee = data
        .amount
        .checked_mul(PROTOCOL_FEE_BPS)
        .map(|v| v.div_ceil(10_000))
        .unwrap_or(u64::MAX);

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
fn truncate_str(s: &str, max: usize) -> Cow<'_, str> {
    match s.char_indices().nth(max) {
        Some((i, _)) => Cow::Owned(format!("{}…", &s[..i])),
        None => Cow::Borrowed(s),
    }
}

/// Format lamports as a numeric SOL string with 9 decimal places (integer math, no f64).
/// Returns just the number (e.g. "1.500000000"), suitable for JSON fields.
fn format_sol_numeric(lamports: u64) -> String {
    format!("{}.{:09}", lamports / 1_000_000_000, lamports % 1_000_000_000)
}

/// Format lamports as SOL with 9 decimal places and " SOL" suffix (integer math, no f64).
fn format_sol(lamports: u64) -> String {
    format!("{} SOL", format_sol_numeric(lamports))
}

/// Format lamports as SOL with 4 decimal places (integer math, no f64).
/// Note: the sub-SOL part is truncated (not rounded) to 4 decimal places.
fn format_sol_short(lamports: u64) -> String {
    format!(
        "{}.{:04} SOL",
        lamports / 1_000_000_000,
        (lamports % 1_000_000_000) / 100_000
    )
}

/// Heartbeat message for ping/pong liveness checks (NIP-17 encrypted).
///
/// Wire format: JSON `{"type": "elisym_ping"|"elisym_pong", "nonce": "<hex>"}` sent as
/// a NIP-17 gift-wrapped encrypted message. The pinger generates a random nonce and sends
/// `elisym_ping`; the responder replies with `elisym_pong` and the same nonce.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HeartbeatMessage {
    #[serde(rename = "type")]
    msg_type: String,
    nonce: String,
}

/// Simple sliding-window rate limiter using atomics.
/// Allows `max_calls` per `window_secs` second window.
struct RateLimiter {
    /// Packed: upper 32 bits = window start (unix secs truncated to u32), lower 32 bits = count.
    /// The u32 unix timestamp will overflow in 2106 — acceptable for a rate limiter.
    state: AtomicU64,
    max_calls: u32,
    window_secs: u32,
}

impl RateLimiter {
    const fn new(max_calls: u32, window_secs: u32) -> Self {
        assert!(window_secs > 0, "window_secs must be > 0");
        Self {
            state: AtomicU64::new(0),
            max_calls,
            window_secs,
        }
    }

    fn check(&self) -> Result<(), String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;
        let window = now / self.window_secs;

        loop {
            let current = self.state.load(Ordering::Relaxed);
            let stored_window = (current >> 32) as u32;
            let count = current as u32;

            let (new_window, new_count) = if stored_window == window {
                if count >= self.max_calls {
                    return Err(format!(
                        "Rate limit exceeded: max {} calls per {}s. Try again shortly.",
                        self.max_calls, self.window_secs
                    ));
                }
                (window, count + 1)
            } else {
                (window, 1)
            };

            let new_state = ((new_window as u64) << 32) | (new_count as u64);
            if self
                .state
                .compare_exchange_weak(current, new_state, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return Ok(());
            }
        }
    }
}

/// Global rate limiter shared across all payment/messaging tools:
/// send_payment, send_message, create_payment_request, submit_and_pay_job.
/// Limits aggregate throughput to 10 calls per 10s window.
/// Shared across all HTTP sessions; for stdio transport this is a single-client process.
static TOOL_RATE_LIMITER: RateLimiter = RateLimiter::new(10, 10);

pub struct ElisymServer {
    agent: Arc<AgentNode>,
    /// Stores raw events for received job requests (provider flow).
    job_cache: Arc<Mutex<JobEventsCache>>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl ElisymServer {
    pub fn new(agent: AgentNode) -> Self {
        Self {
            agent: Arc::new(agent),
            job_cache: Arc::new(Mutex::new(JobEventsCache::new())),
            tool_router: Self::tool_router(),
        }
    }

    /// Create from shared state (used by HTTP transport factory).
    #[cfg(feature = "transport-http")]
    pub fn from_shared(
        agent: Arc<AgentNode>,
        job_cache: Arc<Mutex<JobEventsCache>>,
    ) -> Self {
        Self {
            agent,
            job_cache,
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
        if input.capabilities.len() > MAX_CAPABILITIES {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Too many capabilities: {} (max {MAX_CAPABILITIES})",
                input.capabilities.len()
            ))]));
        }
        let filter = AgentFilter {
            capabilities: input.capabilities,
            job_kind: input.job_kind,
            ..Default::default()
        };

        match self.agent.discovery.search_agents(&filter).await {
            Ok(agents) => {
                let infos: Vec<AgentInfo> = agents
                    .iter()
                    .map(|a| {
                        let meta = a.card.metadata.as_ref();
                        AgentInfo {
                            npub: a.pubkey.to_bech32().unwrap_or_default(),
                            name: a.card.name.clone(),
                            description: a.card.description.clone(),
                            capabilities: a.card.capabilities.clone(),
                            supported_kinds: a.supported_kinds.clone(),
                            job_price_lamports: meta
                                .and_then(|m| m["job_price"].as_u64()),
                            chain: meta
                                .and_then(|m| m["chain"].as_str())
                                .map(String::from),
                            network: meta
                                .and_then(|m| m["network"].as_str())
                                .map(String::from),
                        }
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
    fn get_identity(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let meta = self.agent.capability_card.metadata.as_ref();
        let info = AgentInfo {
            npub: self.agent.identity.npub(),
            name: self.agent.capability_card.name.clone(),
            description: self.agent.capability_card.description.clone(),
            capabilities: self.agent.capability_card.capabilities.clone(),
            supported_kinds: vec![DEFAULT_KIND_OFFSET],
            job_price_lamports: meta.and_then(|m| m["job_price"].as_u64()),
            chain: meta.and_then(|m| m["chain"].as_str()).map(String::from),
            network: meta.and_then(|m| m["network"].as_str()).map(String::from),
        };
        let json = serde_json::to_string_pretty(&info)
            .unwrap_or_else(|e| format!("Error serializing identity: {e}"));
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    #[tool(description = "Get a snapshot of the elisym network — top agents ranked by earnings, with total protocol earnings. Shows agent name, capabilities, price, and earned amount.")]
    async fn get_dashboard(
        &self,
        Parameters(input): Parameters<GetDashboardInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let top_n = input.top_n.unwrap_or(10).min(100);
        let timeout_secs = input.timeout_secs.unwrap_or(15).min(MAX_TIMEOUT_SECS);
        let filter_chain = input.chain.unwrap_or_else(|| "solana".into());
        let filter_network = input.network.unwrap_or_else(|| "devnet".into());
        let fetch_timeout = Some(std::time::Duration::from_secs(timeout_secs));

        // 1. Discover all agents and filter by chain + network locally.
        //    Protocol-level filtering by chain/network is not yet supported in elisym-core.
        // TODO(elisym-core): Add chain/network filter to AgentFilter to avoid fetching all agents.
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

        // Build a pubkey → npub cache (bech32 encoding is not free)
        let pk_to_npub: HashMap<PublicKey, String> = agents
            .iter()
            .filter_map(|a| Some((a.pubkey, a.pubkey.to_bech32().ok()?)))
            .collect();

        // 2. Fetch job result events (kind 6100) to calculate earnings.
        //    Filter by author pubkeys and last 30 days to bound the query.
        let result_kind = kind(KIND_JOB_RESULT_BASE + DEFAULT_KIND_OFFSET);
        let author_pks: Vec<PublicKey> = agents.iter().map(|a| a.pubkey).collect();
        let thirty_days_ago = Timestamp::from(
            Timestamp::now().as_u64().saturating_sub(30 * 24 * 60 * 60),
        );
        let event_filter = if author_pks.is_empty() {
            nostr_sdk::Filter::new().kind(result_kind).since(thirty_days_ago)
        } else {
            nostr_sdk::Filter::new()
                .kind(result_kind)
                .authors(author_pks)
                .since(thirty_days_ago)
        };
        let events = self
            .agent
            .client
            .fetch_events(vec![event_filter], fetch_timeout)
            .await;

        // 3. Accumulate earnings per provider (only agents in this network)
        let mut earnings: HashMap<&str, u64> = HashMap::new();
        let (event_list, fetch_warning) = match &events {
            Ok(ev) => (ev.iter().collect::<Vec<_>>(), None),
            Err(e) => {
                tracing::warn!("Failed to fetch job result events: {e}");
                (vec![], Some(format!("Warning: could not fetch earnings data: {e}")))
            }
        };
        let mut total_job_results = 0usize;
        for event in event_list.iter() {
            let npub = match pk_to_npub.get(&event.pubkey) {
                Some(n) => n.as_str(),
                None => continue, // not an agent in this network
            };
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
                let entry = earnings.entry(npub).or_insert(0);
                *entry = entry.saturating_add(amt);
            }
        }

        // Total earned across ALL agents in this network
        let total_earned_lamports: u64 = earnings.values().copied().fold(0u64, u64::saturating_add);

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
                let npub = pk_to_npub.get(&a.pubkey).cloned().unwrap_or_default();
                let earned = earnings.get(npub.as_str()).copied().unwrap_or(0);
                let price = a
                    .card
                    .metadata
                    .as_ref()
                    .and_then(|m| m["job_price"].as_u64())
                    .unwrap_or(0);
                let price_str = if price == 0 {
                    "—".into()
                } else {
                    format_sol_short(price)
                };
                AgentRow {
                    name: a.card.name.clone(),
                    npub: truncate_str(&npub, 20).into_owned(),
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
             Agents: {}  |  Total Earned (30d): {}  |  Job Results: {}\n\n",
            filter_chain,
            filter_network,
            agents.len(),
            format_sol_short(total_earned_lamports),
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
                    format_sol_short(row.earned)
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

        if let Some(warning) = fetch_warning {
            output.push_str(&format!("\n{warning}\n"));
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
        if let Err(err) = check_len("input", &input.input, MAX_INPUT_LEN) {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }
        if let Some(ref npub) = input.provider_npub {
            if let Err(err) = check_len("provider_npub", npub, MAX_NPUB_LEN) {
                return Ok(CallToolResult::error(vec![Content::text(err)]));
            }
        }
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

    #[tool(description = "Wait for and retrieve the result of a previously submitted job request. Subscribes to NIP-90 results and waits up to the specified timeout. Note: only captures results arriving after this tool is called — if the provider already responded, the result may be missed. Use submit_and_pay_job for a race-free flow.")]
    async fn get_job_result(
        &self,
        Parameters(input): Parameters<GetJobResultInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(err) = check_len("job_event_id", &input.job_event_id, MAX_EVENT_ID_LEN) {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }
        let timeout_secs = input.timeout_secs.unwrap_or(60).min(MAX_TIMEOUT_SECS);

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

    #[tool(description = "Wait for job feedback (PaymentRequired, Processing, Error, etc.) on a previously submitted job. Returns the first feedback event matching the job ID. Note: only captures feedback arriving after this tool is called. Use submit_and_pay_job for a race-free flow.")]
    async fn get_job_feedback(
        &self,
        Parameters(input): Parameters<GetJobFeedbackInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(err) = check_len("job_event_id", &input.job_event_id, MAX_EVENT_ID_LEN) {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }
        let timeout_secs = input.timeout_secs.unwrap_or(60).min(MAX_TIMEOUT_SECS);

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
        if let Err(err) = TOOL_RATE_LIMITER.check() {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }
        if let Err(err) = check_len("input", &input.input, MAX_INPUT_LEN) {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }
        if let Err(err) = check_len("provider_npub", &input.provider_npub, MAX_NPUB_LEN) {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }
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
        let total_timeout = input.timeout_secs.unwrap_or(300).min(MAX_TIMEOUT_SECS);

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
        let mut feedback_closed = false;
        let mut result_closed = false;

        // 3. Event loop: handle feedback and results
        loop {
            tokio::select! {
                fb_opt = feedback_rx.recv(), if !feedback_closed => {
                    let Some(fb) = fb_opt else {
                        feedback_closed = true;
                        if result_closed {
                            status_log.push("Both channels closed unexpectedly.".into());
                            return Ok(CallToolResult::error(vec![Content::text(
                                status_log.join("\n")
                            )]));
                        }
                        continue;
                    };
                    if fb.request_id != event_id {
                        continue;
                    }
                    // Always handle errors, even after payment
                    if fb.status.as_str() == "error" {
                        let info = fb.extra_info.as_deref().unwrap_or("unknown error");
                        status_log.push(format!("Provider error: {info}"));
                        return Ok(CallToolResult::error(vec![Content::text(
                            status_log.join("\n")
                        )]));
                    }
                    // Skip non-error feedback after payment
                    if paid {
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
                                if self.agent.solana_payments().is_none() {
                                    status_log.push("Payment required but Solana payments not configured.".into());
                                    return Ok(CallToolResult::error(vec![Content::text(
                                        status_log.join("\n")
                                    )]));
                                }
                                let agent = Arc::clone(&self.agent);
                                let pr = payment_request.clone();
                                match tokio::task::spawn_blocking(move || {
                                    agent.solana_payments().unwrap().pay(&pr)
                                }).await {
                                    Ok(Ok(result)) => {
                                        status_log.push(format!(
                                            "Payment sent: {} ({})",
                                            result.payment_id, result.status
                                        ));
                                        paid = true;
                                        tracing::info!("Payment sent, waiting for result");
                                    }
                                    Ok(Err(e)) => {
                                        status_log.push(format!("Payment failed: {e}"));
                                        return Ok(CallToolResult::error(vec![Content::text(
                                            status_log.join("\n")
                                        )]));
                                    }
                                    Err(e) => {
                                        status_log.push(format!("Payment task panicked: {e}"));
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
                        other => {
                            status_log.push(format!("Feedback: {other}"));
                        }
                    }
                }
                res_opt = result_rx.recv(), if !result_closed => {
                    let Some(result) = res_opt else {
                        result_closed = true;
                        if feedback_closed {
                            status_log.push("Both channels closed unexpectedly.".into());
                            return Ok(CallToolResult::error(vec![Content::text(
                                status_log.join("\n")
                            )]));
                        }
                        continue;
                    };
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
        if let Err(err) = check_len("agent_npub", &input.agent_npub, MAX_NPUB_LEN) {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }
        let target = match PublicKey::from_bech32(&input.agent_npub) {
            Ok(pk) => pk,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid npub: {e}"
                ))]))
            }
        };

        let timeout_secs = input.timeout_secs.unwrap_or(15).min(MAX_TIMEOUT_SECS);
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
        if let Err(err) = TOOL_RATE_LIMITER.check() {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }
        if let Err(err) = check_len("message", &input.message, MAX_MESSAGE_LEN) {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }
        if let Err(err) = check_len("recipient_npub", &input.recipient_npub, MAX_NPUB_LEN) {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }
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
        let timeout_secs = input.timeout_secs.unwrap_or(30).min(MAX_TIMEOUT_SECS);
        let max_messages = input.max_messages.unwrap_or(10).min(MAX_MESSAGES);

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
                msg_opt = rx.recv() => {
                    let Some(msg) = msg_opt else {
                        break; // channel closed
                    };
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
    async fn get_balance(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let Some(provider) = self.agent.solana_payments() else {
            return Ok(CallToolResult::error(vec![Content::text(
                "Solana payments not configured. Set ELISYM_AGENT to an agent with a Solana wallet.",
            )]));
        };

        let address = provider.address();
        let agent = Arc::clone(&self.agent);
        match tokio::task::spawn_blocking(move || {
            agent.solana_payments().unwrap().balance()
        }).await {
            Ok(Ok(lamports)) => {
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Address: {address}\nBalance: {} ({lamports} lamports)",
                    format_sol(lamports)
                ))]))
            }
            Ok(Err(e)) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Address: {address}\nError fetching balance: {e}"
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Address: {address}\nBalance check panicked: {e}"
            ))])),
        }
    }

    #[tool(description = "Pay a Solana payment request (from a provider's job feedback). Validates protocol fee before sending. Requires Solana payments to be configured via ELISYM_AGENT.")]
    async fn send_payment(
        &self,
        Parameters(input): Parameters<SendPaymentInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(err) = TOOL_RATE_LIMITER.check() {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }
        if let Err(err) = check_len("payment_request", &input.payment_request, MAX_PAYMENT_REQ_LEN) {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }

        if self.agent.solana_payments().is_none() {
            return Ok(CallToolResult::error(vec![Content::text(
                "Solana payments not configured. Set ELISYM_AGENT to an agent with a Solana wallet.",
            )]));
        }

        // Validate fee params before paying — prevent provider from tampering
        if let Some(err) = validate_payment_fee(&input.payment_request) {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "Fee validation failed: {err}"
            ))]));
        }

        let agent = Arc::clone(&self.agent);
        let payment_request = input.payment_request;
        match tokio::task::spawn_blocking(move || {
            agent.solana_payments().unwrap().pay(&payment_request)
        }).await {
            Ok(Ok(result)) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Payment sent successfully.\nTransaction: {}\nStatus: {}",
                result.payment_id, result.status
            ))])),
            Ok(Err(e)) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Payment failed: {e}"
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Payment task panicked: {e}"
            ))])),
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
        let timeout_secs = input.timeout_secs.unwrap_or(60).min(MAX_TIMEOUT_SECS);

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
                {
                    let mut cache = self.job_cache.lock().await;
                    cache.insert(event_id, job.raw_event);
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
        if let Err(err) = check_len("job_event_id", &input.job_event_id, MAX_EVENT_ID_LEN) {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }
        if let Some(ref pr) = input.payment_request {
            if let Err(err) = check_len("payment_request", pr, MAX_PAYMENT_REQ_LEN) {
                return Ok(CallToolResult::error(vec![Content::text(err)]));
            }
        }
        if let Some(ref info) = input.extra_info {
            if let Err(err) = check_len("extra_info", info, MAX_DESCRIPTION_LEN) {
                return Ok(CallToolResult::error(vec![Content::text(err)]));
            }
        }
        let event_id = match EventId::parse(&input.job_event_id) {
            Ok(id) => id,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid event ID: {e}"
                ))]))
            }
        };

        let raw_event = match self.job_cache.lock().await.get(&event_id) {
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
        if let Err(err) = check_len("content", &input.content, MAX_INPUT_LEN) {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }
        if let Err(err) = check_len("job_event_id", &input.job_event_id, MAX_EVENT_ID_LEN) {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }
        let event_id = match EventId::parse(&input.job_event_id) {
            Ok(id) => id,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Invalid event ID: {e}"
                ))]))
            }
        };

        let raw_event = match self.job_cache.lock().await.get(&event_id) {
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
                self.job_cache.lock().await.remove(&event_id);
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Result delivered. Event ID: {result_id}"
                ))]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Error submitting result: {e}"
            ))])),
        }
    }

    #[tool(description = "Generate a Solana payment request with 3% protocol fee to send to a customer (provider mode). Returns a JSON object with the request string to use in send_job_feedback with status 'payment-required'.")]
    async fn create_payment_request(
        &self,
        Parameters(input): Parameters<CreatePaymentRequestInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(err) = TOOL_RATE_LIMITER.check() {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }
        if let Err(err) = check_len("description", &input.description, MAX_DESCRIPTION_LEN) {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }

        if input.amount == 0 {
            return Ok(CallToolResult::error(vec![Content::text(
                "Amount must be greater than 0.",
            )]));
        }

        if self.agent.solana_payments().is_none() {
            return Ok(CallToolResult::error(vec![Content::text(
                "Solana payments not configured. Set ELISYM_AGENT to an agent with a Solana wallet.",
            )]));
        }

        let expiry = input.expiry_secs.unwrap_or(600);
        // Fee rounds up to nearest lamport (div_ceil favors protocol).
        // checked_mul guards against overflow on very large amounts.
        let fee_amount = input
            .amount
            .checked_mul(PROTOCOL_FEE_BPS)
            .map(|v| v.div_ceil(10_000))
            .unwrap_or(u64::MAX);
        let agent = Arc::clone(&self.agent);
        let amount = input.amount;
        let description = input.description;
        match tokio::task::spawn_blocking(move || {
            agent.solana_payments().unwrap().create_payment_request_with_fee(
                amount,
                &description,
                expiry,
                PROTOCOL_TREASURY,
                fee_amount,
            )
        }).await {
            Ok(Ok(req)) => {
                let provider_net = amount.saturating_sub(fee_amount);
                let result = serde_json::json!({
                    "payment_request": req.request,
                    "amount_lamports": req.amount,
                    "provider_net_lamports": provider_net,
                    "fee_lamports": fee_amount,
                    "chain": format!("{:?}", req.chain),
                });
                let json = serde_json::to_string_pretty(&result).unwrap_or_default();
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            Ok(Err(e)) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Error creating payment request: {e}"
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Payment request task panicked: {e}"
            ))])),
        }
    }

    #[tool(description = "Check whether a payment request has been paid (provider mode). Use this after sending a PaymentRequired feedback to verify the customer has paid before processing the job.")]
    async fn check_payment_status(
        &self,
        Parameters(input): Parameters<CheckPaymentStatusInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Err(err) = check_len("payment_request", &input.payment_request, MAX_PAYMENT_REQ_LEN) {
            return Ok(CallToolResult::error(vec![Content::text(err)]));
        }

        if self.agent.solana_payments().is_none() {
            return Ok(CallToolResult::error(vec![Content::text(
                "Solana payments not configured. Set ELISYM_AGENT to an agent with a Solana wallet.",
            )]));
        }

        let agent = Arc::clone(&self.agent);
        let payment_request = input.payment_request;
        match tokio::task::spawn_blocking(move || {
            agent.solana_payments().unwrap().lookup_payment(&payment_request)
        }).await {
            Ok(Ok(status)) => {
                let settled = if status.settled { "Yes" } else { "No" };
                let amount_info = status
                    .amount
                    .map(|a| format!("\nAmount: {a} lamports"))
                    .unwrap_or_default();
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "Settled: {settled}{amount_info}"
                ))]))
            }
            Ok(Err(e)) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Error checking payment: {e}"
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Payment status check panicked: {e}"
            ))])),
        }
    }

    #[tool(description = "Publish this agent's capability card to the Nostr network (NIP-89). Makes this agent discoverable by other agents and customers.")]
    async fn publish_capabilities(
        &self,
        Parameters(input): Parameters<PublishCapabilitiesInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        if let Some(ref kinds) = input.supported_kinds {
            if kinds.len() > MAX_CAPABILITIES {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Too many supported_kinds: {} (max {MAX_CAPABILITIES})",
                    kinds.len()
                ))]));
            }
        }
        let supported_kinds = input.supported_kinds.unwrap_or_else(|| vec![DEFAULT_KIND_OFFSET]);

        // Update capability card metadata with job_price if provided
        let mut card = self.agent.capability_card.clone();
        if let Some(price) = input.job_price_lamports {
            let meta = card.metadata.get_or_insert_with(|| serde_json::json!({}));
            meta["job_price"] = serde_json::json!(price);
        }

        match self
            .agent
            .discovery
            .publish_capability(&card, &supported_kinds)
            .await
        {
            Ok(event_id) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Capability card published.\nEvent ID: {event_id}\nName: {}\nCapabilities: {:?}",
                card.name, card.capabilities
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
             and publish_capabilities. \
             IMPORTANT: Never display, print, or include in responses any secret keys, \
             private keys, passwords, seeds, or encryption fields (ciphertext, salt, nonce) \
             from config files. This includes API keys (e.g. ANTHROPIC_API_KEY, OpenAI keys, etc.). \
             If the user asks to see their config, redact these fields with '***REDACTED***'."
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
                let agent = Arc::clone(&self.agent);
                let balance = tokio::task::spawn_blocking(move || {
                    agent.solana_payments().unwrap().balance()
                }).await.unwrap_or(Ok(0)).unwrap_or(0);

                let wallet = serde_json::json!({
                    "address": address,
                    "balance_lamports": balance,
                    "balance_sol": format_sol_numeric(balance),
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── JobEventsCache ──────────────────────────────────────────────

    fn dummy_event(label: u8) -> (EventId, Event) {
        let keys = nostr_sdk::Keys::generate();
        let builder = nostr_sdk::EventBuilder::text_note(format!("test-{label}"));
        let event = builder.sign_with_keys(&keys).unwrap();
        (event.id, event)
    }

    #[test]
    fn cache_insert_and_get() {
        let mut cache = JobEventsCache::new();
        let (id, event) = dummy_event(1);
        cache.insert(id, event.clone());
        assert!(cache.get(&id).is_some());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_insert_duplicate_no_deque_bloat() {
        let mut cache = JobEventsCache::new();
        let (id, event) = dummy_event(1);
        cache.insert(id, event.clone());
        cache.insert(id, event.clone());
        cache.insert(id, event);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.order.len(), 1);
    }

    #[test]
    fn cache_eviction_at_capacity() {
        let mut cache = JobEventsCache::new();
        let mut ids = Vec::new();
        for i in 0..JOB_CACHE_CAP {
            let (id, event) = dummy_event(i as u8);
            ids.push(id);
            cache.insert(id, event);
        }
        assert_eq!(cache.len(), JOB_CACHE_CAP);
        // Insert one more — oldest should be evicted
        let (new_id, new_event) = dummy_event(255);
        cache.insert(new_id, new_event);
        assert_eq!(cache.len(), JOB_CACHE_CAP);
        assert!(cache.get(&ids[0]).is_none());
        assert!(cache.get(&new_id).is_some());
    }

    #[test]
    fn cache_remove() {
        let mut cache = JobEventsCache::new();
        let (id, event) = dummy_event(1);
        cache.insert(id, event);
        cache.remove(&id);
        assert!(cache.get(&id).is_none());
        assert_eq!(cache.len(), 0);
        assert!(cache.order.is_empty());
    }

    // ── check_len ───────────────────────────────────────────────────

    #[test]
    fn check_len_within_limit() {
        assert!(check_len("field", "hello", 10).is_ok());
    }

    #[test]
    fn check_len_exceeds_limit() {
        assert!(check_len("field", "hello", 3).is_err());
    }

    #[test]
    fn check_len_exact_boundary() {
        assert!(check_len("field", "abc", 3).is_ok());
    }

    // ── truncate_str ────────────────────────────────────────────────

    #[test]
    fn truncate_str_no_truncation() {
        let result = truncate_str("hello", 10);
        assert_eq!(&*result, "hello");
        assert!(matches!(result, Cow::Borrowed(_)));
    }

    #[test]
    fn truncate_str_truncation() {
        let result = truncate_str("hello world", 5);
        assert_eq!(&*result, "hello…");
    }

    #[test]
    fn truncate_str_unicode_safe() {
        // Multi-byte chars should not panic
        let result = truncate_str("Привет мир", 6);
        assert_eq!(&*result, "Привет…");
    }

    // ── validate_payment_fee ────────────────────────────────────────

    fn make_payment_json(amount: u64, fee_address: Option<&str>, fee_amount: Option<u64>) -> String {
        let mut obj = serde_json::json!({
            "recipient": "SomeAddress",
            "amount": amount,
            "reference": "ref123",
        });
        if let Some(addr) = fee_address {
            obj["fee_address"] = serde_json::json!(addr);
        }
        if let Some(amt) = fee_amount {
            obj["fee_amount"] = serde_json::json!(amt);
        }
        serde_json::to_string(&obj).unwrap()
    }

    #[test]
    fn valid_fee() {
        let amount = 10_000_000u64;
        let fee = (amount * PROTOCOL_FEE_BPS).div_ceil(10_000);
        let json = make_payment_json(amount, Some(PROTOCOL_TREASURY), Some(fee));
        assert!(validate_payment_fee(&json).is_none());
    }

    #[test]
    fn wrong_treasury_address() {
        let amount = 10_000_000u64;
        let fee = (amount * PROTOCOL_FEE_BPS).div_ceil(10_000);
        let json = make_payment_json(amount, Some("WrongAddress"), Some(fee));
        let err = validate_payment_fee(&json).unwrap();
        assert!(err.contains("Fee address mismatch"));
    }

    #[test]
    fn wrong_fee_amount() {
        let amount = 10_000_000u64;
        let json = make_payment_json(amount, Some(PROTOCOL_TREASURY), Some(1));
        let err = validate_payment_fee(&json).unwrap();
        assert!(err.contains("Fee amount mismatch"));
    }

    #[test]
    fn missing_fee() {
        let json = make_payment_json(10_000_000, None, None);
        let err = validate_payment_fee(&json).unwrap();
        assert!(err.contains("missing protocol fee"));
    }

    #[test]
    fn invalid_json() {
        assert!(validate_payment_fee("not json").is_some());
    }

    // ── format_sol ──────────────────────────────────────────────────

    #[test]
    fn format_sol_zero() {
        assert_eq!(format_sol(0), "0.000000000 SOL");
    }

    #[test]
    fn format_sol_one_sol() {
        assert_eq!(format_sol(1_000_000_000), "1.000000000 SOL");
    }

    #[test]
    fn format_sol_fractional() {
        assert_eq!(format_sol(1_500_000_000), "1.500000000 SOL");
    }

    #[test]
    fn format_sol_short_zero() {
        assert_eq!(format_sol_short(0), "0.0000 SOL");
    }

    #[test]
    fn format_sol_short_one_sol() {
        assert_eq!(format_sol_short(1_000_000_000), "1.0000 SOL");
    }

    #[test]
    fn format_sol_short_fractional() {
        assert_eq!(format_sol_short(10_000_000), "0.0100 SOL");
    }

    // ── fee calculation ─────────────────────────────────────────────

    #[test]
    fn fee_calculation_standard() {
        let amount = 10_000_000u64;
        let fee = (amount * PROTOCOL_FEE_BPS).div_ceil(10_000);
        assert_eq!(fee, 300_000); // 3% of 10M
    }

    #[test]
    fn fee_calculation_rounds_up() {
        // 1 lamport: (1 * 300) / 10_000 = 0.03 → rounds up to 1
        let fee = (1u64 * PROTOCOL_FEE_BPS).div_ceil(10_000);
        assert_eq!(fee, 1);
    }

    #[test]
    fn fee_calculation_zero() {
        let fee = (0u64 * PROTOCOL_FEE_BPS).div_ceil(10_000);
        assert_eq!(fee, 0);
    }

    #[test]
    fn fee_calculation_overflow_safe() {
        // Very large amount that would overflow with unchecked mul
        let large = u64::MAX / 100;
        let fee = large
            .checked_mul(PROTOCOL_FEE_BPS)
            .map(|v| v.div_ceil(10_000))
            .unwrap_or(u64::MAX);
        assert_eq!(fee, u64::MAX);
    }

    // ── format_sol_numeric ──────────────────────────────────────────

    #[test]
    fn format_sol_numeric_value() {
        assert_eq!(format_sol_numeric(1_500_000_000), "1.500000000");
        assert_eq!(format_sol_numeric(0), "0.000000000");
    }

    // ── RateLimiter ─────────────────────────────────────────────────

    #[test]
    fn rate_limiter_allows_within_limit() {
        let limiter = RateLimiter::new(5, 10);
        for _ in 0..5 {
            assert!(limiter.check().is_ok());
        }
    }

    #[test]
    fn rate_limiter_rejects_over_limit() {
        let limiter = RateLimiter::new(3, 10);
        for _ in 0..3 {
            assert!(limiter.check().is_ok());
        }
        assert!(limiter.check().is_err());
    }
}
