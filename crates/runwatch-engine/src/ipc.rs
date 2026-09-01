use anyhow::{Context, Result, bail};
use runwatch_core::{
    AgentSessionRegistration, ContinuationBinding, RunRecord, RunStore, SubmitRunSpec,
};
use runwatch_ssh::HostPool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

pub const PROTOCOL_VERSION: u32 = 1;
const MAX_REQUEST_BYTES: usize = 256 * 1024;

#[cfg(windows)]
pub const ENDPOINT: &str = r"\\.\pipe\runwatch-v1";

#[cfg(not(windows))]
pub const ENDPOINT_NAME: &str = "runwatch-v1.sock";

#[cfg(windows)]
fn endpoint() -> String {
    std::env::var("RUNWATCH_ENDPOINT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| ENDPOINT.to_string())
}

#[cfg(not(windows))]
fn endpoint_path() -> Result<std::path::PathBuf> {
    if let Some(value) = std::env::var_os("RUNWATCH_ENDPOINT").filter(|value| !value.is_empty()) {
        return Ok(std::path::PathBuf::from(value));
    }
    Ok(runwatch_core::data_dir()?.join(ENDPOINT_NAME))
}

#[derive(Debug, Deserialize)]
struct Request {
    id: String,
    op: String,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    run: Option<RunRecord>,
    #[serde(default)]
    spec: Option<SubmitRunSpec>,
    #[serde(default)]
    registration: Option<AgentSessionRegistration>,
    #[serde(default)]
    binding: Option<ContinuationBinding>,
    #[serde(default)]
    agent_kind: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    owner_instance_id: Option<String>,
    #[serde(default)]
    delivery_id: Option<String>,
    #[serde(default)]
    invocation_id: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    tail: Option<usize>,
    #[serde(default)]
    timeout_sec: Option<u64>,
    #[serde(default)]
    paused: Option<bool>,
}

#[derive(Debug, Serialize)]
struct Response {
    id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Response {
    fn ok(id: String, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: String, error: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error.into()),
        }
    }
}

fn hello_response(id: String) -> Response {
    Response::ok(
        id,
        json!({
            "protocol_version": PROTOCOL_VERSION,
            "service": "runwatchd",
            "storage": "sqlite-wal",
            "capabilities": [
                "hello", "daemon_status", "set_paused", "list_runs", "get_run", "get_observation", "adopt_run_v1", "tick", "wait_run", "submit_run_v2", "logs", "artifacts", "cancel_run",
                "register_agent_session", "release_agent_session",
                "claim_deliveries", "delivery_status", "ack_delivery", "rebind_continuation",
                "verify_offline_invocation", "offline_pi_continuation", "offline_codex_continuation"
            ],
        }),
    )
}

fn unsupported_response(id: String, op: &str) -> Response {
    Response::error(id, format!("unsupported operation {op}"))
}

fn handle_control_request(request: Request, store: &RunStore, paused: &AtomicBool) -> Response {
    let id = request.id.clone();
    match request.op.as_str() {
        "hello" => hello_response(id),
        "daemon_status" => Response::ok(
            id,
            json!({
                "paused": paused.load(Ordering::SeqCst),
                "pid": std::process::id(),
            }),
        ),
        "set_paused" => {
            let Some(value) = request.paused else {
                return Response::error(id, "set_paused requires paused");
            };
            paused.store(value, Ordering::SeqCst);
            Response::ok(id, json!({ "paused": value }))
        }
        "list_runs" => match (store.list(), store.list_observations()) {
            (Ok(runs), Ok(observations)) => {
                Response::ok(id, json!({ "runs": runs, "observations": observations }))
            }
            (Err(err), _) | (_, Err(err)) => Response::error(id, err.to_string()),
        },
        "get_run" => {
            let Some(run_id) = request.run_id.clone() else {
                return Response::error(id, "get_run requires run_id");
            };
            match store.get(&run_id) {
                Ok(run) => {
                    let observation = run
                        .as_ref()
                        .and_then(|record| record.attempt_no.map(|attempt_no| (record, attempt_no)))
                        .map(|(record, attempt_no)| {
                            store.get_observation(&record.run_id, attempt_no)
                        })
                        .transpose();
                    match observation {
                        Ok(observation) => Response::ok(
                            id,
                            json!({ "run": run, "observation": observation.flatten() }),
                        ),
                        Err(err) => Response::error(id, err.to_string()),
                    }
                }
                Err(err) => Response::error(id, err.to_string()),
            }
        }
        "get_observation" => {
            let Some(run_id) = request.run_id.clone() else {
                return Response::error(id, "get_observation requires run_id");
            };
            match store.get(&run_id) {
                Ok(Some(run)) => match run.attempt_no {
                    Some(attempt_no) => match store.get_observation(&run.run_id, attempt_no) {
                        Ok(observation) => Response::ok(id, json!({ "observation": observation })),
                        Err(err) => Response::error(id, err.to_string()),
                    },
                    None => Response::ok(id, json!({ "observation": null })),
                },
                Ok(None) => Response::error(id, format!("unknown run {run_id}")),
                Err(err) => Response::error(id, err.to_string()),
            }
        }
        "adopt_run_v1" => {
            let Some(run) = request.run else {
                return Response::error(id, "adopt_run_v1 requires run");
            };
            if run.run_id.trim().is_empty() || run.host.trim().is_empty() {
                return Response::error(id, "adopt_run_v1 requires non-empty run_id and host");
            }
            match store.upsert(&run) {
                Ok(()) => Response::ok(id, json!({ "run": run, "adopted": true })),
                Err(err) => Response::error(id, err.to_string()),
            }
        }
        "rebind_continuation" => {
            let Some(run_id) = request.run_id.clone() else {
                return Response::error(id, "rebind_continuation requires run_id");
            };
            let Some(binding) = request.binding else {
                return Response::error(id, "rebind_continuation requires binding");
            };
            match store.rebind_continuation(&run_id, &binding) {
                Ok(reset_deliveries) => Response::ok(
                    id,
                    json!({ "rebound": true, "reset_deliveries": reset_deliveries }),
                ),
                Err(err) => Response::error(id, err.to_string()),
            }
        }
        "register_agent_session" => {
            let Some(registration) = request.registration else {
                return Response::error(id, "register_agent_session requires registration");
            };
            match store.register_agent_session(&registration, std::time::Duration::from_secs(35)) {
                Ok(expires_at) => {
                    Response::ok(id, json!({ "registered": true, "expires_at": expires_at }))
                }
                Err(err) => Response::error(id, err.to_string()),
            }
        }
        "release_agent_session" => {
            let (Some(agent_kind), Some(session_id), Some(owner_instance_id)) = (
                request.agent_kind,
                request.session_id,
                request.owner_instance_id,
            ) else {
                return Response::error(
                    id,
                    "release_agent_session requires agent_kind, session_id, owner_instance_id",
                );
            };
            match store.release_agent_session(&agent_kind, &session_id, &owner_instance_id) {
                Ok(released) => Response::ok(id, json!({ "released": released })),
                Err(err) => Response::error(id, err.to_string()),
            }
        }
        "claim_deliveries" => {
            let (Some(agent_kind), Some(session_id), Some(owner_instance_id)) = (
                request.agent_kind,
                request.session_id,
                request.owner_instance_id,
            ) else {
                return Response::error(
                    id,
                    "claim_deliveries requires agent_kind, session_id, owner_instance_id",
                );
            };
            match store.claim_deliveries(
                &agent_kind,
                &session_id,
                &owner_instance_id,
                request.limit.unwrap_or(8),
            ) {
                Ok(deliveries) => Response::ok(id, json!({ "deliveries": deliveries })),
                Err(err) => Response::error(id, err.to_string()),
            }
        }
        "delivery_status" => {
            let (Some(agent_kind), Some(session_id), Some(owner_instance_id)) = (
                request.agent_kind,
                request.session_id,
                request.owner_instance_id,
            ) else {
                return Response::error(
                    id,
                    "delivery_status requires agent_kind, session_id, owner_instance_id",
                );
            };
            match store.delivery_status(&agent_kind, &session_id, &owner_instance_id) {
                Ok(status) => Response::ok(id, json!({ "status": status })),
                Err(err) => Response::error(id, err.to_string()),
            }
        }
        "verify_offline_invocation" => {
            let (Some(invocation_id), Some(delivery_id), Some(owner_instance_id)) = (
                request.invocation_id,
                request.delivery_id,
                request.owner_instance_id,
            ) else {
                return Response::error(
                    id,
                    "verify_offline_invocation requires invocation_id, delivery_id, owner_instance_id",
                );
            };
            match store.offline_invocation_is_owned(
                &invocation_id,
                &delivery_id,
                &owner_instance_id,
            ) {
                Ok(owned) => Response::ok(id, json!({ "owned": owned })),
                Err(err) => Response::error(id, err.to_string()),
            }
        }
        "ack_delivery" => {
            let (
                Some(agent_kind),
                Some(session_id),
                Some(owner_instance_id),
                Some(delivery_id),
                Some(outcome),
            ) = (
                request.agent_kind,
                request.session_id,
                request.owner_instance_id,
                request.delivery_id,
                request.outcome,
            )
            else {
                return Response::error(
                    id,
                    "ack_delivery requires agent_kind, session_id, owner_instance_id, delivery_id, outcome",
                );
            };
            match store.finish_delivery(
                &agent_kind,
                &session_id,
                &owner_instance_id,
                &delivery_id,
                &outcome,
                request.error.as_deref(),
            ) {
                Ok(updated) => Response::ok(id, json!({ "updated": updated })),
                Err(err) => Response::error(id, err.to_string()),
            }
        }
        other => unsupported_response(id, other),
    }
}

async fn handle_request(
    request: Request,
    store: &RunStore,
    pool: &HostPool,
    paused: &AtomicBool,
) -> Response {
    let id = request.id.clone();
    match request.op.as_str() {
        "tick" => match crate::tick(store, pool).await {
            Ok(report) => Response::ok(
                id,
                json!({
                    "runs": report.runs,
                    "transitions": report.transitions.iter().map(|transition| json!({
                        "run_id": transition.run_id,
                        "from": transition.from,
                        "to": transition.to,
                    })).collect::<Vec<_>>(),
                    "errors": report.errors,
                }),
            ),
            Err(err) => Response::error(id, format!("{err:#}")),
        },
        "wait_run" => {
            let Some(run_id) = request.run_id else {
                return Response::error(id, "wait_run requires run_id");
            };
            let timeout =
                std::time::Duration::from_secs(request.timeout_sec.unwrap_or(3600).min(86_400));
            match wait_for_snapshot(store, &run_id, timeout).await {
                Ok(run) => Response::ok(id, json!({ "run": run })),
                Err(err) => Response::error(id, format!("{err:#}")),
            }
        }
        "submit_run_v2" => {
            let Some(spec) = request.spec else {
                return Response::error(id, "submit_run_v2 requires spec");
            };
            match crate::submission::submit_run(store, pool, spec).await {
                Ok(run) => Response::ok(id, json!({ "run": run })),
                Err(err) => Response::error(id, format!("{err:#}")),
            }
        }
        "logs" => {
            let Some(run_id) = request.run_id else {
                return Response::error(id, "logs requires run_id");
            };
            match crate::operations::logs_run(store, pool, &run_id, request.tail).await {
                Ok(logs) => Response::ok(id, json!({ "logs": logs })),
                Err(err) => Response::error(id, format!("{err:#}")),
            }
        }
        "artifacts" => {
            let Some(run_id) = request.run_id else {
                return Response::error(id, "artifacts requires run_id");
            };
            match crate::operations::artifacts_run(store, &run_id) {
                Ok(artifacts) => Response::ok(id, json!({ "artifacts": artifacts })),
                Err(err) => Response::error(id, format!("{err:#}")),
            }
        }
        "cancel_run" => {
            let Some(run_id) = request.run_id else {
                return Response::error(id, "cancel_run requires run_id");
            };
            match crate::operations::cancel_run(store, pool, &run_id).await {
                Ok(run) => Response::ok(id, json!({ "cancel_requested": true, "run": run })),
                Err(err) => Response::error(id, format!("{err:#}")),
            }
        }
        _ => handle_control_request(request, store, paused),
    }
}

enum BoundedLine {
    Eof,
    Line(Vec<u8>),
    TooLarge,
}

async fn read_bounded_line<R>(reader: &mut BufReader<R>) -> Result<BoundedLine>
where
    R: AsyncRead + Unpin,
{
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(BoundedLine::Eof)
            } else {
                Ok(BoundedLine::Line(line))
            };
        }

        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if line.len().saturating_add(newline) > MAX_REQUEST_BYTES {
                return Ok(BoundedLine::TooLarge);
            }
            line.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            return Ok(BoundedLine::Line(line));
        }

        if line.len().saturating_add(available.len()) > MAX_REQUEST_BYTES {
            return Ok(BoundedLine::TooLarge);
        }
        let consumed = available.len();
        line.extend_from_slice(available);
        reader.consume(consumed);
    }
}

async fn wait_for_snapshot(
    store: &RunStore,
    run_id: &str,
    timeout: std::time::Duration,
) -> Result<Option<runwatch_core::RunRecord>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let run = store.get(run_id)?;
        match run {
            Some(ref row) if row.status.is_terminal() => return Ok(run),
            None => return Ok(None),
            _ if tokio::time::Instant::now() >= deadline => return Ok(run),
            _ => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
    }
}

pub async fn call_local(op: &str, payload: Value) -> Result<Value> {
    let id = format!(
        "client-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let mut request = match payload {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        _ => bail!("runwatch IPC payload must be a JSON object"),
    };
    request.insert("id".into(), Value::String(id.clone()));
    request.insert("op".into(), Value::String(op.to_string()));
    let encoded = serde_json::to_vec(&Value::Object(request))?;
    let response = request_once(&encoded).await?;
    let value: Value = serde_json::from_slice(&response)?;
    if value.get("id").and_then(Value::as_str) != Some(id.as_str()) {
        bail!("runwatch IPC response id mismatch");
    }
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        bail!(
            "runwatch IPC {op} failed: {}",
            value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
        );
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

async fn serve_stream<S>(
    stream: S,
    store: Arc<RunStore>,
    pool: Arc<HostPool>,
    paused: Arc<AtomicBool>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    loop {
        let line = match read_bounded_line(&mut reader).await? {
            BoundedLine::Eof => break,
            BoundedLine::TooLarge => {
                let mut encoded = serde_json::to_vec(&Response::error(
                    String::new(),
                    "request exceeds 256 KiB limit",
                ))?;
                encoded.push(b'\n');
                write_half.write_all(&encoded).await?;
                write_half.flush().await?;
                break;
            }
            BoundedLine::Line(line) => line,
        };
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        let response = match serde_json::from_slice::<Request>(&line) {
            Ok(request) => handle_request(request, &store, &pool, &paused).await,
            Err(err) => Response::error(String::new(), format!("invalid request: {err}")),
        };
        let mut encoded = serde_json::to_vec(&response)?;
        encoded.push(b'\n');
        write_half.write_all(&encoded).await?;
        write_half.flush().await?;
    }
    Ok(())
}

pub fn spawn_local_server(
    store: Arc<RunStore>,
    pool: Arc<HostPool>,
    paused: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(err) = run_local_server(store, pool, paused).await {
            eprintln!("runwatch local IPC stopped: {err:#}");
        }
    })
}

#[cfg(windows)]
fn current_user_sid_string() -> Result<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::core::PWSTR;

    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        bail!(
            "open current-process token for runwatch IPC ACL: {}",
            std::io::Error::last_os_error()
        );
    }

    let mut needed = 0u32;
    unsafe {
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        let err = std::io::Error::last_os_error();
        unsafe {
            CloseHandle(token);
        }
        bail!("size current-user token information for runwatch IPC ACL: {err}");
    }

    let word = std::mem::size_of::<usize>();
    let mut buffer = vec![0usize; (needed as usize).div_ceil(word)];
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        let err = std::io::Error::last_os_error();
        unsafe {
            CloseHandle(token);
        }
        bail!("read current-user token information for runwatch IPC ACL: {err}");
    }

    let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid_text: PWSTR = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut sid_text) } == 0 {
        let err = std::io::Error::last_os_error();
        unsafe {
            CloseHandle(token);
        }
        bail!("format current-user SID for runwatch IPC ACL: {err}");
    }

    let mut len = 0usize;
    unsafe {
        while *sid_text.add(len) != 0 {
            len += 1;
        }
    }
    let sid = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, len)) };
    unsafe {
        LocalFree(sid_text.cast());
        CloseHandle(token);
    }
    Ok(sid)
}

#[cfg(windows)]
fn create_user_only_named_pipe(
    endpoint: &str,
    user_sid: &str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use tokio::net::windows::named_pipe::ServerOptions;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

    let sddl = format!("O:{user_sid}D:P(A;;GA;;;SY)(A;;GA;;;{user_sid})");
    let wide: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        bail!(
            "build current-user-only security descriptor for named pipe {endpoint}: {}",
            std::io::Error::last_os_error()
        );
    }

    let mut attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let mut options = ServerOptions::new();
    options.reject_remote_clients(true);
    let result = unsafe {
        options.create_with_security_attributes_raw(
            endpoint,
            (&mut attrs as *mut SECURITY_ATTRIBUTES).cast(),
        )
    };
    unsafe {
        LocalFree(descriptor);
    }
    result.with_context(|| format!("create current-user-only named pipe {endpoint}"))
}

#[cfg(windows)]
async fn run_local_server(
    store: Arc<RunStore>,
    pool: Arc<HostPool>,
    paused: Arc<AtomicBool>,
) -> Result<()> {
    let endpoint = endpoint();
    let user_sid = current_user_sid_string()?;
    loop {
        let server = create_user_only_named_pipe(&endpoint, &user_sid)?;
        server.connect().await.context("accept named pipe client")?;
        let store = store.clone();
        let pool = pool.clone();
        let paused = paused.clone();
        tokio::spawn(async move {
            if let Err(err) = serve_stream(server, store, pool, paused).await {
                eprintln!("runwatch IPC client error: {err:#}");
            }
        });
    }
}

#[cfg(not(windows))]
async fn run_local_server(
    store: Arc<RunStore>,
    pool: Arc<HostPool>,
    paused: Arc<AtomicBool>,
) -> Result<()> {
    use tokio::net::UnixListener;

    let path = endpoint_path()?;
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("remove stale IPC socket {}", path.display()))?;
    }
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("bind local IPC socket {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restrict local IPC socket {}", path.display()))?;
    }
    loop {
        let (stream, _) = listener.accept().await?;
        let store = store.clone();
        let pool = pool.clone();
        let paused = paused.clone();
        tokio::spawn(async move {
            if let Err(err) = serve_stream(stream, store, pool, paused).await {
                eprintln!("runwatch IPC client error: {err:#}");
            }
        });
    }
}

pub async fn probe_local_server() -> Result<Value> {
    let request = serde_json::to_vec(&json!({ "id": "probe", "op": "hello" }))?;
    let response = request_once(&request).await?;
    let value: Value = serde_json::from_slice(&response)?;
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        bail!("runwatch IPC hello failed: {value}");
    }
    Ok(value)
}

#[cfg(windows)]
fn is_named_pipe_busy(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(231)
}

#[cfg(windows)]
async fn request_once(request: &[u8]) -> Result<Vec<u8>> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let endpoint = endpoint();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    let client = loop {
        match ClientOptions::new().open(&endpoint) {
            Ok(client) => break client,
            Err(err) if is_named_pipe_busy(&err) && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(err) => {
                return Err(err).with_context(|| format!("open named pipe {endpoint}"));
            }
        }
    };
    request_once_stream(client, request).await
}

#[cfg(not(windows))]
async fn request_once(request: &[u8]) -> Result<Vec<u8>> {
    use tokio::net::UnixStream;

    let path = endpoint_path()?;
    let client = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connect local IPC socket {}", path.display()))?;
    request_once_stream(client, request).await
}

async fn request_once_stream<S>(mut stream: S, request: &[u8]) -> Result<Vec<u8>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut payload = request.to_vec();
    payload.push(b'\n');
    stream.write_all(&payload).await?;
    stream.flush().await?;
    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut line).await?;
    if line.is_empty() {
        bail!("runwatch IPC closed without a response");
    }
    Ok(line.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_frame_is_rejected_before_buffering_past_limit() {
        let (mut writer, reader) = tokio::io::duplex(MAX_REQUEST_BYTES + 4096);
        let payload = vec![b'x'; MAX_REQUEST_BYTES + 1];
        tokio::spawn(async move {
            writer.write_all(&payload).await.unwrap();
            writer.write_all(b"\n").await.unwrap();
        });
        let mut reader = BufReader::new(reader);
        assert!(matches!(
            read_bounded_line(&mut reader).await.unwrap(),
            BoundedLine::TooLarge
        ));
    }

    #[tokio::test]
    async fn request_frame_accepts_exact_limit() {
        let (mut writer, reader) = tokio::io::duplex(MAX_REQUEST_BYTES + 16);
        let payload = vec![b'x'; MAX_REQUEST_BYTES];
        tokio::spawn(async move {
            writer.write_all(&payload).await.unwrap();
            writer.write_all(b"\n").await.unwrap();
        });
        let mut reader = BufReader::new(reader);
        match read_bounded_line(&mut reader).await.unwrap() {
            BoundedLine::Line(line) => assert_eq!(line.len(), MAX_REQUEST_BYTES),
            _ => panic!("exact-limit request should be accepted"),
        }
    }

    #[test]
    fn hello_advertises_v2_submit_capability() {
        let response = hello_response("1".into());
        assert!(response.ok);
        let result = response.result.expect("hello result");
        assert_eq!(result["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(result["storage"], "sqlite-wal");
        assert!(
            result["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "submit_run_v2")
        );
        assert!(
            result["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "logs")
        );
        assert!(
            result["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "cancel_run")
        );
        assert!(
            result["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "get_observation")
        );
        assert!(
            result["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "offline_pi_continuation")
        );
        assert!(
            result["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "offline_codex_continuation")
        );
        assert!(
            result["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "verify_offline_invocation")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_current_user_sid_is_available_for_pipe_acl() {
        let sid = current_user_sid_string().expect("current user SID");
        assert!(sid.starts_with("S-1-"), "unexpected SID {sid}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_current_user_only_named_pipe_can_be_created() {
        let sid = current_user_sid_string().expect("current user SID");
        let endpoint = format!(
            r"\\.\pipe\runwatch-acl-test-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let pipe = create_user_only_named_pipe(&endpoint, &sid).expect("private named pipe");
        drop(pipe);
    }

    #[cfg(windows)]
    #[test]
    fn windows_named_pipe_busy_is_retryable_but_other_errors_are_not() {
        assert!(is_named_pipe_busy(&std::io::Error::from_raw_os_error(231)));
        assert!(!is_named_pipe_busy(&std::io::Error::from_raw_os_error(2)));
    }

    #[test]
    fn unknown_operation_fails_closed() {
        let response = unsupported_response("2".into(), "delete_everything");
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("unsupported operation"));
    }
}
