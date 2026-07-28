"use strict";

const encoder = new TextEncoder();
const state = {
  credential: "",
  project: "",
  threads: [],
  runs: [],
  details: [],
  selected: "",
  approvals: [],
  authRequests: [],
  prompts: new Map(),
  openSteps: new Set(),
  cursor: "",
  refreshing: false,
  refreshQueued: false,
};

const ACTIVE_STATES = new Set([
  "queued",
  "acquiring_workspace",
  "starting",
  "running",
  "waiting_for_input",
  "waiting_for_approval",
  "waiting_for_auth",
  "cancelling",
]);

const STATE_LABELS = {
  queued: "Queued",
  acquiring_workspace: "Starting",
  starting: "Starting",
  running: "Running",
  waiting_for_input: "Needs input",
  waiting_for_approval: "Needs approval",
  waiting_for_auth: "Needs authorization",
  cancelling: "Stopping",
  cancelled: "Stopped",
  interrupted: "Interrupted",
  failed: "Failed",
  completed: "Done",
};

const $ = (id) => document.getElementById(id);
const app = $("app");
const locked = $("locked");
const messages = $("messages");
const prompt = $("prompt");
const composer = $("composer");
let refreshTimer = 0;

function hex(bytes) {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

function randomBytes(length = 16) {
  return crypto.getRandomValues(new Uint8Array(length));
}

function timestampBytes(timestamp) {
  const bytes = new Uint8Array(8);
  let value = BigInt(timestamp);
  for (let index = 7; index >= 0; index -= 1) {
    bytes[index] = Number(value & 255n);
    value >>= 8n;
  }
  return bytes;
}

function joinBytes(...parts) {
  const joined = new Uint8Array(parts.reduce((length, part) => length + part.length, 0));
  let offset = 0;
  for (const part of parts) {
    joined.set(part, offset);
    offset += part.length;
  }
  return joined;
}

async function signedHeaders(extra = {}) {
  const timestamp = Math.floor(Date.now() / 1000);
  const nonce = hex(randomBytes());
  const verifier = await crypto.subtle.digest("SHA-256", encoder.encode(state.credential));
  const key = await crypto.subtle.importKey(
    "raw",
    verifier,
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const signature = await crypto.subtle.sign(
    "HMAC",
    key,
    joinBytes(
      encoder.encode("KIT-LOOPBACK-REQUEST-V1\0"),
      timestampBytes(timestamp),
      encoder.encode(nonce),
    ),
  );
  return new Headers({
    Authorization: `Bearer ${state.credential}`,
    "X-Kit-Nonce": nonce,
    "X-Kit-Origin": location.origin,
    "X-Kit-Signature": hex(new Uint8Array(signature)),
    "X-Kit-Timestamp": String(timestamp),
    ...extra,
  });
}

async function request(path, options = {}) {
  const headers = await signedHeaders(options.headers || {});
  const response = await fetch(path, { ...options, headers, cache: "no-store" });
  if (!response.ok) {
    let detail = `${response.status} ${response.statusText}`;
    try {
      const problem = await response.json();
      detail = problem.detail || problem.title || detail;
    } catch (_) {
      // The response was not Problem Details JSON.
    }
    const error = new Error(detail);
    error.status = response.status;
    throw error;
  }
  return response;
}

async function json(path, options) {
  return (await request(path, options)).json();
}

async function mutation(path, body) {
  return json(path, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "Idempotency-Key": `ui-${hex(randomBytes())}`,
    },
    body: JSON.stringify(body),
  });
}

function randomId(prefix) {
  const alphabet = "0123456789abcdefghjkmnpqrstvwxyz";
  // Ids decode to 128 bits: the first of the 26 base32 chars may only be 0-7.
  const payload = Array.from(randomBytes(26), (byte) => alphabet[byte % 32]);
  payload[0] = alphabet[randomBytes(1)[0] % 8];
  return `${prefix}_${payload.join("")}`;
}

function node(tag, className, text) {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== undefined) element.textContent = text;
  return element;
}

function showNotice(text) {
  const notice = $("notice");
  notice.textContent = text;
  notice.hidden = !text;
}

function setStream(text, live) {
  const element = $("stream-state");
  element.classList.toggle("live", live);
  element.querySelector("b").textContent = text;
}

// ---- markdown ----

function escapeHtml(text) {
  return text.replace(/[&<>"']/g, (char) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  })[char]);
}

function inlineMarkdown(escaped) {
  const codes = [];
  let text = escaped.replace(/`([^`]+)`/g, (_, code) => {
    codes.push(code);
    return `\u0000${codes.length - 1}\u0000`;
  });
  text = text
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[^*])\*([^*\s][^*]*)\*/g, "$1<em>$2</em>")
    .replace(/\[([^\]]+)\]\((https?:[^)\s]+)\)/g, '<a href="$2" target="_blank" rel="noopener noreferrer">$1</a>');
  return text.replace(/\u0000(\d+)\u0000/g, (_, index) => `<code>${codes[Number(index)]}</code>`);
}

function renderMarkdown(text, partial = false) {
  const lines = text.split("\n");
  const blocks = [];
  let index = 0;
  const isList = (line) => /^\s*([-*]|\d+[.)])\s+/.test(line);
  while (index < lines.length) {
    const line = lines[index];
    if (line.startsWith("```")) {
      const buffer = [];
      index += 1;
      while (index < lines.length && !lines[index].startsWith("```")) {
        buffer.push(lines[index]);
        index += 1;
      }
      index += 1;
      blocks.push(`<pre><code>${escapeHtml(buffer.join("\n"))}</code></pre>`);
      continue;
    }
    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    if (heading) {
      const level = Math.min(heading[1].length + 2, 6);
      blocks.push(`<h${level}>${inlineMarkdown(escapeHtml(heading[2]))}</h${level}>`);
      index += 1;
      continue;
    }
    if (isList(line)) {
      const ordered = /^\s*\d/.test(line);
      const items = [];
      while (index < lines.length && isList(lines[index])) {
        const content = lines[index].replace(/^\s*([-*]|\d+[.)])\s+/, "");
        items.push(`<li>${inlineMarkdown(escapeHtml(content))}</li>`);
        index += 1;
      }
      const tag = ordered ? "ol" : "ul";
      blocks.push(`<${tag}>${items.join("")}</${tag}>`);
      continue;
    }
    if (!line.trim()) {
      index += 1;
      continue;
    }
    const buffer = [];
    while (
      index < lines.length
      && lines[index].trim()
      && !lines[index].startsWith("```")
      && !/^#{1,6}\s/.test(lines[index])
      && !isList(lines[index])
    ) {
      buffer.push(lines[index]);
      index += 1;
    }
    blocks.push(`<p>${inlineMarkdown(escapeHtml(buffer.join("\n")))}</p>`);
  }
  const container = node("div", partial ? "md partial" : "md");
  container.innerHTML = blocks.join("");
  return container;
}

// ---- data loading ----

async function promptText(artifact) {
  if (!state.prompts.has(artifact)) {
    const text = await request(`/v1/repository-artifacts/${encodeURIComponent(artifact)}`)
      .then((response) => response.text())
      .catch(() => "");
    state.prompts.set(artifact, text);
  }
  return state.prompts.get(artifact);
}

async function ensureProject() {
  try {
    await request(`/v1/projects/${state.project}`);
  } catch (error) {
    if (error.status !== 404) throw error;
    await mutation("/v1/projects", { id: state.project });
  }
}

async function createThread() {
  const id = randomId("thread");
  await mutation(`/v1/projects/${state.project}/threads`, { id });
  state.selected = id;
  await refresh();
  prompt.focus();
  return id;
}

async function loadRun(run) {
  const [user, transcript, cost] = await Promise.all([
    promptText(run.input),
    json(`/v1/runs/${run.id}/transcript`).catch(() => ({ items: [] })),
    json(`/v1/runs/${run.id}/cost`).catch(() => ({ usage: null, cost: null })),
  ]);
  return { run, user, transcript, cost };
}

async function refresh() {
  if (state.refreshing) {
    state.refreshQueued = true;
    return;
  }
  state.refreshing = true;
  try {
    const [threads, runs, approvals, authRequests] = await Promise.all([
      json(`/v1/projects/${state.project}/threads`),
      json(`/v1/projects/${state.project}/runs`),
      json(`/v1/projects/${state.project}/approvals`),
      json(`/v1/projects/${state.project}/auth-requests`),
    ]);
    state.threads = threads.items.filter((thread) => !thread.deletion_requested);
    state.runs = runs.items;
    state.approvals = approvals.items.filter((item) => item.decision === null);
    state.authRequests = authRequests.items.filter((item) => item.granted === null);
    if (!state.threads.some((thread) => thread.id === state.selected)) {
      state.selected = state.threads[0]?.id || "";
    }
    const selectedRuns = state.runs.filter((run) => run.thread_id === state.selected);
    const firstRuns = state.threads
      .map((thread) => state.runs.find((run) => run.thread_id === thread.id))
      .filter(Boolean);
    const [details] = await Promise.all([
      Promise.all(selectedRuns.map(loadRun)),
      Promise.all(firstRuns.map((run) => promptText(run.input))),
    ]);
    state.details = details;
    render();
    showNotice("");
  } catch (error) {
    showNotice(error.message);
  } finally {
    state.refreshing = false;
    if (state.refreshQueued) {
      state.refreshQueued = false;
      refresh();
    }
  }
}

function scheduleRefresh() {
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(refresh, 250);
}

// ---- rendering ----

function threadLabel(thread) {
  const first = state.runs.find((run) => run.thread_id === thread.id);
  const text = first ? (state.prompts.get(first.input) || "").trim().split("\n")[0] : "";
  if (!text) return "New thread";
  return text.length > 64 ? `${text.slice(0, 64)}…` : text;
}

function pendingForRun(runId) {
  return [
    ...state.approvals.filter((item) => item.run_id === runId).map((item) => ({ type: "approval", item })),
    ...state.authRequests.filter((item) => item.run_id === runId).map((item) => ({ type: "auth", item })),
  ];
}

function renderThreads() {
  const list = $("thread-list");
  list.replaceChildren();
  if (!state.threads.length) {
    list.append(node("p", "empty-threads", "No threads yet."));
    return;
  }
  for (const thread of state.threads) {
    const runs = state.runs.filter((run) => run.thread_id === thread.id);
    const attention = runs.some((run) => pendingForRun(run.id).length);
    const busy = runs.some((run) => ACTIVE_STATES.has(run.state));
    const button = node("button", `thread-button${thread.id === state.selected ? " active" : ""}`);
    button.type = "button";
    button.title = thread.id;
    const label = node("div", "thread-label");
    if (attention) label.append(node("i", "thread-flag attention"));
    else if (busy) label.append(node("i", "thread-flag busy"));
    label.append(node("span", "", threadLabel(thread)));
    button.append(label);
    button.append(node("div", "thread-sub", `${runs.length} run${runs.length === 1 ? "" : "s"}`));
    button.addEventListener("click", () => {
      state.selected = thread.id;
      render();
      refresh();
    });
    list.append(button);
  }
}

function timelineBlocks(detail) {
  const items = [...detail.transcript.items].sort((a, b) => a.sequence - b.sequence);
  const results = new Map();
  for (const item of items) {
    if (item.kind === "tool_result" && item.content?.call_id) {
      results.set(item.content.call_id, item.content);
    }
  }
  const blocks = [];
  let text = "";
  const flushText = () => {
    if (text.trim()) blocks.push({ type: "text", text });
    text = "";
  };
  for (const item of items) {
    if (item.kind === "model_text_delta") {
      text += item.content?.Delta?.AppendText?.chunk || "";
    } else if (item.kind === "model_tool_call" && item.content?.ToolCall) {
      flushText();
      const call = item.content.ToolCall;
      blocks.push({ type: "tool", call, sequence: item.sequence, result: results.get(call.id) });
    } else if (item.kind === "assistant_final") {
      text = "";
      blocks.push({ type: "final", text: item.content?.preview || "" });
    }
  }
  flushText();
  return blocks;
}

function callSummary(input) {
  if (!input || typeof input !== "object") return "";
  if (typeof input.path === "string") return input.path;
  if (Array.isArray(input.terms) && input.terms.length) return input.terms.join(", ");
  if (typeof input.command === "string") return input.command;
  const entry = Object.entries(input).find(
    ([key, value]) => key !== "expected_revision" && typeof value === "string",
  );
  return entry ? entry[1] : "";
}

function toolOutputText(output) {
  if (!output) return "";
  if (typeof output.Text === "string") return output.Text;
  if (output.Structured !== undefined) return JSON.stringify(output.Structured, null, 2);
  return JSON.stringify(output, null, 2);
}

function stepNode(block, runActive) {
  const key = block.call.id;
  const step = node("details", "step");
  step.dataset.key = key;
  step.open = state.openSteps.has(key);
  step.addEventListener("toggle", () => {
    if (step.open) state.openSteps.add(key);
    else state.openSteps.delete(key);
  });
  const summary = node("summary");
  const status = block.result
    ? (block.result.is_error ? "err" : "ok")
    : (runActive ? "pending" : "ok");
  summary.append(node("i", `step-status ${status}`));
  summary.append(node("code", "step-name", block.call.name));
  summary.append(node("span", "step-arg", callSummary(block.call.input)));
  summary.append(node("span", "step-seq", `#${block.sequence}`));
  step.append(summary);

  const body = node("div", "step-body");
  const inputSection = node("div", "step-io");
  inputSection.append(node("label", "", "Input"));
  inputSection.append(node("pre", "", JSON.stringify(block.call.input, null, 2)));
  body.append(inputSection);
  if (block.result) {
    const resultSection = node("div", "step-io");
    resultSection.append(node("label", "", block.result.is_error ? "Error" : "Result"));
    resultSection.append(node("pre", "", toolOutputText(block.result.output)));
    body.append(resultSection);
  }
  step.append(body);
  return step;
}

function interruptCard(pending, detail) {
  const card = node("div", "interrupt-card");
  if (pending.type === "approval") {
    card.append(node("strong", "", "Kit is asking to run a tool"));
    const blocks = timelineBlocks(detail);
    const awaiting = blocks.findLast((block) => block.type === "tool" && !block.result);
    if (awaiting) {
      card.append(node("code", "interrupt-detail", `${awaiting.call.name} ${callSummary(awaiting.call.input)}`.trim()));
    }
  } else {
    card.append(node("strong", "", "Kit is asking for provider authorization"));
    card.append(node("code", "interrupt-detail", "Allow this run to call the configured model provider."));
  }
  const actions = node("div", "interrupt-actions");
  const approve = node("button", "approve", "Approve");
  const deny = node("button", "deny", "Deny");
  approve.type = deny.type = "button";
  const resolve = async (granted) => {
    approve.disabled = deny.disabled = true;
    try {
      if (pending.type === "approval") {
        await mutation(`/v1/approvals/${pending.item.id}/resolve`, {
          decision: granted ? "approved" : "denied",
          expected_version: pending.item.version,
        });
      } else {
        await mutation(`/v1/runs/${pending.item.run_id}/auth/resolve`, {
          granted,
          expected_version: pending.item.version,
        });
      }
      await refresh();
    } catch (error) {
      showNotice(error.message);
    }
  };
  approve.addEventListener("click", () => resolve(true));
  deny.addEventListener("click", () => resolve(false));
  actions.append(approve, deny);
  card.append(actions);
  return card;
}

async function stopRun(run) {
  try {
    await mutation(`/v1/runs/${run.id}/cancel`, { expected_version: run.version });
    await refresh();
  } catch (error) {
    showNotice(error.message);
  }
}

function assistantTurn(detail, index) {
  const turn = node("article", "turn assistant");
  const meta = node("div", "turn-meta");
  meta.append(node("b", "", "Kit"));
  const run = detail.run;
  const active = ACTIVE_STATES.has(run.state);
  if (run.state !== "completed") {
    const chip = node("span", `chip${active ? " active" : ""}${run.state === "failed" ? " failed" : ""}`);
    chip.append(document.createTextNode(STATE_LABELS[run.state] || run.state));
    chip.title = run.id;
    meta.append(chip);
  }
  if (active && run.state !== "cancelling") {
    const stop = node("button", "stop-run", "Stop");
    stop.type = "button";
    stop.addEventListener("click", () => stopRun(run));
    meta.append(stop);
  }
  turn.append(meta);

  const timeline = node("div", "timeline");
  const blocks = timelineBlocks(detail);
  let steps = null;
  blocks.forEach((block, position) => {
    if (block.type === "tool") {
      if (!steps) {
        steps = node("div", "steps");
        timeline.append(steps);
      }
      steps.append(stepNode(block, active));
      return;
    }
    steps = null;
    const streaming = active && position === blocks.length - 1;
    if (block.type === "text") timeline.append(renderMarkdown(block.text, streaming));
    if (block.type === "final") timeline.append(renderMarkdown(block.text));
  });
  if (run.failure) {
    timeline.append(node("div", "run-failure", run.failure.detail));
  } else if (!blocks.length && active) {
    timeline.append(node("div", "chip active", STATE_LABELS[run.state] || run.state));
  }
  for (const pending of pendingForRun(run.id)) {
    timeline.append(interruptCard(pending, detail));
  }
  turn.append(timeline);
  return turn;
}

function renderMessages() {
  const pinned = messages.scrollHeight - messages.scrollTop - messages.clientHeight < 80;
  const previousScroll = messages.scrollTop;
  const preScrolls = new Map();
  for (const step of messages.querySelectorAll(".step[open]")) {
    step.querySelectorAll("pre").forEach((pre, index) => {
      if (pre.scrollTop || pre.scrollLeft) {
        preScrolls.set(`${step.dataset.key}:${index}`, [pre.scrollTop, pre.scrollLeft]);
      }
    });
  }
  messages.replaceChildren();
  prompt.disabled = false;
  $("send").disabled = false;

  const inner = node("div", "messages-inner");
  if (!state.details.length) {
    const empty = node("div", "empty-conversation");
    empty.append(node("h2", "", state.selected ? "No runs in this thread" : "Start a thread"));
    empty.append(node("p", "", "Send a prompt below. Kit will read the repository, run tools, and reply here."));
    inner.append(empty);
  } else {
    state.details.forEach((detail, index) => {
      const user = node("article", "turn user");
      const meta = node("div", "turn-meta");
      meta.append(node("b", "", "You"));
      meta.append(node("span", "", `Run ${index + 1}`));
      user.append(meta);
      user.append(node("div", "user-body", detail.user || "Prompt unavailable"));
      inner.append(user);
      inner.append(assistantTurn(detail, index));
    });
  }
  messages.append(inner);
  for (const step of messages.querySelectorAll(".step[open]")) {
    step.querySelectorAll("pre").forEach((pre, index) => {
      const saved = preScrolls.get(`${step.dataset.key}:${index}`);
      if (saved) [pre.scrollTop, pre.scrollLeft] = saved;
    });
  }
  messages.scrollTop = pinned ? messages.scrollHeight : previousScroll;
}

function renderMetrics() {
  let micros = 0;
  let tokens = 0;
  for (const detail of state.details) {
    micros += detail.cost.cost?.effective?.micros || 0;
    tokens += detail.cost.usage?.reservation_debit?.tokens || 0;
  }
  $("total-cost").textContent = `$${(micros / 1_000_000).toFixed(4)}`;
  $("total-tokens").textContent = tokens.toLocaleString();
}

let renderedFingerprint = "";

function render() {
  const fingerprint = JSON.stringify([
    state.threads,
    state.runs,
    state.details,
    state.approvals,
    state.authRequests,
    state.selected,
    [...state.prompts.values()],
  ]);
  if (fingerprint === renderedFingerprint) return;
  renderedFingerprint = fingerprint;
  renderThreads();
  renderMessages();
  renderMetrics();
  const thread = state.threads.find((item) => item.id === state.selected);
  $("thread-title").textContent = thread ? threadLabel(thread) : "Kit";
}

// ---- event stream ----

function parseFrame(frame) {
  let data = "";
  for (const line of frame.split("\n")) {
    if (line.startsWith("id:")) state.cursor = line.slice(3).trim();
    if (line.startsWith("data:")) data += line.slice(5).trim();
  }
  if (data) scheduleRefresh();
}

async function followEvents() {
  while (state.credential) {
    try {
      const cursor = state.cursor ? `?cursor=${encodeURIComponent(state.cursor)}` : "";
      const response = await request(`/v1/projects/${state.project}/events/stream${cursor}`, {
        headers: { Accept: "text/event-stream" },
      });
      setStream("Live", true);
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = "";
      while (true) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true }).replaceAll("\r\n", "\n");
        let boundary;
        while ((boundary = buffer.indexOf("\n\n")) >= 0) {
          parseFrame(buffer.slice(0, boundary));
          buffer = buffer.slice(boundary + 2);
        }
      }
    } catch (_) {
      setStream("Reconnecting", false);
    }
    await new Promise((resolve) => setTimeout(resolve, 1200));
  }
}

async function boot() {
  try {
    const repository = await json("/v1/repository/status");
    state.project = repository.project_id;
    $("project-id").textContent = state.project;
    $("repository-state").textContent = repository.available
      ? "Repository available"
      : "Repository unavailable";
    $("status-dot").classList.add(repository.available ? "online" : "offline");
    await ensureProject();
    await refresh();
    followEvents();
  } catch (error) {
    $("repository-state").textContent = "Repository unavailable";
    $("status-dot").classList.add("offline");
    showNotice(error.message);
    setStream("Offline", false);
  }
}

composer.addEventListener("submit", async (event) => {
  event.preventDefault();
  const message = prompt.value.trim();
  if (!message) return;
  prompt.disabled = true;
  $("send").disabled = true;
  try {
    if (!state.selected) await createThread();
    await mutation(`/v1/threads/${state.selected}/runs`, { message });
    prompt.value = "";
    prompt.style.height = "auto";
    await refresh();
  } catch (error) {
    showNotice(error.message);
  } finally {
    prompt.disabled = false;
    $("send").disabled = false;
    prompt.focus();
  }
});

prompt.addEventListener("keydown", (event) => {
  if (event.key === "Enter" && !event.shiftKey) {
    event.preventDefault();
    composer.requestSubmit();
  }
});

prompt.addEventListener("input", () => {
  prompt.style.height = "auto";
  prompt.style.height = `${Math.min(prompt.scrollHeight, 200)}px`;
});

$("new-thread").addEventListener("click", () => createThread().catch((error) => showNotice(error.message)));

const fragment = location.hash.slice(1);
const storageKey = `kit.credential:${location.origin}`;
if (/^[0-9a-f]{64}$/i.test(fragment)) {
  sessionStorage.setItem(storageKey, fragment);
  history.replaceState(null, "", `${location.pathname}${location.search}`);
}
state.credential = fragment || sessionStorage.getItem(storageKey) || "";
if (/^[0-9a-f]{64}$/i.test(state.credential)) {
  app.hidden = false;
  boot();
} else {
  state.credential = "";
  locked.hidden = false;
}
