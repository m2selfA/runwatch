use crate::model::{DashboardSnapshot, RunDetailView, RunRow};
use chrono::{Duration, Utc};
use runwatch_core::{RunStatus, RunnerKind};

pub struct GuiFixture {
    pub snapshot: DashboardSnapshot,
    pub detail: Option<RunDetailView>,
    pub offline_error: Option<String>,
    pub hosts: String,
    pub service: String,
    pub open_create_dialog: bool,
}

pub fn named(name: &str) -> Option<GuiFixture> {
    match name {
        "dashboard" => Some(build(false, false, false)),
        "detail" => Some(build(true, false, false)),
        "offline" => Some(build(false, true, false)),
        "new-run" => Some(build(false, false, true)),
        _ => None,
    }
}

fn build(with_detail: bool, offline: bool, open_create_dialog: bool) -> GuiFixture {
    let now = Utc::now();
    let rows = vec![
        RunRow {
            run_id: "run-refine-map-quiet-cedar".into(),
            name: "refine-map".into(),
            status: RunStatus::Running,
            runner: RunnerKind::Slurm,
            host: "gm00".into(),
            handle: "31842".into(),
            observation: "fresh / scheduler / 12s".into(),
            continuation: "bound".into(),
            updated: "12s".into(),
            workspace: "/share/project/refine".into(),
            active: true,
            attention: false,
            attention_reason: None,
            updated_at: now - Duration::seconds(12),
        },
        RunRow {
            run_id: "run-reconstruction-still-water".into(),
            name: "reconstruction".into(),
            status: RunStatus::Queued,
            runner: RunnerKind::Slurm,
            host: "gm00".into(),
            handle: "31843".into(),
            observation: "fresh / scheduler / 25s".into(),
            continuation: "pending (1)".into(),
            updated: "25s".into(),
            workspace: "/share/project/reconstruction".into(),
            active: true,
            attention: false,
            attention_reason: None,
            updated_at: now - Duration::seconds(25),
        },
        RunRow {
            run_id: "run-mask-fit-golden-pine".into(),
            name: "mask-fit".into(),
            status: RunStatus::Failed,
            runner: RunnerKind::Process,
            host: "local".into(),
            handle: "local:4242:fixture".into(),
            observation: "fresh / local process / 1m".into(),
            continuation: "needs rebind (1)".into(),
            updated: "1m".into(),
            workspace: "C:\\science\\mask-fit".into(),
            active: false,
            attention: true,
            attention_reason: Some("Run failed".into()),
            updated_at: now - Duration::seconds(75),
        },
    ];
    let snapshot = DashboardSnapshot {
        daemon_version: "0.2.0-dev".into(),
        daemon_protocol: 1,
        daemon_capabilities: 24,
        daemon_pid: 4242,
        paused: false,
        manual_submit_supported: true,
        active: 2,
        attention: 1,
        recent_terminal: 1,
        total: rows.len(),
        rows,
    };
    let detail = with_detail.then(|| RunDetailView {
        run_id: "run-mask-fit-golden-pine".into(),
        title: "mask-fit".into(),
        overview: "Run ID: run-mask-fit-golden-pine\nStatus: failed\nRunner: Process\nHost: local\nHandle: local:4242:fixture\nWorkspace: local:C:\\science\\mask-fit\nUpdated: 2026-09-04T03:10:00Z\n\nObservation\nfresh / local process\nobserved: 2026-09-04T03:09:58Z\nraw: EXITED\nreason: process exited with code 1\nexit: 1\n\nAttempt\nattempt 1\nworkdir: C:\\science\\mask-fit\ncommand: python mask_fit.py --input map.mrc\nresources: {}".into(),
        logs: "STDOUT\nloading map...\niteration 1\niteration 2\n\nSTDERR\nfit diverged after iteration 2\n".into(),
        artifacts: "script\nC:\\science\\mask-fit\\.runwatch\\run.ps1\n\nstdout\nC:\\science\\mask-fit\\.runwatch\\stdout.log\n\nstderr\nC:\\science\\mask-fit\\.runwatch\\stderr.log\n\nterminal\nC:\\science\\mask-fit\\.runwatch\\terminal.json".into(),
        timeline: "2026-09-04T03:08:00Z  submission_intent\n{\"attempt_no\":1}\n\n2026-09-04T03:08:02Z  observation_changed\n{\"execution_status\":\"running\"}\n\n2026-09-04T03:10:00Z  terminal\n{\"status\":\"failed\",\"exit_code\":1}".into(),
        continuation: "Agent: pi\nSession: 7df42c...913c\nProject: C:\\science\\mask-fit\nPending: 0  Delivering: 0  Retrying: 0  Needs rebind: 1  Delivered: 0\nLast state: needs_rebind\nLast error: active branch diverged\n\nAction required: return to the bound agent/Pi session and perform an explicit rebind there.".into(),
        can_cancel: false,
    });
    GuiFixture {
        snapshot,
        detail,
        offline_error: offline.then(|| "fixture: runwatchd named pipe is unavailable".into()),
        hosts: "gm00   shark@gm00.example:22\ncompute-gw   user@10.0.0.5:22 via bastion".into(),
        service: if offline {
            "runwatchd unavailable\nfixture: named pipe unavailable\n\nThe GUI will not take over scheduler polling.".into()
        } else {
            "runwatchd 0.2.0-dev\nprotocol: 1 · capabilities: 24\npid: 4242\npolling: active\nresident service: enabled\nGUI autostart: enabled\npackage siblings: complete".into()
        },
        open_create_dialog,
    }
}
