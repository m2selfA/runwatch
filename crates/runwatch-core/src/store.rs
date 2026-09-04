use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::ensure_data_dir;
use crate::types::{
    AgentInvocationRecord, AgentSessionRegistration, ClaimedDelivery, ContinuationBinding,
    DeliveryPayload, DeliveryStatusSummary, ObservationRecord, RetryRunSpec, RunAttemptRecord,
    RunContinuationStatus, RunEventRecord, RunRecord, RunStatus,
};

const SCHEMA_VERSION: i64 = 3;

pub struct RunStore {
    path: PathBuf,
    legacy_path: PathBuf,
}

impl RunStore {
    pub fn open_default() -> Result<Self> {
        let dir = ensure_data_dir()?;
        Self::open_at(dir.join("runwatch.db"), dir.join("runs.jsonl"))
    }

    fn open_at(path: PathBuf, legacy_path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let store = Self { path, legacy_path };
        store.initialize()?;
        store.import_legacy_if_needed()?;
        Ok(store)
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn legacy_path(&self) -> &PathBuf {
        &self.legacy_path
    }

    fn connect(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)
            .with_context(|| format!("open runwatch database {}", self.path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(conn)
    }

    fn initialize(&self) -> Result<()> {
        let conn = self.connect()?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runs (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL UNIQUE,
                record_json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS run_attempts (
                run_id TEXT NOT NULL,
                attempt_no INTEGER NOT NULL,
                record_json TEXT NOT NULL,
                PRIMARY KEY (run_id, attempt_no)
            );
            CREATE TABLE IF NOT EXISTS retry_intents (
                run_id TEXT NOT NULL,
                request_id TEXT NOT NULL,
                attempt_no INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                PRIMARY KEY (run_id, request_id),
                UNIQUE (run_id, attempt_no)
            );
            CREATE TABLE IF NOT EXISTS run_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL,
                at TEXT NOT NULL,
                kind TEXT NOT NULL,
                payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS observations (
                run_id TEXT NOT NULL,
                attempt_no INTEGER NOT NULL,
                observed_at TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                PRIMARY KEY (run_id, attempt_no)
            );
            CREATE TABLE IF NOT EXISTS deliveries (
                delivery_id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                state TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                next_retry_at TEXT,
                last_error TEXT,
                payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS continuation_bindings (
                run_id TEXT PRIMARY KEY,
                payload_json TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS artifacts (
                run_id TEXT NOT NULL,
                path TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                PRIMARY KEY (run_id, path)
            );
            CREATE TABLE IF NOT EXISTS agent_session_leases (
                agent_kind TEXT NOT NULL,
                session_id TEXT NOT NULL,
                owner_instance_id TEXT NOT NULL,
                expires_at TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                PRIMARY KEY (agent_kind, session_id)
            );
            CREATE TABLE IF NOT EXISTS agent_invocations (
                invocation_id TEXT PRIMARY KEY,
                delivery_id TEXT NOT NULL,
                owner_instance_id TEXT NOT NULL,
                state TEXT NOT NULL,
                pid INTEGER,
                started_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                last_error TEXT,
                payload_json TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_agent_invocations_delivery_state
                ON agent_invocations(delivery_id, state);
            ",
        )?;
        conn.execute(
            "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    fn import_legacy_if_needed(&self) -> Result<()> {
        if !self.legacy_path.exists() {
            return Ok(());
        }
        let mut conn = self.connect()?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))?;
        if count != 0 {
            return Ok(());
        }
        let records = read_legacy_latest(&self.legacy_path)?;
        if records.is_empty() {
            return Ok(());
        }
        let tx = conn.transaction()?;
        for record in records {
            let json = serde_json::to_string(&record)?;
            tx.execute(
                "INSERT INTO runs(run_id, record_json, updated_at) VALUES(?1, ?2, ?3)",
                params![record.run_id, json, record.updated_at.to_rfc3339()],
            )?;
        }
        tx.execute(
            "INSERT INTO meta(key, value) VALUES('legacy_imported_at', ?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<RunRecord>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare("SELECT record_json FROM runs ORDER BY seq")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let json = row?;
            out.push(serde_json::from_str(&json).context("parse runwatch.db runs.record_json")?);
        }
        Ok(out)
    }

    pub fn upsert(&self, record: &RunRecord) -> Result<()> {
        let conn = self.connect()?;
        let json = serde_json::to_string(record)?;
        conn.execute(
            "INSERT INTO runs(run_id, record_json, updated_at) VALUES(?1, ?2, ?3)
             ON CONFLICT(run_id) DO UPDATE SET
               record_json=excluded.record_json,
               updated_at=excluded.updated_at",
            params![record.run_id, json, record.updated_at.to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn get(&self, run_id: &str) -> Result<Option<RunRecord>> {
        let conn = self.connect()?;
        let json = conn
            .query_row(
                "SELECT record_json FROM runs WHERE run_id=?1",
                [run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        json.map(|json| serde_json::from_str(&json).context("parse runwatch.db run"))
            .transpose()
    }

    pub fn set_status(&self, run_id: &str, status: RunStatus) -> Result<Option<RunRecord>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let json = tx
            .query_row(
                "SELECT record_json FROM runs WHERE run_id=?1",
                [run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(json) = json else {
            return Ok(None);
        };
        let mut record: RunRecord = serde_json::from_str(&json).context("parse runwatch.db run")?;
        record.status = status;
        record.updated_at = Utc::now();
        let updated_json = serde_json::to_string(&record)?;
        tx.execute(
            "UPDATE runs SET record_json=?2, updated_at=?3 WHERE run_id=?1",
            params![run_id, updated_json, record.updated_at.to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(Some(record))
    }

    pub fn create_submission_intent(
        &self,
        run: &RunRecord,
        attempt: &RunAttemptRecord,
        binding: Option<&ContinuationBinding>,
    ) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let run_json = serde_json::to_string(run)?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO runs(run_id, record_json, updated_at) VALUES(?1, ?2, ?3)",
            params![run.run_id, run_json, run.updated_at.to_rfc3339()],
        )?;
        if inserted == 0 {
            tx.commit()?;
            return Ok(false);
        }

        let attempt_json = serde_json::to_string(attempt)?;
        tx.execute(
            "INSERT INTO run_attempts(run_id, attempt_no, record_json) VALUES(?1, ?2, ?3)",
            params![attempt.run_id, attempt.attempt_no, attempt_json],
        )?;
        if let Some(binding) = binding {
            tx.execute(
                "INSERT INTO continuation_bindings(run_id, payload_json) VALUES(?1, ?2)",
                params![run.run_id, serde_json::to_string(binding)?],
            )?;
        }
        tx.execute(
            "INSERT INTO run_events(run_id, at, kind, payload_json) VALUES(?1, ?2, 'submission_intent', ?3)",
            params![
                run.run_id,
                run.updated_at.to_rfc3339(),
                serde_json::to_string(&serde_json::json!({
                    "attempt_no": attempt.attempt_no,
                    "runner": attempt.runner,
                    "host": attempt.host,
                    "workdir": attempt.workdir,
                    "job_name": attempt.job_name,
                }))?
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn create_retry_intent(
        &self,
        spec: &RetryRunSpec,
        run: &RunRecord,
        attempt: &RunAttemptRecord,
    ) -> Result<bool> {
        let next_attempt_no = spec
            .expected_attempt_no
            .checked_add(1)
            .context("retry attempt number overflow")?;
        if attempt.run_id != spec.run_id || attempt.attempt_no != next_attempt_no {
            bail!("retry Attempt identity does not match request");
        }
        if run.run_id != spec.run_id
            || run.attempt_no != Some(next_attempt_no)
            || run.status != RunStatus::Submitting
            || run.job_id.is_some()
        {
            bail!("retry Run intent is not a clean submitting N+1 snapshot");
        }

        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((existing_attempt_no, payload_json)) = tx
            .query_row(
                "SELECT attempt_no, payload_json FROM retry_intents WHERE run_id=?1 AND request_id=?2",
                params![spec.run_id, spec.request_id],
                |row| Ok((row.get::<_, u32>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            let existing: RetryRunSpec =
                serde_json::from_str(&payload_json).context("parse durable retry intent")?;
            if existing != *spec || existing_attempt_no != next_attempt_no {
                bail!("retry request_id already exists with a different request");
            }
            tx.commit()?;
            return Ok(false);
        }

        let current_json = tx
            .query_row(
                "SELECT record_json FROM runs WHERE run_id=?1",
                [spec.run_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .with_context(|| format!("unknown run {}", spec.run_id))?;
        let current: RunRecord =
            serde_json::from_str(&current_json).context("parse current Run for retry")?;
        if current.attempt_no != Some(spec.expected_attempt_no) {
            bail!(
                "retry expected attempt {} but Run is at {:?}",
                spec.expected_attempt_no,
                current.attempt_no
            );
        }
        if !matches!(current.status, RunStatus::Failed | RunStatus::Cancelled) {
            bail!("retry requires the current Run to be failed or cancelled");
        }
        let bound = tx
            .query_row(
                "SELECT 1 FROM continuation_bindings WHERE run_id=?1",
                [spec.run_id.as_str()],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if bound {
            bail!(
                "retry_run_v1 currently requires an unbound Run; retry agent-bound workflows through the owning Agent Integration"
            );
        }
        if current.session_id.is_some() || current.agent.is_some() || current.project_root.is_some()
        {
            bail!(
                "retry_run_v1 refuses a Run that carries agent identity without a durable continuation binding; retry through the owning Agent Integration"
            );
        }
        let previous_json = tx
            .query_row(
                "SELECT record_json FROM run_attempts WHERE run_id=?1 AND attempt_no=?2",
                params![spec.run_id, spec.expected_attempt_no],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .context("current Run is missing its durable Attempt")?;
        let previous: RunAttemptRecord =
            serde_json::from_str(&previous_json).context("parse previous Attempt for retry")?;
        if !matches!(previous.status, RunStatus::Failed | RunStatus::Cancelled) {
            bail!("retry requires the current Attempt to be failed or cancelled");
        }
        let expected_resources = spec.resources.as_ref().unwrap_or(&previous.resources);
        if attempt.runner != previous.runner
            || attempt.host != previous.host
            || attempt.workdir != previous.workdir
            || attempt.command != previous.command
            || &attempt.resources != expected_resources
            || attempt.job_id.is_some()
            || attempt.status != RunStatus::Submitting
        {
            bail!("retry Attempt changed immutable execution identity or resource request");
        }
        if run.runner != current.runner
            || run.host != current.host
            || run.workspace != current.workspace
            || run.name != current.name
            || run.session_id.is_some()
            || run.agent.is_some()
            || run.project_root.is_some()
        {
            bail!("retry Run changed immutable logical identity");
        }

        tx.execute(
            "INSERT INTO run_attempts(run_id, attempt_no, record_json) VALUES(?1, ?2, ?3)",
            params![
                attempt.run_id,
                attempt.attempt_no,
                serde_json::to_string(attempt)?
            ],
        )?;
        tx.execute(
            "UPDATE runs SET record_json=?2, updated_at=?3 WHERE run_id=?1",
            params![
                run.run_id,
                serde_json::to_string(run)?,
                run.updated_at.to_rfc3339()
            ],
        )?;
        tx.execute(
            "INSERT INTO retry_intents(run_id, request_id, attempt_no, created_at, payload_json)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                spec.run_id,
                spec.request_id,
                next_attempt_no,
                run.updated_at.to_rfc3339(),
                serde_json::to_string(spec)?
            ],
        )?;
        tx.execute(
            "INSERT INTO run_events(run_id, at, kind, payload_json)
             VALUES(?1, ?2, 'retry_intent', ?3)",
            params![
                run.run_id,
                run.updated_at.to_rfc3339(),
                serde_json::to_string(&serde_json::json!({
                    "request_id": spec.request_id,
                    "from_attempt_no": spec.expected_attempt_no,
                    "attempt_no": next_attempt_no,
                    "resources": spec.resources,
                }))?
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn list_pending_retry_intents(&self) -> Result<Vec<RetryRunSpec>> {
        let conn = self.connect()?;
        // This runs every daemon tick, so never walk historical retry intents in Rust. The bundled
        // SQLite has JSON functions; join the canonical Run/Attempt rows and return only the tiny
        // recovery frontier: current N+1 is still Submitting and neither durable row owns a handle.
        let mut stmt = conn.prepare(
            "SELECT ri.payload_json
             FROM retry_intents AS ri
             JOIN runs AS r ON r.run_id=ri.run_id
             JOIN run_attempts AS a ON a.run_id=ri.run_id AND a.attempt_no=ri.attempt_no
             WHERE json_extract(r.record_json, '$.status')='submitting'
               AND CAST(json_extract(r.record_json, '$.attempt_no') AS INTEGER)=ri.attempt_no
               AND json_extract(r.record_json, '$.job_id') IS NULL
               AND json_extract(a.record_json, '$.status')='submitting'
               AND json_extract(a.record_json, '$.job_id') IS NULL
             ORDER BY ri.created_at, ri.rowid",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut pending = Vec::new();
        for row in rows {
            pending
                .push(serde_json::from_str(&row?).context("parse pending durable retry intent")?);
        }
        Ok(pending)
    }

    pub fn rebind_continuation(&self, run_id: &str, binding: &ContinuationBinding) -> Result<u32> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists = tx
            .query_row("SELECT 1 FROM runs WHERE run_id=?1", [run_id], |_| Ok(()))
            .optional()?
            .is_some();
        if !exists {
            bail!("unknown run {run_id}");
        }
        let delivering: i64 = tx.query_row(
            "SELECT COUNT(*) FROM deliveries WHERE run_id=?1 AND state='delivering'",
            [run_id],
            |row| row.get(0),
        )?;
        if delivering > 0 {
            bail!("cannot rebind run {run_id} while a continuation Delivery is in flight");
        }

        tx.execute(
            "INSERT INTO continuation_bindings(run_id, payload_json) VALUES(?1, ?2)
             ON CONFLICT(run_id) DO UPDATE SET payload_json=excluded.payload_json",
            params![run_id, serde_json::to_string(binding)?],
        )?;

        // Delivery payloads are durable snapshots consumed by live/offline claimers. Rebinding only
        // the continuation_bindings row would requeue a blocked Delivery with its old branch
        // identity, causing it to immediately become needs_rebind again. Refresh every unclaimed
        // snapshot in the same transaction, and only reset blocked rows to pending.
        let delivery_rows = {
            let mut stmt = tx.prepare(
                "SELECT delivery_id, state, payload_json FROM deliveries
                 WHERE run_id=?1 AND state IN ('pending', 'retrying', 'needs_rebind')",
            )?;
            let rows = stmt.query_map([run_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut reset = 0u32;
        for (delivery_id, state, payload_json) in delivery_rows {
            let mut payload: DeliveryPayload = serde_json::from_str(&payload_json)
                .context("parse delivery during continuation rebind")?;
            payload.binding = binding.clone();
            let rebound_payload = serde_json::to_string(&payload)?;
            if state == "needs_rebind" {
                reset += tx.execute(
                    "UPDATE deliveries
                     SET payload_json=?2, state='pending', next_retry_at=NULL, last_error=NULL
                     WHERE delivery_id=?1 AND state='needs_rebind'",
                    params![delivery_id, rebound_payload],
                )? as u32;
            } else {
                tx.execute(
                    "UPDATE deliveries SET payload_json=?2 WHERE delivery_id=?1 AND state=?3",
                    params![delivery_id, rebound_payload, state],
                )?;
            }
        }
        tx.execute(
            "INSERT INTO run_events(run_id, at, kind, payload_json)
             VALUES(?1, ?2, 'continuation_rebound', ?3)",
            params![
                run_id,
                Utc::now().to_rfc3339(),
                serde_json::to_string(&serde_json::json!({
                    "session_id": binding.session_id,
                    "session_file": binding.session_file,
                    "origin_leaf_id": binding.origin_leaf_id,
                    "reset_deliveries": reset,
                }))?
            ],
        )?;
        tx.commit()?;
        Ok(reset)
    }

    pub fn get_continuation_binding(&self, run_id: &str) -> Result<Option<ContinuationBinding>> {
        let conn = self.connect()?;
        let json = conn
            .query_row(
                "SELECT payload_json FROM continuation_bindings WHERE run_id=?1",
                [run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        json.map(|json| serde_json::from_str(&json).context("parse continuation binding"))
            .transpose()
    }

    pub fn list_run_events(&self, run_id: &str, limit: usize) -> Result<Vec<RunEventRecord>> {
        let conn = self.connect()?;
        let exists = conn
            .query_row("SELECT 1 FROM runs WHERE run_id=?1", [run_id], |_| Ok(()))
            .optional()?
            .is_some();
        if !exists {
            bail!("unknown run {run_id}");
        }
        let limit = limit.clamp(1, 200) as i64;
        let mut stmt = conn.prepare(
            "SELECT id, at, kind, payload_json FROM run_events
             WHERE run_id=?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![run_id, limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, at, kind, payload_json) = row?;
            out.push(RunEventRecord {
                id,
                run_id: run_id.to_string(),
                at: parse_utc(&at)?,
                kind,
                payload: serde_json::from_str(&payload_json).context("parse run event payload")?,
            });
        }
        out.reverse();
        Ok(out)
    }

    pub fn get_run_continuation_status(&self, run_id: &str) -> Result<RunContinuationStatus> {
        let conn = self.connect()?;
        let exists = conn
            .query_row("SELECT 1 FROM runs WHERE run_id=?1", [run_id], |_| Ok(()))
            .optional()?
            .is_some();
        if !exists {
            bail!("unknown run {run_id}");
        }
        let binding_json = conn
            .query_row(
                "SELECT payload_json FROM continuation_bindings WHERE run_id=?1",
                [run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let binding = binding_json
            .map(|json| {
                serde_json::from_str::<ContinuationBinding>(&json)
                    .context("parse continuation binding")
            })
            .transpose()?;
        let mut status = RunContinuationStatus {
            run_id: run_id.to_string(),
            configured: binding.is_some(),
            agent_kind: binding.as_ref().map(|value| value.agent_kind.clone()),
            session_id: binding.as_ref().map(|value| value.session_id.clone()),
            project_root: binding.as_ref().map(|value| value.project_root.clone()),
            ..RunContinuationStatus::default()
        };
        let counts: (i64, i64, i64, i64, i64) = conn.query_row(
            "SELECT
                COALESCE(SUM(CASE WHEN state='pending' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state='delivering' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state='retrying' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state='needs_rebind' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state='delivered' THEN 1 ELSE 0 END), 0)
             FROM deliveries WHERE run_id=?1",
            [run_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        status.pending = counts.0.try_into().unwrap_or(u32::MAX);
        status.delivering = counts.1.try_into().unwrap_or(u32::MAX);
        status.retrying = counts.2.try_into().unwrap_or(u32::MAX);
        status.needs_rebind = counts.3.try_into().unwrap_or(u32::MAX);
        status.delivered = counts.4.try_into().unwrap_or(u32::MAX);
        if let Some((state, last_error)) = conn
            .query_row(
                "SELECT state, last_error FROM deliveries WHERE run_id=?1 ORDER BY rowid DESC LIMIT 1",
                [run_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?
        {
            status.last_state = Some(state);
            status.last_error = last_error;
        }
        Ok(status)
    }

    pub fn list_run_continuation_statuses(&self) -> Result<Vec<RunContinuationStatus>> {
        let conn = self.connect()?;
        let mut statuses = HashMap::<String, RunContinuationStatus>::new();

        let mut binding_stmt =
            conn.prepare("SELECT run_id, payload_json FROM continuation_bindings ORDER BY run_id")?;
        let binding_rows = binding_stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in binding_rows {
            let (run_id, payload_json) = row?;
            let binding: ContinuationBinding = serde_json::from_str(&payload_json)
                .context("parse continuation binding for GUI projection")?;
            statuses.insert(
                run_id.clone(),
                RunContinuationStatus {
                    run_id,
                    configured: true,
                    agent_kind: Some(binding.agent_kind),
                    session_id: Some(binding.session_id),
                    project_root: Some(binding.project_root),
                    ..RunContinuationStatus::default()
                },
            );
        }

        let mut count_stmt = conn.prepare(
            "SELECT run_id,
                COALESCE(SUM(CASE WHEN state='pending' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state='delivering' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state='retrying' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state='needs_rebind' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN state='delivered' THEN 1 ELSE 0 END), 0)
             FROM deliveries GROUP BY run_id",
        )?;
        let count_rows = count_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        for row in count_rows {
            let (run_id, pending, delivering, retrying, needs_rebind, delivered) = row?;
            let status = statuses
                .entry(run_id.clone())
                .or_insert_with(|| RunContinuationStatus {
                    run_id,
                    ..RunContinuationStatus::default()
                });
            status.pending = pending.try_into().unwrap_or(u32::MAX);
            status.delivering = delivering.try_into().unwrap_or(u32::MAX);
            status.retrying = retrying.try_into().unwrap_or(u32::MAX);
            status.needs_rebind = needs_rebind.try_into().unwrap_or(u32::MAX);
            status.delivered = delivered.try_into().unwrap_or(u32::MAX);
        }

        let mut latest_stmt = conn.prepare(
            "SELECT d.run_id, d.state, d.last_error
             FROM deliveries d
             JOIN (
                 SELECT run_id, MAX(rowid) AS max_rowid
                 FROM deliveries GROUP BY run_id
             ) latest ON d.rowid=latest.max_rowid",
        )?;
        let latest_rows = latest_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in latest_rows {
            let (run_id, state, last_error) = row?;
            let status = statuses
                .entry(run_id.clone())
                .or_insert_with(|| RunContinuationStatus {
                    run_id,
                    ..RunContinuationStatus::default()
                });
            status.last_state = Some(state);
            status.last_error = last_error;
        }

        let mut out: Vec<_> = statuses.into_values().collect();
        out.sort_by(|left, right| left.run_id.cmp(&right.run_id));
        Ok(out)
    }

    pub fn register_agent_session(
        &self,
        registration: &AgentSessionRegistration,
        ttl: Duration,
    ) -> Result<DateTime<Utc>> {
        let ttl_secs = ttl.as_secs().clamp(5, 300) as i64;
        let now = Utc::now();
        let expires_at = now + ChronoDuration::seconds(ttl_secs);
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some((owner, expires)) = tx
            .query_row(
                "SELECT owner_instance_id, expires_at FROM agent_session_leases
                 WHERE agent_kind=?1 AND session_id=?2",
                params![registration.agent_kind, registration.session_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            let active_until = parse_utc(&expires)?;
            if owner != registration.owner_instance_id && active_until > now {
                bail!(
                    "agent session {}:{} is already leased by another live instance until {}",
                    registration.agent_kind,
                    registration.session_id,
                    active_until.to_rfc3339()
                );
            }
        }

        tx.execute(
            "INSERT INTO agent_session_leases(
                agent_kind, session_id, owner_instance_id, expires_at, payload_json
             ) VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(agent_kind, session_id) DO UPDATE SET
                owner_instance_id=excluded.owner_instance_id,
                expires_at=excluded.expires_at,
                payload_json=excluded.payload_json",
            params![
                registration.agent_kind,
                registration.session_id,
                registration.owner_instance_id,
                expires_at.to_rfc3339(),
                serde_json::to_string(registration)?,
            ],
        )?;
        tx.commit()?;
        Ok(expires_at)
    }

    pub fn release_agent_session(
        &self,
        agent_kind: &str,
        session_id: &str,
        owner_instance_id: &str,
    ) -> Result<bool> {
        let conn = self.connect()?;
        Ok(conn.execute(
            "DELETE FROM agent_session_leases
             WHERE agent_kind=?1 AND session_id=?2 AND owner_instance_id=?3",
            params![agent_kind, session_id, owner_instance_id],
        )? > 0)
    }

    pub fn ensure_terminal_delivery(&self, run: &RunRecord) -> Result<Option<String>> {
        if !run.status.is_terminal() {
            return Ok(None);
        }
        let attempt_no = run.attempt_no.unwrap_or(1);
        let now = Utc::now();
        let delivery_id = format!("{}:a{}:terminal", run.run_id, attempt_no);
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let binding_json = tx
            .query_row(
                "SELECT payload_json FROM continuation_bindings WHERE run_id=?1",
                [run.run_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(binding_json) = binding_json else {
            tx.commit()?;
            return Ok(None);
        };
        let binding: ContinuationBinding =
            serde_json::from_str(&binding_json).context("parse terminal continuation binding")?;
        let payload = DeliveryPayload {
            delivery_id: delivery_id.clone(),
            run_id: run.run_id.clone(),
            attempt_no,
            status: run.status,
            job_id: run.job_id.clone(),
            workspace: run
                .workspace
                .clone()
                .unwrap_or_else(|| binding.workspace.clone()),
            binding,
            created_at: now,
        };
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO deliveries(
                delivery_id, run_id, state, attempts, next_retry_at, last_error, payload_json
             ) VALUES(?1, ?2, 'pending', 0, NULL, NULL, ?3)",
            params![delivery_id, run.run_id, serde_json::to_string(&payload)?],
        )?;
        if inserted > 0 {
            tx.execute(
                "INSERT INTO run_events(run_id, at, kind, payload_json)
                 VALUES(?1, ?2, 'continuation_pending', ?3)",
                params![
                    run.run_id,
                    now.to_rfc3339(),
                    serde_json::to_string(&serde_json::json!({
                        "delivery_id": payload.delivery_id,
                        "attempt_no": attempt_no,
                        "session_id": payload.binding.session_id,
                    }))?
                ],
            )?;
        }
        tx.commit()?;
        Ok(Some(delivery_id))
    }

    pub fn claim_deliveries(
        &self,
        agent_kind: &str,
        session_id: &str,
        owner_instance_id: &str,
        limit: usize,
    ) -> Result<Vec<ClaimedDelivery>> {
        let limit = limit.clamp(1, 16);
        let now = Utc::now();
        let claim_until = now + ChronoDuration::seconds(60);
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_active_session_lease(&tx, agent_kind, session_id, owner_instance_id, now)?;

        requeue_expired_delivery_claims(&tx, now)?;

        let candidates = {
            let mut stmt = tx.prepare(
                "SELECT delivery_id, attempts, payload_json FROM deliveries
                 WHERE state IN ('pending', 'retrying')
                   AND (next_retry_at IS NULL OR next_retry_at<=?1)
                 ORDER BY rowid LIMIT 128",
            )?;
            let rows = stmt.query_map([now.to_rfc3339()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut claimed = Vec::new();
        for (delivery_id, attempts, payload_json) in candidates {
            if claimed.len() >= limit {
                break;
            }
            let payload: DeliveryPayload =
                serde_json::from_str(&payload_json).context("parse pending delivery")?;
            if payload.binding.agent_kind != agent_kind || payload.binding.session_id != session_id
            {
                continue;
            }
            let changed = tx.execute(
                "UPDATE deliveries
                 SET state='delivering', attempts=attempts+1, next_retry_at=?2, last_error=NULL
                 WHERE delivery_id=?1 AND state IN ('pending', 'retrying')",
                params![delivery_id, claim_until.to_rfc3339()],
            )?;
            if changed > 0 {
                claimed.push(ClaimedDelivery {
                    delivery_id,
                    attempts: attempts + 1,
                    payload,
                });
            }
        }
        tx.commit()?;
        Ok(claimed)
    }

    pub fn delivery_status(
        &self,
        agent_kind: &str,
        session_id: &str,
        owner_instance_id: &str,
    ) -> Result<DeliveryStatusSummary> {
        let now = Utc::now();
        let conn = self.connect()?;
        verify_active_session_lease_ref(&conn, agent_kind, session_id, owner_instance_id, now)?;
        let mut stmt = conn.prepare(
            "SELECT state, payload_json FROM deliveries
             WHERE state IN ('pending', 'delivering', 'retrying', 'needs_rebind')",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut summary = DeliveryStatusSummary::default();
        for row in rows {
            let (state, payload_json) = row?;
            let payload: DeliveryPayload =
                serde_json::from_str(&payload_json).context("parse delivery status payload")?;
            if payload.binding.agent_kind != agent_kind || payload.binding.session_id != session_id
            {
                continue;
            }
            match state.as_str() {
                "pending" => summary.pending += 1,
                "delivering" => summary.delivering += 1,
                "retrying" => summary.retrying += 1,
                "needs_rebind" => summary.needs_rebind += 1,
                _ => {}
            }
        }
        Ok(summary)
    }

    pub fn reserve_offline_invocation(
        &self,
        grace: Duration,
    ) -> Result<Option<AgentInvocationRecord>> {
        let now = Utc::now();
        let cutoff =
            now - ChronoDuration::from_std(grace).unwrap_or_else(|_| ChronoDuration::seconds(20));
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        requeue_expired_delivery_claims(&tx, now)?;

        let candidates = {
            let mut stmt = tx.prepare(
                "SELECT delivery_id, payload_json FROM deliveries
                 WHERE state IN ('pending', 'retrying')
                   AND (next_retry_at IS NULL OR next_retry_at<=?1)
                 ORDER BY rowid LIMIT 128",
            )?;
            let rows = stmt.query_map([now.to_rfc3339()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        for (delivery_id, payload_json) in candidates {
            let payload: DeliveryPayload =
                serde_json::from_str(&payload_json).context("parse offline delivery candidate")?;
            if payload.created_at > cutoff {
                continue;
            }
            let agent_kind = payload.binding.agent_kind.clone();
            if !matches!(agent_kind.as_str(), "pi" | "codex") {
                tx.execute(
                    "UPDATE deliveries SET state='needs_rebind', last_error=?2
                     WHERE delivery_id=?1 AND state IN ('pending','retrying')",
                    params![
                        delivery_id,
                        format!("offline continuation does not support agent_kind={agent_kind}"),
                    ],
                )?;
                continue;
            }
            let Some(session_file) = payload.binding.session_file.clone() else {
                tx.execute(
                    "UPDATE deliveries SET state='needs_rebind', last_error=?2
                     WHERE delivery_id=?1 AND state IN ('pending','retrying')",
                    params![
                        delivery_id,
                        format!(
                            "offline {agent_kind} continuation requires a durable session/rollout file"
                        ),
                    ],
                )?;
                continue;
            };
            let adapter_path = payload.binding.adapter_path.clone();
            if agent_kind == "pi" && adapter_path.is_none() {
                tx.execute(
                    "UPDATE deliveries SET state='needs_rebind', last_error='offline Pi continuation requires the pi-runs adapter path; rebind from a current Pi instance'
                     WHERE delivery_id=?1 AND state IN ('pending','retrying')",
                    [&delivery_id],
                )?;
                continue;
            }

            if let Some(expires) = tx
                .query_row(
                    "SELECT expires_at FROM agent_session_leases WHERE agent_kind=?1 AND session_id=?2",
                    params![agent_kind, payload.binding.session_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
            {
                if parse_utc(&expires)? > now {
                    continue;
                }
            }

            let nonce = now.timestamp_nanos_opt().unwrap_or_default();
            let owner_instance_id = format!("offline:{agent_kind}:{}:{nonce}", delivery_id);
            let invocation_id = format!("inv:{agent_kind}:{}:{nonce}", delivery_id);
            let lease_expires = now + ChronoDuration::seconds(120);
            let registration = AgentSessionRegistration {
                agent_kind: agent_kind.clone(),
                session_id: payload.binding.session_id.clone(),
                owner_instance_id: owner_instance_id.clone(),
                session_file: Some(session_file.clone()),
                project_root: payload.binding.project_root.clone(),
                current_leaf_id: payload.binding.origin_leaf_id.clone(),
            };
            tx.execute(
                "INSERT INTO agent_session_leases(agent_kind, session_id, owner_instance_id, expires_at, payload_json)
                 VALUES(?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(agent_kind, session_id) DO UPDATE SET
                   owner_instance_id=excluded.owner_instance_id,
                   expires_at=excluded.expires_at,
                   payload_json=excluded.payload_json",
                params![
                    agent_kind,
                    payload.binding.session_id,
                    owner_instance_id,
                    lease_expires.to_rfc3339(),
                    serde_json::to_string(&registration)?,
                ],
            )?;
            let changed = tx.execute(
                "UPDATE deliveries SET state='delivering', attempts=attempts+1, next_retry_at=?2, last_error=NULL
                 WHERE delivery_id=?1 AND state IN ('pending','retrying')",
                params![delivery_id, (now + ChronoDuration::hours(6)).to_rfc3339()],
            )?;
            if changed == 0 {
                continue;
            }

            let invocation = AgentInvocationRecord {
                invocation_id: invocation_id.clone(),
                delivery_id: delivery_id.clone(),
                owner_instance_id: owner_instance_id.clone(),
                payload,
                session_file: Some(session_file),
                adapter_path,
                project_root: registration.project_root,
                state: "starting".into(),
                pid: None,
                started_at: now,
                updated_at: now,
                last_error: None,
            };
            tx.execute(
                "INSERT INTO agent_invocations(
                    invocation_id, delivery_id, owner_instance_id, state, pid,
                    started_at, updated_at, last_error, payload_json
                 ) VALUES(?1, ?2, ?3, ?4, NULL, ?5, ?6, NULL, ?7)",
                params![
                    invocation.invocation_id,
                    invocation.delivery_id,
                    invocation.owner_instance_id,
                    invocation.state,
                    invocation.started_at.to_rfc3339(),
                    invocation.updated_at.to_rfc3339(),
                    serde_json::to_string(&invocation)?,
                ],
            )?;
            tx.commit()?;
            return Ok(Some(invocation));
        }

        tx.commit()?;
        Ok(None)
    }

    pub fn set_agent_invocation_pid(&self, invocation_id: &str, pid: u32) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE agent_invocations SET state='running', pid=?2, updated_at=?3 WHERE invocation_id=?1",
            params![invocation_id, pid, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn get_delivery_state(&self, delivery_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT state FROM deliveries WHERE delivery_id=?1",
            [delivery_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn offline_invocation_is_owned(
        &self,
        invocation_id: &str,
        delivery_id: &str,
        owner_instance_id: &str,
    ) -> Result<bool> {
        let now = Utc::now();
        let conn = self.connect()?;
        let row = conn
            .query_row(
                "SELECT state, delivery_id, owner_instance_id, payload_json
                 FROM agent_invocations WHERE invocation_id=?1",
                [invocation_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((invocation_state, actual_delivery_id, actual_owner, invocation_json)) = row
        else {
            return Ok(false);
        };
        if !matches!(invocation_state.as_str(), "starting" | "running")
            || actual_delivery_id != delivery_id
            || actual_owner != owner_instance_id
        {
            return Ok(false);
        }
        let invocation: AgentInvocationRecord =
            serde_json::from_str(&invocation_json).context("parse agent invocation ownership")?;
        let delivery_state = conn
            .query_row(
                "SELECT state FROM deliveries WHERE delivery_id=?1",
                [delivery_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if delivery_state.as_deref() != Some("delivering") {
            return Ok(false);
        }
        let lease = conn
            .query_row(
                "SELECT owner_instance_id, expires_at FROM agent_session_leases
                 WHERE agent_kind=?1 AND session_id=?2",
                params![
                    invocation.payload.binding.agent_kind,
                    invocation.payload.binding.session_id,
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        let Some((lease_owner, expires_at)) = lease else {
            return Ok(false);
        };
        Ok(lease_owner == owner_instance_id && parse_utc(&expires_at)? > now)
    }

    pub fn reconcile_orphaned_agent_invocations(&self, retry_delay: Duration) -> Result<u32> {
        let now = Utc::now();
        let retry_delay =
            ChronoDuration::from_std(retry_delay).unwrap_or_else(|_| ChronoDuration::seconds(30));
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let candidates = {
            let mut stmt = tx.prepare(
                "SELECT invocation_id, delivery_id, owner_instance_id, state, payload_json
                 FROM agent_invocations WHERE state IN ('starting','running') ORDER BY rowid",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut reconciled = 0u32;
        for (invocation_id, delivery_id, owner_instance_id, prior_state, invocation_json) in
            candidates
        {
            let invocation: AgentInvocationRecord = serde_json::from_str(&invocation_json)
                .context("parse orphaned agent invocation")?;
            let lease = tx
                .query_row(
                    "SELECT owner_instance_id, expires_at FROM agent_session_leases
                     WHERE agent_kind=?1 AND session_id=?2",
                    params![
                        invocation.payload.binding.agent_kind,
                        invocation.payload.binding.session_id,
                    ],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            let active_lease = match lease {
                Some((owner, expires)) => owner == owner_instance_id && parse_utc(&expires)? > now,
                None => false,
            };
            if active_lease {
                continue;
            }

            let delivery_state = tx
                .query_row(
                    "SELECT state FROM deliveries WHERE delivery_id=?1",
                    [&delivery_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .unwrap_or_else(|| "missing".into());
            let mut error: Option<String> = None;
            let invocation_state = match delivery_state.as_str() {
                "delivered" => "completed",
                "needs_rebind" => "blocked",
                "retrying" => "retrying",
                "delivering" => {
                    let message = format!(
                        "offline {} invocation ownership expired after daemon/process interruption (prior invocation state={prior_state})",
                        invocation.payload.binding.agent_kind
                    );
                    tx.execute(
                        "UPDATE deliveries
                         SET state='retrying', next_retry_at=?2, last_error=?3
                         WHERE delivery_id=?1 AND state='delivering'",
                        params![delivery_id, (now + retry_delay).to_rfc3339(), message,],
                    )?;
                    error = Some(message);
                    "failed"
                }
                "pending" => {
                    error = Some(
                        "orphaned offline invocation no longer owns its pending Delivery".into(),
                    );
                    "failed"
                }
                other => {
                    error = Some(format!(
                        "orphaned offline invocation has Delivery state {other}"
                    ));
                    "failed"
                }
            };
            tx.execute(
                "UPDATE agent_invocations SET state=?2, updated_at=?3, last_error=?4
                 WHERE invocation_id=?1 AND state IN ('starting','running')",
                params![invocation_id, invocation_state, now.to_rfc3339(), error],
            )?;
            tx.execute(
                "DELETE FROM agent_session_leases
                 WHERE agent_kind=?1 AND session_id=?2 AND owner_instance_id=?3",
                params![
                    invocation.payload.binding.agent_kind,
                    invocation.payload.binding.session_id,
                    owner_instance_id,
                ],
            )?;
            tx.execute(
                "INSERT INTO run_events(run_id, at, kind, payload_json)
                 VALUES(?1, ?2, 'agent_invocation_reconciled', ?3)",
                params![
                    invocation.payload.run_id,
                    now.to_rfc3339(),
                    serde_json::to_string(&serde_json::json!({
                        "invocation_id": invocation_id,
                        "delivery_id": delivery_id,
                        "prior_invocation_state": prior_state,
                        "delivery_state": delivery_state,
                        "reconciled_invocation_state": invocation_state,
                        "error": error,
                    }))?
                ],
            )?;
            reconciled += 1;
        }
        tx.commit()?;
        Ok(reconciled)
    }

    pub fn finish_agent_invocation_process(
        &self,
        invocation_id: &str,
        exit_code: Option<i32>,
        process_error: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = tx
            .query_row(
                "SELECT delivery_id, owner_instance_id, payload_json FROM agent_invocations WHERE invocation_id=?1",
                [invocation_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
            )
            .optional()?;
        let Some((delivery_id, owner_instance_id, invocation_json)) = row else {
            tx.commit()?;
            return Ok(());
        };
        let invocation: AgentInvocationRecord =
            serde_json::from_str(&invocation_json).context("parse agent invocation")?;
        let delivery_state = tx
            .query_row(
                "SELECT state FROM deliveries WHERE delivery_id=?1",
                [&delivery_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_else(|| "missing".into());

        let (invocation_state, last_error) = match delivery_state.as_str() {
            "delivered" => ("completed", None),
            "needs_rebind" => ("blocked", process_error.map(str::to_string)),
            "retrying" => ("retrying", process_error.map(str::to_string)),
            "delivering" => {
                let message = process_error.map(str::to_string).unwrap_or_else(|| {
                    format!(
                        "offline {} exited before durable delivery ack (exit={exit_code:?})",
                        invocation.payload.binding.agent_kind
                    )
                });
                tx.execute(
                    "UPDATE deliveries SET state='retrying', next_retry_at=?2, last_error=?3 WHERE delivery_id=?1",
                    params![
                        delivery_id,
                        (now + ChronoDuration::seconds(30)).to_rfc3339(),
                        message,
                    ],
                )?;
                ("failed", Some(message))
            }
            other => (other, process_error.map(str::to_string)),
        };
        tx.execute(
            "UPDATE agent_invocations SET state=?2, updated_at=?3, last_error=?4 WHERE invocation_id=?1",
            params![invocation_id, invocation_state, now.to_rfc3339(), last_error],
        )?;
        tx.execute(
            "DELETE FROM agent_session_leases WHERE agent_kind=?1 AND session_id=?2 AND owner_instance_id=?3",
            params![
                invocation.payload.binding.agent_kind,
                invocation.payload.binding.session_id,
                owner_instance_id,
            ],
        )?;
        tx.execute(
            "INSERT INTO run_events(run_id, at, kind, payload_json) VALUES(?1, ?2, 'agent_invocation_exit', ?3)",
            params![
                invocation.payload.run_id,
                now.to_rfc3339(),
                serde_json::to_string(&serde_json::json!({
                    "invocation_id": invocation_id,
                    "delivery_id": delivery_id,
                    "state": invocation_state,
                    "exit_code": exit_code,
                    "error": process_error,
                }))?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn finish_delivery(
        &self,
        agent_kind: &str,
        session_id: &str,
        owner_instance_id: &str,
        delivery_id: &str,
        outcome: &str,
        error: Option<&str>,
    ) -> Result<bool> {
        let now = Utc::now();
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_active_session_lease(&tx, agent_kind, session_id, owner_instance_id, now)?;
        let payload_json = tx
            .query_row(
                "SELECT payload_json FROM deliveries WHERE delivery_id=?1",
                [delivery_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(payload_json) = payload_json else {
            tx.commit()?;
            return Ok(false);
        };
        let payload: DeliveryPayload =
            serde_json::from_str(&payload_json).context("parse delivery to finish")?;
        if payload.binding.agent_kind != agent_kind || payload.binding.session_id != session_id {
            bail!("delivery does not belong to the active agent session");
        }

        let (state, next_retry_at, last_error) = match outcome {
            "delivered" => ("delivered", None, None),
            "retry" => (
                "retrying",
                Some((now + ChronoDuration::seconds(30)).to_rfc3339()),
                error.map(str::to_string),
            ),
            "needs_rebind" => (
                "needs_rebind",
                None,
                Some(
                    error
                        .unwrap_or("agent continuation binding requires explicit rebind")
                        .to_string(),
                ),
            ),
            other => bail!("unsupported delivery outcome {other}"),
        };
        let changed = tx.execute(
            "UPDATE deliveries SET state=?2, next_retry_at=?3, last_error=?4
             WHERE delivery_id=?1 AND state='delivering'",
            params![delivery_id, state, next_retry_at, last_error],
        )?;
        if changed > 0 {
            tx.execute(
                "INSERT INTO run_events(run_id, at, kind, payload_json) VALUES(?1, ?2, ?3, ?4)",
                params![
                    payload.run_id,
                    now.to_rfc3339(),
                    format!("continuation_{state}"),
                    serde_json::to_string(&serde_json::json!({
                        "delivery_id": delivery_id,
                        "outcome": outcome,
                        "error": error,
                    }))?
                ],
            )?;
        }
        tx.commit()?;
        Ok(changed > 0)
    }

    pub fn get_attempt(&self, run_id: &str, attempt_no: u32) -> Result<Option<RunAttemptRecord>> {
        let conn = self.connect()?;
        let json = conn
            .query_row(
                "SELECT record_json FROM run_attempts WHERE run_id=?1 AND attempt_no=?2",
                params![run_id, attempt_no],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        json.map(|json| serde_json::from_str(&json).context("parse runwatch.db attempt"))
            .transpose()
    }

    pub fn list_attempts(&self, run_id: &str) -> Result<Vec<RunAttemptRecord>> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare("SELECT record_json FROM run_attempts WHERE run_id=?1 ORDER BY attempt_no")?;
        let rows = stmt.query_map([run_id], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?).context("parse runwatch.db attempt list row")?);
        }
        Ok(out)
    }

    pub fn get_observation(
        &self,
        run_id: &str,
        attempt_no: u32,
    ) -> Result<Option<ObservationRecord>> {
        let conn = self.connect()?;
        let json = conn
            .query_row(
                "SELECT payload_json FROM observations WHERE run_id=?1 AND attempt_no=?2",
                params![run_id, attempt_no],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        json.map(|json| serde_json::from_str(&json).context("parse runwatch.db observation"))
            .transpose()
    }

    pub fn list_observations(&self) -> Result<Vec<ObservationRecord>> {
        let conn = self.connect()?;
        let mut stmt =
            conn.prepare("SELECT payload_json FROM observations ORDER BY run_id, attempt_no")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let json = row?;
            out.push(
                serde_json::from_str(&json).context("parse runwatch.db observation list row")?,
            );
        }
        Ok(out)
    }

    pub fn upsert_observation(&self, observation: &ObservationRecord) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous_json = tx
            .query_row(
                "SELECT payload_json FROM observations WHERE run_id=?1 AND attempt_no=?2",
                params![observation.run_id, observation.attempt_no],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let changed = match previous_json {
            Some(json) => {
                let previous: ObservationRecord = serde_json::from_str(&json)
                    .context("parse previous runwatch.db observation")?;
                previous.source != observation.source
                    || previous.health != observation.health
                    || previous.execution_status != observation.execution_status
                    || previous.raw_state != observation.raw_state
                    || previous.reason != observation.reason
                    || previous.command_exit_code != observation.command_exit_code
            }
            None => true,
        };
        let payload = serde_json::to_string(observation)?;
        tx.execute(
            "INSERT INTO observations(run_id, attempt_no, observed_at, payload_json)
             VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(run_id, attempt_no) DO UPDATE SET
               observed_at=excluded.observed_at,
               payload_json=excluded.payload_json",
            params![
                observation.run_id,
                observation.attempt_no,
                observation.observed_at.to_rfc3339(),
                payload,
            ],
        )?;
        if changed {
            tx.execute(
                "INSERT INTO run_events(run_id, at, kind, payload_json)
                 VALUES(?1, ?2, 'observation_changed', ?3)",
                params![
                    observation.run_id,
                    observation.observed_at.to_rfc3339(),
                    serde_json::to_string(observation)?,
                ],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    pub fn persist_run_attempt_event(
        &self,
        run: &RunRecord,
        attempt: &RunAttemptRecord,
        event_kind: &str,
    ) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let run_json = serde_json::to_string(run)?;
        tx.execute(
            "INSERT INTO runs(run_id, record_json, updated_at) VALUES(?1, ?2, ?3)
             ON CONFLICT(run_id) DO UPDATE SET
               record_json=excluded.record_json,
               updated_at=excluded.updated_at",
            params![run.run_id, run_json, run.updated_at.to_rfc3339()],
        )?;
        let attempt_json = serde_json::to_string(attempt)?;
        tx.execute(
            "INSERT INTO run_attempts(run_id, attempt_no, record_json) VALUES(?1, ?2, ?3)
             ON CONFLICT(run_id, attempt_no) DO UPDATE SET record_json=excluded.record_json",
            params![attempt.run_id, attempt.attempt_no, attempt_json],
        )?;
        tx.execute(
            "INSERT INTO run_events(run_id, at, kind, payload_json) VALUES(?1, ?2, ?3, ?4)",
            params![
                run.run_id,
                run.updated_at.to_rfc3339(),
                event_kind,
                serde_json::to_string(&serde_json::json!({
                    "attempt_no": attempt.attempt_no,
                    "status": attempt.status,
                    "job_id": attempt.job_id,
                    "error": attempt.error,
                }))?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn persist_submission_acceptance(
        &self,
        run: &RunRecord,
        attempt: &RunAttemptRecord,
        event_kind: &str,
    ) -> Result<bool> {
        if run.run_id != attempt.run_id || run.attempt_no != Some(attempt.attempt_no) {
            bail!("submission acceptance Run/Attempt identity mismatch");
        }
        let job_id = attempt
            .job_id
            .as_deref()
            .context("submission acceptance Attempt has no handle")?;
        if run.job_id.as_deref() != Some(job_id)
            || run.status != attempt.status
            || !matches!(run.status, RunStatus::Queued | RunStatus::Running)
        {
            bail!("submission acceptance must carry one queued/running handle snapshot");
        }

        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_run_json = tx
            .query_row(
                "SELECT record_json FROM runs WHERE run_id=?1",
                [run.run_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .with_context(|| format!("unknown run {} during submission acceptance", run.run_id))?;
        let current_attempt_json = tx
            .query_row(
                "SELECT record_json FROM run_attempts WHERE run_id=?1 AND attempt_no=?2",
                params![attempt.run_id, attempt.attempt_no],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .with_context(|| {
                format!(
                    "Run {} is missing Attempt {} during submission acceptance",
                    attempt.run_id, attempt.attempt_no
                )
            })?;
        let current_run: RunRecord =
            serde_json::from_str(&current_run_json).context("parse current submission Run")?;
        let current_attempt: RunAttemptRecord = serde_json::from_str(&current_attempt_json)
            .context("parse current submission Attempt")?;

        if current_run.attempt_no != Some(attempt.attempt_no) {
            bail!(
                "submission acceptance Attempt {} is no longer current for Run {}",
                attempt.attempt_no,
                run.run_id
            );
        }
        if let Some(existing) = current_attempt.job_id.as_deref() {
            if existing != job_id || current_run.job_id.as_deref() != Some(existing) {
                bail!(
                    "submission acceptance handle conflict for Run {} Attempt {}",
                    run.run_id,
                    attempt.attempt_no
                );
            }
            tx.commit()?;
            return Ok(false);
        }
        if current_run.job_id.is_some()
            || current_run.status != RunStatus::Submitting
            || current_attempt.status != RunStatus::Submitting
        {
            bail!("submission acceptance canonical state is not a clean Submitting snapshot");
        }
        if current_run.runner != run.runner
            || current_run.host != run.host
            || current_run.workspace != run.workspace
            || current_run.name != run.name
            || current_attempt.runner != attempt.runner
            || current_attempt.host != attempt.host
            || current_attempt.workdir != attempt.workdir
            || current_attempt.command != attempt.command
            || current_attempt.resources != attempt.resources
            || current_attempt.job_name != attempt.job_name
            || current_attempt.script_path != attempt.script_path
            || current_attempt.stdout_path != attempt.stdout_path
            || current_attempt.stderr_path != attempt.stderr_path
            || current_attempt.terminal_path != attempt.terminal_path
            || current_attempt.receipt_path != attempt.receipt_path
        {
            bail!("submission acceptance changed immutable execution identity");
        }

        tx.execute(
            "UPDATE runs SET record_json=?2, updated_at=?3 WHERE run_id=?1",
            params![
                run.run_id,
                serde_json::to_string(run)?,
                run.updated_at.to_rfc3339()
            ],
        )?;
        tx.execute(
            "UPDATE run_attempts SET record_json=?3 WHERE run_id=?1 AND attempt_no=?2",
            params![
                attempt.run_id,
                attempt.attempt_no,
                serde_json::to_string(attempt)?
            ],
        )?;
        tx.execute(
            "INSERT INTO run_events(run_id, at, kind, payload_json) VALUES(?1, ?2, ?3, ?4)",
            params![
                run.run_id,
                run.updated_at.to_rfc3339(),
                event_kind,
                serde_json::to_string(&serde_json::json!({
                    "attempt_no": attempt.attempt_no,
                    "status": attempt.status,
                    "job_id": attempt.job_id,
                    "error": attempt.error,
                }))?
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }
}

fn requeue_expired_delivery_claims(tx: &Transaction<'_>, now: DateTime<Utc>) -> Result<usize> {
    Ok(tx.execute(
        "UPDATE deliveries SET state='pending', next_retry_at=NULL,
                last_error=COALESCE(last_error, 'delivery claim expired before durable acknowledgement')
         WHERE state='delivering' AND next_retry_at IS NOT NULL AND next_retry_at<=?1",
        [now.to_rfc3339()],
    )?)
}

fn parse_utc(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("parse UTC timestamp {value}"))?
        .with_timezone(&Utc))
}

fn verify_active_session_lease_ref(
    conn: &Connection,
    agent_kind: &str,
    session_id: &str,
    owner_instance_id: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    let lease = conn
        .query_row(
            "SELECT owner_instance_id, expires_at FROM agent_session_leases
             WHERE agent_kind=?1 AND session_id=?2",
            params![agent_kind, session_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((owner, expires)) = lease else {
        bail!("agent session has no live lease");
    };
    if owner != owner_instance_id {
        bail!("agent session lease is owned by another instance");
    }
    if parse_utc(&expires)? <= now {
        bail!("agent session lease expired");
    }
    Ok(())
}

fn verify_active_session_lease(
    tx: &Transaction<'_>,
    agent_kind: &str,
    session_id: &str,
    owner_instance_id: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    let lease = tx
        .query_row(
            "SELECT owner_instance_id, expires_at FROM agent_session_leases
             WHERE agent_kind=?1 AND session_id=?2",
            params![agent_kind, session_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((owner, expires)) = lease else {
        bail!("agent session has no live lease");
    };
    if owner != owner_instance_id {
        bail!("agent session lease is owned by another instance");
    }
    if parse_utc(&expires)? <= now {
        bail!("agent session lease expired");
    }
    Ok(())
}

fn read_legacy_latest(path: &Path) -> Result<Vec<RunRecord>> {
    let file = File::open(path)?;
    let lines = BufReader::new(file)
        .lines()
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut ordered_ids = Vec::new();
    let mut latest = HashMap::new();
    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<RunRecord>(line) {
            Ok(row) => {
                if !latest.contains_key(&row.run_id) {
                    ordered_ids.push(row.run_id.clone());
                }
                latest.insert(row.run_id.clone(), row);
            }
            Err(_) if index + 1 == lines.len() => {
                // R1a append journal may have a partial final line after a crash.
            }
            Err(err) => return Err(err).context("parse legacy runs.jsonl journal"),
        }
    }
    Ok(ordered_ids
        .into_iter()
        .filter_map(|run_id| latest.remove(&run_id))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RunnerKind;
    use std::io::Write;

    fn test_paths(name: &str) -> (PathBuf, PathBuf) {
        let stem = format!(
            "runwatch-store-{name}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let dir = std::env::temp_dir().join(stem);
        fs::create_dir_all(&dir).expect("create test dir");
        (dir.join("runwatch.db"), dir.join("runs.jsonl"))
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn sqlite_store_returns_latest_record_per_run() {
        let (db, legacy) = test_paths("latest");
        let store = RunStore::open_at(db.clone(), legacy).expect("open store");
        let mut row = RunRecord::new("r1".into(), "host".into(), RunnerKind::Slurm);
        store.upsert(&row).expect("first upsert");
        row.status = RunStatus::Running;
        store.upsert(&row).expect("second upsert");
        let rows = store.list().expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, RunStatus::Running);
        cleanup(&db);
    }

    #[test]
    fn submission_acceptance_is_idempotent_and_never_regresses_advanced_state() {
        use crate::types::{RemoteWorkspaceRef, RunAttemptRecord, RunResources};

        let (db, legacy) = test_paths("submission-acceptance-cas");
        let store = RunStore::open_at(db.clone(), legacy).expect("open store");
        let now = Utc::now();
        let workspace = RemoteWorkspaceRef {
            host_alias: "cluster".into(),
            cwd: "/shared/project".into(),
        };
        let mut submitting = RunRecord::new("r-accept".into(), "cluster".into(), RunnerKind::Slurm);
        submitting.status = RunStatus::Submitting;
        submitting.workspace = Some(workspace.clone());
        submitting.attempt_no = Some(1);
        submitting.updated_at = now;
        let submitting_attempt = RunAttemptRecord {
            run_id: submitting.run_id.clone(),
            attempt_no: 1,
            runner: RunnerKind::Slurm,
            host: "cluster".into(),
            workdir: workspace.cwd.clone(),
            command: "science --run".into(),
            resources: RunResources::default(),
            job_name: "rw-r-accept-a1".into(),
            job_id: None,
            script_path: "/shared/project/.runwatch/r-accept/attempt-1/run.sh".into(),
            stdout_path: "/shared/project/.runwatch/r-accept/attempt-1/stdout.log".into(),
            stderr_path: "/shared/project/.runwatch/r-accept/attempt-1/stderr.log".into(),
            terminal_path: "/shared/project/.runwatch/r-accept/attempt-1/terminal.json".into(),
            receipt_path: "/shared/project/.runwatch/r-accept/attempt-1/submission.receipt".into(),
            status: RunStatus::Submitting,
            created_at: now,
            updated_at: now,
            error: None,
        };
        assert!(
            store
                .create_submission_intent(&submitting, &submitting_attempt, None)
                .unwrap()
        );

        let mut queued = submitting.clone();
        queued.status = RunStatus::Queued;
        queued.job_id = Some("31852".into());
        queued.updated_at += ChronoDuration::seconds(1);
        let mut queued_attempt = submitting_attempt.clone();
        queued_attempt.status = RunStatus::Queued;
        queued_attempt.job_id = Some("31852".into());
        queued_attempt.updated_at = queued.updated_at;
        assert!(
            store
                .persist_submission_acceptance(&queued, &queued_attempt, "scheduler_submitted")
                .unwrap()
        );
        assert!(
            !store
                .persist_submission_acceptance(&queued, &queued_attempt, "scheduler_submitted")
                .unwrap()
        );
        assert_eq!(
            store
                .list_run_events(&queued.run_id, 50)
                .unwrap()
                .into_iter()
                .filter(|event| event.kind == "scheduler_submitted")
                .count(),
            1
        );

        let mut running = queued.clone();
        running.status = RunStatus::Running;
        running.updated_at += ChronoDuration::seconds(1);
        let mut running_attempt = queued_attempt.clone();
        running_attempt.status = RunStatus::Running;
        running_attempt.updated_at = running.updated_at;
        store
            .persist_run_attempt_event(&running, &running_attempt, "scheduler_observed")
            .unwrap();

        assert!(
            !store
                .persist_submission_acceptance(&queued, &queued_attempt, "scheduler_submitted")
                .unwrap()
        );
        let canonical = store.get(&queued.run_id).unwrap().unwrap();
        assert_eq!(canonical.status, RunStatus::Running);
        assert_eq!(canonical.job_id.as_deref(), Some("31852"));
        let canonical_attempt = store.get_attempt(&queued.run_id, 1).unwrap().unwrap();
        assert_eq!(canonical_attempt.status, RunStatus::Running);
        assert_eq!(
            store
                .list_run_events(&queued.run_id, 50)
                .unwrap()
                .into_iter()
                .filter(|event| event.kind == "scheduler_submitted")
                .count(),
            1
        );

        let mut conflict_run = queued.clone();
        conflict_run.job_id = Some("99999".into());
        let mut conflict_attempt = queued_attempt.clone();
        conflict_attempt.job_id = Some("99999".into());
        let error = store
            .persist_submission_acceptance(&conflict_run, &conflict_attempt, "scheduler_submitted")
            .unwrap_err()
            .to_string();
        assert!(error.contains("handle conflict"), "{error}");
        cleanup(&db);
    }

    #[test]
    fn gui_read_models_are_bounded_and_do_not_expose_binding_paths() {
        use crate::types::{
            ContinuationBinding, DeliveryPayload, RemoteWorkspaceRef, RunAttemptRecord,
            RunResources,
        };

        let (db, legacy) = test_paths("gui-read-models");
        let store = RunStore::open_at(db.clone(), legacy).expect("open store");
        let now = Utc::now();
        let workspace = RemoteWorkspaceRef {
            host_alias: "gm00".into(),
            cwd: "/share/project".into(),
        };
        let binding = ContinuationBinding {
            agent_kind: "pi".into(),
            session_id: "session-readable-id".into(),
            session_file: Some("C:/private/pi/session.jsonl".into()),
            origin_leaf_id: Some("private-origin-leaf".into()),
            project_root: "C:/science".into(),
            workspace: workspace.clone(),
            adapter_path: Some("C:/private/pi-runs/extensions/runs/index.ts".into()),
        };
        let mut run = RunRecord::new("r-gui".into(), "gm00".into(), RunnerKind::Slurm);
        run.status = RunStatus::Running;
        run.workspace = Some(workspace.clone());
        run.attempt_no = Some(1);
        run.updated_at = now;
        let attempt = RunAttemptRecord {
            run_id: run.run_id.clone(),
            attempt_no: 1,
            runner: RunnerKind::Slurm,
            host: "gm00".into(),
            workdir: workspace.cwd.clone(),
            command: "science --run".into(),
            resources: RunResources::default(),
            job_name: "science".into(),
            job_id: Some("31842".into()),
            script_path: "/share/project/.runwatch/r-gui/run.sh".into(),
            stdout_path: "/share/project/.runwatch/r-gui/stdout.log".into(),
            stderr_path: "/share/project/.runwatch/r-gui/stderr.log".into(),
            terminal_path: "/share/project/.runwatch/r-gui/terminal.json".into(),
            receipt_path: "/share/project/.runwatch/r-gui/receipt.json".into(),
            status: RunStatus::Running,
            created_at: now,
            updated_at: now,
            error: None,
        };
        assert!(
            store
                .create_submission_intent(&run, &attempt, Some(&binding))
                .unwrap()
        );
        for index in 0..250 {
            let mut updated = run.clone();
            updated.updated_at = now + ChronoDuration::seconds(index);
            store
                .persist_run_attempt_event(&updated, &attempt, &format!("gui_event_{index:03}"))
                .unwrap();
        }
        let events = store.list_run_events(&run.run_id, usize::MAX).unwrap();
        assert_eq!(events.len(), 200);
        assert_eq!(events.first().unwrap().kind, "gui_event_050");
        assert_eq!(events.last().unwrap().kind, "gui_event_249");
        assert!(store.list_run_events("missing-run", 20).is_err());

        let payload = DeliveryPayload {
            delivery_id: "d-retry".into(),
            run_id: run.run_id.clone(),
            attempt_no: 1,
            status: RunStatus::Succeeded,
            job_id: Some("31842".into()),
            workspace,
            binding,
            created_at: now,
        };
        let conn = store.connect().unwrap();
        conn.execute(
            "INSERT INTO deliveries(delivery_id, run_id, state, attempts, last_error, payload_json)
             VALUES(?1, ?2, 'retrying', 1, 'provider retry', ?3)",
            params![
                payload.delivery_id,
                payload.run_id,
                serde_json::to_string(&payload).unwrap()
            ],
        )
        .unwrap();
        let mut blocked = payload.clone();
        blocked.delivery_id = "d-rebind".into();
        conn.execute(
            "INSERT INTO deliveries(delivery_id, run_id, state, attempts, last_error, payload_json)
             VALUES(?1, ?2, 'needs_rebind', 2, 'branch changed', ?3)",
            params![
                blocked.delivery_id,
                blocked.run_id,
                serde_json::to_string(&blocked).unwrap()
            ],
        )
        .unwrap();
        drop(conn);

        let status = store.get_run_continuation_status(&run.run_id).unwrap();
        assert_eq!(status.run_id, run.run_id);
        assert!(status.configured);
        assert_eq!(status.agent_kind.as_deref(), Some("pi"));
        assert_eq!(status.session_id.as_deref(), Some("session-readable-id"));
        assert_eq!(status.retrying, 1);
        assert_eq!(status.needs_rebind, 1);
        assert_eq!(status.last_state.as_deref(), Some("needs_rebind"));
        assert_eq!(status.last_error.as_deref(), Some("branch changed"));
        let listed = store.list_run_continuation_statuses().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], status);
        let encoded = serde_json::to_string(&status).unwrap();
        assert!(!encoded.contains("session_file"));
        assert!(!encoded.contains("private-origin-leaf"));
        assert!(!encoded.contains("adapter_path"));
        assert!(!encoded.contains("C:/private"));
        assert!(store.get_run_continuation_status("missing-run").is_err());
        cleanup(&db);
    }

    #[test]
    fn imports_latest_legacy_rows_and_tolerates_partial_final_line() {
        let (db, legacy) = test_paths("migration");
        let mut row = RunRecord::new("r1".into(), "host".into(), RunnerKind::Slurm);
        let mut file = File::create(&legacy).expect("create legacy");
        let mut legacy_json = serde_json::to_value(&row).unwrap();
        let legacy_object = legacy_json.as_object_mut().unwrap();
        legacy_object.insert("on_complete".into(), serde_json::json!("spawn"));
        legacy_object.insert("on_success".into(), serde_json::json!("echo old-success"));
        legacy_object.insert("on_failure".into(), serde_json::json!("echo old-failure"));
        legacy_object.insert("acked_at".into(), serde_json::json!("2026-08-01T00:00:00Z"));
        writeln!(file, "{}", serde_json::to_string(&legacy_json).unwrap()).unwrap();
        row.status = RunStatus::Running;
        let mut legacy_json = serde_json::to_value(&row).unwrap();
        let legacy_object = legacy_json.as_object_mut().unwrap();
        legacy_object.insert("on_complete".into(), serde_json::json!("event"));
        legacy_object.insert("on_success".into(), serde_json::json!("echo old-success"));
        legacy_object.insert("on_failure".into(), serde_json::json!("echo old-failure"));
        legacy_object.insert("acked_at".into(), serde_json::json!(null));
        writeln!(file, "{}", serde_json::to_string(&legacy_json).unwrap()).unwrap();
        write!(file, "{{\"run_id\":").unwrap();
        file.sync_all().unwrap();
        drop(file);

        let store = RunStore::open_at(db.clone(), legacy.clone()).expect("open migrated store");
        let rows = store.list().expect("list migrated rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, RunStatus::Running);
        assert_eq!(store.path(), &db);
        assert_eq!(store.legacy_path(), &legacy);
        cleanup(&db);
    }

    #[test]
    fn submission_intent_is_atomic_and_idempotent_by_run_id() {
        use crate::types::{RemoteWorkspaceRef, RunResources};

        let (db, legacy) = test_paths("submission-intent");
        let store = RunStore::open_at(db.clone(), legacy).expect("open store");
        let now = Utc::now();
        let workspace = RemoteWorkspaceRef {
            host_alias: "cluster".into(),
            cwd: "/shared/project".into(),
        };
        let mut run = RunRecord::new("r-submit".into(), "cluster".into(), RunnerKind::Slurm);
        run.status = RunStatus::Submitting;
        run.workspace = Some(workspace);
        run.attempt_no = Some(1);
        run.updated_at = now;
        let attempt = RunAttemptRecord {
            run_id: run.run_id.clone(),
            attempt_no: 1,
            runner: RunnerKind::Slurm,
            host: "cluster".into(),
            workdir: "/shared/project".into(),
            command: "python run.py".into(),
            resources: RunResources::default(),
            job_name: "rw-r-submit-a1".into(),
            job_id: None,
            script_path: "/shared/project/.runwatch/r-submit/attempt-1.sh".into(),
            stdout_path: "/shared/project/.runwatch/r-submit/stdout.log".into(),
            stderr_path: "/shared/project/.runwatch/r-submit/stderr.log".into(),
            terminal_path: "/shared/project/.runwatch/r-submit/terminal.json".into(),
            receipt_path: "/shared/project/.runwatch/r-submit/submission.receipt".into(),
            status: RunStatus::Submitting,
            created_at: now,
            updated_at: now,
            error: None,
        };

        assert!(
            store
                .create_submission_intent(&run, &attempt, None)
                .unwrap()
        );
        assert!(
            !store
                .create_submission_intent(&run, &attempt, None)
                .unwrap()
        );
        assert_eq!(
            store.get("r-submit").unwrap().unwrap().status,
            RunStatus::Submitting
        );
        assert_eq!(
            store.get_attempt("r-submit", 1).unwrap(),
            Some(attempt.clone())
        );
        let mut run2 = run.clone();
        run2.attempt_no = Some(2);
        let mut attempt2 = attempt.clone();
        attempt2.attempt_no = 2;
        attempt2.job_name = "rw-r-submit-a2".into();
        attempt2.script_path = "/shared/project/.runwatch/r-submit/attempt-2/run.sh".into();
        store
            .persist_run_attempt_event(&run2, &attempt2, "retry_intent_test")
            .unwrap();
        assert_eq!(
            store
                .list_attempts("r-submit")
                .unwrap()
                .into_iter()
                .map(|value| value.attempt_no)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        cleanup(&db);
    }

    #[test]
    fn retry_intent_is_idempotent_and_preserves_previous_attempt() {
        use crate::types::{RemoteWorkspaceRef, RetryRunSpec, RunResources};

        let (db, legacy) = test_paths("retry-intent");
        let store = RunStore::open_at(db.clone(), legacy).expect("open store");
        let now = Utc::now();
        let workspace = RemoteWorkspaceRef {
            host_alias: "cluster".into(),
            cwd: "/shared/project".into(),
        };
        let mut run = RunRecord::new("r-retry".into(), "cluster".into(), RunnerKind::Slurm);
        run.status = RunStatus::Submitting;
        run.workspace = Some(workspace);
        run.attempt_no = Some(1);
        run.remote_terminal = Some("/shared/project/.runwatch/r-retry/terminal.json".into());
        run.updated_at = now;
        let mut attempt = RunAttemptRecord {
            run_id: run.run_id.clone(),
            attempt_no: 1,
            runner: RunnerKind::Slurm,
            host: "cluster".into(),
            workdir: "/shared/project".into(),
            command: "python run.py".into(),
            resources: RunResources::default(),
            job_name: "rw-r-retry-a1".into(),
            job_id: Some("1001".into()),
            script_path: "/shared/project/.runwatch/r-retry/attempt-1.sh".into(),
            stdout_path: "/shared/project/.runwatch/r-retry/stdout.log".into(),
            stderr_path: "/shared/project/.runwatch/r-retry/stderr.log".into(),
            terminal_path: "/shared/project/.runwatch/r-retry/terminal.json".into(),
            receipt_path: "/shared/project/.runwatch/r-retry/submission.receipt".into(),
            status: RunStatus::Submitting,
            created_at: now,
            updated_at: now,
            error: None,
        };
        assert!(
            store
                .create_submission_intent(&run, &attempt, None)
                .unwrap()
        );
        run.status = RunStatus::Failed;
        run.updated_at += ChronoDuration::seconds(1);
        run.note = Some("attempt 1 failed".into());
        attempt.status = RunStatus::Failed;
        attempt.updated_at = run.updated_at;
        store
            .persist_run_attempt_event(&run, &attempt, "terminal_test")
            .unwrap();

        let retry = RetryRunSpec {
            run_id: run.run_id.clone(),
            expected_attempt_no: 1,
            request_id: "retry-request-01".into(),
            resources: Some(RunResources {
                time: Some("02:00:00".into()),
                mem: Some("64G".into()),
                ..RunResources::default()
            }),
        };
        let mut run2 = run.clone();
        run2.attempt_no = Some(2);
        run2.job_id = None;
        run2.status = RunStatus::Submitting;
        run2.remote_terminal =
            Some("/shared/project/.runwatch/r-retry/attempt-2/terminal.json".into());
        run2.updated_at += ChronoDuration::seconds(1);
        run2.note = None;
        let mut attempt2 = attempt.clone();
        attempt2.attempt_no = 2;
        attempt2.resources = retry.resources.clone().unwrap();
        attempt2.job_name = "rw-r-retry-a2".into();
        attempt2.job_id = None;
        attempt2.script_path = "/shared/project/.runwatch/r-retry/attempt-2/run.sh".into();
        attempt2.stdout_path = "/shared/project/.runwatch/r-retry/attempt-2/stdout.log".into();
        attempt2.stderr_path = "/shared/project/.runwatch/r-retry/attempt-2/stderr.log".into();
        attempt2.terminal_path = "/shared/project/.runwatch/r-retry/attempt-2/terminal.json".into();
        attempt2.receipt_path =
            "/shared/project/.runwatch/r-retry/attempt-2/submission.receipt".into();
        attempt2.status = RunStatus::Submitting;
        attempt2.created_at = run2.updated_at;
        attempt2.updated_at = run2.updated_at;
        attempt2.error = None;

        assert!(store.create_retry_intent(&retry, &run2, &attempt2).unwrap());
        assert!(!store.create_retry_intent(&retry, &run2, &attempt2).unwrap());
        assert_eq!(
            store.list_pending_retry_intents().unwrap(),
            vec![retry.clone()]
        );
        let attempts = store.list_attempts(&run.run_id).unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].attempt_no, 1);
        assert_eq!(attempts[0].status, RunStatus::Failed);
        assert_eq!(
            attempts[0].stdout_path,
            "/shared/project/.runwatch/r-retry/stdout.log"
        );
        assert_eq!(attempts[1].attempt_no, 2);
        assert_eq!(attempts[1].status, RunStatus::Submitting);
        assert_ne!(attempts[0].stdout_path, attempts[1].stdout_path);
        let current = store.get(&run.run_id).unwrap().unwrap();
        assert_eq!(current.attempt_no, Some(2));
        assert_eq!(current.status, RunStatus::Submitting);
        let retry_events = store
            .list_run_events(&run.run_id, 100)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == "retry_intent")
            .count();
        assert_eq!(retry_events, 1);

        let stale = RetryRunSpec {
            request_id: "retry-request-02".into(),
            ..retry.clone()
        };
        let error = store
            .create_retry_intent(&stale, &run2, &attempt2)
            .unwrap_err()
            .to_string();
        assert!(error.contains("expected attempt 1"), "{error}");

        let mut queued_run = run2.clone();
        queued_run.status = RunStatus::Queued;
        queued_run.job_id = Some("2002".into());
        queued_run.updated_at += ChronoDuration::seconds(1);
        let mut queued_attempt = attempt2.clone();
        queued_attempt.status = RunStatus::Queued;
        queued_attempt.job_id = Some("2002".into());
        queued_attempt.updated_at = queued_run.updated_at;
        store
            .persist_run_attempt_event(&queued_run, &queued_attempt, "retry_recovered_test")
            .unwrap();
        assert!(store.list_pending_retry_intents().unwrap().is_empty());
        cleanup(&db);
    }

    #[test]
    fn retry_intent_rejects_agent_bound_run() {
        use crate::types::{ContinuationBinding, RemoteWorkspaceRef, RetryRunSpec, RunResources};

        let (db, legacy) = test_paths("retry-bound");
        let store = RunStore::open_at(db.clone(), legacy).expect("open store");
        let now = Utc::now();
        let workspace = RemoteWorkspaceRef {
            host_alias: "cluster".into(),
            cwd: "/shared/project".into(),
        };
        let binding = ContinuationBinding {
            agent_kind: "pi".into(),
            session_id: "s-bound".into(),
            session_file: Some("C:/sessions/s-bound.jsonl".into()),
            origin_leaf_id: Some("leaf-bound".into()),
            project_root: "C:/science".into(),
            workspace: workspace.clone(),
            adapter_path: Some("C:/pi-runs/extensions/runs/index.ts".into()),
        };
        let mut run = RunRecord::new("r-bound-retry".into(), "cluster".into(), RunnerKind::Slurm);
        run.status = RunStatus::Failed;
        run.workspace = Some(workspace);
        run.attempt_no = Some(1);
        run.updated_at = now;
        let attempt = RunAttemptRecord {
            run_id: run.run_id.clone(),
            attempt_no: 1,
            runner: RunnerKind::Slurm,
            host: "cluster".into(),
            workdir: "/shared/project".into(),
            command: "python run.py".into(),
            resources: RunResources::default(),
            job_name: "rw-r-bound-retry-a1".into(),
            job_id: Some("1002".into()),
            script_path: "/shared/project/.runwatch/r-bound-retry/attempt-1.sh".into(),
            stdout_path: "/shared/project/.runwatch/r-bound-retry/stdout.log".into(),
            stderr_path: "/shared/project/.runwatch/r-bound-retry/stderr.log".into(),
            terminal_path: "/shared/project/.runwatch/r-bound-retry/terminal.json".into(),
            receipt_path: "/shared/project/.runwatch/r-bound-retry/submission.receipt".into(),
            status: RunStatus::Failed,
            created_at: now,
            updated_at: now,
            error: Some("failed".into()),
        };
        assert!(
            store
                .create_submission_intent(&run, &attempt, Some(&binding))
                .unwrap()
        );
        let retry = RetryRunSpec {
            run_id: run.run_id.clone(),
            expected_attempt_no: 1,
            request_id: "retry-bound-01".into(),
            resources: None,
        };
        let mut run2 = run.clone();
        run2.attempt_no = Some(2);
        run2.job_id = None;
        run2.status = RunStatus::Submitting;
        run2.updated_at += ChronoDuration::seconds(1);
        let mut attempt2 = attempt.clone();
        attempt2.attempt_no = 2;
        attempt2.job_name = "rw-r-bound-retry-a2".into();
        attempt2.job_id = None;
        attempt2.status = RunStatus::Submitting;
        attempt2.created_at = run2.updated_at;
        attempt2.updated_at = run2.updated_at;
        attempt2.error = None;
        let error = store
            .create_retry_intent(&retry, &run2, &attempt2)
            .unwrap_err()
            .to_string();
        assert!(error.contains("requires an unbound Run"), "{error}");
        assert_eq!(store.list_attempts(&run.run_id).unwrap().len(), 1);
        cleanup(&db);
    }

    #[test]
    fn retry_intent_rejects_orphaned_agent_identity_without_binding() {
        use crate::types::{RemoteWorkspaceRef, RetryRunSpec, RunResources};

        let (db, legacy) = test_paths("retry-orphan-agent-identity");
        let store = RunStore::open_at(db.clone(), legacy).expect("open store");
        let now = Utc::now();
        let workspace = RemoteWorkspaceRef {
            host_alias: "cluster".into(),
            cwd: "/shared/project".into(),
        };
        let mut run = RunRecord::new(
            "r-orphan-agent-retry".into(),
            "cluster".into(),
            RunnerKind::Slurm,
        );
        run.status = RunStatus::Failed;
        run.workspace = Some(workspace.clone());
        run.attempt_no = Some(1);
        run.session_id = Some("orphan-session".into());
        run.agent = Some("pi".into());
        run.project_root = Some("C:/science".into());
        run.updated_at = now;
        let attempt = RunAttemptRecord {
            run_id: run.run_id.clone(),
            attempt_no: 1,
            runner: RunnerKind::Slurm,
            host: "cluster".into(),
            workdir: workspace.cwd.clone(),
            command: "python run.py".into(),
            resources: RunResources::default(),
            job_name: "rw-r-orphan-agent-retry-a1".into(),
            job_id: Some("1003".into()),
            script_path: "/shared/project/.runwatch/r-orphan-agent-retry/attempt-1/run.sh".into(),
            stdout_path: "/shared/project/.runwatch/r-orphan-agent-retry/attempt-1/stdout.log"
                .into(),
            stderr_path: "/shared/project/.runwatch/r-orphan-agent-retry/attempt-1/stderr.log"
                .into(),
            terminal_path: "/shared/project/.runwatch/r-orphan-agent-retry/attempt-1/terminal.json"
                .into(),
            receipt_path:
                "/shared/project/.runwatch/r-orphan-agent-retry/attempt-1/submission.receipt".into(),
            status: RunStatus::Failed,
            created_at: now,
            updated_at: now,
            error: Some("failed".into()),
        };
        assert!(
            store
                .create_submission_intent(&run, &attempt, None)
                .unwrap()
        );

        let retry = RetryRunSpec {
            run_id: run.run_id.clone(),
            expected_attempt_no: 1,
            request_id: "retry-orphan-agent-01".into(),
            resources: None,
        };
        let mut desired_run = run.clone();
        desired_run.attempt_no = Some(2);
        desired_run.job_id = None;
        desired_run.status = RunStatus::Submitting;
        desired_run.session_id = None;
        desired_run.agent = None;
        desired_run.project_root = None;
        desired_run.updated_at += ChronoDuration::seconds(1);
        let mut desired_attempt = attempt.clone();
        desired_attempt.attempt_no = 2;
        desired_attempt.job_name = "rw-r-orphan-agent-retry-a2".into();
        desired_attempt.job_id = None;
        desired_attempt.status = RunStatus::Submitting;
        desired_attempt.script_path =
            "/shared/project/.runwatch/r-orphan-agent-retry/attempt-2/run.sh".into();
        desired_attempt.stdout_path =
            "/shared/project/.runwatch/r-orphan-agent-retry/attempt-2/stdout.log".into();
        desired_attempt.stderr_path =
            "/shared/project/.runwatch/r-orphan-agent-retry/attempt-2/stderr.log".into();
        desired_attempt.terminal_path =
            "/shared/project/.runwatch/r-orphan-agent-retry/attempt-2/terminal.json".into();
        desired_attempt.receipt_path =
            "/shared/project/.runwatch/r-orphan-agent-retry/attempt-2/submission.receipt".into();
        desired_attempt.created_at = desired_run.updated_at;
        desired_attempt.updated_at = desired_run.updated_at;
        desired_attempt.error = None;

        let error = store
            .create_retry_intent(&retry, &desired_run, &desired_attempt)
            .unwrap_err()
            .to_string();
        assert!(error.contains("carries agent identity"), "{error}");
        assert_eq!(store.list_attempts(&run.run_id).unwrap().len(), 1);
        assert!(store.list_pending_retry_intents().unwrap().is_empty());
        cleanup(&db);
    }

    #[test]
    fn observation_snapshot_refreshes_timestamp_but_events_only_on_semantic_change() {
        use crate::types::{ObservationHealth, ObservationSource};

        let (db, legacy) = test_paths("observation-snapshot");
        let store = RunStore::open_at(db.clone(), legacy).expect("open store");
        let first = ObservationRecord {
            run_id: "r-observe".into(),
            attempt_no: 1,
            observed_at: Utc::now(),
            source: ObservationSource::Scheduler,
            health: ObservationHealth::Fresh,
            execution_status: RunStatus::Running,
            raw_state: Some("RUNNING".into()),
            reason: None,
            command_exit_code: Some(0),
        };
        assert!(store.upsert_observation(&first).unwrap());

        let mut refreshed = first.clone();
        refreshed.observed_at = first.observed_at + ChronoDuration::seconds(30);
        assert!(!store.upsert_observation(&refreshed).unwrap());
        assert_eq!(
            store.get_observation("r-observe", 1).unwrap(),
            Some(refreshed.clone())
        );

        let mut failed = refreshed;
        failed.observed_at += ChronoDuration::seconds(30);
        failed.health = ObservationHealth::Unreachable;
        failed.reason = Some("SSH timeout".into());
        failed.command_exit_code = None;
        assert!(store.upsert_observation(&failed).unwrap());

        let conn = store.connect().unwrap();
        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM run_events WHERE run_id='r-observe' AND kind='observation_changed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(events, 2);
        cleanup(&db);
    }

    #[test]
    fn submission_intent_persists_continuation_binding_atomically() {
        use crate::types::{ContinuationBinding, RemoteWorkspaceRef, RunResources};

        let (db, legacy) = test_paths("submission-binding");
        let store = RunStore::open_at(db.clone(), legacy).expect("open store");
        let now = Utc::now();
        let workspace = RemoteWorkspaceRef {
            host_alias: "cluster".into(),
            cwd: "/shared/project".into(),
        };
        let mut run = RunRecord::new("r-binding".into(), "cluster".into(), RunnerKind::Slurm);
        run.status = RunStatus::Submitting;
        run.workspace = Some(workspace.clone());
        run.attempt_no = Some(1);
        run.session_id = Some("session-1".into());
        run.agent = Some("pi".into());
        run.project_root = Some("C:/science".into());
        run.updated_at = now;
        let attempt = RunAttemptRecord {
            run_id: run.run_id.clone(),
            attempt_no: 1,
            runner: RunnerKind::Slurm,
            host: "cluster".into(),
            workdir: "/shared/project".into(),
            command: "python run.py".into(),
            resources: RunResources::default(),
            job_name: "rw-r-binding-a1".into(),
            job_id: None,
            script_path: "/shared/project/.runwatch/r-binding/attempt-1.sh".into(),
            stdout_path: "/shared/project/.runwatch/r-binding/stdout.log".into(),
            stderr_path: "/shared/project/.runwatch/r-binding/stderr.log".into(),
            terminal_path: "/shared/project/.runwatch/r-binding/terminal.json".into(),
            receipt_path: "/shared/project/.runwatch/r-binding/submission.receipt".into(),
            status: RunStatus::Submitting,
            created_at: now,
            updated_at: now,
            error: None,
        };
        let binding = ContinuationBinding {
            agent_kind: "pi".into(),
            session_id: "session-1".into(),
            session_file: Some("C:/sessions/s1.jsonl".into()),
            origin_leaf_id: Some("leaf-1".into()),
            project_root: "C:/science".into(),
            workspace,
            adapter_path: Some("C:/pi-runs/extensions/runs/index.ts".into()),
        };

        assert!(
            store
                .create_submission_intent(&run, &attempt, Some(&binding))
                .unwrap()
        );
        assert_eq!(
            store.get_continuation_binding("r-binding").unwrap(),
            Some(binding)
        );
        cleanup(&db);
    }

    #[test]
    fn live_session_lease_prevents_second_owner() {
        use crate::types::AgentSessionRegistration;

        let (db, legacy) = test_paths("session-lease");
        let store = RunStore::open_at(db.clone(), legacy).expect("open store");
        let first = AgentSessionRegistration {
            agent_kind: "pi".into(),
            session_id: "s1".into(),
            owner_instance_id: "owner-a".into(),
            session_file: Some("C:/sessions/s1.jsonl".into()),
            project_root: "C:/science".into(),
            current_leaf_id: Some("leaf-a".into()),
        };
        store
            .register_agent_session(&first, Duration::from_secs(30))
            .expect("first lease");
        store
            .register_agent_session(&first, Duration::from_secs(30))
            .expect("same owner refresh");
        let mut second = first.clone();
        second.owner_instance_id = "owner-b".into();
        assert!(
            store
                .register_agent_session(&second, Duration::from_secs(30))
                .is_err()
        );
        assert!(store.release_agent_session("pi", "s1", "owner-a").unwrap());
        store
            .register_agent_session(&second, Duration::from_secs(30))
            .expect("new owner after release");
        cleanup(&db);
    }

    #[test]
    fn terminal_delivery_is_deterministic_claimed_and_acked() {
        use crate::types::{
            AgentSessionRegistration, ContinuationBinding, RemoteWorkspaceRef, RunResources,
        };

        let (db, legacy) = test_paths("delivery");
        let store = RunStore::open_at(db.clone(), legacy).expect("open store");
        let now = Utc::now();
        let workspace = RemoteWorkspaceRef {
            host_alias: "cluster".into(),
            cwd: "/shared/project".into(),
        };
        let binding = ContinuationBinding {
            agent_kind: "pi".into(),
            session_id: "s1".into(),
            session_file: Some("C:/sessions/s1.jsonl".into()),
            origin_leaf_id: Some("leaf-origin".into()),
            project_root: "C:/science".into(),
            workspace: workspace.clone(),
            adapter_path: Some("C:/pi-runs/extensions/runs/index.ts".into()),
        };
        let mut run = RunRecord::new("r-delivery".into(), "cluster".into(), RunnerKind::Slurm);
        run.status = RunStatus::Submitting;
        run.workspace = Some(workspace);
        run.attempt_no = Some(1);
        run.session_id = Some("s1".into());
        run.agent = Some("pi".into());
        run.updated_at = now;
        let attempt = RunAttemptRecord {
            run_id: run.run_id.clone(),
            attempt_no: 1,
            runner: RunnerKind::Slurm,
            host: "cluster".into(),
            workdir: "/shared/project".into(),
            command: "python run.py".into(),
            resources: RunResources::default(),
            job_name: "rw-r-delivery-a1".into(),
            job_id: Some("123".into()),
            script_path: "/shared/project/.runwatch/r-delivery/attempt-1.sh".into(),
            stdout_path: "/shared/project/.runwatch/r-delivery/stdout.log".into(),
            stderr_path: "/shared/project/.runwatch/r-delivery/stderr.log".into(),
            terminal_path: "/shared/project/.runwatch/r-delivery/terminal.json".into(),
            receipt_path: "/shared/project/.runwatch/r-delivery/submission.receipt".into(),
            status: RunStatus::Queued,
            created_at: now,
            updated_at: now,
            error: None,
        };
        assert!(
            store
                .create_submission_intent(&run, &attempt, Some(&binding))
                .unwrap()
        );
        run.status = RunStatus::Succeeded;
        run.job_id = Some("123".into());
        run.updated_at = Utc::now();
        store.upsert(&run).unwrap();
        let first = store.ensure_terminal_delivery(&run).unwrap().unwrap();
        let second = store.ensure_terminal_delivery(&run).unwrap().unwrap();
        assert_eq!(first, second);

        let registration = AgentSessionRegistration {
            agent_kind: "pi".into(),
            session_id: "s1".into(),
            owner_instance_id: "owner-a".into(),
            session_file: binding.session_file.clone(),
            project_root: binding.project_root.clone(),
            current_leaf_id: Some("leaf-origin".into()),
        };
        store
            .register_agent_session(&registration, Duration::from_secs(30))
            .unwrap();
        let claimed = store.claim_deliveries("pi", "s1", "owner-a", 8).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].payload.run_id, "r-delivery");
        assert!(
            store
                .finish_delivery("pi", "s1", "owner-a", &first, "delivered", None)
                .unwrap()
        );
        assert!(
            store
                .claim_deliveries("pi", "s1", "owner-a", 8)
                .unwrap()
                .is_empty()
        );
        cleanup(&db);
    }

    #[test]
    fn expired_live_delivery_claim_is_recovered_by_offline_reservation() {
        use crate::types::{ContinuationBinding, RemoteWorkspaceRef};

        let (db, legacy) = test_paths("expired-live-claim-offline-recovery");
        let store = RunStore::open_at(db.clone(), legacy).expect("open store");
        let now = Utc::now();
        let workspace = RemoteWorkspaceRef {
            host_alias: "cluster".into(),
            cwd: "/shared/project".into(),
        };
        let binding = ContinuationBinding {
            agent_kind: "pi".into(),
            session_id: "s-fast-terminal".into(),
            session_file: Some("C:/sessions/s-fast-terminal.jsonl".into()),
            origin_leaf_id: Some("leaf-origin".into()),
            project_root: "C:/science".into(),
            workspace: workspace.clone(),
            adapter_path: Some("C:/pi-runs/extensions/runs/index.ts".into()),
        };
        let payload = DeliveryPayload {
            delivery_id: "r-fast-terminal:a1:terminal".into(),
            run_id: "r-fast-terminal".into(),
            attempt_no: 1,
            status: RunStatus::Succeeded,
            job_id: Some("31804".into()),
            workspace,
            binding: binding.clone(),
            created_at: now - ChronoDuration::seconds(120),
        };
        let expired = (now - ChronoDuration::seconds(1)).to_rfc3339();
        let conn = store.connect().unwrap();
        conn.execute(
            "INSERT INTO deliveries(delivery_id, run_id, state, attempts, next_retry_at, last_error, payload_json)
             VALUES(?1, ?2, 'delivering', 1, ?3, NULL, ?4)",
            params![
                payload.delivery_id,
                payload.run_id,
                expired,
                serde_json::to_string(&payload).unwrap()
            ],
        )
        .unwrap();
        let registration = AgentSessionRegistration {
            agent_kind: "pi".into(),
            session_id: binding.session_id.clone(),
            owner_instance_id: "dead-live-pi".into(),
            session_file: binding.session_file.clone(),
            project_root: binding.project_root.clone(),
            current_leaf_id: binding.origin_leaf_id.clone(),
        };
        conn.execute(
            "INSERT INTO agent_session_leases(agent_kind, session_id, owner_instance_id, expires_at, payload_json)
             VALUES('pi', ?1, ?2, ?3, ?4)",
            params![
                binding.session_id,
                registration.owner_instance_id,
                (now - ChronoDuration::seconds(30)).to_rfc3339(),
                serde_json::to_string(&registration).unwrap()
            ],
        )
        .unwrap();
        drop(conn);

        let invocation = store
            .reserve_offline_invocation(Duration::ZERO)
            .unwrap()
            .expect("expired live claim must become an offline invocation");
        assert_eq!(invocation.delivery_id, payload.delivery_id);
        assert!(invocation.owner_instance_id.starts_with("offline:pi:"));

        let conn = store.connect().unwrap();
        let (state, attempts): (String, u32) = conn
            .query_row(
                "SELECT state, attempts FROM deliveries WHERE delivery_id=?1",
                [&payload.delivery_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "delivering");
        assert_eq!(attempts, 2);
        let invocation_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_invocations WHERE delivery_id=?1",
                [&payload.delivery_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(invocation_count, 1);
        drop(conn);
        cleanup(&db);
    }

    #[test]
    fn rebind_updates_binding_and_requeues_blocked_delivery() {
        use crate::types::{ContinuationBinding, DeliveryPayload, RemoteWorkspaceRef};

        let (db, legacy) = test_paths("rebind");
        let store = RunStore::open_at(db.clone(), legacy).expect("open store");
        let workspace = RemoteWorkspaceRef {
            host_alias: "cluster".into(),
            cwd: "/shared/project".into(),
        };
        let mut run = RunRecord::new("r-rebind".into(), "cluster".into(), RunnerKind::Slurm);
        run.status = RunStatus::Succeeded;
        run.workspace = Some(workspace.clone());
        run.attempt_no = Some(1);
        store.upsert(&run).unwrap();
        let original = ContinuationBinding {
            agent_kind: "pi".into(),
            session_id: "s1".into(),
            session_file: Some("C:/sessions/s1.jsonl".into()),
            origin_leaf_id: Some("old-leaf".into()),
            project_root: "C:/science".into(),
            workspace: workspace.clone(),
            adapter_path: Some("C:/pi-runs/extensions/runs/index.ts".into()),
        };
        let payload = DeliveryPayload {
            delivery_id: "r-rebind:a1:terminal".into(),
            run_id: run.run_id.clone(),
            attempt_no: 1,
            status: RunStatus::Succeeded,
            job_id: Some("123".into()),
            workspace,
            binding: original.clone(),
            created_at: Utc::now(),
        };
        let conn = store.connect().unwrap();
        conn.execute(
            "INSERT INTO continuation_bindings(run_id, payload_json) VALUES(?1, ?2)",
            params![run.run_id, serde_json::to_string(&original).unwrap()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO deliveries(delivery_id, run_id, state, attempts, payload_json)
             VALUES(?1, ?2, 'needs_rebind', 1, ?3)",
            params![
                payload.delivery_id,
                run.run_id,
                serde_json::to_string(&payload).unwrap()
            ],
        )
        .unwrap();
        drop(conn);

        let rebound = ContinuationBinding {
            origin_leaf_id: Some("new-leaf".into()),
            ..original
        };
        assert_eq!(store.rebind_continuation("r-rebind", &rebound).unwrap(), 1);
        assert_eq!(
            store.get_continuation_binding("r-rebind").unwrap(),
            Some(rebound.clone())
        );
        let conn = store.connect().unwrap();
        let state: String = conn
            .query_row(
                "SELECT state FROM deliveries WHERE delivery_id='r-rebind:a1:terminal'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "pending");
        let payload_json: String = conn
            .query_row(
                "SELECT payload_json FROM deliveries WHERE delivery_id='r-rebind:a1:terminal'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let rebound_payload: DeliveryPayload = serde_json::from_str(&payload_json).unwrap();
        assert_eq!(rebound_payload.binding, rebound);
        drop(conn);

        let registration = AgentSessionRegistration {
            agent_kind: "pi".into(),
            session_id: "s1".into(),
            owner_instance_id: "owner-rebind".into(),
            session_file: rebound.session_file.clone(),
            project_root: rebound.project_root.clone(),
            current_leaf_id: rebound.origin_leaf_id.clone(),
        };
        store
            .register_agent_session(&registration, Duration::from_secs(30))
            .unwrap();
        let claimed = store
            .claim_deliveries("pi", "s1", "owner-rebind", 8)
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].payload.binding, rebound);
        let newer = ContinuationBinding {
            origin_leaf_id: Some("newer-leaf".into()),
            ..rebound
        };
        assert!(store.rebind_continuation("r-rebind", &newer).is_err());
        cleanup(&db);
    }

    fn seed_offline_invocation_fixture_for_agent(
        name: &str,
        run_id: &str,
        session_id: &str,
        agent_kind: &str,
    ) -> (PathBuf, RunStore, String, AgentInvocationRecord) {
        use crate::types::{ContinuationBinding, RemoteWorkspaceRef};

        let (db, legacy) = test_paths(name);
        let store = RunStore::open_at(db.clone(), legacy).expect("open store");
        let workspace = RemoteWorkspaceRef {
            host_alias: "cluster".into(),
            cwd: "/shared/project".into(),
        };
        let binding = ContinuationBinding {
            agent_kind: agent_kind.into(),
            session_id: session_id.into(),
            session_file: Some(format!("C:/sessions/{session_id}.jsonl")),
            origin_leaf_id: (agent_kind == "pi").then(|| "leaf-origin".into()),
            project_root: "C:/science".into(),
            workspace: workspace.clone(),
            adapter_path: (agent_kind == "pi")
                .then(|| "C:/pi-runs/extensions/runs/index.ts".into()),
        };
        let mut run = RunRecord::new(run_id.into(), "cluster".into(), RunnerKind::Slurm);
        run.status = RunStatus::Succeeded;
        run.workspace = Some(workspace);
        run.attempt_no = Some(1);
        run.job_id = Some("123".into());
        store.upsert(&run).unwrap();
        let conn = store.connect().unwrap();
        conn.execute(
            "INSERT INTO continuation_bindings(run_id, payload_json) VALUES(?1, ?2)",
            params![run.run_id, serde_json::to_string(&binding).unwrap()],
        )
        .unwrap();
        drop(conn);
        let delivery_id = store.ensure_terminal_delivery(&run).unwrap().unwrap();
        let invocation = store
            .reserve_offline_invocation(Duration::ZERO)
            .unwrap()
            .expect("offline invocation");
        (db, store, delivery_id, invocation)
    }

    fn seed_offline_invocation_fixture(
        name: &str,
        run_id: &str,
        session_id: &str,
    ) -> (PathBuf, RunStore, String, AgentInvocationRecord) {
        seed_offline_invocation_fixture_for_agent(name, run_id, session_id, "pi")
    }

    fn expire_offline_lease_for_agent(store: &RunStore, agent_kind: &str, session_id: &str) {
        let conn = store.connect().unwrap();
        conn.execute(
            "UPDATE agent_session_leases SET expires_at=?3
             WHERE agent_kind=?1 AND session_id=?2",
            params![
                agent_kind,
                session_id,
                (Utc::now() - ChronoDuration::seconds(1)).to_rfc3339()
            ],
        )
        .unwrap();
    }

    fn expire_offline_lease(store: &RunStore, session_id: &str) {
        expire_offline_lease_for_agent(store, "pi", session_id);
    }

    #[test]
    fn offline_invocation_ownership_requires_active_lease_and_delivering_state() {
        let (db, store, delivery_id, invocation) =
            seed_offline_invocation_fixture("offline-owned", "r-owned", "s-owned");
        assert!(
            store
                .offline_invocation_is_owned(
                    &invocation.invocation_id,
                    &delivery_id,
                    &invocation.owner_instance_id,
                )
                .unwrap()
        );
        assert!(
            !store
                .offline_invocation_is_owned(&invocation.invocation_id, &delivery_id, "wrong-owner")
                .unwrap()
        );
        expire_offline_lease(&store, "s-owned");
        assert!(
            !store
                .offline_invocation_is_owned(
                    &invocation.invocation_id,
                    &delivery_id,
                    &invocation.owner_instance_id,
                )
                .unwrap()
        );
        cleanup(&db);
    }

    #[test]
    fn orphaned_starting_invocation_requeues_after_lease_expiry() {
        let (db, store, delivery_id, invocation) = seed_offline_invocation_fixture(
            "offline-orphan-start",
            "r-orphan-start",
            "s-orphan-start",
        );
        expire_offline_lease(&store, "s-orphan-start");
        assert_eq!(
            store
                .reconcile_orphaned_agent_invocations(Duration::ZERO)
                .unwrap(),
            1
        );
        let conn = store.connect().unwrap();
        let delivery_state: String = conn
            .query_row(
                "SELECT state FROM deliveries WHERE delivery_id=?1",
                [&delivery_id],
                |row| row.get(0),
            )
            .unwrap();
        let invocation_state: String = conn
            .query_row(
                "SELECT state FROM agent_invocations WHERE invocation_id=?1",
                [&invocation.invocation_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(delivery_state, "retrying");
        assert_eq!(invocation_state, "failed");
        cleanup(&db);
    }

    #[test]
    fn active_offline_invocation_is_not_reconciled() {
        let (db, store, delivery_id, invocation) =
            seed_offline_invocation_fixture("offline-active", "r-active", "s-active");
        assert_eq!(
            store
                .reconcile_orphaned_agent_invocations(Duration::ZERO)
                .unwrap(),
            0
        );
        assert!(
            store
                .offline_invocation_is_owned(
                    &invocation.invocation_id,
                    &delivery_id,
                    &invocation.owner_instance_id,
                )
                .unwrap()
        );
        cleanup(&db);
    }

    #[test]
    fn delivered_orphan_reconciles_completed_without_requeue() {
        let (db, store, delivery_id, invocation) =
            seed_offline_invocation_fixture("offline-acked", "r-acked", "s-acked");
        assert!(
            store
                .finish_delivery(
                    "pi",
                    "s-acked",
                    &invocation.owner_instance_id,
                    &delivery_id,
                    "delivered",
                    None,
                )
                .unwrap()
        );
        expire_offline_lease(&store, "s-acked");
        assert_eq!(
            store
                .reconcile_orphaned_agent_invocations(Duration::ZERO)
                .unwrap(),
            1
        );
        let conn = store.connect().unwrap();
        let delivery_state: String = conn
            .query_row(
                "SELECT state FROM deliveries WHERE delivery_id=?1",
                [&delivery_id],
                |row| row.get(0),
            )
            .unwrap();
        let invocation_state: String = conn
            .query_row(
                "SELECT state FROM agent_invocations WHERE invocation_id=?1",
                [&invocation.invocation_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(delivery_state, "delivered");
        assert_eq!(invocation_state, "completed");
        cleanup(&db);
    }

    #[test]
    fn orphaned_running_invocation_requeues_after_lease_expiry() {
        let (db, store, delivery_id, invocation) = seed_offline_invocation_fixture(
            "offline-orphan-running",
            "r-orphan-running",
            "s-orphan-running",
        );
        store
            .set_agent_invocation_pid(&invocation.invocation_id, 4242)
            .unwrap();
        expire_offline_lease(&store, "s-orphan-running");
        assert_eq!(
            store
                .reconcile_orphaned_agent_invocations(Duration::ZERO)
                .unwrap(),
            1
        );
        let conn = store.connect().unwrap();
        let delivery_state: String = conn
            .query_row(
                "SELECT state FROM deliveries WHERE delivery_id=?1",
                [&delivery_id],
                |row| row.get(0),
            )
            .unwrap();
        let invocation_state: String = conn
            .query_row(
                "SELECT state FROM agent_invocations WHERE invocation_id=?1",
                [&invocation.invocation_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(delivery_state, "retrying");
        assert_eq!(invocation_state, "failed");
        cleanup(&db);
    }

    #[test]
    fn offline_invocation_reserves_exact_delivery_and_requeues_unacked_exit() {
        use crate::types::{ContinuationBinding, RemoteWorkspaceRef};

        let (db, legacy) = test_paths("offline-invocation");
        let store = RunStore::open_at(db.clone(), legacy).expect("open store");
        let workspace = RemoteWorkspaceRef {
            host_alias: "cluster".into(),
            cwd: "/shared/project".into(),
        };
        let binding = ContinuationBinding {
            agent_kind: "pi".into(),
            session_id: "s-offline".into(),
            session_file: Some("C:/sessions/offline.jsonl".into()),
            origin_leaf_id: Some("leaf-origin".into()),
            project_root: "C:/science".into(),
            workspace: workspace.clone(),
            adapter_path: Some("C:/pi-runs/extensions/runs/index.ts".into()),
        };
        let mut run = RunRecord::new("r-offline".into(), "cluster".into(), RunnerKind::Slurm);
        run.status = RunStatus::Succeeded;
        run.workspace = Some(workspace);
        run.attempt_no = Some(1);
        run.job_id = Some("123".into());
        store.upsert(&run).unwrap();
        let conn = store.connect().unwrap();
        conn.execute(
            "INSERT INTO continuation_bindings(run_id, payload_json) VALUES(?1, ?2)",
            params![run.run_id, serde_json::to_string(&binding).unwrap()],
        )
        .unwrap();
        drop(conn);
        let delivery_id = store.ensure_terminal_delivery(&run).unwrap().unwrap();

        let invocation = store
            .reserve_offline_invocation(Duration::ZERO)
            .unwrap()
            .expect("offline invocation");
        assert_eq!(invocation.delivery_id, delivery_id);
        assert_eq!(
            invocation.session_file.as_deref(),
            Some("C:/sessions/offline.jsonl")
        );
        assert_eq!(
            invocation.adapter_path.as_deref(),
            Some("C:/pi-runs/extensions/runs/index.ts")
        );
        assert!(
            store
                .reserve_offline_invocation(Duration::ZERO)
                .unwrap()
                .is_none()
        );

        let conn = store.connect().unwrap();
        let delivery_state: String = conn
            .query_row(
                "SELECT state FROM deliveries WHERE delivery_id=?1",
                [&delivery_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(delivery_state, "delivering");
        let lease_owner: String = conn
            .query_row(
                "SELECT owner_instance_id FROM agent_session_leases WHERE agent_kind='pi' AND session_id='s-offline'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(lease_owner, invocation.owner_instance_id);
        drop(conn);

        store
            .finish_agent_invocation_process(&invocation.invocation_id, Some(1), None)
            .unwrap();
        let conn = store.connect().unwrap();
        let delivery_state: String = conn
            .query_row(
                "SELECT state FROM deliveries WHERE delivery_id=?1",
                [&delivery_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(delivery_state, "retrying");
        let leases: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_session_leases WHERE agent_kind='pi' AND session_id='s-offline'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(leases, 0);
        drop(conn);
        cleanup(&db);
    }

    #[test]
    fn codex_offline_invocation_reserves_exact_agent_lease_without_pi_adapter() {
        let (db, store, delivery_id, invocation) = seed_offline_invocation_fixture_for_agent(
            "offline-codex",
            "r-codex",
            "codex-thread-1",
            "codex",
        );
        assert_eq!(invocation.delivery_id, delivery_id);
        assert_eq!(invocation.payload.binding.agent_kind, "codex");
        assert!(invocation.session_file.is_some());
        assert!(invocation.adapter_path.is_none());
        assert!(
            store
                .offline_invocation_is_owned(
                    &invocation.invocation_id,
                    &delivery_id,
                    &invocation.owner_instance_id,
                )
                .unwrap()
        );
        let conn = store.connect().unwrap();
        let codex_leases: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_session_leases WHERE agent_kind='codex' AND session_id='codex-thread-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let pi_leases: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_session_leases WHERE agent_kind='pi' AND session_id='codex-thread-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(codex_leases, 1);
        assert_eq!(pi_leases, 0);
        drop(conn);
        expire_offline_lease_for_agent(&store, "codex", "codex-thread-1");
        assert_eq!(
            store
                .reconcile_orphaned_agent_invocations(Duration::ZERO)
                .unwrap(),
            1
        );
        assert_eq!(
            store.get_delivery_state(&delivery_id).unwrap().as_deref(),
            Some("retrying")
        );
        cleanup(&db);
    }

    #[test]
    fn offline_invocation_without_adapter_path_blocks_for_rebind() {
        use crate::types::{ContinuationBinding, RemoteWorkspaceRef};

        let (db, legacy) = test_paths("offline-adapter-missing");
        let store = RunStore::open_at(db.clone(), legacy).expect("open store");
        let workspace = RemoteWorkspaceRef {
            host_alias: "cluster".into(),
            cwd: "/shared/project".into(),
        };
        let binding = ContinuationBinding {
            agent_kind: "pi".into(),
            session_id: "s-old".into(),
            session_file: Some("C:/sessions/old.jsonl".into()),
            origin_leaf_id: Some("leaf-old".into()),
            project_root: "C:/science".into(),
            workspace: workspace.clone(),
            adapter_path: None,
        };
        let mut run = RunRecord::new("r-old".into(), "cluster".into(), RunnerKind::Slurm);
        run.status = RunStatus::Succeeded;
        run.workspace = Some(workspace);
        run.attempt_no = Some(1);
        store.upsert(&run).unwrap();
        let conn = store.connect().unwrap();
        conn.execute(
            "INSERT INTO continuation_bindings(run_id, payload_json) VALUES(?1, ?2)",
            params![run.run_id, serde_json::to_string(&binding).unwrap()],
        )
        .unwrap();
        drop(conn);
        let delivery_id = store.ensure_terminal_delivery(&run).unwrap().unwrap();
        assert!(
            store
                .reserve_offline_invocation(Duration::ZERO)
                .unwrap()
                .is_none()
        );
        let conn = store.connect().unwrap();
        let state: String = conn
            .query_row(
                "SELECT state FROM deliveries WHERE delivery_id=?1",
                [&delivery_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "needs_rebind");
        drop(conn);
        cleanup(&db);
    }

    #[test]
    fn opens_schema_v2_in_place_and_adds_retry_intents() {
        let (db, legacy) = test_paths("schema-v2-upgrade");
        let conn = Connection::open(&db).expect("create v2 database");
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta(key, value) VALUES('schema_version', '2');
             CREATE TABLE runs (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id TEXT NOT NULL UNIQUE,
                 record_json TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );
             CREATE TABLE run_attempts (
                 run_id TEXT NOT NULL,
                 attempt_no INTEGER NOT NULL,
                 record_json TEXT NOT NULL,
                 PRIMARY KEY (run_id, attempt_no)
             );",
        )
        .unwrap();
        drop(conn);

        let store = RunStore::open_at(db.clone(), legacy).expect("upgrade v2 store");
        let conn = store.connect().expect("connect upgraded store");
        let version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "3");
        let retry_table: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='retry_intents'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retry_table, 1);
        drop(conn);
        cleanup(&db);
    }

    #[test]
    fn initializes_future_model_tables() {
        let (db, legacy) = test_paths("schema");
        let store = RunStore::open_at(db.clone(), legacy).expect("open store");
        let conn = store.connect().expect("connect");
        for table in [
            "runs",
            "run_attempts",
            "retry_intents",
            "run_events",
            "observations",
            "deliveries",
            "continuation_bindings",
            "artifacts",
            "agent_session_leases",
            "agent_invocations",
        ] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "missing table {table}");
        }
        cleanup(&db);
    }
}
