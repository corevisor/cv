use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::body::HyperOutgoingBody;
use wasmtime_wasi_http::types::{
    default_send_request, HostFutureIncomingResponse, OutgoingRequestConfig,
};
use wasmtime_wasi_http::{HttpError, WasiHttpCtx, WasiHttpView};

use crate::types::{ApprovalStatus, ApproveResponse, CredentialEntry, RuleAction, ServiceConfig};

use crate::credential_store::CredentialStore;
use crate::hub_client::ApprovalChecker;

/// Abstraction over credential lookup for testability.
trait CredentialLookup {
    fn get_credential(
        &self,
        profile_id: &str,
        domain: &str,
    ) -> anyhow::Result<Option<CredentialEntry>>;
}

impl CredentialLookup for CredentialStore {
    fn get_credential(
        &self,
        profile_id: &str,
        domain: &str,
    ) -> anyhow::Result<Option<CredentialEntry>> {
        self.get(profile_id, domain)
    }
}

/// Header to inject into an outbound request.
#[derive(Debug, Clone, PartialEq)]
struct CredentialHeader {
    name: String,
    value: String,
}

static GUEST_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/boa_wasm_guest.wasm"));

/// Per-invocation state held in the wasmtime Store.
struct GuestState {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    /// Profile ID for credential lookup.
    profile_id: Option<String>,
    /// Allowed services for this profile.
    services: Vec<ServiceConfig>,
    /// Local credential store.
    credential_store: Option<Arc<CredentialStore>>,
    /// Hub client for approval checks.
    hub_client: Arc<dyn ApprovalChecker>,
    /// Optional context describing why the code is being executed.
    context: Option<String>,
    /// Flag to pause the execution timeout while waiting for user approval.
    approval_pending: Arc<AtomicBool>,
}

impl WasiView for GuestState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// Decision returned by `authorize_request`.
#[derive(Debug, PartialEq)]
enum RequestDecision {
    Allow,
    Deny(String),
    NeedsApproval(String),
}

/// Pure decision logic for whether an outbound request should proceed.
///
/// `hub_result`: `Err` = hub unreachable or not available, `Ok` = hub responded.
fn authorize_request(
    services: &[ServiceConfig],
    domain: &str,
    method: &str,
    path: &str,
    hub_result: Result<&ApproveResponse, &str>,
) -> RequestDecision {
    if !services.iter().any(|s| s.domain == domain) {
        return RequestDecision::Deny(format!("domain not allowed: {domain}"));
    }

    match hub_result {
        Err(e) => RequestDecision::Deny(format!("hub unreachable, request denied: {e}")),
        Ok(resp) => match resp.action {
            RuleAction::Allow => RequestDecision::Allow,
            RuleAction::Deny => {
                RequestDecision::Deny(format!("request denied by approval rule: {method} {path}"))
            }
            RuleAction::RequireApproval => {
                let id = resp.approval_id.clone().unwrap_or_default();
                RequestDecision::NeedsApproval(id)
            }
        },
    }
}

/// Evaluate the result of polling for an approval decision.
fn check_poll_result(
    result: &Result<ApprovalStatus, String>,
    method: &str,
    path: &str,
) -> Result<(), String> {
    match result {
        Ok(ApprovalStatus::Approved) => Ok(()),
        Ok(status) => Err(format!("request {status}: {method} {path}")),
        Err(e) => Err(format!("approval poll error: {e}")),
    }
}

/// Call the hub to check if a request is approved.
/// Spawns a dedicated thread to avoid deadlocking the tokio runtime.
fn fetch_hub_approval(
    hub_client: &Arc<dyn ApprovalChecker>,
    profile_id: &str,
    domain: &str,
    method: &str,
    path: &str,
    context: Option<&str>,
) -> Result<ApproveResponse, String> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| "no async runtime available for approval check".to_string())?;
    let client = hub_client.clone();
    let pid = profile_id.to_string();
    let domain = domain.to_string();
    let method = method.to_string();
    let path = path.to_string();
    let context = context.map(String::from);

    std::thread::spawn(move || {
        handle.block_on(async {
            client
                .check_approval(&pid, &domain, &method, &path, context.as_deref())
                .await
        })
    })
    .join()
    .map_err(|_| "approval check thread panicked".to_string())?
    .map_err(|e| e.to_string())
}

/// Poll the hub for an approval decision.
/// Spawns a dedicated thread to avoid deadlocking the tokio runtime.
fn poll_hub_approval(
    hub_client: &Arc<dyn ApprovalChecker>,
    profile_id: &str,
    approval_id: &str,
) -> Result<ApprovalStatus, String> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| "no async runtime available for approval poll".to_string())?;
    let client = hub_client.clone();
    let pid = profile_id.to_string();
    let aid = approval_id.to_string();
    let timeout = Duration::from_secs(120);

    std::thread::spawn(move || {
        handle.block_on(async { client.poll_approval(&pid, &aid, timeout).await })
    })
    .join()
    .map_err(|_| "approval poll thread panicked".to_string())?
    .map_err(|e| e.to_string())
}

/// Enforce a request decision. Returns `Err(message)` if the request should be blocked.
///
/// For `NeedsApproval`, the caller must provide the poll result from [`poll_hub_approval`].
fn enforce_decision(
    decision: RequestDecision,
    poll_result: Option<&Result<ApprovalStatus, String>>,
    method: &str,
    path: &str,
) -> Result<(), String> {
    match decision {
        RequestDecision::Allow => Ok(()),
        RequestDecision::Deny(msg) => Err(msg),
        RequestDecision::NeedsApproval(_) => match poll_result {
            Some(result) => check_poll_result(result, method, path),
            None => Err(format!(
                "approval required but no poll result: {method} {path}"
            )),
        },
    }
}

/// Authorize an outbound request and resolve the credential header to inject.
///
/// All I/O is provided via parameters: `hub_response` from the hub check,
/// `poll_fn` for approval polling, and `creds` for credential lookup.
/// Returns `Ok(Some(header))` if a credential should be injected,
/// `Ok(None)` if allowed but no credential found, or `Err(msg)` to deny.
fn process_outbound_request(
    services: &[ServiceConfig],
    profile_id: &str,
    domain: &str,
    method: &str,
    path: &str,
    hub_response: Result<ApproveResponse, String>,
    poll_fn: impl FnOnce(&str) -> Result<ApprovalStatus, String>,
    creds: &dyn CredentialLookup,
) -> Result<Option<CredentialHeader>, String> {
    let decision = authorize_request(
        services,
        domain,
        method,
        path,
        hub_response.as_ref().map_err(|e| e.as_str()),
    );

    if hub_response.is_err() {
        if let RequestDecision::Deny(ref msg) = decision {
            tracing::warn!("{msg}");
        }
    }

    let poll_result = if let RequestDecision::NeedsApproval(ref approval_id) = decision {
        Some(poll_fn(approval_id))
    } else {
        None
    };

    enforce_decision(decision, poll_result.as_ref(), method, path)?;

    let service = services.iter().find(|s| s.domain == domain);
    match service {
        Some(svc) => match creds.get_credential(profile_id, domain) {
            Ok(Some(cred)) => Ok(Some(CredentialHeader {
                name: svc.header_name.clone(),
                value: cred.header_value,
            })),
            Ok(None) => Ok(None),
            Err(e) => {
                tracing::warn!(domain = %domain, error = %e, "credential lookup failed");
                Ok(None)
            }
        },
        None => Ok(None),
    }
}

impl WasiHttpView for GuestState {
    fn ctx(&mut self) -> &mut WasiHttpCtx {
        &mut self.http
    }
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }

    fn send_request(
        &mut self,
        mut request: hyper::Request<HyperOutgoingBody>,
        config: OutgoingRequestConfig,
    ) -> wasmtime_wasi_http::HttpResult<HostFutureIncomingResponse> {
        let (profile_id, store) = match (&self.profile_id, &self.credential_store) {
            (Some(p), Some(s)) => (p, s),
            _ => {
                return Err(HttpError::trap(wasmtime::Error::msg(
                    "request denied: no profile or credential store configured",
                )));
            }
        };

        let target_domain = request.uri().host().unwrap_or("").to_string();
        let req_method = request.method().to_string();
        let req_path = request.uri().path().to_string();

        let hub_response = fetch_hub_approval(
            &self.hub_client,
            profile_id,
            &target_domain,
            &req_method,
            &req_path,
            self.context.as_deref(),
        );

        let hub_client = self.hub_client.clone();
        let pid = profile_id.clone();
        let approval_flag = self.approval_pending.clone();

        let header = process_outbound_request(
            &self.services,
            profile_id,
            &target_domain,
            &req_method,
            &req_path,
            hub_response,
            |approval_id| {
                approval_flag.store(true, Ordering::Relaxed);
                let result = poll_hub_approval(&hub_client, &pid, approval_id);
                approval_flag.store(false, Ordering::Relaxed);
                result
            },
            store.as_ref(),
        )
        .map_err(|msg| HttpError::trap(wasmtime::Error::msg(msg)))?;

        if let Some(h) = header {
            if let Ok(name) = http::header::HeaderName::from_bytes(h.name.as_bytes()) {
                if let Ok(val) = http::header::HeaderValue::from_str(&h.value) {
                    request.headers_mut().insert(name, val);
                }
            }
        }

        Ok(default_send_request(request, config))
    }
}

/// Result of a JS execution.
#[derive(Debug, Clone)]
pub struct JsResult {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

/// Pre-compiled WASM engine for running JS code.
#[derive(Clone)]
pub struct JsEngine {
    engine: Engine,
    component: Arc<Component>,
    linker: Arc<Linker<GuestState>>,
}

impl JsEngine {
    pub fn new() -> anyhow::Result<Self> {
        let mut config = wasmtime::Config::new();
        config.epoch_interruption(true);
        config.wasm_component_model(true);

        let engine = Engine::new(&config)?;
        let component = Component::new(&engine, GUEST_WASM)?;

        let mut linker: Linker<GuestState> = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        wasmtime_wasi_http::add_only_http_to_linker_async(&mut linker)?;

        Ok(Self {
            engine,
            component: Arc::new(component),
            linker: Arc::new(linker),
        })
    }

    /// Execute JavaScript code in a fresh WASM sandbox.
    pub async fn execute(
        &self,
        code: &str,
        timeout: Duration,
        profile_id: Option<String>,
        services: Vec<ServiceConfig>,
        credential_store: Option<Arc<CredentialStore>>,
        hub_client: Arc<dyn ApprovalChecker>,
        context: Option<String>,
    ) -> anyhow::Result<JsResult> {
        let stdin_pipe = MemoryInputPipe::new(code.to_string().into_bytes());
        let stdout_pipe = MemoryOutputPipe::new(4 * 1024 * 1024);
        let stderr_pipe = MemoryOutputPipe::new(1024 * 1024);

        let mut wasi_builder = WasiCtxBuilder::new();
        wasi_builder.stdin(stdin_pipe);
        wasi_builder.stdout(stdout_pipe.clone());
        wasi_builder.stderr(stderr_pipe.clone());

        let approval_pending = Arc::new(AtomicBool::new(false));

        let state = GuestState {
            wasi: wasi_builder.build(),
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            profile_id,
            services,
            credential_store,
            hub_client,
            context,
            approval_pending: approval_pending.clone(),
        };

        let mut store = Store::new(&self.engine, state);
        store.set_epoch_deadline(1);

        // Spawn epoch incrementer for timeout.
        // The clock pauses while approval_pending is true so that user
        // approval time does not count against the execution timeout.
        let engine_clone = self.engine.clone();
        let timeout_handle = tokio::task::spawn_blocking(move || {
            let mut elapsed = Duration::ZERO;
            let mut last_tick = Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let now = Instant::now();
                if !approval_pending.load(Ordering::Relaxed) {
                    elapsed += now - last_tick;
                }
                last_tick = now;
                if elapsed >= timeout {
                    engine_clone.increment_epoch();
                    break;
                }
            }
        });

        use wasmtime_wasi::p2::bindings::Command;

        let command = Command::instantiate_async(&mut store, &self.component, &self.linker).await?;
        let run_result = command.wasi_cli_run().call_run(&mut store).await;

        let stdout = String::from_utf8_lossy(&stdout_pipe.contents()).to_string();
        let stderr = String::from_utf8_lossy(&stderr_pipe.contents()).to_string();

        timeout_handle.abort();

        match run_result {
            Ok(Ok(())) => Ok(JsResult {
                stdout,
                stderr: stderr.clone(),
                success: stderr.is_empty(),
            }),
            Ok(Err(())) => Ok(JsResult {
                stdout,
                stderr: stderr.clone(),
                success: false,
            }),
            Err(e) => {
                let err_str = format!("{e:#}");
                if err_str.contains("epoch") {
                    Ok(JsResult {
                        stdout,
                        stderr: format!("{stderr}\nExecution timed out after {timeout:?}"),
                        success: false,
                    })
                } else {
                    Ok(JsResult {
                        stdout,
                        stderr: if stderr.is_empty() { err_str } else { stderr },
                        success: false,
                    })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        ApprovalStatus, ApproveResponse, CredentialEntry, RuleAction, ServiceConfig,
    };

    fn test_services() -> Vec<ServiceConfig> {
        vec![ServiceConfig {
            domain: "api.example.com".to_string(),
            catalog_id: None,
            header_name: "Authorization".to_string(),
        }]
    }

    fn approve_response(action: RuleAction, approval_id: Option<&str>) -> ApproveResponse {
        ApproveResponse {
            action,
            approval_id: approval_id.map(String::from),
            expires_at: None,
        }
    }

    struct MockCredentials {
        entries: Vec<CredentialEntry>,
    }

    impl MockCredentials {
        fn empty() -> Self {
            Self { entries: vec![] }
        }

        fn with(entry: CredentialEntry) -> Self {
            Self {
                entries: vec![entry],
            }
        }
    }

    impl CredentialLookup for MockCredentials {
        fn get_credential(
            &self,
            profile_id: &str,
            domain: &str,
        ) -> anyhow::Result<Option<CredentialEntry>> {
            Ok(self
                .entries
                .iter()
                .find(|e| e.profile_id == profile_id && e.domain == domain)
                .cloned())
        }
    }

    fn test_credential() -> CredentialEntry {
        CredentialEntry {
            profile_id: "prof1".to_string(),
            domain: "api.example.com".to_string(),
            header_name: "Authorization".to_string(),
            header_value: "Bearer tok123".to_string(),
        }
    }

    fn no_poll(_: &str) -> Result<ApprovalStatus, String> {
        panic!("poll_fn should not be called")
    }

    // -- authorize_request tests --

    #[test]
    fn authorize_domain_not_in_services() {
        let result = authorize_request(
            &test_services(),
            "unknown.com",
            "GET",
            "/foo",
            Ok(&approve_response(RuleAction::Allow, None)),
        );
        assert_eq!(
            result,
            RequestDecision::Deny("domain not allowed: unknown.com".to_string())
        );
    }

    #[test]
    fn authorize_hub_unreachable() {
        let result = authorize_request(
            &test_services(),
            "api.example.com",
            "GET",
            "/foo",
            Err("connection refused"),
        );
        assert_eq!(
            result,
            RequestDecision::Deny(
                "hub unreachable, request denied: connection refused".to_string()
            )
        );
    }

    #[test]
    fn authorize_hub_allows() {
        let resp = approve_response(RuleAction::Allow, None);
        let result = authorize_request(
            &test_services(),
            "api.example.com",
            "GET",
            "/foo",
            Ok(&resp),
        );
        assert_eq!(result, RequestDecision::Allow);
    }

    #[test]
    fn authorize_hub_denies() {
        let resp = approve_response(RuleAction::Deny, None);
        let result = authorize_request(
            &test_services(),
            "api.example.com",
            "POST",
            "/bar",
            Ok(&resp),
        );
        assert_eq!(
            result,
            RequestDecision::Deny("request denied by approval rule: POST /bar".to_string())
        );
    }

    #[test]
    fn authorize_hub_requires_approval() {
        let resp = approve_response(RuleAction::RequireApproval, Some("abc-123"));
        let result = authorize_request(
            &test_services(),
            "api.example.com",
            "DELETE",
            "/resource",
            Ok(&resp),
        );
        assert_eq!(
            result,
            RequestDecision::NeedsApproval("abc-123".to_string())
        );
    }

    // -- check_poll_result tests --

    #[test]
    fn poll_approved() {
        let result = check_poll_result(&Ok(ApprovalStatus::Approved), "GET", "/foo");
        assert!(result.is_ok());
    }

    #[test]
    fn poll_denied() {
        let result = check_poll_result(&Ok(ApprovalStatus::Denied), "GET", "/foo");
        assert_eq!(result, Err("request denied: GET /foo".to_string()));
    }

    #[test]
    fn poll_expired() {
        let result = check_poll_result(&Ok(ApprovalStatus::Expired), "POST", "/bar");
        assert_eq!(result, Err("request expired: POST /bar".to_string()));
    }

    #[test]
    fn poll_error() {
        let result = check_poll_result(&Err("connection reset".to_string()), "GET", "/foo");
        assert_eq!(
            result,
            Err("approval poll error: connection reset".to_string())
        );
    }

    // -- enforce_decision tests --

    #[test]
    fn enforce_allow() {
        let result = enforce_decision(RequestDecision::Allow, None, "GET", "/foo");
        assert!(result.is_ok());
    }

    #[test]
    fn enforce_deny() {
        let decision = RequestDecision::Deny("not allowed".to_string());
        let result = enforce_decision(decision, None, "GET", "/foo");
        assert_eq!(result, Err("not allowed".to_string()));
    }

    #[test]
    fn enforce_needs_approval_approved() {
        let decision = RequestDecision::NeedsApproval("abc".to_string());
        let poll = Ok(ApprovalStatus::Approved);
        let result = enforce_decision(decision, Some(&poll), "GET", "/foo");
        assert!(result.is_ok());
    }

    #[test]
    fn enforce_needs_approval_denied() {
        let decision = RequestDecision::NeedsApproval("abc".to_string());
        let poll = Ok(ApprovalStatus::Denied);
        let result = enforce_decision(decision, Some(&poll), "POST", "/bar");
        assert_eq!(result, Err("request denied: POST /bar".to_string()));
    }

    #[test]
    fn enforce_needs_approval_expired() {
        let decision = RequestDecision::NeedsApproval("abc".to_string());
        let poll = Ok(ApprovalStatus::Expired);
        let result = enforce_decision(decision, Some(&poll), "DELETE", "/x");
        assert_eq!(result, Err("request expired: DELETE /x".to_string()));
    }

    #[test]
    fn enforce_needs_approval_poll_error() {
        let decision = RequestDecision::NeedsApproval("abc".to_string());
        let poll = Err("timeout".to_string());
        let result = enforce_decision(decision, Some(&poll), "GET", "/foo");
        assert_eq!(result, Err("approval poll error: timeout".to_string()));
    }

    #[test]
    fn enforce_needs_approval_no_poll_result() {
        let decision = RequestDecision::NeedsApproval("abc".to_string());
        let result = enforce_decision(decision, None, "GET", "/foo");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("approval required"));
    }

    // -- process_outbound_request tests --

    #[test]
    fn process_denies_unknown_domain() {
        let creds = MockCredentials::empty();
        let result = process_outbound_request(
            &test_services(),
            "prof1",
            "unknown.com",
            "GET",
            "/",
            Ok(approve_response(RuleAction::Allow, None)),
            no_poll,
            &creds,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("domain not allowed"));
    }

    #[test]
    fn process_denies_hub_unreachable() {
        let creds = MockCredentials::empty();
        let result = process_outbound_request(
            &test_services(),
            "prof1",
            "api.example.com",
            "GET",
            "/",
            Err("connection refused".to_string()),
            no_poll,
            &creds,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("hub unreachable"));
    }

    #[test]
    fn process_denies_by_rule() {
        let creds = MockCredentials::empty();
        let result = process_outbound_request(
            &test_services(),
            "prof1",
            "api.example.com",
            "POST",
            "/bar",
            Ok(approve_response(RuleAction::Deny, None)),
            no_poll,
            &creds,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("denied by approval rule"));
    }

    #[test]
    fn process_allows_and_injects_credential() {
        let creds = MockCredentials::with(test_credential());
        let result = process_outbound_request(
            &test_services(),
            "prof1",
            "api.example.com",
            "GET",
            "/foo",
            Ok(approve_response(RuleAction::Allow, None)),
            no_poll,
            &creds,
        );
        assert_eq!(
            result,
            Ok(Some(CredentialHeader {
                name: "Authorization".to_string(),
                value: "Bearer tok123".to_string(),
            }))
        );
    }

    #[test]
    fn process_allows_no_credential() {
        let creds = MockCredentials::empty();
        let result = process_outbound_request(
            &test_services(),
            "prof1",
            "api.example.com",
            "GET",
            "/foo",
            Ok(approve_response(RuleAction::Allow, None)),
            no_poll,
            &creds,
        );
        assert_eq!(result, Ok(None));
    }

    #[test]
    fn process_approval_flow_approved() {
        let creds = MockCredentials::empty();
        let result = process_outbound_request(
            &test_services(),
            "prof1",
            "api.example.com",
            "DELETE",
            "/resource",
            Ok(approve_response(RuleAction::RequireApproval, Some("req-1"))),
            |_| Ok(ApprovalStatus::Approved),
            &creds,
        );
        assert_eq!(result, Ok(None));
    }

    #[test]
    fn process_approval_flow_denied() {
        let creds = MockCredentials::empty();
        let result = process_outbound_request(
            &test_services(),
            "prof1",
            "api.example.com",
            "DELETE",
            "/resource",
            Ok(approve_response(RuleAction::RequireApproval, Some("req-1"))),
            |_| Ok(ApprovalStatus::Denied),
            &creds,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("denied"));
    }

    #[test]
    fn process_approval_flow_poll_error() {
        let creds = MockCredentials::empty();
        let result = process_outbound_request(
            &test_services(),
            "prof1",
            "api.example.com",
            "DELETE",
            "/resource",
            Ok(approve_response(RuleAction::RequireApproval, Some("req-1"))),
            |_| Err("network error".to_string()),
            &creds,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("network error"));
    }

    #[test]
    fn process_approval_passes_correct_id_to_poll() {
        let creds = MockCredentials::empty();
        let result = process_outbound_request(
            &test_services(),
            "prof1",
            "api.example.com",
            "POST",
            "/data",
            Ok(approve_response(
                RuleAction::RequireApproval,
                Some("my-approval-id"),
            )),
            |id| {
                assert_eq!(id, "my-approval-id");
                Ok(ApprovalStatus::Approved)
            },
            &creds,
        );
        assert!(result.is_ok());
    }
}
