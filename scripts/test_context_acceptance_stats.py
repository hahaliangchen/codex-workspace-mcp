import hashlib
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest

from context_acceptance_stats import collect


class MetricsTests(unittest.TestCase):
    def test_interleaved_nodes_and_split_sources_own_each_request_once(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "events.db"
            conn = sqlite3.connect(path)
            conn.executescript("""
                CREATE TABLE agent_task_events(task_id TEXT,seq INTEGER,kind TEXT,data TEXT,node_id TEXT);
                CREATE TABLE agent_request_contexts(task_id TEXT,metadata TEXT);
            """)
            events = [
                (1, "task/node_planned", {"node_id": "page", "title": "Page 11", "metadata": None}, "page"),
                (2, "context/usage", {"nodeId": "r1", "estimated_input_tokens": 100}, "page"),
                (3, "task/node_planned", {"node_id": "other", "metadata": {"page_index": 12}}, "other"),
                (4, "subagent/event", {"event_type": "context/usage", "event": {"nodeId": "r2", "estimated_input_tokens": 300}}, "other"),
                (5, "task/node_returned", {"node_id": "split1", "parent_id": "page", "source": {"node_id": "page", "after_seq": 1, "through_seq": 2}}, "split1"),
                (6, "context/usage", {"nodeId": "r3", "estimated_input_tokens": 200}, "page"),
                (7, "task/node_returned", {"node_id": "split2", "parent_id": "page", "source": {"node_id": "page", "after_seq": 3, "through_seq": 6}}, "split2"),
            ]
            conn.executemany("INSERT INTO agent_task_events VALUES ('task',?,?,?,?)", [(seq, kind, json.dumps(data), node) for seq, kind, data, node in events])
            conn.executemany("INSERT INTO agent_request_contexts VALUES ('task',?)", [(json.dumps({"nodeId": request, "task_node_id": node, "outcome": {"usage": {"prompt_tokens": size}}}),) for request, node, size in [("r1", "page", 80), ("r2", "other", 250), ("r3", "page", 160)]])
            conn.commit()
            conn.close()
            result = collect(path, "task")
            self.assertEqual(result["per_node"]["split1"]["actual_prompt_tokens"], 80)
            self.assertEqual(result["per_node"]["split2"]["actual_prompt_tokens"], 160)
            self.assertEqual(result["per_page_estimates"][11]["estimated_input_tokens"], 300)
            self.assertEqual(result["per_page_estimates"][12]["estimated_input_tokens"], 300)
            self.assertEqual(sum(n["actual_prompt_tokens"] for n in result["per_node"].values()), 490)
            self.assertEqual(result["unassigned_estimated_input_tokens"], 0)

    def test_usage_is_counted_once_and_collection_does_not_change_database(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "events.db"
            conn = sqlite3.connect(path)
            conn.executescript("""
                CREATE TABLE agent_task_events(task_id TEXT,seq INTEGER,kind TEXT,data TEXT);
                CREATE TABLE agent_request_contexts(task_id TEXT,metadata TEXT);
            """)
            events = [
                ("context/usage", {"turn": 1, "step": 1, "estimated_input_tokens": 300}),
                ("tool/call", {"turn": 1, "step": 1, "callId": "write", "name": "write_file", "arguments": {"path": "page01.html"}}),
                ("tool/call", {"turn": 1, "step": 2, "callId": "read-a", "name": "read_file", "arguments": {"path": "design_spec.md"}}),
                ("tool/call", {"turn": 1, "step": 3, "callId": "read-b", "name": "read_file", "arguments": {"path": "design_spec.md"}}),
                ("context/model_usage", {"usage": {"prompt_tokens": 250}}),
                ("context/compression_start", {}),
                ("context/compression_failed", {}),
                ("tool/call", {"turn": 1, "step": 4, "callId": "check", "name": "run_program", "arguments": {"args": ["check_pages.py", "--footer-from", "11"]}}),
                ("tool/result", {"message": {"toolCallId": "check"}, "meta": {"result": {"stdout": json.dumps({"passed": True, "passed_pages": 20})}}}),
            ]
            conn.executemany("INSERT INTO agent_task_events VALUES ('task',?,?,?)",
                             [(seq, kind, json.dumps(data)) for seq, (kind, data) in enumerate(events)])
            conn.execute("INSERT INTO agent_request_contexts VALUES ('task',?)", (json.dumps({"outcome": {"usage": {"prompt_tokens": 250, "prompt_tokens_details": {"cached_tokens": 100}}}}),))
            conn.commit()
            conn.close()
            before = hashlib.sha256(path.read_bytes()).hexdigest()
            result = collect(path, "task")
            self.assertEqual(result["per_page_estimates"][1]["estimated_input_tokens"], 300)
            self.assertEqual(result["actual_prompt_tokens"], 250)
            self.assertEqual(result["cached_prompt_tokens"], 100)
            self.assertEqual(result["repeated_reads"], 1)
            self.assertEqual(result["compression_failures"], 1)
            self.assertEqual(result["final_check"]["passed_pages"], 20)
            self.assertIsNone(result["last_five_over_first_five"])
            self.assertEqual(hashlib.sha256(path.read_bytes()).hexdigest(), before)


if __name__ == "__main__":
    unittest.main()
