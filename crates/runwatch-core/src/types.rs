use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Submitting,
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationHealth {
    Fresh,
    ProbeError,
    Unreachable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationSource {
    LocalProcess,
    Sentinel,
    Scheduler,
    Compatibility,
    Transport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationRecord {
    pub run_id: String,
    pub attempt_no: u32,
    pub observed_at: DateTime<Utc>,
    pub source: ObservationSource,
    pub health: ObservationHealth,
    pub execution_status: RunStatus,
    #[serde(default)]
    pub raw_state: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub command_exit_code: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerKind {
    Slurm,
    Lsf,
    Process,
    File,
    Powershell,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteWorkspaceRef {
    pub host_alias: String,
    pub cwd: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunResources {
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default)]
    pub partition: Option<String>,
    #[serde(default)]
    pub queue: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub cpus: Option<u32>,
    #[serde(default)]
    pub mem: Option<String>,
    #[serde(default)]
    pub gpus: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuationBinding {
    pub agent_kind: String,
    pub session_id: String,
    #[serde(default)]
    pub session_file: Option<String>,
    #[serde(default)]
    pub origin_leaf_id: Option<String>,
    pub project_root: String,
    pub workspace: RemoteWorkspaceRef,
    #[serde(default)]
    pub adapter_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitRunSpec {
    pub run_id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub workspace: RemoteWorkspaceRef,
    pub runner: RunnerKind,
    pub command: String,
    #[serde(default)]
    pub resources: RunResources,
    #[serde(default)]
    pub continuation: Option<ContinuationBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunAttemptRecord {
    pub run_id: String,
    pub attempt_no: u32,
    pub runner: RunnerKind,
    pub host: String,
    pub workdir: String,
    pub command: String,
    pub resources: RunResources,
    pub job_name: String,
    #[serde(default)]
    pub job_id: Option<String>,
    pub script_path: String,
    pub stdout_path: String,
    pub stderr_path: String,
    pub terminal_path: String,
    pub receipt_path: String,
    pub status: RunStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionRegistration {
    pub agent_kind: String,
    pub session_id: String,
    pub owner_instance_id: String,
    #[serde(default)]
    pub session_file: Option<String>,
    pub project_root: String,
    #[serde(default)]
    pub current_leaf_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryPayload {
    pub delivery_id: String,
    pub run_id: String,
    pub attempt_no: u32,
    pub status: RunStatus,
    #[serde(default)]
    pub job_id: Option<String>,
    pub workspace: RemoteWorkspaceRef,
    pub binding: ContinuationBinding,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimedDelivery {
    pub delivery_id: String,
    pub attempts: u32,
    pub payload: DeliveryPayload,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryStatusSummary {
    pub pending: u32,
    pub delivering: u32,
    pub retrying: u32,
    pub needs_rebind: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInvocationRecord {
    pub invocation_id: String,
    pub delivery_id: String,
    pub owner_instance_id: String,
    pub payload: DeliveryPayload,
    #[serde(default)]
    pub session_file: Option<String>,
    #[serde(default)]
    pub adapter_path: Option<String>,
    pub project_root: String,
    pub state: String,
    #[serde(default)]
    pub pid: Option<u32>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub host: String,
    pub job_id: Option<String>,
    pub runner: RunnerKind,
    pub remote_terminal: Option<String>,
    pub status: RunStatus,
    #[serde(default)]
    pub workspace: Option<RemoteWorkspaceRef>,
    #[serde(default)]
    pub attempt_no: Option<u32>,
    pub session_id: Option<String>,
    pub project_root: Option<String>,
    pub agent: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub note: Option<String>,
}

impl RunRecord {
    pub fn new(run_id: String, host: String, runner: RunnerKind) -> Self {
        Self {
            run_id,
            host,
            name: None,
            job_id: None,
            runner,
            remote_terminal: None,
            status: RunStatus::Queued,
            workspace: None,
            attempt_no: None,
            session_id: None,
            project_root: None,
            agent: None,
            updated_at: Utc::now(),
            note: None,
        }
    }
}
