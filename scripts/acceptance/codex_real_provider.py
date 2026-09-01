#!/usr/bin/env python3
"""Opt-in real-provider acceptance for Codex durable continuation.

This harness intentionally performs one real Codex provider turn and one short
remote Slurm job. It isolates runwatch state and IPC, registers a random Codex
MCP name only after all local preflight succeeds, and removes that registration
in a finally block.

It never opens the normal runwatch ledger and never passes Codex dangerous
approval/trust bypass flags.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any

MAX_CODEX_STDOUT_BYTES = 8 * 1024 * 1024
MAX_CODEX_STDERR_BYTES = 1024 * 1024
MAX_ROLLOUT_LINE_BYTES = 1024 * 1024
TERMINAL_FAILURES = {"failed", "timed_out", "cancelled", "canceled"}
HOST_RE = re.compile(r"^[A-Za-z0-9_.-]+$")


class AcceptanceError(RuntimeError):
    pass


@dataclass
class CodexInitialEvidence:
    thread_id: str
    submit_call_id: str
    submit_run_id: str
    submitted_marker_seen: bool
    turn_completed: bool


def repo_root() -> Path:
    return Path(__file__).resolve().parents[2]


def native_name(stem: str) -> str:
    return stem + (".exe" if os.name == "nt" else "")


def run_capture(
    argv: list[str],
    *,
    env: dict[str, str] | None = None,
    cwd: Path | None = None,
    timeout: float = 30,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        argv,
        env=env,
        cwd=str(cwd) if cwd else None,
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=timeout,
        check=False,
    )


def require_success(proc: subprocess.CompletedProcess[str], action: str) -> None:
    if proc.returncode == 0:
        return
    stderr = proc.stderr.strip()
    if len(stderr) > 1000:
        stderr = stderr[-1000:]
    raise AcceptanceError(f"{action} failed with exit {proc.returncode}: {stderr}")


def resolve_executable(explicit: str | None, default: Path | str, label: str) -> Path:
    candidate: str | None
    if explicit:
        candidate = explicit
    elif isinstance(default, Path):
        candidate = str(default)
    else:
        candidate = shutil.which(default)
    if not candidate:
        raise AcceptanceError(f"cannot resolve {label}")
    path = Path(candidate).expanduser().resolve()
    if not path.is_file():
        raise AcceptanceError(f"{label} is not a file: {path}")
    if os.name == "nt" and path.suffix.lower() in {".cmd", ".bat", ".ps1"}:
        raise AcceptanceError(f"{label} must be a native executable, not {path.suffix}: {path}")
    return path


def validate_remote_inputs(host: str, workdir: str) -> None:
    if not HOST_RE.fullmatch(host):
        raise AcceptanceError("--host must be a plain ~/.ssh/config alias")
    if not workdir.startswith("/") or any(ch in workdir for ch in "\r\n\0"):
        raise AcceptanceError("--workdir must be one absolute POSIX path without control characters")


def make_isolated_root(scratch_root: Path | None) -> tuple[Path, str]:
    base = scratch_root.expanduser().resolve() if scratch_root else Path(tempfile.gettempdir()).resolve()
    base.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="runwatch-r10c-", dir=base)).resolve()
    nonce = uuid.uuid4().hex
    marker = root / ".runwatch-r10c-marker"
    marker.write_text(nonce, encoding="utf-8")
    return root, nonce


def safe_remove_isolated_root(root: Path, nonce: str, *, keep: bool) -> None:
    if keep:
        return
    root = root.resolve()
    marker = root / ".runwatch-r10c-marker"
    if not root.name.startswith("runwatch-r10c-"):
        raise AcceptanceError(f"refusing cleanup of non-R10c path: {root}")
    if not marker.is_file() or marker.read_text(encoding="utf-8") != nonce:
        raise AcceptanceError(f"refusing cleanup without matching R10c marker: {root}")
    shutil.rmtree(root)


def isolated_endpoint(root: Path, nonce: str) -> str:
    if os.name == "nt":
        return rf"\\.\pipe\runwatch-r10c-{nonce}"
    return str(root / "runwatch-r10c.sock")


def wait_daemon(runwatch: Path, env: dict[str, str], timeout: float = 15) -> None:
    deadline = time.monotonic() + timeout
    last = ""
    while time.monotonic() < deadline:
        proc = run_capture([str(runwatch), "daemon-status"], env=env, timeout=3)
        if proc.returncode == 0:
            try:
                payload = json.loads(proc.stdout)
                caps = payload.get("result", {}).get("capabilities", [])
            except json.JSONDecodeError:
                caps = []
            if "offline_codex_continuation" in caps:
                return
            last = "daemon responded without offline_codex_continuation"
        else:
            last = proc.stderr.strip()[-500:]
        time.sleep(0.1)
    raise AcceptanceError(f"isolated runwatchd did not become ready: {last}")


def codex_mcp_missing(codex: Path, name: str) -> bool:
    proc = run_capture([str(codex), "mcp", "get", name], timeout=15)
    if proc.returncode == 0:
        return False
    text = proc.stdout + "\n" + proc.stderr
    if "No MCP server named" not in text:
        raise AcceptanceError(f"codex mcp get {name} failed unexpectedly")
    return True


def add_temp_mcp(
    codex: Path,
    name: str,
    mcp: Path,
    data_dir: Path,
    endpoint: str,
) -> None:
    proc = run_capture(
        [
            str(codex),
            "mcp",
            "add",
            name,
            "--env",
            f"RUNWATCH_DATA_DIR={data_dir}",
            "--env",
            f"RUNWATCH_ENDPOINT={endpoint}",
            "--",
            str(mcp),
        ],
        timeout=30,
    )
    require_success(proc, f"register temporary Codex MCP {name}")
    verify = run_capture([str(codex), "mcp", "get", name], timeout=15)
    require_success(verify, f"verify temporary Codex MCP {name}")
    if str(mcp).lower() not in verify.stdout.lower():
        raise AcceptanceError(f"temporary Codex MCP {name} did not round-trip to expected executable")


def remove_temp_mcp(codex: Path, name: str) -> None:
    proc = run_capture([str(codex), "mcp", "remove", name], timeout=30)
    require_success(proc, f"remove temporary Codex MCP {name}")
    if not codex_mcp_missing(codex, name):
        raise AcceptanceError(f"temporary Codex MCP {name} still exists after remove")


def build_initial_prompt(name: str, run_id: str, host: str, workdir: str) -> str:
    command = "sleep 2; printf RUNWATCH_R10C_OK"
    return (
        "R10c real-provider release acceptance. "
        f"Use only the MCP server {name} and its submit_science_run tool. "
        "Call submit_science_run exactly once with these exact arguments: "
        f"run_id={json.dumps(run_id)}, host={json.dumps(host)}, "
        f"workdir={json.dumps(workdir)}, runner=\"slurm\", command={json.dumps(command)}. "
        "Do not wait, poll, run shell commands, edit files, or submit anything else. "
        f"After the tool returns successfully, reply exactly: R10C_SUBMITTED {run_id}"
    )


def parse_codex_initial(stdout: str, *, mcp_name: str, run_id: str) -> CodexInitialEvidence:
    if len(stdout.encode("utf-8")) > MAX_CODEX_STDOUT_BYTES:
        raise AcceptanceError("initial Codex JSONL exceeded bounded stdout budget")
    threads: list[str] = []
    submit_ids: set[str] = set()
    completed_submit_ids: set[str] = set()
    submitted_marker = False
    turn_completed = False
    for raw in stdout.splitlines():
        if not raw.strip():
            continue
        if len(raw.encode("utf-8")) > MAX_ROLLOUT_LINE_BYTES:
            raise AcceptanceError("initial Codex JSONL contained an oversized event")
        try:
            value = json.loads(raw)
        except json.JSONDecodeError as exc:
            raise AcceptanceError(f"initial Codex stdout contained malformed JSONL: {exc}") from exc
        typ = value.get("type")
        if typ == "thread.started" and isinstance(value.get("thread_id"), str):
            threads.append(value["thread_id"])
        if typ == "turn.completed":
            turn_completed = True
        item = value.get("item")
        if not isinstance(item, dict):
            continue
        if item.get("type") == "mcp_tool_call" and item.get("server") == mcp_name and item.get("tool") == "submit_science_run":
            item_id = str(item.get("id") or "")
            if not item_id:
                raise AcceptanceError("submit_science_run event had no item id")
            submit_ids.add(item_id)
            args = item.get("arguments") or {}
            if not isinstance(args, dict) or args.get("run_id") != run_id:
                raise AcceptanceError("submit_science_run used a different run_id")
            if typ == "item.completed":
                if item.get("status") != "completed" or item.get("error") not in (None, {}):
                    raise AcceptanceError("submit_science_run did not complete successfully")
                completed_submit_ids.add(item_id)
        if typ == "item.completed" and item.get("type") == "agent_message":
            if item.get("text") == f"R10C_SUBMITTED {run_id}":
                submitted_marker = True
    if len(set(threads)) != 1:
        raise AcceptanceError(f"expected one exact Codex thread id, got {len(set(threads))}")
    if len(submit_ids) != 1 or submit_ids != completed_submit_ids:
        raise AcceptanceError("expected exactly one successful submit_science_run tool call")
    if not submitted_marker or not turn_completed:
        raise AcceptanceError("initial Codex turn did not settle with the required submission marker")
    return CodexInitialEvidence(
        thread_id=threads[0],
        submit_call_id=next(iter(submit_ids)),
        submit_run_id=run_id,
        submitted_marker_seen=True,
        turn_completed=True,
    )


def run_initial_codex(
    codex: Path,
    project_root: Path,
    prompt: str,
    *,
    mcp_name: str,
    run_id: str,
    timeout: float,
) -> CodexInitialEvidence:
    proc = run_capture(
        [
            str(codex),
            "-s",
            "read-only",
            "-a",
            "never",
            "-C",
            str(project_root),
            "exec",
            "--json",
            prompt,
        ],
        cwd=project_root,
        timeout=timeout,
    )
    if len(proc.stderr.encode("utf-8")) > MAX_CODEX_STDERR_BYTES:
        raise AcceptanceError("initial Codex stderr exceeded bounded diagnostic budget")
    require_success(proc, "initial real-provider Codex turn")
    return parse_codex_initial(proc.stdout, mcp_name=mcp_name, run_id=run_id)


def open_store(db: Path) -> sqlite3.Connection:
    con = sqlite3.connect(f"file:{db.as_posix()}?mode=ro", uri=True, timeout=2)
    con.row_factory = sqlite3.Row
    con.execute("pragma busy_timeout=2000")
    return con


def canonical_snapshot(db: Path, run_id: str) -> dict[str, Any]:
    if not db.is_file():
        return {"run": None, "attempt": None, "binding": None, "delivery": None, "invocations": []}
    with open_store(db) as con:
        run = con.execute("select payload_json from runs where run_id=?", (run_id,)).fetchone()
        attempt = con.execute(
            "select attempt_no, job_id, scheduler_state, conclusion, finished_at from run_attempts where run_id=? order by attempt_no desc limit 1",
            (run_id,),
        ).fetchone()
        binding = con.execute(
            "select agent_kind, session_id, session_file, project_root, payload_json from continuation_bindings where run_id=?",
            (run_id,),
        ).fetchone()
        delivery = con.execute(
            "select delivery_id, state, attempts, last_error, payload_json from deliveries where run_id=? order by created_at desc limit 1",
            (run_id,),
        ).fetchone()
        invocations: list[sqlite3.Row] = []
        if delivery is not None:
            invocations = con.execute(
                "select invocation_id, state, pid, last_error, agent_kind, session_id from agent_invocations where delivery_id=? order by created_at",
                (delivery["delivery_id"],),
            ).fetchall()
    return {
        "run": json.loads(run["payload_json"]) if run else None,
        "attempt": dict(attempt) if attempt else None,
        "binding": ({**dict(binding), "payload_json": json.loads(binding["payload_json"])}) if binding else None,
        "delivery": ({**dict(delivery), "payload_json": json.loads(delivery["payload_json"])}) if delivery else None,
        "invocations": [dict(row) for row in invocations],
    }


def wait_delivery(db: Path, run_id: str, thread_id: str, timeout: float) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    last: dict[str, Any] = {}
    while time.monotonic() < deadline:
        snap = canonical_snapshot(db, run_id)
        last = snap
        run = snap.get("run") or {}
        status = str(run.get("status") or "").lower()
        if status in TERMINAL_FAILURES:
            raise AcceptanceError(f"scientific acceptance Run reached terminal failure state {status}")
        delivery = snap.get("delivery") or {}
        if delivery.get("state") in {"needs_rebind", "failed"}:
            raise AcceptanceError(
                f"Codex Delivery entered {delivery.get('state')}: {str(delivery.get('last_error') or '')[-500:]}"
            )
        if delivery.get("state") == "delivered":
            binding = snap.get("binding") or {}
            if binding.get("agent_kind") != "codex" or binding.get("session_id") != thread_id:
                raise AcceptanceError("canonical continuation binding does not match the initiating Codex thread")
            if status != "succeeded":
                raise AcceptanceError(f"Delivery completed but Run status is {status!r}, expected succeeded")
            if int(delivery.get("attempts") or 0) != 1:
                raise AcceptanceError("real-provider gate expected Delivery attempts=1")
            invocations = snap.get("invocations") or []
            if len(invocations) != 1:
                raise AcceptanceError(f"expected one AgentInvocation, got {len(invocations)}")
            invocation = invocations[0]
            if invocation.get("state") != "completed" or invocation.get("last_error"):
                raise AcceptanceError("offline Codex AgentInvocation did not complete cleanly")
            if invocation.get("agent_kind") != "codex" or invocation.get("session_id") != thread_id:
                raise AcceptanceError("AgentInvocation resumed a different Codex thread")
            return snap
        time.sleep(0.25)
    delivery = (last.get("delivery") or {}).get("state")
    raise AcceptanceError(f"timed out waiting for durable Codex Delivery; last delivery state={delivery!r}")


def inspect_rollout(path: Path, *, thread_id: str, delivery_id: str) -> dict[str, Any]:
    if not path.is_file():
        raise AcceptanceError(f"bound Codex rollout is unavailable: {path}")
    marker = f"[runwatch continuation delivery_id={delivery_id}]"
    active_turn: str | None = None
    marker_turn: str | None = None
    marker_count = 0
    marker_completed = False
    malformed = 0
    first_value: dict[str, Any] | None = None
    with path.open("rb") as handle:
        for raw in handle:
            if len(raw) > MAX_ROLLOUT_LINE_BYTES:
                continue
            if not raw.strip():
                continue
            try:
                value = json.loads(raw)
            except json.JSONDecodeError:
                malformed += 1
                continue
            if first_value is None:
                first_value = value
            top = value.get("type")
            payload = value.get("payload") if isinstance(value.get("payload"), dict) else {}
            payload_type = payload.get("type")
            if top == "event_msg" and payload_type == "task_started" and isinstance(payload.get("turn_id"), str):
                active_turn = payload["turn_id"]
                continue
            if top == "event_msg" and payload_type == "task_complete" and isinstance(payload.get("turn_id"), str):
                turn_id = payload["turn_id"]
                if marker_turn == turn_id:
                    marker_completed = True
                if active_turn == turn_id:
                    active_turn = None
                continue
            if top == "response_item" and payload_type == "message" and payload.get("role") == "user":
                content = payload.get("content")
                if isinstance(content, list) and any(
                    isinstance(item, dict)
                    and item.get("type") == "input_text"
                    and marker in str(item.get("text") or "")
                    for item in content
                ):
                    marker_count += 1
                    if marker_count == 1:
                        marker_turn = active_turn
    if first_value is None or first_value.get("type") != "session_meta":
        raise AcceptanceError("Codex rollout does not start with session_meta")
    meta = first_value.get("payload") if isinstance(first_value.get("payload"), dict) else {}
    if meta.get("id") != thread_id:
        raise AcceptanceError("persisted Codex session_meta.id does not match initiating thread")
    if meta.get("session_id") not in (None, thread_id):
        raise AcceptanceError("persisted Codex session_meta.session_id does not match initiating thread")
    if marker_count != 1 or marker_turn is None or not marker_completed:
        raise AcceptanceError(
            f"rollout continuation evidence invalid: marker_count={marker_count}, marker_turn={marker_turn!r}, completed={marker_completed}"
        )
    if active_turn is not None:
        raise AcceptanceError(f"Codex rollout still has active turn {active_turn}")
    return {
        "path": str(path),
        "session_id": thread_id,
        "marker_count": marker_count,
        "marker_turn": marker_turn,
        "marker_turn_completed": marker_completed,
        "active_turn": active_turn,
        "malformed_lines": malformed,
    }


def evidence_summary(
    *,
    run_id: str,
    initial: CodexInitialEvidence,
    snap: dict[str, Any],
    rollout: dict[str, Any],
) -> dict[str, Any]:
    attempt = snap["attempt"] or {}
    delivery = snap["delivery"] or {}
    invocation = snap["invocations"][0]
    return {
        "ok": True,
        "run_id": run_id,
        "thread_id": initial.thread_id,
        "job_id": attempt.get("job_id"),
        "run_status": (snap["run"] or {}).get("status"),
        "delivery_id": delivery.get("delivery_id"),
        "delivery_state": delivery.get("state"),
        "delivery_attempts": delivery.get("attempts"),
        "agent_invocation_id": invocation.get("invocation_id"),
        "agent_invocation_state": invocation.get("state"),
        "rollout": rollout,
        "human_continue": False,
    }


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--confirm-real-provider", action="store_true", help="required opt-in: consumes a real Codex provider turn and submits one short Slurm job")
    parser.add_argument("--host", required=True, help="SSH alias from ~/.ssh/config")
    parser.add_argument("--workdir", required=True, help="existing writable remote POSIX work directory")
    parser.add_argument("--project-root", type=Path, default=repo_root())
    parser.add_argument("--scratch-root", type=Path, default=None, help="parent for disposable isolated runwatch state")
    parser.add_argument("--runwatch-exe", default=None)
    parser.add_argument("--mcp-exe", default=None)
    parser.add_argument("--codex-exe", default=None)
    parser.add_argument("--provider-timeout-sec", type=float, default=180)
    parser.add_argument("--delivery-timeout-sec", type=float, default=300)
    parser.add_argument("--evidence-json", type=Path, default=None, help="optional non-secret summary written outside isolated state")
    parser.add_argument("--keep-isolated-state", action="store_true", help="keep disposable SQLite/log state after cleanup; temporary MCP registration is still removed")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    if not args.confirm_real_provider:
        raise AcceptanceError("refusing real provider/Slurm acceptance without --confirm-real-provider")
    validate_remote_inputs(args.host, args.workdir)
    project_root = args.project_root.expanduser().resolve()
    if not (project_root / "Cargo.toml").is_file():
        raise AcceptanceError(f"project root does not look like runwatch: {project_root}")
    runwatch = resolve_executable(
        args.runwatch_exe,
        project_root / "target" / "debug" / native_name("runwatch"),
        "runwatch executable",
    )
    mcp = resolve_executable(
        args.mcp_exe,
        project_root / "target" / "debug" / native_name("runwatch-mcp"),
        "runwatch-mcp executable",
    )
    codex_default = native_name("codex") if os.name == "nt" else "codex"
    codex = resolve_executable(args.codex_exe, codex_default, "Codex executable")
    version = run_capture([str(codex), "--version"], timeout=15)
    require_success(version, "codex --version preflight")

    root, nonce = make_isolated_root(args.scratch_root)
    data_dir = root / "data"
    data_dir.mkdir(parents=True, exist_ok=True)
    endpoint = isolated_endpoint(root, nonce)
    env = os.environ.copy()
    env["RUNWATCH_DATA_DIR"] = str(data_dir)
    env["RUNWATCH_ENDPOINT"] = endpoint
    env["RUNWATCH_CODEX_EXECUTABLE"] = str(codex)
    mcp_name = f"runwatch_r10c_{nonce[:12]}"
    run_id = f"r10c_codex_provider_{time.strftime('%Y%m%d_%H%M%S')}_{nonce[:6]}"
    registration_added = False
    daemon: subprocess.Popen[bytes] | None = None
    main_error: BaseException | None = None
    cleanup_errors: list[str] = []
    summary: dict[str, Any] | None = None

    try:
        if not codex_mcp_missing(codex, mcp_name):
            raise AcceptanceError(f"random temporary MCP name unexpectedly exists: {mcp_name}")
        flags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
        daemon = subprocess.Popen(
            [str(runwatch), "serve", "--interval", "1"],
            env=env,
            cwd=str(project_root),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            creationflags=flags,
        )
        wait_daemon(runwatch, env)
        add_temp_mcp(codex, mcp_name, mcp, data_dir, endpoint)
        registration_added = True
        prompt = build_initial_prompt(mcp_name, run_id, args.host, args.workdir)
        initial = run_initial_codex(
            codex,
            project_root,
            prompt,
            mcp_name=mcp_name,
            run_id=run_id,
            timeout=args.provider_timeout_sec,
        )
        db = data_dir / "runwatch.db"
        snap = wait_delivery(db, run_id, initial.thread_id, args.delivery_timeout_sec)
        binding = snap["binding"] or {}
        session_file = binding.get("session_file")
        if not session_file:
            raise AcceptanceError("canonical Codex binding has no persisted rollout file")
        delivery_id = (snap["delivery"] or {}).get("delivery_id")
        if not delivery_id:
            raise AcceptanceError("delivered row has no delivery_id")
        rollout = inspect_rollout(Path(session_file), thread_id=initial.thread_id, delivery_id=delivery_id)
        summary = evidence_summary(run_id=run_id, initial=initial, snap=snap, rollout=rollout)
    except BaseException as exc:
        main_error = exc
    finally:
        if daemon is not None and daemon.poll() is None:
            daemon.terminate()
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=5)
        if registration_added:
            try:
                remove_temp_mcp(codex, mcp_name)
            except BaseException as exc:
                cleanup_errors.append(f"temporary MCP cleanup failed: {exc}")
        try:
            safe_remove_isolated_root(root, nonce, keep=args.keep_isolated_state)
        except BaseException as exc:
            cleanup_errors.append(f"isolated-state cleanup failed: {exc}")

    if main_error is not None:
        if cleanup_errors:
            raise AcceptanceError(f"{main_error}; cleanup: {'; '.join(cleanup_errors)}") from main_error
        raise main_error
    if cleanup_errors:
        raise AcceptanceError("; ".join(cleanup_errors))
    assert summary is not None
    summary["temporary_mcp_removed"] = True
    summary["isolated_state_kept"] = bool(args.keep_isolated_state)
    if args.evidence_json:
        evidence_path = args.evidence_json.expanduser().resolve()
        evidence_path.parent.mkdir(parents=True, exist_ok=True)
        evidence_path.write_text(json.dumps(summary, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except AcceptanceError as exc:
        print(f"R10C_FAIL: {exc}", file=sys.stderr)
        raise SystemExit(1)
