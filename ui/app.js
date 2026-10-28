const { invoke } = window.__TAURI__.core;

const $ = (selector) => document.querySelector(selector);
const list = $("#download-list");
const count = $("#download-count");
const notice = $("#notice");
const form = $("#add-form");
const urlInput = $("#download-url");

function showNotice(message, type = "") {
  notice.textContent = message;
  notice.className = `notice ${type}`.trim();
}

function escapeText(value) {
  return String(value ?? "");
}

function formatBytes(bytes) {
  if (bytes == null || !Number.isFinite(Number(bytes))) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let size = Number(bytes), unit = 0;
  while (size >= 1024 && unit < units.length - 1) { size /= 1024; unit++; }
  return `${size.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`;
}

function statusLabel(status) {
  return ({ queued: "Queued", downloading: "Downloading", paused: "Paused", completed: "Completed", error: "Error" })[status] ?? String(status);
}

function safeUrlHost(raw) {
  try { return new URL(raw).host; } catch { return raw; }
}

function renderDownloads(items) {
  count.textContent = `${items.length} ${items.length === 1 ? "item" : "items"}`;
  if (!items.length) {
    list.innerHTML = '<div class="empty-state">No downloads yet. Paste a URL above to begin.</div>';
    return;
  }

  list.replaceChildren();
  for (const view of items) {
    const item = view.item ?? view;
    const status = String(item.status ?? "queued").toLowerCase();
    const downloaded = Number(item.downloaded ?? 0);
    const total = item.size == null ? null : Number(item.size);
    const progress = total && total > 0 ? Math.min(100, downloaded / total * 100) : 0;
    const row = document.createElement("article");
    row.className = "download-row";

    const main = document.createElement("div");
    main.className = "download-main";
    const title = document.createElement("div");
    title.className = "download-title";
    title.textContent = escapeText(item.filename || safeUrlHost(item.request?.url));
    const source = document.createElement("div");
    source.className = "download-url-text";
    source.textContent = escapeText(item.request?.url ?? "");
    const track = document.createElement("div");
    track.className = "progress-track";
    const fill = document.createElement("div");
    fill.className = "progress-fill";
    fill.style.width = `${progress}%`;
    track.append(fill);
    const meta = document.createElement("div");
    meta.className = "download-meta";
    const state = document.createElement("span");
    state.className = `status ${status}`;
    state.textContent = statusLabel(status);
    const size = document.createElement("span");
    size.textContent = total == null ? formatBytes(downloaded) : `${formatBytes(downloaded)} / ${formatBytes(total)}`;
    const speed = document.createElement("span");
    speed.textContent = status === "downloading" ? `${formatBytes(view.speed ?? 0)}/s` : "";
    const eta = document.createElement("span");
    eta.textContent = status === "downloading" && Number.isFinite(Number(view.eta))
      ? `${Math.max(0, Number(view.eta))}s left`
      : "";
    meta.append(state, size, speed, eta);
    main.append(title, source, track, meta);

    if (status === "error" && item.error) {
      const error = document.createElement("p");
      error.className = "download-error";
      error.textContent = escapeText(item.error);
      main.append(error);
    }

    const actions = document.createElement("div");
    actions.className = "row-actions";
    if (status === "downloading" || status === "queued") {
      actions.append(makeAction("Pause", () => invoke("pause_download", { id: item.id })));
    } else if (status === "paused" || status === "error") {
      actions.append(makeAction("Resume", () => invoke("resume_download", { id: item.id })));
    }
    if (status === "completed" || status === "error" || status === "paused") {
      actions.append(makeAction("Remove", () => invoke("remove_download", { id: item.id, deleteFile: false }), true));
    }
    row.append(main, actions);
    list.append(row);
  }
}

function makeAction(label, action, quiet = false) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = `button ${quiet ? "button-quiet" : ""}`.trim();
  button.textContent = label;
  button.addEventListener("click", async () => {
    button.disabled = true;
    try { await action(); await refresh(); }
    catch (error) { showNotice(String(error), "error"); }
    finally { button.disabled = false; }
  });
  return button;
}

async function refresh() {
  try {
    const items = await invoke("list_downloads");
    renderDownloads(items);
  } catch (error) {
    list.innerHTML = '<div class="empty-state">The desktop service could not be reached.</div>';
    showNotice(String(error), "error");
  }
}

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  const url = urlInput.value.trim();
  if (!url) return;
  const submit = form.querySelector("button[type=submit]");
  submit.disabled = true;
  showNotice("Adding download…");
  try {
    await invoke("add_download", { url });
    urlInput.value = "";
    showNotice("Download added to the queue.", "success");
    await refresh();
  } catch (error) {
    showNotice(String(error), "error");
  } finally {
    submit.disabled = false;
  }
});

$("#refresh-button").addEventListener("click", refresh);
$("#pause-all-button").addEventListener("click", async () => {
  try { await invoke("pause_all_downloads"); await refresh(); }
  catch (error) { showNotice(String(error), "error"); }
});
$("#resume-all-button").addEventListener("click", async () => {
  try { await invoke("resume_all_downloads"); await refresh(); }
  catch (error) { showNotice(String(error), "error"); }
});

void refresh();
window.setInterval(() => { void refresh(); }, 1200);
