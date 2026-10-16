import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SOURCE = Path(__file__).resolve().parents[2] / "src-tauri/src/services/provider/remote_usage_collector.py"
spec = importlib.util.spec_from_file_location("collector", SOURCE)
collector = importlib.util.module_from_spec(spec)
spec.loader.exec_module(collector)
TS = "2026-10-14T01:00:00Z"


NODE_SOURCE = SOURCE.with_suffix(".cjs")


class NodeCollector:
    """Runs the Node.js port with the same interface as the Python module."""
    MAX_EVENTS = collector.MAX_EVENTS
    stamp = staticmethod(collector.stamp)

    def collect(self, config):
        script = ("const c = require(process.argv[1]); c.LIMITS.events = Number(process.argv[2]);"
                  "process.stdout.write(c.serialize(c.collect(c.parseConfig(require('fs').readFileSync(0, 'utf8')))));")
        output = subprocess.run(["node", "-e", script, str(NODE_SOURCE), str(self.MAX_EVENTS)],
                                input=json.dumps(config), capture_output=True, text=True, check=True).stdout
        return json.loads(output)


class RemoteUsageCollectorTest(unittest.TestCase):
    collector = collector

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.roots = {app: [str(self.root / app)] for app in ("claude", "codex", "gemini", "grokbuild")}
        for paths in self.roots.values():
            Path(paths[0]).mkdir()
        self.config = {"roots": self.roots, "since": 0, "cursors": {}}

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, app, records, name="log.jsonl", mode="w"):
        file = Path(self.roots[app][0]) / name
        with file.open(mode) as stream:
            for record in records:
                stream.write(json.dumps(record) + "\n")
        return file

    def claude(self, ident="msg-1", output=20):
        return {"type": "assistant", "timestamp": TS, "sessionId": "session-1", "message": {
            "id": ident, "model": "claude-sonnet-4-5", "content": "PRIVATE_PROMPT_AND_RESPONSE",
            "usage": {"input_tokens": 100, "output_tokens": output, "cache_read_input_tokens": 80}}}

    def token(self, total, last=None, **kwargs):
        return {"type": "event_msg", "timestamp": kwargs.get("timestamp", TS), "payload": {
            "type": "token_count", "rate_limits": {"limit_id": kwargs.get("lane", "codex")}, "info": {
                "total_token_usage": total, "last_token_usage": last}}}

    def test_metadata_only_and_incremental_partial_tail(self):
        file = self.write("claude", [self.claude()])
        result = self.collector.collect(self.config)
        self.assertEqual(len(result["events"]), 1)
        self.assertNotIn("PRIVATE_PROMPT_AND_RESPONSE", json.dumps(result))
        self.config["cursors"] = result["cursors"]
        self.assertEqual(self.collector.collect(self.config)["events"], [])
        record = json.dumps(self.claude("msg-2"))
        with file.open("a") as stream:
            stream.write(record[:20])
        incomplete = self.collector.collect(self.config)
        self.assertEqual(incomplete["events"], [])
        self.config["cursors"] = incomplete["cursors"]
        with file.open("a") as stream:
            stream.write(record[20:] + "\n")
        self.assertEqual([e["id"] for e in self.collector.collect(self.config)["events"]], ["msg-2"])

    def test_codex_cumulative_deltas_lane_repeat_and_model_switch(self):
        a = {"input_tokens": 100, "output_tokens": 10, "cached_input_tokens": 80}
        b = {"input_tokens": 150, "output_tokens": 30, "cached_input_tokens": 110}
        self.write("codex", [
            {"type": "session_meta", "payload": {"id": "thread-1"}},
            {"type": "turn_context", "payload": {"model": "gpt-5"}},
            self.token(a), self.token(a, lane="other"),
            {"type": "turn_context", "payload": {"model": "gpt-5.1"}},
            self.token(b)])
        result = self.collector.collect(self.config)
        self.assertEqual([(e["input"], e["output"], e["cache_read"], e["model"]) for e in result["events"]],
                         [(100, 10, 80, "gpt-5"), (50, 20, 30, "gpt-5.1")])
        self.config["cursors"] = result["cursors"]
        self.write("codex", [self.token({"input_tokens": 180, "output_tokens": 35, "cached_input_tokens": 120})], mode="a")
        self.assertEqual(self.collector.collect(self.config)["events"][0]["input"], 30)

    def test_codex_prefers_last_and_preserves_identity_after_rewrite(self):
        record = self.token({"input_tokens": 500, "output_tokens": 80}, {"input_tokens": 30, "output_tokens": 5})
        meta = {"type": "session_meta", "payload": {"id": "thread-1"}}
        file = self.write("codex", [meta, record])
        result = self.collector.collect(self.config)
        self.assertEqual(result["events"][0]["input"], 30)
        self.config["cursors"] = result["cursors"]
        original_stat = file.stat()
        # Keep the old inode allocated: unlink + recreate may reuse it on Linux,
        # in which case identical content correctly resumes at EOF.
        retired = file.with_suffix(".retired")
        file.rename(retired)
        self.write("codex", [meta, record])
        self.assertNotEqual(file.stat().st_ino, retired.stat().st_ino)
        # Even a replacement preserving size and timestamps must be rescanned.
        os.utime(file, ns=(original_stat.st_atime_ns, original_stat.st_mtime_ns))
        rewritten = self.collector.collect(self.config)
        self.assertEqual(rewritten["errors"], [])
        self.assertEqual(len(rewritten["events"]), 1)
        self.assertEqual(rewritten["events"][0]["id"], result["events"][0]["id"])

    def test_error_keeps_original_cursor_and_has_no_conversation(self):
        self.write("claude", [self.claude()])
        result = self.collector.collect(self.config)
        self.config["cursors"] = result["cursors"]
        file = self.write("claude", [self.claude("msg-2")], mode="a")
        with file.open("a") as stream:
            stream.write("PRIVATE_MALFORMED_CONTENT\n")
        failed = self.collector.collect(self.config)
        self.assertEqual(failed["events"], [])
        self.assertEqual(failed["cursors"], self.config["cursors"])
        self.assertNotIn("PRIVATE_MALFORMED_CONTENT", json.dumps(failed))
        self.assertTrue(failed["errors"])

    def test_gemini_and_grok_per_prompt(self):
        self.write("gemini", [{"sessionId": "g1", "messages": [{"id": "m1", "type": "gemini", "timestamp": TS,
            "model": "gemini-2.5-pro", "content": "PRIVATE", "tokens": {"input": 100, "output": 20, "thoughts": 10, "cached": 80}}]}], "session-1.json")
        records = [{"timestamp": TS, "params": {"update": {"sessionUpdate": "turn_completed", "prompt_id": prompt,
                    "usage": {"modelUsage": {"grok-4": {"inputTokens": 100, "outputTokens": 30, "cachedReadTokens": 80}}}}}}
                   for prompt in ["p1", "p2"]]
        self.write("grokbuild", records, "updates.jsonl")
        events = self.collector.collect(self.config)["events"]
        self.assertEqual(len(events), 3)
        self.assertEqual(sum(e["output"] for e in events), 90)
        self.assertNotIn("PRIVATE", json.dumps(events))

    @unittest.skipUnless(shutil.which("zstd"), "zstd not installed")
    def test_compressed_codex_archive_resumes(self):
        records = [{"type": "session_meta", "payload": {"id": "archive-thread"}}]
        records += [self.token({"input_tokens": 100 * i, "output_tokens": 10 * i}) for i in range(1, 6)]
        raw = self.write("codex", records)
        compressed = raw.with_suffix(".jsonl.zst")
        subprocess.run(["zstd", "-q", "-f", str(raw), "-o", str(compressed)], check=True)
        raw.unlink()
        original = self.collector.MAX_EVENTS
        self.collector.MAX_EVENTS = 2
        try:
            events = []
            for _ in range(4):
                result = self.collector.collect(self.config)
                self.assertEqual(result["errors"], [])
                events += result["events"]
                self.config["cursors"] = result["cursors"]
            self.assertEqual(len(events), 5)
            self.assertEqual(sum(e["input"] for e in events), 500)
        finally:
            self.collector.MAX_EVENTS = original

    def test_codex_error_does_not_mutate_committed_signature(self):
        a = {"input_tokens": 100, "output_tokens": 10}
        b = {"input_tokens": 200, "output_tokens": 20}
        file = self.write("codex", [self.token(a)])
        result = self.collector.collect(self.config)
        self.config["cursors"] = result["cursors"]
        before = json.dumps(self.config["cursors"], sort_keys=True)
        self.write("codex", [self.token(b)], mode="a")
        with file.open("a") as stream:
            stream.write("invalid\n")
        failed = self.collector.collect(self.config)
        self.assertEqual(json.dumps(failed["cursors"], sort_keys=True), before)


    def test_codex_same_second_requests_keep_distinct_identity(self):
        last = {"input_tokens": 100, "output_tokens": 10}
        self.write("codex", [self.token(None, last, timestamp="2026-10-14T01:00:00.100Z"),
                             self.token(None, last, timestamp="2026-10-14T01:00:00.200Z")])
        events = self.collector.collect(self.config)["events"]
        self.assertEqual(len(events), 2)
        self.assertNotEqual(events[0]["id"], events[1]["id"])


    def test_cutoff_still_advances_codex_baseline(self):
        self.config["since"] = self.collector.stamp(TS)
        self.write("codex", [self.token({"input_tokens": 100, "output_tokens": 10}, timestamp="2026-09-01T01:00:00Z"),
                             self.token({"input_tokens": 150, "output_tokens": 20})])
        self.assertEqual([e["input"] for e in self.collector.collect(self.config)["events"]], [50])

    def test_oversized_record_is_skipped(self):
        file = Path(self.roots["claude"][0]) / "log.jsonl"
        with file.open("w") as stream:
            stream.write(json.dumps({"type": "compacted", "payload": "x" * (collector.MAX_LINE + 10)}) + "\n")
            stream.write(json.dumps(self.claude()) + "\n")
        result = self.collector.collect(self.config)
        self.assertEqual(result["errors"], [])
        self.assertEqual([e["id"] for e in result["events"]], ["msg-1"])

    def test_batch_limit_resumes_without_losing_records(self):
        self.write("claude", [self.claude(str(i)) for i in range(5)])
        original = self.collector.MAX_EVENTS
        self.collector.MAX_EVENTS = 2
        try:
            ids = []
            for _ in range(3):
                result = self.collector.collect(self.config)
                ids += [e["id"] for e in result["events"]]
                self.config["cursors"] = result["cursors"]
            self.assertEqual(ids, [str(i) for i in range(5)])
        finally:
            self.collector.MAX_EVENTS = original



@unittest.skipUnless(collector._libzstd() and shutil.which("zstd"), "libzstd not found")
class LibzstdCollectorTest(unittest.TestCase):
    def test_reads_archive_without_cli_or_module(self):
        with tempfile.TemporaryDirectory() as tmp:
            raw = Path(tmp) / "log.jsonl"
            lines = [json.dumps({"n": i, "pad": "y" * 5000}) + "\n" for i in range(200)]
            raw.write_text("".join(lines))
            subprocess.run(["zstd", "-q", str(raw), "-o", str(raw) + ".zst"], check=True)
            saved = sys.modules.get("zstandard")
            sys.modules["zstandard"] = None
            try:
                with collector.open_log(Path(str(raw) + ".zst"), True) as stream:
                    self.assertIsInstance(stream.raw, collector._ZstdReader)
                    self.assertEqual(stream.read().decode(), "".join(lines))
            finally:
                if saved is None:
                    sys.modules.pop("zstandard", None)
                else:
                    sys.modules["zstandard"] = saved


@unittest.skipUnless(shutil.which("node"), "node not installed")
class NodeRemoteUsageCollectorTest(RemoteUsageCollectorTest):
    collector = NodeCollector()

    def test_matches_python_output(self):
        a = {"input_tokens": 100, "output_tokens": 10, "cached_input_tokens": 80}
        self.write("claude", [self.claude(), self.claude("msg-2")])
        self.write("codex", [{"type": "session_meta", "payload": {"id": "thread-é"}},
                             {"type": "turn_context", "payload": {"model": "gpt-5"}}, self.token(a)])
        self.write("grokbuild", [{"timestamp": TS, "params": {"update": {"usage": {"inputTokens": 5}}}}], "updates.jsonl")
        expected = collector.collect(self.config)
        actual = self.collector.collect(self.config)
        self.assertEqual(actual, json.loads(json.dumps(expected)))
        self.config["cursors"] = actual["cursors"]
        self.assertEqual(collector.collect(self.config)["events"], [])


if __name__ == "__main__":
    unittest.main()
