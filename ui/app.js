const { invoke } = window.__TAURI__.core;

const $ = (selector) => document.querySelector(selector);
const list = $("#download-list");
const count = $("#download-count");
const notice = $("#notice");
const form = $("#add-form");
const urlInput = $("#download-url");
const settingsToggle = $("#settings-toggle");
const settingsPanel = $("#settings-panel");
const settingsForm = $("#settings-form");
const settingsNotice = $("#settings-notice");
const infoPanel = $("#download-info-panel");
const infoForm = $("#download-info-form");
const infoNotice = $("#info-notice");
let pendingDownloadUrl = "";

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
    eta.textContent = status === "downloading" && view.eta != null && Number.isFinite(Number(view.eta))
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

let refreshInFlight = false;
let refreshQueued = false;

async function refresh() {
  // Avoid overlapping list requests when a previous refresh is still in flight.
  if (refreshInFlight) {
    refreshQueued = true;
    return;
  }

  refreshInFlight = true;
  try {
    const items = await invoke("list_downloads");
    renderDownloads(items);
  } catch (error) {
    list.innerHTML = '<div class="empty-state">The desktop service could not be reached.</div>';
    showNotice(String(error), "error");
  } finally {
    refreshInFlight = false;
    if (refreshQueued) {
      refreshQueued = false;
      void refresh();
    }
  }
}

function showInfoNotice(message, type = "") {
  infoNotice.textContent = message;
  infoNotice.className = `notice ${type}`.trim();
}

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  const url = urlInput.value.trim();
  if (!url) return;
  const submit = form.querySelector("button[type=submit]");
  submit.disabled = true;
  showNotice("Checking download details…");
  try {
    const info = await invoke("inspect_download", { url });
    pendingDownloadUrl = url;
    $("#info-filename").value = info.probe?.filename ?? "";
    $("#info-directory").value = info.dir ?? "";
    $("#info-category").value = info.category ?? "other";
    const size = info.probe?.size == null ? "Unknown size" : formatBytes(info.probe.size);
    const range = info.probe?.accepts_ranges ? "Resume and multi-connection supported" : "Server does not support range requests";
    const kind = info.kind === "hls" ? "HLS video stream" : "Direct file";
    $("#info-summary").textContent = `${kind} · ${size} · ${range}`;
    infoPanel.hidden = false;
    showInfoNotice("");
    showNotice("Review the details, then start the download.");
  } catch (error) {
    showNotice(String(error), "error");
  } finally {
    submit.disabled = false;
  }
});

$("#info-cancel").addEventListener("click", () => {
  infoPanel.hidden = true;
  pendingDownloadUrl = "";
  showInfoNotice("");
});

infoForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  if (!pendingDownloadUrl) {
    showInfoNotice("Enter a URL and inspect it first.", "error");
    return;
  }
  const filename = $("#info-filename").value.trim();
  const directory = $("#info-directory").value.trim();
  const category = $("#info-category").value;
  if (!filename || !directory) {
    showInfoNotice("File name and destination folder are required.", "error");
    return;
  }
  const submit = infoForm.querySelector("button[type=submit]");
  submit.disabled = true;
  showInfoNotice("Adding download…");
  try {
    await invoke("add_download", { url: pendingDownloadUrl, filename, directory, category });
    urlInput.value = "";
    pendingDownloadUrl = "";
    infoPanel.hidden = true;
    showInfoNotice("");
    showNotice("Download added to the queue.", "success");
    await refresh();
  } catch (error) {
    showInfoNotice(String(error), "error");
  } finally {
    submit.disabled = false;
  }
});

function showSettingsNotice(message, type = "") {
  settingsNotice.textContent = message;
  settingsNotice.className = `notice ${type}`.trim();
}

async function openSettings() {
  settingsToggle.disabled = true;
  showSettingsNotice("Loading settings…");
  try {
    const settings = await invoke("get_settings");
    $("#download-dir").value = String(settings.download_dir ?? "");
    $("#max-concurrent").value = String(settings.max_concurrent ?? 3);
    $("#auto-resume").checked = Boolean(settings.auto_resume);
    settingsPanel.hidden = false;
    settingsToggle.setAttribute("aria-expanded", "true");
    showSettingsNotice("");
  } catch (error) {
    showSettingsNotice(String(error), "error");
  } finally {
    settingsToggle.disabled = false;
  }
}

settingsToggle.addEventListener("click", async () => {
  if (!settingsPanel.hidden) {
    settingsPanel.hidden = true;
    settingsToggle.setAttribute("aria-expanded", "false");
    return;
  }
  await openSettings();
});

$("#settings-cancel").addEventListener("click", () => {
  settingsPanel.hidden = true;
  settingsToggle.setAttribute("aria-expanded", "false");
  showSettingsNotice("");
});

settingsForm.addEventListener("submit", async (event) => {
  event.preventDefault();
  const downloadDir = $("#download-dir").value.trim();
  const maxConcurrent = Number($("#max-concurrent").value);
  const autoResume = $("#auto-resume").checked;
  if (!downloadDir) {
    showSettingsNotice("Download folder cannot be empty.", "error");
    return;
  }
  if (!Number.isInteger(maxConcurrent) || maxConcurrent < 1 || maxConcurrent > 16) {
    showSettingsNotice("Concurrent downloads must be between 1 and 16.", "error");
    return;
  }

  const submit = settingsForm.querySelector("button[type=submit]");
  submit.disabled = true;
  showSettingsNotice("Saving settings…");
  try {
    await invoke("update_settings", {
      downloadDir,
      maxConcurrent,
      autoResume
    });
    showSettingsNotice("Settings saved.", "success");
  } catch (error) {
    showSettingsNotice(String(error), "error");
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
