"""Read-only context acceptance metrics. Emit JSON; never modify the event DB."""
import argparse
from collections import Counter, defaultdict
import json
from pathlib import Path
import re
import sqlite3


def page_number(args):
    path = str(args.get("path", "")).replace("\\", "/")
    match = re.search(r"(?:^|/)page(\d{2})\.html$", path)
    if match:
        return int(match[1])
    argv = args.get("args", [])
    if "--page" in argv:
        try:
            return int(argv[argv.index("--page") + 1])
        except (ValueError, IndexError):
            pass
    return None


def collect(database, task):
    conn = sqlite3.connect(Path(database).resolve().as_uri() + "?mode=ro", uri=True)
    try:
        events = [(seq, kind, json.loads(raw)) for seq, kind, raw in conn.execute(
            "SELECT seq,kind,data FROM agent_task_events WHERE task_id=? ORDER BY seq", (task,))]
        if not events:
            raise ValueError("task has no events")
        calls = {}
        by_request = {}
        reads = Counter()
        checks = Counter()
        counters = Counter()
        latest_check = None
        node_columns = {row[1] for row in conn.execute("PRAGMA table_info(agent_task_events)")}
        node_ownership = {seq: node for seq, node in conn.execute(
            "SELECT seq,node_id FROM agent_task_events WHERE task_id=?", (task,))} if "node_id" in node_columns else {}
        nodes = {}
        node_pages = defaultdict(set)
        requests = []
        for seq, kind, data in events:
            if kind == "subagent/event":
                kind, data = data.get("event_type"), {**data.get("event", {}), "agent_id": data.get("agent_id")}
            counters[kind] += 1
            if kind and kind.startswith("task/node_") and data.get("node_id"):
                nodes[data["node_id"]] = data
            if kind == "tool/call":
                args = data.get("arguments", {})
                args = json.loads(args) if isinstance(args, str) else args
                name = data.get("name")
                counters["tool:" + str(name)] += 1
                number = page_number(args)
                calls[data.get("callId")] = (name, args)
                if number is not None:
                    by_request[(data.get("turn"), data.get("step"), data.get("agent_id"))] = number
                    if node_ownership.get(seq):
                        node_pages[node_ownership[seq]].add(number)
                if name in {"read_file", "read_file_lines", "read_asset"}:
                    reads[str(args.get("path") or args.get("asset_id") or "unknown")] += 1
                if name == "run_program" and "check_pages.py" in args.get("args", []):
                    checks[str(number) if number is not None else "all"] += 1
            elif kind == "context/usage":
                requests.append((seq, data))
            elif kind == "tool/result":
                call = calls.get(data.get("message", {}).get("toolCallId"))
                result = data.get("meta", {}).get("result", {})
                if call and call[0] == "run_program" and "check_pages.py" in call[1].get("args", []):
                    try:
                        check = json.loads(result.get("stdout", ""))
                        if call[1].get("args", []).count("--page") == 0:
                            latest_check = check
                    except (TypeError, ValueError):
                        pass
        sources = [(node_id, node["source"]) for node_id, node in nodes.items() if node.get("source")]

        def owner_for(seq):
            owner = node_ownership.get(seq)
            for node_id, source in sources:
                if source.get("node_id") == owner and source.get("after_seq", 0) <= seq <= source.get("through_seq", -1):
                    return node_id
            return owner

        def page_for(owner, seen=None):
            if not owner:
                return None
            seen = set() if seen is None else seen
            if owner in seen:
                return None
            seen.add(owner)
            node = nodes.get(owner, {})
            explicit = (node.get("metadata") or {}).get("page_index")
            if explicit:
                return explicit
            match = re.match(r"Page\s+(\d+)(?:\D|$)", node.get("title", ""), re.IGNORECASE)
            if match:
                return int(match[1])
            candidates = node_pages.get(owner, set())
            if len(candidates) == 1 and owner != "n0":
                return next(iter(candidates))
            return page_for(node.get("parent_id"), seen)

        per_page = defaultdict(lambda: {"estimated_input_tokens": 0, "requests": 0, "actual_prompt_tokens": 0})
        per_node = defaultdict(lambda: {"estimated_input_tokens": 0, "requests": 0, "actual_prompt_tokens": 0})
        unassigned = 0
        for seq, data in requests:
            owner = owner_for(seq)
            number = page_for(owner)
            if number is None and not nodes:
                number = by_request.get((data.get("turn"), data.get("step"), data.get("agent_id")))
            size = data.get("estimated_input_tokens", 0)
            if owner:
                per_node[owner]["estimated_input_tokens"] += size
                per_node[owner]["requests"] += 1
            if number is None:
                unassigned += size
                continue
            per_page[number]["estimated_input_tokens"] += size
            per_page[number]["requests"] += 1
        actual = cached = 0
        actual_requests = 0
        # Usage is saved once in each request outcome, regardless of whether a
        # provider emitted an optional context/model_usage event.
        if conn.execute("SELECT 1 FROM sqlite_master WHERE name='agent_request_contexts'").fetchone():
            for (raw,) in conn.execute("SELECT metadata FROM agent_request_contexts WHERE task_id=?", (task,)):
                meta = json.loads(raw)
                usage = meta.get("outcome", {}).get("usage") or {}
                prompt = usage.get("prompt_tokens", usage.get("input_tokens"))
                if prompt is not None:
                    actual_requests += 1
                    actual += prompt
                    cached += usage.get("prompt_tokens_details", {}).get("cached_tokens", usage.get("prompt_cache_hit_tokens", 0))
                    owner = meta.get("task_node_id")
                    # Request nodeId links usage to its event, including a split
                    # source interval. Each request contributes to one owner only.
                    matching = next((seq for seq, data in requests if data.get("nodeId") == meta.get("nodeId") and meta.get("nodeId")), None)
                    if matching is not None:
                        owner = owner_for(matching)
                    if owner:
                        per_node[owner]["actual_prompt_tokens"] += prompt
                    number = page_for(owner)
                    if number is not None:
                        per_page[number]["actual_prompt_tokens"] += prompt
        first = [per_page[p]["estimated_input_tokens"] for p in range(1, 6) if p in per_page]
        last = [per_page[p]["estimated_input_tokens"] for p in range(16, 21) if p in per_page]
        ratio = (sum(last) / len(last)) / (sum(first) / len(first)) if len(first) == len(last) == 5 and sum(first) else None
        return {"task_id": task, "event_count": len(events), "node_attribution_available": any(node_ownership.values()),
                "page_attribution": "node_id and disjoint split sources" if nodes else "legacy tool paths and explicit --page checks",
                "per_node": dict(sorted(per_node.items())),
                "per_page_estimates": dict(sorted(per_page.items())), "unassigned_estimated_input_tokens": unassigned,
                "last_five_over_first_five": ratio, "actual_prompt_tokens": actual if actual_requests else None,
                "requests_with_actual_usage": actual_requests, "cached_prompt_tokens": cached if actual_requests else None,
                "reads": dict(reads), "repeated_reads": sum(max(0, count - 1) for count in reads.values()),
                "page_checks": dict(checks), "compression_attempts": counters["context/compression_start"],
                "compression_failures": counters["context/compression_failed"], "splits": counters["tool:split_task"],
                "final_check": latest_check}
    finally:
        conn.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("database", type=Path)
    parser.add_argument("task_id")
    args = parser.parse_args()
    print(json.dumps(collect(args.database, args.task_id), ensure_ascii=True, indent=2))


if __name__ == "__main__":
    main()
