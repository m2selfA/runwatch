use crate::codex;
use anyhow::{Context, Result as AnyResult, bail};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars,
    service::{RequestContext, RoleServer},
    task_manager::{TaskExit, TaskManager, TaskOptions},
    tool, tool_router,
};
use runwatch_core::{
    ContinuationBinding, RemoteWorkspaceRef, RunRecord, RunResources, RunnerKind, SubmitRunSpec,
};
use runwatch_ssh::parse_ssh_config;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MAX_SYNC_WAIT_SEC: u64 = 60;
const MAX_TASK_WAIT_SEC: u64 = 86_400;
const TASK_OBSERVATION_GRACE_SEC: u64 = 300;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EmptyArgs {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RunIdArgs {
    run_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WaitArgs {
    run_id: String,
    #[serde(default = "default_wait_timeout")]
    timeout_sec: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct LogsArgs {
    run_id: String,
    #[serde(default = "default_log_tail")]
    tail: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SubmitArgs {
    run_id: String,
    host: String,
    #[serde(default)]
    job_id: Option<String>,
    #[serde(default = "default_runner")]
    runner: String,
    #[serde(default)]
    terminal: Option<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
struct SubmitScienceArgs {
    run_id: String,
    #[serde(default)]
    name: Option<String>,
    host: String,
    workdir: String,
    #[serde(default = "default_runner")]
    runner: String,
    command: String,
    #[serde(default)]
    time: Option<String>,
    #[serde(default)]
    partition: Option<String>,
    #[serde(default)]
    queue: Option<String>,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    cpus: Option<u32>,
    #[serde(default)]
    mem: Option<String>,
    #[serde(default)]
    gpus: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ToolErrorView {
    error: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum RunStatusView {
    Submitting,
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum RunnerKindView {
    Slurm,
    Lsf,
    Process,
    File,
    Powershell,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WorkspaceView {
    host_alias: String,
    cwd: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct RunView {
    run_id: String,
    name: Option<String>,
    host: String,
    job_id: Option<String>,
    runner: RunnerKindView,
    remote_terminal: Option<String>,
    status: RunStatusView,
    workspace: Option<WorkspaceView>,
    attempt_no: Option<u32>,
    session_id: Option<String>,
    project_root: Option<String>,
    agent: Option<String>,
    updated_at: String,
    note: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct HostView {
    alias: String,
    hostname: String,
    user: Option<String>,
    port: u16,
    proxy_jump: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct TransitionView {
    run_id: String,
    from: RunStatusView,
    to: RunStatusView,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct TickView {
    transitions: Vec<TransitionView>,
    errors: Vec<String>,
    count: usize,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct RunLogsView {
    run_id: String,
    status: RunStatusView,
    attempt_no: u32,
    stdout: String,
    stderr: String,
    tail_lines: usize,
    byte_limit_per_stream: usize,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct LogsEnvelopeView {
    logs: RunLogsView,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ArtifactView {
    kind: String,
    path: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct RunArtifactsView {
    run_id: String,
    status: RunStatusView,
    attempt_no: u32,
    artifacts: Vec<ArtifactView>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ArtifactsEnvelopeView {
    artifacts: RunArtifactsView,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct CancelEnvelopeView {
    cancel_requested: bool,
    run: RunView,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
enum RunOutputView {
    Success(RunView),
    Error(ToolErrorView),
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
enum RunListOutputView {
    Success(Vec<RunView>),
    Error(ToolErrorView),
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
enum HostListOutputView {
    Success(Vec<HostView>),
    Error(ToolErrorView),
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
enum TickOutputView {
    Success(TickView),
    Error(ToolErrorView),
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
enum LogsOutputView {
    Success(LogsEnvelopeView),
    Error(ToolErrorView),
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
enum ArtifactsOutputView {
    Success(ArtifactsEnvelopeView),
    Error(ToolErrorView),
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
enum CancelOutputView {
    Success(CancelEnvelopeView),
    Error(ToolErrorView),
}

fn default_wait_timeout() -> u64 {
    30
}

fn default_log_tail() -> u64 {
    80
}

fn default_runner() -> String {
    "slurm".into()
}

fn runner_from_name(value: &str) -> AnyResult<RunnerKind> {
    match value.to_ascii_lowercase().as_str() {
        "slurm" => Ok(RunnerKind::Slurm),
        "lsf" => Ok(RunnerKind::Lsf),
        "process" => Ok(RunnerKind::Process),
        other => bail!("durable submission does not support runner {other}"),
    }
}

fn thread_id_from_meta_value(meta: &Value) -> AnyResult<String> {
    let thread_id = meta
        .get("threadId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .context("Codex-bound submission requires MCP _meta.threadId")?;
    if thread_id.len() > 128 {
        bail!("MCP _meta.threadId exceeds 128 bytes");
    }
    Ok(thread_id.to_string())
}

fn build_codex_submit_spec(
    args: SubmitScienceArgs,
    thread_id: &str,
    session: codex::CodexSessionMeta,
) -> AnyResult<SubmitRunSpec> {
    if session.thread_id != thread_id {
        bail!("Codex session metadata thread id does not match MCP thread id");
    }
    let runner = runner_from_name(&args.runner)?;
    let workspace = RemoteWorkspaceRef {
        host_alias: if runner == RunnerKind::Process {
            "local".into()
        } else {
            args.host
        },
        cwd: args.workdir,
    };
    let continuation = ContinuationBinding {
        agent_kind: "codex".into(),
        session_id: thread_id.to_string(),
        session_file: Some(session.session_file),
        origin_leaf_id: None,
        project_root: session.cwd,
        workspace: workspace.clone(),
        adapter_path: None,
    };
    Ok(SubmitRunSpec {
        run_id: args.run_id,
        name: args.name,
        workspace,
        runner,
        command: args.command,
        resources: RunResources {
            time: args.time,
            partition: args.partition,
            queue: args.queue,
            account: args.account,
            cpus: args.cpus,
            mem: args.mem,
            gpus: args.gpus,
        },
        continuation: Some(continuation),
    })
}

async fn submit_science_run_for_codex(
    args: SubmitScienceArgs,
    thread_id: String,
) -> CallToolResult {
    let locator_id = thread_id.clone();
    let session =
        match tokio::task::spawn_blocking(move || codex::locate_codex_session(&locator_id)).await {
            Ok(Ok(session)) => session,
            Ok(Err(error)) => {
                return structured_error(format!("Codex session binding failed: {error:#}"));
            }
            Err(error) => {
                return structured_error(format!("Codex session binding task failed: {error}"));
            }
        };
    let spec = match build_codex_submit_spec(args, &thread_id, session) {
        Ok(spec) => spec,
        Err(error) => {
            return structured_error(format!("Codex durable submission rejected: {error:#}"));
        }
    };
    match runwatch_engine::ipc::call_local("submit_run_v2", json!({ "spec": spec })).await {
        Ok(value) => structured(value.get("run").cloned().unwrap_or(Value::Null)),
        Err(error) => daemon_error(error),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitDispatch {
    Sync(u64),
    Task(u64),
    TasksRequired(u64),
}

fn classify_wait(timeout_sec: u64, client_supports_tasks: bool) -> WaitDispatch {
    let timeout = timeout_sec.min(MAX_TASK_WAIT_SEC);
    if timeout <= MAX_SYNC_WAIT_SEC {
        WaitDispatch::Sync(timeout)
    } else if client_supports_tasks {
        WaitDispatch::Task(timeout)
    } else {
        WaitDispatch::TasksRequired(timeout)
    }
}

fn structured(value: Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    let mut result = CallToolResult::structured(value);
    result.content = vec![ContentBlock::text(text)];
    result
}

fn structured_error(message: impl Into<String>) -> CallToolResult {
    let value = json!({ "error": message.into() });
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    let mut result = CallToolResult::structured_error(value);
    result.content = vec![ContentBlock::text(text)];
    result
}

fn daemon_error(error: anyhow::Error) -> CallToolResult {
    structured_error(format!("{error:#}"))
}

async fn wait_snapshot(run_id: &str, timeout_sec: u64) -> CallToolResult {
    match runwatch_engine::ipc::call_local(
        "wait_run",
        json!({ "run_id": run_id, "timeout_sec": timeout_sec }),
    )
    .await
    {
        Ok(value) => match value.get("run") {
            Some(run) if !run.is_null() => structured(run.clone()),
            _ => structured_error(format!("unknown run {run_id}")),
        },
        Err(error) => daemon_error(error),
    }
}

#[derive(Clone)]
pub struct RunwatchMcp {
    tool_router: ToolRouter<RunwatchMcp>,
    tasks: TaskManager,
}

impl Default for RunwatchMcp {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl RunwatchMcp {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            tasks: TaskManager::new(),
        }
    }

    #[tool(
        description = "List Runs from runwatchd's canonical durable state",
        output_schema = rmcp::handler::server::tool::schema_for_output::<RunListOutputView>(),
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_runs(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(
            match runwatch_engine::ipc::call_local("list_runs", json!({})).await {
                Ok(value) => structured(value.get("runs").cloned().unwrap_or_else(|| json!([]))),
                Err(error) => daemon_error(error),
            },
        )
    }

    #[tool(
        description = "Get one durable Run by id",
        output_schema = rmcp::handler::server::tool::schema_for_output::<RunOutputView>(),
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_run(
        &self,
        Parameters(RunIdArgs { run_id }): Parameters<RunIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(
            match runwatch_engine::ipc::call_local("get_run", json!({ "run_id": run_id })).await {
                Ok(value) => match value.get("run") {
                    Some(run) if !run.is_null() => structured(run.clone()),
                    _ => structured_error("unknown run"),
                },
                Err(error) => daemon_error(error),
            },
        )
    }

    #[tool(
        description = "List SSH Host aliases resolved from ~/.ssh/config",
        output_schema = rmcp::handler::server::tool::schema_for_output::<HostListOutputView>(),
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn list_hosts(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match parse_ssh_config() {
            Ok(hosts) => structured(Value::Array(
                hosts
                    .into_iter()
                    .map(|host| {
                        json!({
                            "alias": host.alias,
                            "hostname": host.hostname,
                            "user": host.user,
                            "port": host.port,
                            "proxy_jump": host.proxy_jump,
                        })
                    })
                    .collect(),
            )),
            Err(error) => daemon_error(error),
        })
    }

    #[tool(
        description = "Adopt/register an existing scheduler Run through runwatchd; durable new scheduler submission uses native agent integrations",
        output_schema = rmcp::handler::server::tool::schema_for_output::<RunOutputView>(),
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = false)
    )]
    async fn submit_run(
        &self,
        Parameters(args): Parameters<SubmitArgs>,
    ) -> Result<CallToolResult, McpError> {
        let runner = match args.runner.as_str() {
            "slurm" => RunnerKind::Slurm,
            "lsf" => RunnerKind::Lsf,
            "process" => RunnerKind::Process,
            "file" => RunnerKind::File,
            "powershell" => RunnerKind::Powershell,
            other => return Ok(structured_error(format!("unknown runner {other}"))),
        };
        let mut run = RunRecord::new(args.run_id, args.host, runner);
        run.job_id = args.job_id;
        run.remote_terminal = args.terminal;
        Ok(
            match runwatch_engine::ipc::call_local("adopt_run_v1", json!({ "run": run })).await {
                Ok(value) => structured(value.get("run").cloned().unwrap_or(Value::Null)),
                Err(error) => daemon_error(error),
            },
        )
    }

    #[tool(
        description = "Submit a new durable scientific Run and bind terminal continuation to the current Codex thread. Thread identity comes only from MCP _meta.threadId and is cross-checked against Codex session_meta; do not use this tool to adopt an already-submitted scheduler job.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<RunOutputView>(),
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = true)
    )]
    async fn submit_science_run(
        &self,
        Parameters(_args): Parameters<SubmitScienceArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(structured_error(
            "submit_science_run requires MCP request metadata and must be invoked through the server handler",
        ))
    }

    #[tool(
        description = "Ask runwatchd to poll every non-terminal Run once",
        output_schema = rmcp::handler::server::tool::schema_for_output::<TickOutputView>(),
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = true)
    )]
    async fn tick(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(
            match runwatch_engine::ipc::call_local("tick", json!({})).await {
                Ok(value) => structured(json!({
                    "transitions": value.get("transitions").cloned().unwrap_or_else(|| json!([])),
                    "errors": value.get("errors").cloned().unwrap_or_else(|| json!([])),
                    "count": value.get("runs").and_then(Value::as_array).map_or(0, Vec::len),
                })),
                Err(error) => daemon_error(error),
            },
        )
    }

    #[tool(
        description = "Wait for runwatchd's canonical Run snapshot. Waits up to 60 seconds synchronously; longer waits require the MCP Tasks extension. Long scientific jobs should normally use durable continuation instead of waiting.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<RunOutputView>(),
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn wait_run(
        &self,
        Parameters(WaitArgs {
            run_id,
            timeout_sec,
        }): Parameters<WaitArgs>,
    ) -> Result<CallToolResult, McpError> {
        match classify_wait(timeout_sec, false) {
            WaitDispatch::Sync(timeout) => Ok(wait_snapshot(&run_id, timeout).await),
            WaitDispatch::Task(_) | WaitDispatch::TasksRequired(_) => Ok(structured_error(
                "long wait requires the MCP Tasks extension; use timeout_sec <= 60 or durable continuation",
            )),
        }
    }

    #[tool(
        description = "Read bounded stdout/stderr tails through runwatchd",
        output_schema = rmcp::handler::server::tool::schema_for_output::<LogsOutputView>(),
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn logs(
        &self,
        Parameters(LogsArgs { run_id, tail }): Parameters<LogsArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(
            match runwatch_engine::ipc::call_local(
                "logs",
                json!({ "run_id": run_id, "tail": tail.min(500) }),
            )
            .await
            {
                Ok(value) => structured(value),
                Err(error) => daemon_error(error),
            },
        )
    }

    #[tool(
        description = "List runwatch-known lifecycle artifacts for a Run",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ArtifactsOutputView>(),
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn artifacts(
        &self,
        Parameters(RunIdArgs { run_id }): Parameters<RunIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(
            match runwatch_engine::ipc::call_local("artifacts", json!({ "run_id": run_id })).await {
                Ok(value) => structured(value),
                Err(error) => daemon_error(error),
            },
        )
    }

    #[tool(
        description = "Request scheduler cancellation through runwatchd",
        output_schema = rmcp::handler::server::tool::schema_for_output::<CancelOutputView>(),
        annotations(read_only_hint = false, destructive_hint = true, open_world_hint = true)
    )]
    async fn cancel_run(
        &self,
        Parameters(RunIdArgs { run_id }): Parameters<RunIdArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(
            match runwatch_engine::ipc::call_local("cancel_run", json!({ "run_id": run_id })).await
            {
                Ok(value) => structured(value),
                Err(error) => daemon_error(error),
            },
        )
    }
}

impl ServerHandler for RunwatchMcp {
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if request.name == "submit_science_run" {
            let args: SubmitScienceArgs = serde_json::from_value(Value::Object(
                request.arguments.clone().unwrap_or_default(),
            ))
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
            // rmcp moves request _meta into RequestContext during negotiated dispatch before
            // ServerHandler::call_tool runs. Read identity from that context, not from
            // CallToolRequestParams (whose meta is intentionally taken/cleared by the SDK).
            let meta = serde_json::to_value(&context.meta)
                .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
            let thread_id = match thread_id_from_meta_value(&meta) {
                Ok(thread_id) => thread_id,
                Err(error) => {
                    return Ok(CallToolResponse::Complete(structured_error(format!(
                        "Codex thread binding unavailable: {error:#}"
                    ))));
                }
            };
            return Ok(CallToolResponse::Complete(
                submit_science_run_for_codex(args, thread_id).await,
            ));
        }

        if request.name == "wait_run" {
            let params: WaitArgs = serde_json::from_value(Value::Object(
                request.arguments.clone().unwrap_or_default(),
            ))
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
            let client_supports_tasks = context
                .client_capabilities()
                .is_some_and(|capabilities| capabilities.supports_tasks());
            match classify_wait(params.timeout_sec, client_supports_tasks) {
                WaitDispatch::Task(timeout) => {
                    let run_id = params.run_id;
                    let ttl_ms = timeout
                        .saturating_add(TASK_OBSERVATION_GRACE_SEC)
                        .saturating_mul(1000);
                    let status_message = format!("Waiting for Run {run_id}");
                    let task = self.tasks.spawn(
                        TaskOptions::default()
                            .with_ttl_ms(Some(ttl_ms))
                            .with_poll_interval_ms(1000)
                            .with_status_message(status_message),
                        move |task_context| {
                            Box::pin(async move {
                                tokio::select! {
                                    _ = task_context.cancelled() => Err(TaskExit::Cancelled),
                                    result = wait_snapshot(&run_id, timeout) => Ok(result),
                                }
                            })
                        },
                    );
                    return Ok(CallToolResponse::Task(CreateTaskResult::new(task)));
                }
                WaitDispatch::TasksRequired(timeout) => {
                    return Ok(CallToolResponse::Complete(structured_error(format!(
                        "wait_run timeout_sec={timeout} requires the MCP Tasks extension; use timeout_sec <= {MAX_SYNC_WAIT_SEC} or durable continuation"
                    ))));
                }
                WaitDispatch::Sync(_) => {}
            }
        }

        let tool_context =
            rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(tool_context).await
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.tool_router.list_all()))
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        Ok(GetTaskResult::new(self.tasks.get_task(&request.task_id)?))
    }

    async fn update_task(
        &self,
        request: UpdateTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.tasks
            .update_task(&request.task_id, request.input_responses)
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.tasks.cancel_task(&request.task_id)
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tasks()
                .build(),
        )
        .with_instructions(
            "runwatch is a durable continuation runtime for long-running scientific computation. MCP is a client surface over runwatchd; the daemon remains the sole Run/scheduler authority. Use wait_run only for bounded observation, and prefer durable agent continuation for minute-to-day jobs."
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_advertises_mcp_2026_07_28() {
        let server = RunwatchMcp::new();
        assert!(
            server
                .supported_protocol_versions()
                .iter()
                .any(|version| version == &ProtocolVersion::V_2026_07_28)
        );
    }

    #[test]
    fn long_wait_requires_tasks_and_is_bounded() {
        assert_eq!(classify_wait(60, false), WaitDispatch::Sync(60));
        assert_eq!(classify_wait(61, false), WaitDispatch::TasksRequired(61));
        assert_eq!(classify_wait(61, true), WaitDispatch::Task(61));
        assert_eq!(
            classify_wait(MAX_TASK_WAIT_SEC + 100, true),
            WaitDispatch::Task(MAX_TASK_WAIT_SEC)
        );
    }

    #[test]
    fn router_exposes_expected_run_tools() {
        let server = RunwatchMcp::new();
        let names = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<std::collections::HashSet<_>>();
        for name in [
            "list_runs",
            "get_run",
            "list_hosts",
            "submit_run",
            "submit_science_run",
            "tick",
            "wait_run",
            "logs",
            "artifacts",
            "cancel_run",
        ] {
            assert!(names.contains(name), "missing tool {name}");
        }
    }

    #[test]
    fn rmcp_call_tool_meta_preserves_codex_thread_id_shape() {
        let thread = "019c1234-5678-7000-8000-000000000004";
        let request: CallToolRequestParams = serde_json::from_value(json!({
            "name": "submit_science_run",
            "arguments": { "run_id": "r9" },
            "_meta": { "threadId": thread }
        }))
        .unwrap();
        let meta = serde_json::to_value(request.meta.as_ref().unwrap()).unwrap();
        assert_eq!(thread_id_from_meta_value(&meta).unwrap(), thread);
    }

    #[test]
    fn codex_thread_metadata_and_submit_spec_are_not_model_supplied() {
        let thread = "019c1234-5678-7000-8000-000000000003";
        assert_eq!(
            thread_id_from_meta_value(&json!({ "threadId": thread })).unwrap(),
            thread
        );
        assert!(thread_id_from_meta_value(&json!({})).is_err());

        let session = codex::CodexSessionMeta {
            thread_id: thread.into(),
            cwd: "C:/science/project".into(),
            session_file: "C:/Users/test/.codex/sessions/rollout.jsonl".into(),
        };
        let spec = build_codex_submit_spec(
            SubmitScienceArgs {
                run_id: "r9-codex".into(),
                name: Some("R9 smoke".into()),
                host: "gm00".into(),
                workdir: "/share/project".into(),
                runner: "slurm".into(),
                command: "python run.py".into(),
                time: Some("00:10:00".into()),
                partition: Some("gpu".into()),
                queue: None,
                account: None,
                cpus: Some(4),
                mem: Some("8G".into()),
                gpus: Some(1),
            },
            thread,
            session,
        )
        .unwrap();
        let binding = spec.continuation.unwrap();
        assert_eq!(binding.agent_kind, "codex");
        assert_eq!(binding.session_id, thread);
        assert_eq!(binding.project_root, "C:/science/project");
        assert_eq!(binding.workspace.host_alias, "gm00");
        assert_eq!(binding.workspace.cwd, "/share/project");
        assert!(binding.origin_leaf_id.is_none());
        assert!(binding.adapter_path.is_none());
    }

    #[test]
    fn every_runwatch_tool_has_typed_output_schema() {
        let server = RunwatchMcp::new();
        for tool in server.tool_router.list_all() {
            assert!(
                tool.output_schema.is_some(),
                "missing outputSchema for {}",
                tool.name
            );
        }
    }
}
