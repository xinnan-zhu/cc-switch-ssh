// Node.js port of remote_usage_collector.py for hosts without Python 3.
// Output, cursors and event identities must match the Python collector.
"use strict";
const crypto = require("crypto");
const fs = require("fs");
const os = require("os");
const childProcess = require("child_process");
const zlib = require("zlib");

const LIMITS = { events: 8000 };
const MAX_LINE = 8 * 1024 * 1024;
const MAX_ARCHIVE = 1024 * 1024 * 1024;
const MAX_COUNT = 4294967295;
const BIGINT = "__cc_switch_bigint__";
const MISSING_ZSTD = "codex: compressed logs require the zstd command or Node.js 23.8+ on the host";

function pyStr(value) {
  if (value === null || value === undefined) return "None";
  if (value === true) return "True";
  if (value === false) return "False";
  return String(value);
}

function isDict(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function get(obj, key, fallback) {
  return isDict(obj) && Object.prototype.hasOwnProperty.call(obj, key) ? obj[key] : fallback;
}

function stamp(value) {
  if (typeof value === "number") {
    const seconds = Math.trunc(value > 100000000000 ? value / 1000 : value);
    return Number.isFinite(seconds) ? seconds : null;
  }
  if (typeof value !== "string") return null;
  let text = value.replace("Z", "+00:00");
  if (!/^\d{4}-\d{2}-\d{2}([T ]\d{2}(:\d{2}(:\d{2}(\.\d+)?)?)?([+-]\d{2}:\d{2})?)?$/.test(text)) return null;
  if (text.length === 10) text += "T00:00:00";
  const ms = Date.parse(text.replace(" ", "T"));
  return Number.isNaN(ms) ? null : Math.floor(ms / 1000);
}

function count(value) {
  const number = Math.trunc(Number(value || 0));
  if (Number.isNaN(number)) throw new Error("invalid count");
  return Math.min(Math.max(number, 0), MAX_COUNT);
}

// Matches Python json.dumps(sort_keys=True, separators=(",", ":")).
function pyJson(value) {
  if (value === null || value === undefined) return "null";
  if (value === true) return "true";
  if (value === false) return "false";
  if (typeof value === "number") return String(value);
  if (typeof value === "string") {
    return JSON.stringify(value).replace(/[\u007f-\uffff]/g, (c) => "\\u" + c.charCodeAt(0).toString(16).padStart(4, "0"));
  }
  if (Array.isArray(value)) return "[" + value.map(pyJson).join(",") + "]";
  return "{" + Object.keys(value).sort().map((k) => pyJson(k) + ":" + pyJson(value[k])).join(",") + "}";
}

function sha256(data) {
  return crypto.createHash("sha256").update(data).digest("hex");
}

function digest(value) {
  return sha256(pyJson(value));
}

function counters(value) {
  return [count(value.input_tokens), count(value.output_tokens),
    count(get(value, "cached_input_tokens", value.cache_read_input_tokens)),
    count(value.cache_write_input_tokens)];
}

function event(app, ident, session, model, ts, input, output, cacheRead, cacheWrite, upstreamId, cache1h) {
  return { app, id: pyStr(ident), session: pyStr(session), model: pyStr(model), timestamp: ts,
    input, output, cache_read: cacheRead, cache_write: cacheWrite,
    cache_1h: cache1h || 0, upstream_id: upstreamId === undefined ? null : upstreamId };
}

function parseRecord(app, record, state, fileKey) {
  const ts = stamp(record.timestamp);
  const session = get(state, "session", fileKey);
  if (app === "claude") {
    const msg = get(record, "message", {});
    const usage = msg.usage;
    if (record.type !== "assistant" || !isDict(usage) || !msg.id) return [];
    const model = get(msg, "model", "unknown");
    if (model === "<synthetic>") return [];
    return [event(app, msg.id, get(record, "sessionId", session), model, ts,
      count(usage.input_tokens), count(usage.output_tokens),
      count(usage.cache_read_input_tokens), count(usage.cache_creation_input_tokens),
      msg.id, count((usage.cache_creation || {}).ephemeral_1h_input_tokens))];
  }
  if (app === "codex") {
    const payload = record.payload || {};
    const kind = record.type;
    if (kind === "session_meta") {
      state.session = get(payload, "id", session);
      if (payload.forked_from_id || payload.parent_thread_id) {
        state.fork_start = stamp(payload.timestamp) || ts;
      }
    }
    if (kind === "turn_context") state.model = get(payload, "model", get(state, "model", "unknown"));
    if (payload.type === "thread_settings_applied") {
      state.model = get(payload.thread_settings || {}, "model", get(state, "model", "unknown"));
    }
    if (kind !== "event_msg" || payload.type !== "token_count") return [];
    const info = payload.info;
    if (!isDict(info)) return [];
    const totalObj = info.total_token_usage;
    const lastObj = info.last_token_usage;
    const total = isDict(totalObj) && Object.keys(totalObj).length ? counters(totalObj) : null;
    const last = isDict(lastObj) && Object.keys(lastObj).length ? counters(lastObj) : null;
    const signature = digest([total, last]);
    const limitId = get(payload.rate_limits || {}, "limit_id", "");
    const source = pyStr(limitId);
    if (!isDict(state.signatures)) state.signatures = {};
    const signatures = state.signatures;
    const repeated = total !== null && (signatures[source] === signature || state.previous === signature);
    if (total !== null) signatures[source] = signature;
    state.previous = signature;
    const old = state.total;
    let delta = last;
    if (delta === null) delta = total && old ? total.map((a, i) => Math.max(a - old[i], 0)) : total;
    if (total) state.total = old ? total.map((a, i) => Math.max(a, old[i])) : total;
    state.model = info.model || info.model_name || payload.model || get(state, "model", "unknown");
    if (repeated || !delta || (state.fork_start && ts && ts < state.fork_start)) return [];
    const ident = digest([get(state, "session", fileKey), get(record, "timestamp", null), total, last, state.model]);
    return [event(app, ident, get(state, "session", fileKey), state.model, ts, ...delta)];
  }
  if (app === "grokbuild") {
    const update = (record.params || {}).update || {};
    if (get(update, "sessionUpdate", "turn_completed") !== "turn_completed") return [];
    const usage = update.usage;
    if (!isDict(usage)) return [];
    const models = usage.modelUsage || { unknown: usage };
    const prompt = update.prompt_id || digest([session, get(record, "timestamp", null), models]);
    return Object.entries(models).filter(([, u]) => isDict(u)).map(([model, u]) =>
      event(app, pyStr(prompt) + ":" + model, session, model, ts,
        count(u.inputTokens), count(u.outputTokens), count(u.cachedReadTokens), 0));
  }
  if (app === "gemini") {
    const result = [];
    for (const msg of get(record, "messages", [record])) {
      const tokens = msg.tokens;
      if (msg.type !== "gemini" || !isDict(tokens) || !msg.id) continue;
      const sid = get(record, "sessionId", session);
      result.push(event(app, pyStr(sid) + ":" + pyStr(msg.id), sid, get(msg, "model", "unknown"),
        stamp(msg.timestamp), count(tokens.input),
        Math.min(count(tokens.output) + count(tokens.thoughts), MAX_COUNT),
        count(tokens.cached), 0, msg.responseId));
    }
    return result;
  }
  return [];
}

function which(name) {
  return (process.env.PATH || "").split(":").some((dir) => {
    try {
      fs.accessSync((dir || ".") + "/" + name, fs.constants.X_OK);
      return true;
    } catch (_) {
      return false;
    }
  });
}

function hasZstd() {
  return typeof zlib.zstdDecompressSync === "function" || which("zstd");
}

// Compressed rollouts are immutable archives. Decode on the server so
// conversation content still never crosses SSH.
function decompress(file) {
  if (typeof zlib.zstdDecompressSync === "function") {
    return zlib.zstdDecompressSync(fs.readFileSync(file), { maxOutputLength: MAX_ARCHIVE });
  }
  if (!which("zstd")) throw new Error("zstd unavailable");
  return childProcess.execFileSync("zstd", ["-dc", file], { maxBuffer: MAX_ARCHIVE, stdio: ["ignore", "pipe", "ignore"] });
}

function readRange(fd, start, end) {
  const buffer = Buffer.alloc(Math.max(0, end - start));
  let done = 0;
  while (done < buffer.length) {
    const n = fs.readSync(fd, buffer, done, buffer.length - done, start + done);
    if (!n) break;
    done += n;
  }
  return buffer.subarray(0, done);
}

function readToEnd(fd, start) {
  const chunks = [];
  let position = start;
  const chunk = Buffer.alloc(1024 * 1024);
  for (;;) {
    const n = fs.readSync(fd, chunk, 0, chunk.length, position);
    if (!n) break;
    chunks.push(Buffer.from(chunk.subarray(0, n)));
    position += n;
  }
  return Buffer.concat(chunks);
}

function tailHash(fd, offset) {
  return sha256(readRange(fd, Math.max(0, offset - 256), offset));
}

// Matches str(pathlib.Path(...)) so cursor keys are shared with Python.
function normalize(path) {
  const parts = path.split("/").filter((part) => part && part !== ".");
  return (path.startsWith("/") ? "/" : "") + parts.join("/") || ".";
}

function expand(path) {
  const home = process.env.HOME || os.homedir();
  const result = path === "~" || path.startsWith("~/") ? home + path.slice(1) : path;
  return result.replace(/\$(\w+|\{[^}]*\})/g, (match, name) => {
    const key = name.startsWith("{") ? name.slice(1, -1) : name;
    return process.env[key] === undefined ? match : process.env[key];
  });
}

function isDir(path) {
  try {
    return fs.statSync(path).isDirectory();
  } catch (_) {
    return false;
  }
}

function isSymlink(path) {
  try {
    return fs.lstatSync(path).isSymbolicLink();
  } catch (_) {
    return false;
  }
}

function* walk(base) {
  let entries;
  try {
    entries = fs.readdirSync(base, { withFileTypes: true });
  } catch (_) {
    return;
  }
  const dirs = [];
  const files = [];
  for (const entry of entries) {
    const full = base + "/" + entry.name;
    if (entry.isDirectory()) dirs.push(entry.name);
    else if (entry.isSymbolicLink() && isDir(full)) dirs.push(entry.name);
    else files.push(entry.name);
  }
  yield [base, files.sort()];
  for (const dir of dirs.sort()) {
    const full = base + "/" + dir;
    if (!isSymlink(full)) yield* walk(full);
  }
}

function exactInt(value) {
  return Number.isSafeInteger(Number(value)) ? Number(value) : BIGINT + value.toString();
}

function sameInt(prior, current) {
  return prior !== undefined && prior !== null && String(prior).replace(BIGINT, "") === current.toString();
}

// JSON.parse rounds integers past 2^53 (st_mtime_ns, inodes). Keep them as
// tagged strings outside string literals; serialize() writes them back raw.
function parseConfig(text) {
  let out = "";
  let i = 0;
  while (i < text.length) {
    const c = text[i];
    if (c === '"') {
      let j = i + 1;
      while (text[j] !== '"') j += text[j] === "\\" ? 2 : 1;
      out += text.slice(i, j + 1);
      i = j + 1;
    } else if (/[-\d]/.test(c)) {
      let j = i + 1;
      while (j < text.length && /[\d.eE+-]/.test(text[j])) j++;
      const token = text.slice(i, j);
      out += /^-?\d{16,}$/.test(token) ? '"' + BIGINT + token + '"' : token;
      i = j;
    } else {
      out += c;
      i++;
    }
  }
  return JSON.parse(out);
}

function collectFile(app, file, key, prior, compressed, isJson, cutoff, events, deadline) {
  const stat = fs.statSync(file, { bigint: true });
  const size = Number(stat.size);
  if (prior.size === size && sameInt(prior.mtime, stat.mtimeNs) && sameInt(prior.inode, stat.ino)) {
    return null;
  }
  let offset = prior.offset || 0;
  let resume;
  let data;
  let start;
  let tail = "";
  const fd = compressed ? null : fs.openSync(file, "r");
  try {
    if (compressed) {
      resume = sameInt(prior.inode, stat.ino) && prior.archive_size === size && sameInt(prior.mtime, stat.mtimeNs);
      offset = resume ? offset : 0;
      data = decompress(file);
      if (data.length < offset) throw new Error("truncated archive");
      start = offset;
      data = data.subarray(offset);
    } else {
      if (isJson && size > MAX_LINE) throw new Error("session JSON exceeds 8 MiB");
      tail = tailHash(fd, offset);
      resume = !isJson && offset <= size && sameInt(prior.inode, stat.ino) && prior.tail === tail;
      start = resume ? offset : 0;
      data = readToEnd(fd, start);
    }
    const state = resume ? JSON.parse(JSON.stringify(prior.state || {})) : {};
    let finished = false;
    if (app === "grokbuild") state.session = file.split("/").slice(-2, -1)[0];
    let batch = [];
    let pos = 0;
    if (isJson) {
      batch = parseRecord(app, JSON.parse(data.toString("utf8")), state, key);
      pos = data.length;
    } else {
      while (events.length + batch.length < LIMITS.events && Date.now() < deadline) {
        if (pos >= data.length) {
          finished = true;
          break;
        }
        const newline = data.indexOf(10, pos);
        if (newline === -1) break;
        const end = newline + 1;
        const line = data.subarray(pos, end);
        pos = end;
        // Oversized records (compaction snapshots, inline images) carry no usage.
        if (line.length > MAX_LINE) continue;
        let record;
        try {
          record = JSON.parse(line.toString("utf8"));
        } catch (_) {
          throw new Error("invalid JSON record; cursor retained for retry");
        }
        if (isDict(record)) batch.push(...parseRecord(app, record, state, key));
      }
    }
    offset = start + pos;
    if (!compressed) {
      tail = tailHash(fd, offset);
      finished = offset >= size;
    } else {
      tail = "";
    }
    const filtered = batch.filter((e) => e.timestamp !== null && e.timestamp >= cutoff &&
      (e.input || e.output || e.cache_read || e.cache_write));
    return {
      events: filtered,
      cursor: { offset, size: finished || isJson ? size : -1, archive_size: size,
        mtime: exactInt(stat.mtimeNs), inode: exactInt(stat.ino), tail, state },
      more: !finished && !isJson,
    };
  } finally {
    if (fd !== null) fs.closeSync(fd);
  }
}

function collect(config) {
  const home = process.env.HOME || os.homedir();
  const codexHome = process.env.CODEX_HOME || home + "/.codex";
  const defaults = {
    claude: [normalize(process.env.CLAUDE_CONFIG_DIR || home + "/.claude") + "/projects"],
    codex: ["sessions", "archived_sessions"].map((d) => normalize(codexHome) + "/" + d),
    gemini: [home + "/.gemini/tmp"],
    grokbuild: ["sessions", "archived_sessions"].map((d) => home + "/.grok/" + d),
  };
  const cursors = Object.assign({}, config.cursors || {});
  const events = [];
  const errors = [];
  const cutoff = config.since || 0;
  const deadline = Date.now() + 45000;
  const roots = config.roots || {};
  let scanned = 0;
  let more = false;
  let stop = false;
  const seen = new Set();
  for (const app of Object.keys(defaults)) {
    for (const root of Object.prototype.hasOwnProperty.call(roots, app) ? roots[app] : defaults[app]) {
      if (stop) break;
      const path = normalize(expand(root));
      if (!isDir(path)) continue;
      for (const [base, files] of walk(path)) {
        for (const name of files) {
          const isJson = app === "gemini" && name.startsWith("session-") && name.endsWith(".json");
          const compressed = app === "codex" && name.endsWith(".jsonl.zst");
          if (!isJson && !compressed && !name.endsWith(".jsonl")) continue;
          if (app === "grokbuild" && name !== "updates.jsonl") continue;
          const file = base + "/" + name;
          if (isSymlink(file)) continue;
          const key = app + ":" + sha256(file);
          if (seen.has(key)) continue;
          seen.add(key);
          if (events.length >= LIMITS.events || Date.now() >= deadline) {
            more = true;
            stop = true;
            break;
          }
          try {
            scanned += 1;
            const result = collectFile(app, file, key, cursors[key] || {}, compressed, isJson, cutoff, events, deadline);
            if (!result) continue;
            events.push(...result.events);
            cursors[key] = result.cursor;
            if (result.more) more = true;
          } catch (_) {
            // Deliberately omit raw exception text (may include content).
            if (compressed && !hasZstd()) {
              if (!errors.includes(MISSING_ZSTD)) errors.push(MISSING_ZSTD);
              continue;
            }
            errors.push(app + ": unreadable or invalid session file (" + key.slice(-12) + ")");
          }
        }
        if (stop) break;
      }
    }
  }
  return { events, cursors, files_scanned: scanned, errors: errors.slice(0, 30), more };
}

function serialize(result) {
  return JSON.stringify(result).replace(new RegExp('"' + BIGINT + '(\\d+)"', "g"), "$1");
}

if (typeof CONFIG_JSON !== "undefined") {
  process.stdout.write(serialize(collect(parseConfig(CONFIG_JSON))));
} else {
  module.exports = { collect, parseConfig, serialize, LIMITS };
}
