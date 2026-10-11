"use strict";

const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];
let STATE = { sid: null, detail: null };

async function api(path, body) {
  const opt = body
    ? { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) }
    : {};
  const res = await fetch(path, opt);
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || `HTTP ${res.status}`);
  return data;
}

function toast(msg, isErr = false) {
  const t = $("#toast");
  t.textContent = msg;
  t.classList.toggle("err", isErr);
  t.classList.add("show");
  clearTimeout(toast._t);
  toast._t = setTimeout(() => t.classList.remove("show"), 2600);
}

function fmtTime(ts) {
  if (!ts) return "";
  const d = new Date(ts * 1000);
  return d.toLocaleString();
}

// ---- session list --------------------------------------------------------
async function loadSessions() {
  const q = $("#search").value.trim();
  const internal = $("#show-internal").checked ? "1" : "0";
  const qs = new URLSearchParams();
  if (q) qs.set("q", q);
  qs.set("internal", internal);
  const list = await api("/api/sessions?" + qs.toString());
  const ul = $("#session-list");
  ul.innerHTML = "";
  for (const s of list) {
    const li = document.createElement("li");
    li.className = "s-item" + (s.id === STATE.sid ? " active" : "");
    const badges = [];
    if (s.pinned) badges.push("📌");
    if (s.archived) badges.push("🗄");
    li.innerHTML = `
      <div class="t">${badges.join("")} ${escapeHtml(s.title || "(无标题)")}
        <span class="badge">${escapeHtml(s.source || "?")}</span></div>
      <div class="sub">${escapeHtml(s.id)} · ${s.message_count ?? 0} 条 · ${fmtTime(s.last_activity_at || s.started_at)}</div>`;
    li.onclick = () => openSession(s.id);
    ul.appendChild(li);
  }
  $("#side-foot").textContent = `${list.length} 个会话`;
}

// ---- open & render session -----------------------------------------------
async function openSession(sid) {
  STATE.sid = sid;
  const d = await api("/api/session?id=" + encodeURIComponent(sid));
  STATE.detail = d;
  $$(".s-item").forEach((e) => e.classList.remove("active"));
  $("#empty").hidden = true;
  $("#editor").hidden = false;

  const s = d.session;
  $("#s-title").value = s.title || "";
  $("#s-source").value = s.source || "cli";
  $("#s-pinned").checked = !!s.pinned;
  $("#s-archived").checked = !!s.archived;
  $("#s-meta").textContent =
    `ID ${s.id} · model ${s.model || "?"} · 创建 ${fmtTime(s.started_at)} · profile ${s.profile_name || "default"}`;

  renderMessages(d.messages);
  loadSessions();
}

function renderMessages(msgs) {
  const box = $("#messages");
  box.innerHTML = "";
  for (const m of msgs) box.appendChild(msgEl(m));
}

function msgEl(m) {
  const el = document.createElement("div");
  el.className = "msg " + (m.role || "user");
  el.dataset.id = m.id;
  const flags = [];
  if (!m.active) flags.push("inactive");
  if (m.compacted) flags.push("compacted");
  if (m.tool_name) flags.push("tool:" + m.tool_name);

  const roleIcon = { user: "user", assistant: "robot", system: "settings", tool: "message" }[m.role] || "message";

  // Decode tool_calls (assistant → function invocations) for a readable summary.
  let toolCallSummary = "";
  if (m.tool_calls) {
    try {
      const tc = typeof m.tool_calls === "string" ? JSON.parse(m.tool_calls) : m.tool_calls;
      const names = (Array.isArray(tc) ? tc : [])
        .map((c) => c?.function?.name || c?.name || c?.tool_name)
        .filter(Boolean);
      if (names.length) toolCallSummary = names.join(", ");
    } catch { /* leave empty */ }
  }

  const content = m.content || "";
  const isEmpty = content.trim() === "";
  const isToolResult = m.role === "tool";

  // Pretty-print tool-result JSON so it reads instead of being one long line.
  let displayContent = content;
  if (isToolResult && content) {
    try { displayContent = JSON.stringify(JSON.parse(content), null, 2); } catch { /* keep raw */ }
  }

  // Human label for the body region.
  let bodyNote = "";
  if (isEmpty && toolCallSummary) bodyNote = `工具调用 → ${toolCallSummary}`;
  else if (isEmpty && m.role === "assistant") bodyNote = "(空内容 — 工具调用载体 / 占位)";
  else if (isToolResult) bodyNote = `${m.tool_name || "tool"} 返回结果`;

  const rows = isToolResult ? 6 : (isEmpty ? 1 : 3);

  el.innerHTML = `
    <div class="head">
      <span class="role-ic">${icon(roleIcon, 16)}</span>
      <select class="m-role">
        ${["user", "assistant", "system", "tool"]
          .map((r) => `<option value="${r}" ${r === m.role ? "selected" : ""}>${r}</option>`)
          .join("")}
      </select>
      <span class="id">#${m.id}</span>
      <span class="dirty-dot"></span>
      <span class="flags">${escapeHtml(flags.join(" · "))}</span>
      <span class="acts">
        <button class="save" title="保存">${icon("save")}</button>
        <button class="ins" title="在此条后插入">${icon("insert")}</button>
        <button class="trunc" title="裁剪至此（rewind）">${icon("scissors")}</button>
        <button class="del danger" title="删除">${icon("trash")}</button>
      </span>
    </div>
    ${bodyNote ? `<div class="body-note">${escapeHtml(bodyNote)}</div>` : ""}
    ${isToolResult
      ? `<details class="tool-body"><summary>展开工具结果 (${content.length} 字符)</summary>
           <textarea class="m-content" rows="${rows}">${escapeHtml(displayContent)}</textarea></details>`
      : `<textarea class="m-content" rows="${rows}" placeholder="${isEmpty ? "（空内容）" : ""}">${escapeHtml(content)}</textarea>`}
    <details class="reason" ${m.reasoning_content ? "" : "hidden"}>
      <summary>reasoning</summary>
      <textarea class="m-reason" rows="2">${escapeHtml(m.reasoning_content || "")}</textarea>
    </details>`;

  if (isEmpty) el.classList.add("is-empty");
  if (isToolResult) el.classList.add("is-tool");

  const markDirty = () => el.classList.add("dirty");
  $(".m-content", el).addEventListener("input", markDirty);
  $(".m-role", el).addEventListener("change", markDirty);
  const rz = $(".m-reason", el);
  if (rz) rz.addEventListener("input", markDirty);

  $(".save", el).onclick = async () => {
    try {
      await api("/api/update_message", {
        id: m.id,
        fields: {
          role: $(".m-role", el).value,
          content: $(".m-content", el).value,
          reasoning_content: rz ? rz.value : "",
        },
      });
      el.classList.remove("dirty");
      toast("已保存 #" + m.id);
    } catch (e) { toast(e.message, true); }
  };
  $(".del", el).onclick = async () => {
    if (!confirm("删除消息 #" + m.id + "？")) return;
    try {
      await api("/api/delete_message", { id: m.id });
      toast("已删除 #" + m.id);
      openSession(STATE.sid);
    } catch (e) { toast(e.message, true); }
  };
  $(".ins", el).onclick = async () => {
    const role = prompt("插入新消息的角色 (user/assistant/system/tool):", "assistant");
    if (!role) return;
    const content = prompt("内容:", "");
    if (content === null) return;
    try {
      await api("/api/add_message", {
        session_id: STATE.sid, role, content, after_id: m.id,
      });
      toast("已插入");
      openSession(STATE.sid);
    } catch (e) { toast(e.message, true); }
  };
  $(".trunc", el).onclick = async () => {
    if (!confirm("裁剪（rewind）：删除 #" + m.id + " 之后的所有消息？")) return;
    try {
      const r = await api("/api/truncate_after", { session_id: STATE.sid, id: m.id });
      toast("已删除 " + r.deleted + " 条后续消息");
      openSession(STATE.sid);
    } catch (e) { toast(e.message, true); }
  };
  return el;
}

// ---- session-level actions ------------------------------------------------
$("#save-session").onclick = async () => {
  try {
    await api("/api/update_session", {
      id: STATE.sid,
      fields: {
        title: $("#s-title").value,
        source: $("#s-source").value,
        created_source: $("#s-source").value,
        pinned: $("#s-pinned").checked ? 1 : 0,
        archived: $("#s-archived").checked ? 1 : 0,
      },
    });
    toast("会话属性已保存");
    openSession(STATE.sid);
  } catch (e) { toast(e.message, true); }
};

$("#c-add").onclick = async () => {
  const content = $("#c-content").value;
  if (!content.trim()) return;
  try {
    await api("/api/add_message", {
      session_id: STATE.sid, role: $("#c-role").value, content,
    });
    $("#c-content").value = "";
    toast("已追加");
    openSession(STATE.sid);
  } catch (e) { toast(e.message, true); }
};

$("#new-session").onclick = async () => {
  const title = prompt("新会话标题:", "新建会话");
  if (title === null) return;
  try {
    const r = await api("/api/create_session", { title, source: "cli", model: "manual" });
    toast("已创建 " + r.id);
    await loadSessions();
    openSession(r.id);
  } catch (e) { toast(e.message, true); }
};

$("#refresh").onclick = loadSessions;
$("#search").addEventListener("input", debounce(loadSessions, 250));
$("#show-internal").addEventListener("change", loadSessions);

function debounce(fn, ms) {
  let t;
  return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); };
}
function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

// Hydrate any static <span class="ic" data-icon="name"> placeholders.
$$(".ic[data-icon]").forEach((e) => { e.innerHTML = icon(e.dataset.icon, 16); });

loadSessions();
