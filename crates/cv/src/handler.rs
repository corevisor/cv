use std::sync::Arc;
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::types::ServiceConfig;

use crate::credential_store::CredentialStore;
use crate::engine::JsEngine;
use crate::hub_client::{ApprovalChecker, HubClient};

const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExecuteJsArgs {
    /// The JavaScript code to execute
    pub code: String,
    /// Execution timeout in seconds (default: 30)
    pub timeout_secs: Option<u64>,
    /// Optional context describing why this code is being executed (e.g., "Fetching user's Notion todo list"). Shown in activity logs and approval prompts.
    pub context: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchApiDocsArgs {
    /// Regex pattern to search for in API documentation (e.g. "POST.*page", "param.*filter")
    pub pattern: String,
    /// Domain to search within (e.g. "api.notion.com"). Resolves to catalog via your services.
    pub domain: Option<String>,
    /// API slug to search within (e.g. "gitlab", "notion"). Direct catalog lookup.
    pub slug: Option<String>,
}

#[derive(Clone)]
pub struct JsExecutor {
    engine: JsEngine,
    profile_id: String,
    services: Vec<ServiceConfig>,
    credential_store: Arc<CredentialStore>,
    hub_client: Arc<HubClient>,
    tool_router: ToolRouter<Self>,
}

impl JsExecutor {
    pub fn new(
        engine: JsEngine,
        profile_id: String,
        services: Vec<ServiceConfig>,
        credential_store: CredentialStore,
        hub_client: HubClient,
    ) -> Self {
        Self {
            engine,
            profile_id,
            services,
            credential_store: Arc::new(credential_store),
            hub_client: Arc::new(hub_client),
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl JsExecutor {
    #[tool(
        description = "Execute JavaScript code. Credentials are automatically injected into HTTP requests for configured domains."
    )]
    async fn execute_javascript(
        &self,
        Parameters(args): Parameters<ExecuteJsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let timeout = Duration::from_secs(args.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

        let result = self
            .engine
            .execute(
                &args.code,
                timeout,
                Some(self.profile_id.clone()),
                self.services.clone(),
                Some(self.credential_store.clone()),
                self.hub_client.clone() as Arc<dyn ApprovalChecker>,
                args.context,
            )
            .await
            .map_err(|e| McpError::internal_error(format!("engine error: {e}"), None))?;

        if result.success {
            let mut content = vec![Content::text(result.stdout)];
            if !result.stderr.is_empty() {
                content.push(Content::text(format!("[stderr]: {}", result.stderr)));
            }
            Ok(CallToolResult::success(content))
        } else {
            let mut output = String::new();
            if !result.stdout.is_empty() {
                output.push_str(&result.stdout);
                output.push('\n');
            }
            if !result.stderr.is_empty() {
                output.push_str(&result.stderr);
            }
            Ok(CallToolResult::error(vec![Content::text(
                if output.is_empty() {
                    "Execution failed with no output".to_string()
                } else {
                    output
                },
            )]))
        }
    }

    #[tool(
        description = "List the API services/domains you have access to through corevisor, along with credential metadata (no secrets)."
    )]
    async fn list_services(&self) -> Result<CallToolResult, McpError> {
        let services = self.hub_client
            .get_services(&self.profile_id)
            .await
            .map_err(|e| McpError::internal_error(format!("failed to fetch services: {e}"), None))?;

        if services.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No services configured. Add domains via `cv credential set <domain>` or sync from the hub.",
            )]));
        }

        let store = &self.credential_store;
        let mut lines = vec!["Configured services:".to_string()];
        for svc in &services {
            let has_cred = store
                .get(&self.profile_id, &svc.domain)
                .ok()
                .flatten()
                .is_some();
            if has_cred {
                lines.push(format!("- {} (header: {}, credential: set)", svc.domain, svc.header_name));
            } else {
                lines.push(format!("- {} (header: {}, credential: not set)", svc.domain, svc.header_name));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(
            lines.join("\n"),
        )]))
    }

    #[tool(
        description = "Search API documentation for configured services using regex patterns. Returns matching endpoint sections with context. Specify either domain or slug to identify the API."
    )]
    async fn search_api_docs(
        &self,
        Parameters(args): Parameters<SearchApiDocsArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.domain.is_none() && args.slug.is_none() {
            return Err(McpError::invalid_params("must provide domain or slug", None));
        }

        let results = self.hub_client
            .search_api_docs(&args.pattern, args.domain.as_deref(), args.slug.as_deref())
            .await
            .map_err(|e| {
                McpError::internal_error(format!("failed to search API docs: {e}"), None)
            })?;

        // results is a JSON value — format it
        let text = serde_json::to_string_pretty(&results)
            .unwrap_or_else(|_| "[]".to_string());

        if text == "[]" {
            return Ok(CallToolResult::success(vec![Content::text(
                "No matches found.",
            )]));
        }

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

#[tool_handler]
impl ServerHandler for JsExecutor {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "corevisor".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: None,
                description: None,
                website_url: None,
                icons: None,
            },
            instructions: Some(
                "Execute JavaScript code with automatic credential injection for configured API domains. Credentials are stored locally and never leave your machine."
                    .to_string(),
            ),
        }
    }
}
