"""Read-only SSH usage collector. Only the explicit metadata below crosses SSH.

The caller supplies CONFIG before this source. Cursors advance only over complete
records; the caller commits events and cursors together. No remote files are written.
"""
import datetime
import hashlib
import json
import os
import contextlib
import io
import shutil
import subprocess
import time
from pathlib import Path

MAX_EVENTS = 8000
MAX_LINE = 8 * 1024 * 1024


def stamp(value):
    try:
        if isinstance(value, (int, float)):
            return int(value / 1000 if value > 100000000000 else value)
        return int(datetime.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp())
    except (ValueError, TypeError, AttributeError, OverflowError):
        return None


def count(value):
    return min(max(int(value or 0), 0), 4294967295)


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def counters(value):
    return [count(value.get("input_tokens")), count(value.get("output_tokens")),
            count(value.get("cached_input_tokens", value.get("cache_read_input_tokens"))),
            count(value.get("cache_write_input_tokens"))]


def parse_record(app, record, state, file_key):
    ts = stamp(record.get("timestamp"))
    session = state.get("session", file_key)
    if app == "claude":
        msg = record.get("message", {})
        usage = msg.get("usage")
        if record.get("type") != "assistant" or not isinstance(usage, dict) or not msg.get("id"):
            return []
        model = msg.get("model", "unknown")
        if model == "<synthetic>":
            return []
        return [event(app, msg["id"], record.get("sessionId", session), model, ts,
                      count(usage.get("input_tokens")), count(usage.get("output_tokens")),
                      count(usage.get("cache_read_input_tokens")), count(usage.get("cache_creation_input_tokens")),
                      msg["id"], count((usage.get("cache_creation") or {}).get("ephemeral_1h_input_tokens")))]
    if app == "codex":
        payload = record.get("payload") or {}
        kind = record.get("type")
        if kind == "session_meta":
            state["session"] = payload.get("id", session)
            if payload.get("forked_from_id") or payload.get("parent_thread_id"):
                state["fork_start"] = stamp(payload.get("timestamp")) or ts
        if kind == "turn_context":
            state["model"] = payload.get("model", state.get("model", "unknown"))
        if payload.get("type") == "thread_settings_applied":
            state["model"] = (payload.get("thread_settings") or {}).get("model", state.get("model", "unknown"))
        if kind != "event_msg" or payload.get("type") != "token_count":
            return []
        info = payload.get("info")
        if not isinstance(info, dict):
            return []
        total_obj, last_obj = info.get("total_token_usage"), info.get("last_token_usage")
        total = counters(total_obj) if isinstance(total_obj, dict) and total_obj else None
        last = counters(last_obj) if isinstance(last_obj, dict) and last_obj else None
        signature = digest([total, last])
        source = str((payload.get("rate_limits") or {}).get("limit_id", ""))
        signatures = state.setdefault("signatures", {})
        repeated = total is not None and (signatures.get(source) == signature or state.get("previous") == signature)
        if total is not None:
            signatures[source] = signature
        state["previous"] = signature
        old = state.get("total")
        delta = last if last is not None else ([max(a - b, 0) for a, b in zip(total, old)] if total and old else total)
        if total:
            state["total"] = [max(a, b) for a, b in zip(total, old)] if old else total
        state["model"] = info.get("model") or info.get("model_name") or payload.get("model") or state.get("model", "unknown")
        if repeated or not delta or (state.get("fork_start") and ts and ts < state["fork_start"]):
            return []
        # Codex rollouts do not expose an upstream response ID. This identity
        # survives file rewrites and moves, unlike a line number or byte offset.
        ident = digest([state.get("session", file_key), record.get("timestamp"), total, last, state["model"]])
        return [event(app, ident, state.get("session", file_key), state["model"], ts, *delta)]
    if app == "grokbuild":
        update = (record.get("params") or {}).get("update") or {}
        if update.get("sessionUpdate", "turn_completed") != "turn_completed":
            return []
        usage = update.get("usage")
        if not isinstance(usage, dict):
            return []
        models = usage.get("modelUsage") or {"unknown": usage}
        prompt = update.get("prompt_id") or digest([session, record.get("timestamp"), models])
        return [event(app, str(prompt) + ":" + model, session, model, ts,
                      count(u.get("inputTokens")), count(u.get("outputTokens")), count(u.get("cachedReadTokens")), 0)
                for model, u in models.items() if isinstance(u, dict)]
    if app == "gemini":
        result = []
        for msg in record.get("messages", [record]):
            tokens = msg.get("tokens")
            if msg.get("type") != "gemini" or not isinstance(tokens, dict) or not msg.get("id"):
                continue
            sid = record.get("sessionId", session)
            result.append(event(app, str(sid) + ":" + str(msg["id"]), sid, msg.get("model", "unknown"),
                                stamp(msg.get("timestamp")), count(tokens.get("input")),
                                min(count(tokens.get("output")) + count(tokens.get("thoughts")), 4294967295),
                                count(tokens.get("cached")), 0, msg.get("responseId")))
        return result
    return []


def event(app, ident, session, model, ts, input_tokens, output_tokens, cache_read, cache_write, upstream_id=None, cache_1h=0):
    return dict(app=app, id=str(ident), session=str(session), model=str(model), timestamp=ts,
                input=input_tokens, output=output_tokens, cache_read=cache_read, cache_write=cache_write,
                cache_1h=cache_1h, upstream_id=upstream_id)


@contextlib.contextmanager
def open_log(file, compressed):
    if not compressed:
        with file.open("rb") as stream:
            yield stream
        return
    # Compressed rollouts are immutable archives. Decode on the server so
    # conversation content still never crosses SSH. Reopen/skip on large files.
    try:
        import zstandard
    except ImportError:
        zstandard = None
    if zstandard:
        with file.open("rb") as raw:
            with zstandard.ZstdDecompressor().stream_reader(raw) as reader:
                yield io.BufferedReader(reader)
    elif shutil.which("zstd"):
        process = subprocess.Popen(["zstd", "-dc", str(file)], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        try:
            yield process.stdout
        finally:
            process.terminate()
            process.wait(timeout=5)
    else:
        raise ValueError("zstd unavailable")


def collect(config):
    home = Path.home()
    defaults = {
        "claude": [str(Path(os.environ.get("CLAUDE_CONFIG_DIR", home / ".claude")) / "projects")],
        "codex": [str(Path(os.environ.get("CODEX_HOME", home / ".codex")) / d) for d in ("sessions", "archived_sessions")],
        "gemini": [str(home / ".gemini" / "tmp")],
        "grokbuild": [str(home / ".grok" / d) for d in ("sessions", "archived_sessions")],
    }
    cursors = dict(config.get("cursors", {}))
    events, errors = [], []
    cutoff = config.get("since", 0)
    deadline = time.monotonic() + 45
    scanned = 0
    more = False
    seen = set()
    for app, roots in defaults.items():
        for root in config.get("roots", {}).get(app, roots):
            path = Path(os.path.expandvars(os.path.expanduser(root)))
            if not path.is_dir():
                continue
            for base, dirs, files in os.walk(path, followlinks=False):
                dirs[:] = sorted(d for d in dirs if not (Path(base) / d).is_symlink())
                for name in sorted(files):
                    is_json = app == "gemini" and name.startswith("session-") and name.endswith(".json")
                    compressed = app == "codex" and name.endswith(".jsonl.zst")
                    if not is_json and not compressed and not name.endswith(".jsonl"):
                        continue
                    if app == "grokbuild" and name != "updates.jsonl":
                        continue
                    file = Path(base) / name
                    if file.is_symlink():
                        continue
                    key = app + ":" + hashlib.sha256(str(file).encode()).hexdigest()
                    if key in seen:
                        continue
                    seen.add(key)
                    if len(events) >= MAX_EVENTS or time.monotonic() >= deadline:
                        more = True
                        break
                    try:
                        stat = file.stat()
                        prior = cursors.get(key, {})
                        scanned += 1
                        if prior.get("size") == stat.st_size and prior.get("mtime") == stat.st_mtime_ns:
                            continue
                        with open_log(file, compressed) as stream:
                            offset = prior.get("offset", 0)
                            # Verify the consumed tail as well as inode and size.
                            if compressed:
                                resume = prior.get("inode") == stat.st_ino and prior.get("archive_size") == stat.st_size and prior.get("mtime") == stat.st_mtime_ns
                                offset = offset if resume else 0
                                remaining = offset
                                while remaining:
                                    data = stream.read(min(remaining, 65536))
                                    if not data:
                                        raise ValueError("truncated archive")
                                    remaining -= len(data)
                            else:
                                stream.seek(max(0, offset - 256))
                                tail = hashlib.sha256(stream.read(min(256, offset))).hexdigest()
                                resume = (not is_json and offset <= stat.st_size and prior.get("inode") == stat.st_ino and prior.get("tail") == tail)
                                stream.seek(offset if resume else 0)
                            state = json.loads(json.dumps(prior.get("state", {}))) if resume else {}
                            finished = False
                            if app == "grokbuild":
                                state["session"] = file.parent.name
                            batch = []
                            if is_json:
                                if stat.st_size > MAX_LINE:
                                    raise ValueError("session JSON exceeds 8 MiB")
                                batch = parse_record(app, json.load(stream), state, key)
                            else:
                                while len(events) + len(batch) < MAX_EVENTS and time.monotonic() < deadline:
                                    before = offset if compressed else stream.tell()
                                    line = stream.readline(MAX_LINE + 1)
                                    if not line:
                                        finished = True
                                        break
                                    if len(line) > MAX_LINE:
                                        raise ValueError("record exceeds 8 MiB")
                                    if not line.endswith(b"\n"):
                                        if not compressed:
                                            stream.seek(before)
                                        break
                                    if compressed:
                                        offset += len(line)
                                    try:
                                        record = json.loads(line)
                                    except (ValueError, UnicodeError):
                                        raise ValueError("invalid JSON record; cursor retained for retry")
                                    if isinstance(record, dict):
                                        batch.extend(parse_record(app, record, state, key))
                            if not compressed:
                                offset = stream.tell()
                                stream.seek(max(0, offset - 256))
                                tail = hashlib.sha256(stream.read(min(256, offset))).hexdigest()
                                finished = offset >= stat.st_size
                            else:
                                tail = ""
                        filtered = [e for e in batch if e["timestamp"] is not None and e["timestamp"] >= cutoff and any(e[k] for k in ("input", "output", "cache_read", "cache_write"))]
                        # A large Gemini JSON must retry in full; never commit a
                        # cursor for only a subset of its messages.
                        events.extend(filtered)
                        cursors[key] = dict(offset=offset, size=stat.st_size if finished or is_json else -1, archive_size=stat.st_size,
                                            mtime=stat.st_mtime_ns, inode=stat.st_ino, tail=tail, state=state)
                        if not finished and not is_json:
                            more = True
                    except Exception:
                        # Deliberately omit raw exception text (may include content).
                        if compressed and not shutil.which("zstd"):
                            errors.append("codex: compressed logs require zstd or Python zstandard on the host")
                        errors.append(app + ": unreadable or invalid session file (" + key[-12:] + ")")
                if more and (len(events) >= MAX_EVENTS or time.monotonic() >= deadline):
                    break
    return dict(events=events, cursors=cursors, files_scanned=scanned, errors=errors[:30], more=more)


if __name__ == "__main__":
    print(json.dumps(collect(CONFIG), separators=(",", ":")))
