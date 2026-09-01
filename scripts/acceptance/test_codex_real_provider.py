import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).with_name("codex_real_provider.py")
SPEC = importlib.util.spec_from_file_location("codex_real_provider", MODULE_PATH)
assert SPEC and SPEC.loader
mod = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(mod)


class CodexRealProviderTests(unittest.TestCase):
    def test_initial_jsonl_requires_one_exact_submit(self):
        name = "runwatch_r10c_test"
        run_id = "r10c_test"
        events = [
            {"type": "thread.started", "thread_id": "thread-1"},
            {"type": "turn.started"},
            {
                "type": "item.started",
                "item": {
                    "id": "item-1",
                    "type": "mcp_tool_call",
                    "server": name,
                    "tool": "submit_science_run",
                    "arguments": {"run_id": run_id},
                    "status": "in_progress",
                },
            },
            {
                "type": "item.completed",
                "item": {
                    "id": "item-1",
                    "type": "mcp_tool_call",
                    "server": name,
                    "tool": "submit_science_run",
                    "arguments": {"run_id": run_id},
                    "status": "completed",
                    "error": None,
                },
            },
            {"type": "item.completed", "item": {"id": "msg", "type": "agent_message", "text": f"R10C_SUBMITTED {run_id}"}},
            {"type": "turn.completed"},
        ]
        out = "\n".join(json.dumps(x) for x in events)
        evidence = mod.parse_codex_initial(out, mcp_name=name, run_id=run_id)
        self.assertEqual(evidence.thread_id, "thread-1")
        self.assertEqual(evidence.submit_call_id, "item-1")

    def test_rollout_requires_one_marker_and_matching_task_complete(self):
        delivery = "r10c:a1:terminal"
        marker = f"[runwatch continuation delivery_id={delivery}]"
        rows = [
            {"type": "session_meta", "payload": {"id": "thread-1", "session_id": "thread-1"}},
            {"type": "event_msg", "payload": {"type": "task_started", "turn_id": "turn-1"}},
            {
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": marker}],
                },
            },
            {"type": "event_msg", "payload": {"type": "task_complete", "turn_id": "turn-1"}},
        ]
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "rollout.jsonl"
            path.write_text("\n".join(json.dumps(x) for x in rows) + "\n", encoding="utf-8")
            result = mod.inspect_rollout(path, thread_id="thread-1", delivery_id=delivery)
        self.assertEqual(result["marker_count"], 1)
        self.assertTrue(result["marker_turn_completed"])
        self.assertIsNone(result["active_turn"])

    def test_cleanup_refuses_unmarked_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "runwatch-r10c-unmarked"
            root.mkdir()
            with self.assertRaises(mod.AcceptanceError):
                mod.safe_remove_isolated_root(root, "nonce", keep=False)

    def test_remote_inputs_are_strict(self):
        mod.validate_remote_inputs("gm00", "/share/home/shark/tmp")
        with self.assertRaises(mod.AcceptanceError):
            mod.validate_remote_inputs("gm00;rm", "/tmp")
        with self.assertRaises(mod.AcceptanceError):
            mod.validate_remote_inputs("gm00", "relative/path")


    def test_temporary_mcp_is_process_local_config_only(self):
        overrides = mod.temporary_mcp_overrides(
            "runwatch_r10c_test",
            Path("C:/Tools/runwatch-mcp.exe"),
            Path("E:/scratch/runwatch-r10c/data"),
            r"\\.\pipe\runwatch-r10c-test",
        )
        self.assertEqual(overrides[0], "-c")
        self.assertEqual(overrides[2], "-c")
        joined = " ".join(overrides)
        self.assertIn("mcp_servers.runwatch_r10c_test.command", joined)
        self.assertIn("RUNWATCH_DATA_DIR", joined)
        self.assertIn("RUNWATCH_ENDPOINT", joined)
        self.assertNotIn("mcp add", joined)
        self.assertNotIn("mcp remove", joined)


if __name__ == "__main__":
    unittest.main()
