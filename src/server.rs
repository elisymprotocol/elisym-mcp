use std::sync::Arc;

use elisym_core::{AgentFilter, AgentNode};
use nostr_sdk::prelude::*;
use rmcp::{
    ServerHandler,
    handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};

use crate::tools::discovery::{AgentInfo, SearchAgentsInput};
use crate::tools::marketplace::{CreateJobInput, GetJobResultInput};
use crate::tools::messaging::SendMessageInput;

pub struct ElisymServer {
    agent: Arc<AgentNode>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl ElisymServer {
    pub fn new(agent: AgentNode) -> Self {
        Self {
            agent: Arc::new(agent),
            tool_router: Self::tool_router(),
        }
    }

    // ── Discovery tools ──

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
                    Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                        "No agents found matching the specified capabilities.",
                    )]))
                } else {
                    let json = serde_json::to_string_pretty(&infos)
                        .unwrap_or_else(|e| format!("Error serializing results: {e}"));
                    Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                        json,
                    )]))
                }
            }
            Err(e) => Ok(CallToolResult::error(vec![rmcp::model::Content::text(
                format!("Error searching agents: {e}"),
            )])),
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

    // ── Marketplace tools ──

    #[tool(description = "Submit a job request to the elisym agent marketplace (NIP-90). Optionally target a specific provider by npub. Returns the job event ID.")]
    async fn create_job(
        &self,
        Parameters(input): Parameters<CreateJobInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let kind_offset = input.kind_offset.unwrap_or(100);
        let input_type = input.input_type.as_deref().unwrap_or("text");
        let tags = input.tags.unwrap_or_default();

        let provider = match &input.provider_npub {
            Some(npub) => match PublicKey::from_bech32(npub) {
                Ok(pk) => Some(pk),
                Err(e) => {
                    return Ok(CallToolResult::error(vec![rmcp::model::Content::text(
                        format!("Invalid provider npub: {e}"),
                    )]))
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
            Ok(event_id) => Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                format!("Job submitted successfully.\nEvent ID: {event_id}"),
            )])),
            Err(e) => Ok(CallToolResult::error(vec![rmcp::model::Content::text(
                format!("Error submitting job: {e}"),
            )])),
        }
    }

    #[tool(description = "Wait for and retrieve the result of a previously submitted job request. Subscribes to NIP-90 results and waits up to the specified timeout.")]
    async fn get_job_result(
        &self,
        Parameters(input): Parameters<GetJobResultInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let timeout_secs = input.timeout_secs.unwrap_or(60);

        let mut rx = match self
            .agent
            .marketplace
            .subscribe_to_results(&[100], &[])
            .await
        {
            Ok(rx) => rx,
            Err(e) => {
                return Ok(CallToolResult::error(vec![rmcp::model::Content::text(
                    format!("Error subscribing to results: {e}"),
                )]))
            }
        };

        let target_id = match EventId::parse(&input.job_event_id) {
            Ok(id) => id,
            Err(e) => {
                return Ok(CallToolResult::error(vec![rmcp::model::Content::text(
                    format!("Invalid event ID: {e}"),
                )]))
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
                    .amount_msat
                    .map(|a| format!(" (amount: {a} lamports)"))
                    .unwrap_or_default();
                Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                    format!("Job result received{}:\n\n{}", amount_info, result.content),
                )]))
            }
            Ok(None) => Ok(CallToolResult::error(vec![rmcp::model::Content::text(
                "Result subscription ended without receiving a matching result.",
            )])),
            Err(_) => Ok(CallToolResult::error(vec![rmcp::model::Content::text(
                format!(
                    "Timeout after {timeout_secs}s — no result received. \
                     The provider may still be processing. Try again with a longer timeout."
                ),
            )])),
        }
    }

    // ── Messaging tools ──

    #[tool(description = "Send an encrypted private message (NIP-17 gift wrap) to another agent or user on Nostr.")]
    async fn send_message(
        &self,
        Parameters(input): Parameters<SendMessageInput>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let recipient = match PublicKey::from_bech32(&input.recipient_npub) {
            Ok(pk) => pk,
            Err(e) => {
                return Ok(CallToolResult::error(vec![rmcp::model::Content::text(
                    format!("Invalid recipient npub: {e}"),
                )]))
            }
        };

        match self
            .agent
            .messaging
            .send_message(&recipient, &input.message)
            .await
        {
            Ok(()) => Ok(CallToolResult::success(vec![rmcp::model::Content::text(
                format!("Message sent to {}", input.recipient_npub),
            )])),
            Err(e) => Ok(CallToolResult::error(vec![rmcp::model::Content::text(
                format!("Error sending message: {e}"),
            )])),
        }
    }
}

#[tool_handler]
impl ServerHandler for ElisymServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "elisym protocol MCP server — discover AI agents, submit jobs, \
                 send messages, and manage payments on the Nostr-based agent marketplace. \
                 Use search_agents to find providers, create_job to submit tasks, \
                 and get_job_result to retrieve results."
                    .to_string(),
            )
    }
}
