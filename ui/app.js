"use strict";

const S = {
  config: null,
  monitors: [],
  fx: null,
  devices: [],
  runningApps: [],
  installedGames: [],
  gamesScanning: false,
  gamesTimedOut: false,
  crosshairPresets: [],
  displayPresets: [],
  audioPresets: [],
  selectedSlot: null,
  tab: "aim",
  day: true,
  previewTick: 0,
  lut: null,
  lutObjectUrl: null,
  lutMirrorToken: 0,
  warning: null,
  applied: false,
  meterPeak: 0,
  meterHeadroom: -60,
  editing: null,
  editingDirty: false,
  slotEditorFocused: false,
  slotSaveTimer: null,
  slotSavePending: false,
  slotSwitchPending: null,
  slotSwitchTimer: null,
  awaitingBrowseSlot: null,
  aimMode: "shape",
  aimModeInitialized: false,
  lastWarmth: 40,
  compareActive: false,
  presetMenu: null,
  customSelect: null,
  fxModalShown: false,
  fxModalMode: "setup",
  fxRepairArmed: false,
  backupPreview: null,
  canUndoRestore: false,
  savePresetModule: null,
  emojiSlot: null,
  ready: false,
  appVersion: "1.0.0",
  toastTimer: null,
  colorHsv: null,
  confirmResolve: null,
  confirmFocus: null,
  confirmOutcome: null,
};

const EMOJIS = [
  "🎯","⚡","🎮","🕹️","🔫","🏹","🛡️","🎬","🌙","☀️","🌿","🔥","❄️","💀","👾","🚀",
  "🎧","🔊","🎵","🎚️","🥇","🏆","⭐","✨","💎","💡","🧠","👁️","🦇","🐺","🦊","🐉",
  "🍿","🎞️","📺","🖥️","🖱️","⌨️","🎨","🛠️","⚙️","🔧","🧩","🧊","🌊","🌪️","🌌","🪐",
  "☄️","🛸","👑","💥","🎪","🎲","♟️","🏁","🚦","🟢","🔵","🔴","🟣","🟡","⚪","⚫",
];
const FREQS = [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
const FADERS = [
  ["🌊", "Rumble"], ["🔊", "Bass"], ["🥊", "Punch"], ["🔥", "Warmth"], ["🫀", "Body"],
  ["🎤", "Voice"], ["💎", "Clarity"], ["✨", "Treble"], ["⚡", "Sparkle"], ["🪽", "Air"],
];
const PANELS = [["auto", "Auto"], ["led", "LED"], ["oled", "OLED"], ["ips", "IPS"], ["va", "VA"], ["miniled", "MiniLED"], ["tn", "TN"]];
const DISPLAY_PRESET_FIELDS = ["brightness", "contrast", "gamma", "warmth", "r", "g", "b"];

const $ = (sel) => document.querySelector(sel);
const $$ = (sel) => Array.from(document.querySelectorAll(sel));
const esc = (value) => String(value ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const clone = (value) => JSON.parse(JSON.stringify(value));
const camel = (s) => s.replace(/_([a-z])/g, (_, c) => c.toUpperCase());
const pathKeys = (path) => path.split(".").map(camel);

function getPath(obj, path) {
  if (!obj) return undefined;
  return pathKeys(path).reduce((value, key) => (value == null ? undefined : value[key]), obj);
}
function setPath(obj, path, value) {
  const keys = pathKeys(path);
  let target = obj;
  for (let i = 0; i < keys.length - 1; i++) {
    if (target[keys[i]] == null || typeof target[keys[i]] !== "object") target[keys[i]] = {};
    target = target[keys[i]];
  }
  target[keys[keys.length - 1]] = value;
}

function send(type, payload = {}) {
  window.__lastSent = { type, payload };
  try {
    window.ipc.postMessage(JSON.stringify({ v: 1, type, payload }));
  } catch (error) {
    console.error("IPC unavailable", error);
  }
}

const pendingSends = {};
function scheduleSend(module, immediate = false) {
  if (immediate) { flushSend(module); return; }
  clearTimeout(pendingSends[module]);
  pendingSends[module] = setTimeout(() => flushSend(module), module === "audio" ? 220 : 45);
}
function flushSend(module) {
  clearTimeout(pendingSends[module]);
  delete pendingSends[module];
  if (!S.config) return;
  if (module === "crosshair") {
    S.config.crosshair.activePreset = S.config.crosshair.activePreset || null;
    send("UpdateCrosshair", { crosshair: S.config.crosshair });
    refreshPreview();
  } else if (module === "display") {
    send("UpdateDisplay", { display: S.config.display });
  } else if (module === "audio") {
    send("UpdateSound", { audio: S.config.audio });
    drawEQ();
  }
}

// Every preview frame is a PNG revision. Loads are serialized and the element
// is only pointed at a revision once the previous load settled, so a new frame
// never aborts one in flight and the box keeps its last painted frame instead
// of flashing.
const previewPump = { loading: false, painted: -1 };
function paintPreview() {
  const image = $("#aimPreview");
  if (!image || previewPump.loading || S.previewTick <= previewPump.painted) return;
  const tick = S.previewTick;
  previewPump.loading = true;
  previewPump.painted = tick;
  image.src = `/preview?v=${tick}`;
  const settled = () => {
    previewPump.loading = false;
    paintPreview();
  };
  if (image.decode) image.decode().then(settled, settled);
  else {
    image.onload = settled;
    image.onerror = settled;
  }
}
// Repaint the newest revision. force is for a first paint or a tab switch,
// where frames may have been produced while the page was not asking.
function refreshPreview(force = false) {
  if (force) previewPump.painted = -1;
  paintPreview();
}

function closeBackupModal() {
  $("#backupModal")?.classList.add("hidden");
  S.backupPreview = null;
  renderBackupSummary(null);
  const status = $("#backupStatus");
  if (status) status.textContent = "";
  send("BackupModalOpen", { open: false });
}

function renderBackupUndo() {
  $("#backupUndo")?.classList.toggle("hidden", !S.canUndoRestore);
}

function renderBackupSummary(preview) {
  const summary = $("#backupSummary");
  const restore = $("#backupRestore");
  if (!summary) return;
  if (!preview) {
    summary.textContent = "No backup selected.";
    restore?.classList.add("hidden");
    return;
  }
  const presets = Number(preview.crosshairPresets || 0) + Number(preview.displayPresets || 0) + Number(preview.audioPresets || 0);
  const created = preview.createdUnix ? new Date(Number(preview.createdUnix) * 1000).toLocaleString() : "unknown date";
  summary.innerHTML = `<strong>${presets}</strong> custom presets · <strong>${Number(preview.slots || 0)}</strong> slots · <strong>${Number(preview.images || 0)}</strong> images<br>`
    + `${preview.eqName ? `Imported EQ: <strong>${esc(preview.eqName)}</strong><br>` : "No imported EQ profile<br>"}`
    + `Created ${esc(created)}${preview.appVersion ? ` with v${esc(preview.appVersion)}` : ""}`
    + (preview.warnings?.length ? `<div class="warn">${preview.warnings.map(esc).join("<br>")}</div>` : "");
  restore?.classList.remove("hidden");
}

function showToast(message, error = false, actions = []) {
  const toast = $("#toast");
  if (!toast) return;
  toast.replaceChildren();
  const text = document.createElement("span");
  text.className = "toast-text";
  text.textContent = message;
  toast.appendChild(text);
  if (actions.length) {
    const row = document.createElement("div");
    row.className = "toast-actions";
    actions.forEach((action) => {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "toast-action";
      button.dataset.toastAction = action.id;
      button.textContent = action.label;
      row.appendChild(button);
    });
    toast.appendChild(row);
  }
  // A clamped toast still exposes its whole text on hover.
  toast.title = message;
  toast.classList.toggle("error", !!error);
  toast.classList.toggle("has-actions", actions.length > 0);
  toast.classList.remove("hidden");
  clearTimeout(S.toastTimer);
  S.toastTimer = setTimeout(() => toast.classList.add("hidden"), actions.length ? 10000 : 4200);
}

function hideToast() {
  clearTimeout(S.toastTimer);
  $("#toast")?.classList.add("hidden");
}

// One styled replacement for the browser's native confirm dialogs, so
// destructive actions look like the rest of the app. Automated runs (capture
// self-test and the verify harness) approve immediately instead of parking on
// a dialog.
function confirmAction({ title, message = "", confirm = "Confirm", danger = false }) {
  if (window.__crosshairAutoConfirm || window.__crosshairCapture) return Promise.resolve(true);
  const modal = $("#confirmModal");
  if (!modal) return Promise.resolve(false);
  settleConfirm(false);
  S.confirmFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const heading = $("#confirmTitle");
  if (heading) heading.textContent = title;
  const body = $("#confirmMessage");
  if (body) {
    body.textContent = message;
    body.classList.toggle("hidden", !message);
  }
  const ok = $("#confirmOk");
  if (ok) {
    ok.textContent = confirm;
    ok.classList.toggle("danger", !!danger);
    ok.classList.toggle("primary", !danger);
  }
  modal.classList.remove("hidden");
  const cancel = $("#confirmCancel");
  (danger ? cancel : ok)?.focus();
  return new Promise((resolve) => { S.confirmResolve = resolve; });
}

function settleConfirm(result) {
  const modal = $("#confirmModal");
  if (modal) modal.classList.add("hidden");
  const resolve = S.confirmResolve;
  if (resolve) S.confirmOutcome = result;
  S.confirmResolve = null;
  if (resolve) resolve(result);
  if (S.confirmFocus && document.contains(S.confirmFocus)) S.confirmFocus.focus();
  S.confirmFocus = null;
}

function confirmThen(options, run) {
  confirmAction(options).then((ok) => { if (ok) run(); });
}

function currentPreset(module) {
  const list = module === "crosshair" ? S.crosshairPresets : module === "display" ? S.displayPresets : S.audioPresets;
  const cfg = module === "crosshair" ? S.config?.crosshair : module === "display" ? S.config?.display : S.config?.audio;
  if (!cfg) return null;
  return list.find((p) => p.id === cfg.activePreset) || (module === "crosshair" ? list.find((p) => p.builtin && p.config.style === cfg.style) : null) || null;
}

function presetLabel(module, preset) {
  if (!preset) return module === "crosshair" ? "Custom shape" : "Custom";
  return preset.name;
}

function stripPresetEmoji(name) {
  return String(name || "").replace(/^\p{Extended_Pictographic}\s*/u, "");
}

function presetAssetKey(preset) {
  if (preset?.builtin && preset.id?.startsWith("style_")) return preset.id.slice(6);
  const style = preset?.config?.style || "plus";
  if (style === "ring" && preset?.config?.dot) return "circle_dot";
  return style;
}

function renderPresetToolbar(module) {
  const cfg = module === "crosshair" ? S.config.crosshair : module === "display" ? S.config.display : S.config.audio;
  const preset = currentPreset(module);
  const trigger = $(`#${module === "crosshair" ? "aim" : module}PresetTrigger`);
  const name = $(`#${module === "crosshair" ? "aim" : module}PresetName`);
  const menu = $(`#${module === "crosshair" ? "aim" : module}PresetMenu`);
  const del = $(`#${module === "crosshair" ? "aim" : module}PresetDelete`);
  if (!trigger || !name || !menu) return;
  name.textContent = module === "crosshair" ? presetLabel(module, preset) : stripPresetEmoji(preset?.name || "Custom");
  if (module === "crosshair") {
    const icon = $("#aimPresetIcon");
    const style = presetAssetKey(preset);
    if (icon) icon.src = `/assets/presets/${style}.png`;
  } else {
    trigger.querySelector(".preset-emoji").textContent = preset?.name?.match(/^\p{Extended_Pictographic}/u)?.[0] || (module === "display" ? "⚖️" : "🎚️");
  }
  const list = module === "crosshair" ? S.crosshairPresets : module === "display" ? S.displayPresets : S.audioPresets;
  menu.innerHTML = list.map((p) => {
    const icon = module === "crosshair"
      ? `<img src="/assets/presets/${esc(presetAssetKey(p))}.png" alt="">`
      : `<span class="preset-emoji">${esc(p.name.match(/^\p{Extended_Pictographic}/u)?.[0] || (module === "display" ? "🖥️" : "🎵"))}</span>`;
    const label = module === "crosshair" ? p.name : stripPresetEmoji(p.name);
    return `<button class="preset-option ${cfg.activePreset === p.id ? "active" : ""}" data-action="apply-preset" data-module="${module}" data-id="${esc(p.id)}">${icon}<span>${esc(label)}</span>${cfg.activePreset === p.id ? '<span class="check">✓</span>' : ""}</button>`;
  }).join("");
  del.disabled = !preset || !!preset.builtin;
}

function closePresetMenus() {
  $$(".preset-menu").forEach((menu) => menu.classList.add("hidden"));
  $$(".preset-trigger").forEach((trigger) => trigger.setAttribute("aria-expanded", "false"));
  S.presetMenu = null;
}
function openPresetMenu(module) {
  const menu = $(`#${module === "crosshair" ? "aim" : module}PresetMenu`);
  const trigger = $(`#${module === "crosshair" ? "aim" : module}PresetTrigger`);
  if (!menu || !trigger) return;
  const willOpen = menu.classList.contains("hidden");
  closePresetMenus();
  if (willOpen) {
    menu.classList.remove("hidden");
    trigger.setAttribute("aria-expanded", "true");
    S.presetMenu = module;
  }
}

function displayMonitor() {
  if (!S.monitors.length) return null;
  const wanted = S.config?.display?.selectedMonitor;
  return S.monitors.find((m) => m.id === wanted) || S.monitors.find((m) => m.primary) || S.monitors[0];
}
function displayValues() {
  if (!S.config) return null;
  const cfg = S.config.display;
  const monitor = displayMonitor();
  if (cfg.customizeEach && monitor) {
    if (!cfg.monitors[monitor.id]) cfg.monitors[monitor.id] = clone(cfg.global);
    return cfg.monitors[monitor.id];
  }
  return cfg.global;
}

function formatReadout(path, value) {
  if (value == null) return "—";
  if (path.endsWith("opacity") || path.endsWith("image_scale")) return `${Math.round(value)}%`;
  if (path.endsWith("rotation")) return `${Math.round(value)}°`;
  if (path.endsWith("gif_speed")) return `${Number(value).toFixed(2)}×`;
  if (path === "audio.output_level" || path === "audio.balance") return `${Number(value) > 0 ? "+" : ""}${Math.round(Number(value))}`;
  if (path === "audio.volume_leveling" || path === "audio.filter_q") return Number(value).toFixed(1);
  if (path.startsWith("audio.")) return `${Math.round(Number(value))}`;
  return `${Math.round(value * 10) / 10}`;
}

function animatedImage(path) {
  const ext = String(path || "").toLowerCase().split(".").pop();
  return ext === "gif" || ext === "webp";
}

function formatFaderValue(value) {
  const number = Number(value);
  return `${number > 0 ? "+" : ""}${number.toFixed(1)}`;
}

function setRangeFill(el) {
  const min = Number(el.min || 0);
  const max = Number(el.max || 100);
  const value = Number(el.value);
  const span = Math.max(0.0001, max - min);
  const percent = (raw) => Math.max(0, Math.min(100, ((raw - min) / span) * 100));
  const pct = percent(value);
  // Ranges that cross zero (brightness, contrast, gain, balance and the EQ
  // faders) fill outwards from their own zero mark, so a neutral control looks
  // untouched instead of half-full. Unipolar ranges still fill from the left.
  const zero = min < 0 ? percent(0) : 0;
  el.style.setProperty("--fill-lo", `${Math.min(zero, pct)}%`);
  el.style.setProperty("--fill-hi", `${Math.max(zero, pct)}%`);
  if (el.dataset.band != null) {
    const value = formatFaderValue(el.value);
    const readout = el.closest(".fader")?.querySelector(".fader-value");
    if (readout) readout.textContent = value;
    el.setAttribute("aria-valuetext", value);
  }
}
function updateRangeFills(root = document) {
  root.querySelectorAll("input[type=range]").forEach(setRangeFill);
}

function syncAimConditionals() {
  if (!S.config) return;
  const cfg = S.config.crosshair;
  const gap = $("#gapRow");
  if (gap) gap.classList.toggle("hidden", cfg.style === "dot");
  const dot = $("#dotSizeRow");
  if (dot) dot.classList.toggle("hidden", !cfg.dot);
  const outline = $("#outlineWidthRow");
  if (outline) outline.classList.toggle("hidden", !cfg.outline);
  const imageOptions = $("#imageOptions");
  if (imageOptions) imageOptions.classList.toggle("hidden", !cfg.image);
  const solid = $("#solidColorRow");
  if (solid) solid.classList.toggle("hidden", !cfg.imageSolid && !cfg.imageTint);
  const colorLabel = $("#modeColorLabel");
  if (colorLabel) colorLabel.textContent = cfg.imageSolid ? "Silhouette color" : "Tint color";
  const gif = $("#gifOptions");
  if (gif) gif.classList.toggle("hidden", !cfg.image || !animatedImage(cfg.image));
  const solidMode = $("#solidMode");
  if (solidMode) {
    const mode = cfg.imageSolid ? "solid" : cfg.imageTint ? "tint" : "original";
    $$("#solidMode [data-action=image-mode]").forEach((el) => el.classList.toggle("active", el.dataset.value === mode));
  }
}

function syncControls() {
  if (!S.config) return;
  $$("[data-bind]").forEach((el) => {
    const path = el.dataset.bind;
    if (!path.includes(".")) return;
    const value = getPath(S.config, path);
    if (el.type === "checkbox") el.checked = !!value;
    else if (value !== undefined && value !== null && document.activeElement !== el) el.value = value;
  });
  $$("[data-readout]").forEach((el) => {
    el.textContent = formatReadout(el.dataset.readout, getPath(S.config, el.dataset.readout));
  });
  syncColorControls();
  updateRangeFills();
}

function hexToRgb(hex) {
  const value = String(hex || "").replace("#", "").trim();
  if (!/^[0-9a-f]{6}$/i.test(value)) return null;
  return { r: parseInt(value.slice(0, 2), 16), g: parseInt(value.slice(2, 4), 16), b: parseInt(value.slice(4, 6), 16) };
}
function rgbToHex({ r, g, b }) {
  return "#" + [r, g, b].map((v) => Math.max(0, Math.min(255, Math.round(v))).toString(16).padStart(2, "0")).join("").toUpperCase();
}
function rgbToHsv({ r, g, b }) {
  const rn = r / 255, gn = g / 255, bn = b / 255;
  const max = Math.max(rn, gn, bn), min = Math.min(rn, gn, bn), d = max - min;
  let h = 0;
  if (d > 0) {
    if (max === rn) h = ((gn - bn) / d) % 6;
    else if (max === gn) h = (bn - rn) / d + 2;
    else h = (rn - gn) / d + 4;
    h = (h * 60 + 360) % 360;
  }
  return { h, s: max === 0 ? 0 : d / max, v: max };
}
function hsvToRgb({ h, s, v }) {
  const c = v * s, x = c * (1 - Math.abs(((h / 60) % 2) - 1)), m = v - c;
  let rgb;
  if (h < 60) rgb = [c, x, 0];
  else if (h < 120) rgb = [x, c, 0];
  else if (h < 180) rgb = [0, c, x];
  else if (h < 240) rgb = [0, x, c];
  else if (h < 300) rgb = [x, 0, c];
  else rgb = [c, 0, x];
  return { r: (rgb[0] + m) * 255, g: (rgb[1] + m) * 255, b: (rgb[2] + m) * 255 };
}
function colorHexValue() {
  return S.config?.crosshair?.color || "#4EA8FF";
}
function syncColorControls() {
  const color = colorHexValue();
  const trigger = $("#colorTrigger");
  const cv = $("#colorValue");
  if (cv) cv.textContent = color.toUpperCase();
  const swatches = $$("#swatches .swatch");
  swatches.forEach((el) => el.classList.toggle("active", el.dataset.value.toLowerCase() === color.toLowerCase()));
  // The custom button always keeps its rainbow gradient; it is only marked
  // selected when the current colour is not one of the preset swatches.
  trigger?.classList.toggle("active", !swatches.some((el) => el.dataset.value.toLowerCase() === color.toLowerCase()));
  const rgb = hexToRgb(color);
  if (!rgb) return;
  const hsv = rgbToHsv(rgb);
  if (!S.colorHsv || Math.abs(S.colorHsv.h - hsv.h) > 0.5 || Math.abs(S.colorHsv.s - hsv.s) > 0.01) S.colorHsv = hsv;
  const hex = $("#colorHex");
  if (hex && document.activeElement !== hex) hex.value = color.toUpperCase();
  const hue = $("#colorHue");
  if (hue && document.activeElement !== hue) hue.value = String(Math.round(hsv.h));
  const sv = $("#colorSv");
  const knob = $("#colorSvKnob");
  if (sv) sv.style.setProperty("--hue", String(hsv.h));
  if (knob && sv) {
    knob.style.left = `${hsv.s * 100}%`;
    knob.style.top = `${(1 - hsv.v) * 100}%`;
    knob.style.background = color;
  }
}
function applyCustomColor(hex) {
  if (!hexToRgb(hex)) return;
  S.config.crosshair.color = hex.toUpperCase();
  S.config.crosshair.activePreset = null;
  syncColorControls();
  drawEQ();
  scheduleSend("crosshair");
  refreshPreview();
}
function setHsv(hsv) {
  S.colorHsv = { h: (hsv.h + 360) % 360, s: Math.max(0, Math.min(1, hsv.s)), v: Math.max(0, Math.min(1, hsv.v)) };
  applyCustomColor(rgbToHex(hsvToRgb(S.colorHsv)));
}
function openColorPopover(open) {
  const popover = $("#colorPopover");
  const trigger = $("#colorTrigger");
  if (!popover || !trigger) return;
  popover.classList.toggle("hidden", !open);
  trigger.setAttribute("aria-expanded", String(open));
  if (open) {
    syncColorControls();
    S.colorHsv = rgbToHsv(hexToRgb(colorHexValue()) || { r: 78, g: 168, b: 255 });
    syncColorControls();
  }
}
function handleColorSv(event) {
  const sv = $("#colorSv");
  if (!sv) return;
  const rect = sv.getBoundingClientRect();
  if (!rect.width || !rect.height) return;
  const s = Math.max(0, Math.min(1, (event.clientX - rect.left) / rect.width));
  const v = Math.max(0, Math.min(1, 1 - (event.clientY - rect.top) / rect.height));
  setHsv({ h: S.colorHsv?.h || 0, s, v });
}

function handleBind(input) {
  if (!S.config || !input.dataset.bind) return;
  let value;
  if (input.type === "checkbox") value = input.checked;
  else if (input.type === "range" || input.type === "number") value = Number(input.value);
  else value = input.value;
  if (input.dataset.bind === "crosshair.monitor" && value === "") value = null;
  if (input.dataset.int) value = Math.round(value);
  setPath(S.config, input.dataset.bind, value);
  if (input.dataset.mod === "crosshair") S.config.crosshair.activePreset = null;
  const readout = $(`[data-readout="${input.dataset.bind}"]`);
  if (readout) readout.textContent = formatReadout(input.dataset.bind, value);
  scheduleSend(input.dataset.mod);
  if (input.dataset.mod === "crosshair") {
    syncAimConditionals();
    refreshPreview();
  }
  if (input.dataset.bind === "display.customize_each") renderDisplay();
  if (input.dataset.mod === "audio") drawEQ();
  setRangeFill(input);
}

function setTab(tab) {
  if (!["aim", "display", "audio", "slots"].includes(tab)) return;
  if (S.tab === "slots" && tab !== "slots") flushSlotSave();
  S.tab = tab;
  document.body.dataset.tab = tab;
  $$(".tab").forEach((el) => el.classList.toggle("active", el.dataset.tab === tab));
  $$(".tab").forEach((el) => {
    const active = el.dataset.tab === tab;
    el.setAttribute("aria-selected", String(active));
    el.tabIndex = active ? 0 : -1;
  });
  $$(".view").forEach((el) => el.classList.toggle("active", el.dataset.view === tab));
  send("SetTab", { tab });
  if (tab === "aim") refreshPreview(true);
  if (tab === "display") drawLut();
  if (tab === "audio") drawEQ();
  if (tab === "slots" && !S.editing) {
    const active = S.config?.slots?.list?.find((s) => s.id === S.config.slots.active) || S.config?.slots?.list?.[0];
    if (active) selectSlot(active.id);
  }
}

// Roving tabindex makes only the active tab reachable with Tab, so the tab row
// needs arrow-key navigation or keyboard users could never leave the Aim tab.
document.addEventListener("keydown", (event) => {
  const tab = event.target.closest?.(".tabs [role=tab]");
  if (!tab) return;
  const tabs = $$(".tabs [role=tab]");
  const index = tabs.indexOf(tab);
  if (index < 0) return;
  const step = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
  const target = event.key === "Home"
    ? tabs[0]
    : event.key === "End"
      ? tabs[tabs.length - 1]
      : step
        ? tabs[(index + step + tabs.length) % tabs.length]
        : null;
  if (!target) return;
  event.preventDefault();
  setTab(target.dataset.tab);
  target.focus();
});

function renderHeader() {
  if (!S.config) return;
  const active = S.config.slots.list.find((s) => s.id === S.config.slots.active);
  const badge = $("#slotBadge");
  if (badge) {
    badge.textContent = active ? `${active.emoji}  ${active.name}` : "NO ACTIVE SLOT";
    badge.classList.toggle("active", !!active);
  }
  [["masterCrosshair", "crosshair"], ["masterDisplay", "display"], ["masterAudio", "audio"]].forEach(([id, module]) => {
    const el = $(`#${id}`);
    if (!el) return;
    const on = !!S.config[module].enabled;
    el.classList.toggle("active", on);
    el.setAttribute("aria-pressed", String(on));
  });
}

function renderAim() {
  if (!S.config) return;
  const cfg = S.config.crosshair;
  const visibility = cfg.aimVisibility || (cfg.hideOnRmb ? "hide" : "always");
  $$("#aimVisibility .seg").forEach((el) => {
    const active = el.dataset.value === visibility;
    el.classList.toggle("active", active);
    el.setAttribute("aria-pressed", String(active));
  });
  if (!S.aimModeInitialized) {
    S.aimMode = cfg.image ? "image" : "shape";
    S.aimModeInitialized = true;
  }
  $$("[data-action=aim-mode]").forEach((el) => el.classList.toggle("active", el.dataset.value === S.aimMode));
  $("#shapePanel")?.classList.toggle("hidden", S.aimMode !== "shape");
  $("#imagePanel")?.classList.toggle("hidden", S.aimMode !== "image");
  renderPresetToolbar("crosshair");
  syncAimConditionals();
  const imageName = $("#imageName");
  if (imageName) {
    if (cfg.image) {
      imageName.textContent = cfg.imageName || cfg.image.split(/[\\/]/).pop();
      imageName.nextElementSibling.textContent = "Click to replace";
    } else {
      imageName.textContent = "Drop PNG / JPG / WebP / GIF / SVG / BMP / ICO or click to browse";
      imageName.nextElementSibling.textContent = "Transparent images work best.";
    }
  }
  const remove = document.querySelector('[data-action="remove-image"]');
  if (remove) remove.classList.toggle("hidden", !cfg.image);
  const monitor = S.monitors.find((m) => m.id === cfg.monitor) || S.monitors.find((m) => m.primary) || S.monitors[0];
  const aimMonitor = $("#aimMonitor");
  if (aimMonitor) aimMonitor.textContent = monitor ? monitor.name : "No display detected";
  const select = $("#aimMonitorSelect");
  if (select) {
    const multiple = S.monitors.length > 1;
    select.classList.toggle("hidden", !multiple);
    if (multiple) {
      select.innerHTML = `<option value="">Primary display</option>${S.monitors.map((m) => `<option value="${esc(m.id)}">${esc(m.name)}</option>`).join("")}`;
      if (document.activeElement !== select) select.value = cfg.monitor || "";
    }
  }
  document.querySelectorAll("[data-action=day-night]").forEach((b) => b.classList.toggle("active", (b.dataset.value === "day") === S.day));
}

function renderDisplay() {
  if (!S.config) return;
  const cfg = S.config.display;
  if (cfg.customizeEach) {
    S.monitors.forEach((m) => { if (!cfg.monitors[m.id]) cfg.monitors[m.id] = clone(cfg.global); });
  }
  const values = displayValues() || cfg.global;
  const monitor = displayMonitor();
  const blueLight = Number(values.warmth || 0) > 0;
  const blueLightButton = $("#blueLightToggle");
  if (blueLightButton) {
    blueLightButton.setAttribute("aria-pressed", String(blueLight));
  }
  renderPresetToolbar("display");
  $$("[data-display]").forEach((el) => {
    if (document.activeElement !== el) el.value = values[el.dataset.display] ?? 0;
  });
  $$("[data-display-readout]").forEach((el) => {
    const key = el.dataset.displayReadout;
    const value = key === "gamma" ? Number(values[key] ?? 1).toFixed(2) : String(Math.round(Number(values[key] ?? 0) * 10) / 10);
    el.textContent = value;
  });
  const panel = $("#panelSelect");
  if (panel) {
    const detected = String(monitor?.detectedPanel || "led").toUpperCase();
    panel.innerHTML = PANELS.map(([key, name]) => `<option value="${key}">${key === "auto" ? `Auto (${detected})` : name}</option>`).join("");
    panel.value = values.panel || "auto";
  }
  const backlight = $("#backlightRange");
  const backlightRow = $("#backlightRow");
  const supported = !!monitor?.backlightSupported;
  backlightRow?.classList.toggle("hidden", !supported);
  if (backlight) {
    backlight.disabled = !supported || S.compareActive;
    const bw = cfg.backlight?.[monitor?.id] ?? monitor?.backlight ?? 50;
    if (document.activeElement !== backlight) backlight.value = supported ? bw : 50;
    const out = $("#backlightReadout");
    if (out) out.textContent = supported ? String(bw) : "N/A";
  }
  const multi = S.monitors.length > 1;
  $("#multiMonitorControls")?.classList.toggle("hidden", !multi);
  $$("#displayMode .seg").forEach((el) => el.classList.toggle("active", el.dataset.value === cfg.mode));
  const customize = $("#customizeEach");
  if (customize) customize.checked = !!cfg.customizeEach;
  const pills = $("#monitorPills");
  if (pills) {
    pills.innerHTML = multi ? S.monitors.map((m) => `<button class="pill ${m.id === cfg.selectedMonitor ? "active" : ""}" data-action="select-monitor" data-id="${esc(m.id)}">${esc(m.name)}</button>`).join("") : "";
  }
  const modeReadout = $("#displayModeReadout");
  if (modeReadout) modeReadout.textContent = multi ? (cfg.mode === "this" ? "This display" : "All displays") : "Single display";
  const device = $("#displayDevice");
  if (device) device.textContent = monitor ? monitor.name : "No display detected";
  const hardware = $("#hardwareInfo");
  if (hardware) {
    hardware.textContent = monitor
      ? `${monitor.width}×${monitor.height} @ ${(monitor.scale * 100).toFixed(0)}% · ${monitor.hdr ? "HDR active" : monitor.advancedColor ? "Advanced Color active" : monitor.nightLight ? "Night Light active" : "SDR gamma ready"}`
      : "No display detected.";
  }
  const status = $("#displayStatus");
  if (status) {
    status.textContent = S.warning ? "BLOCKED" : !cfg.enabled ? "OFF" : S.applied ? "APPLIED" : "PREVIEW";
    status.style.color = S.warning ? "var(--red)" : cfg.enabled ? "var(--accent)" : "var(--muted)";
  }
  const banner = $("#displayWarning");
  if (banner) {
    banner.classList.toggle("hidden", !S.warning);
    if (S.warning) {
      $("#warningTitle").textContent = S.warning.code === "hdr" ? "HDR is active" : S.warning.code === "acm" ? "Advanced Color is active" : "Night Light is active";
      $("#warningText").textContent = S.warning.message;
    }
  }
  updateRangeFills();
}

function openFxModal(mode = "setup") {
  S.fxModalShown = true;
  S.fxModalMode = mode;
  $("#fxModal")?.classList.remove("hidden");
  syncFxModal();
}

function closeFxModal(force = false) {
  const modal = $("#fxModal");
  if (!modal) return;
  // A reinstall must not be hidden while it is uninstalling or installing.
  if (!force && S.fxModalMode === "repair" && S.fx?.busy) return;
  S.fxRepairArmed = false;
  modal.classList.add("hidden");
}

function syncFxModal() {
  const fx = S.fx || {};
  const modal = $("#fxModal");
  if (!modal) return;
  if (!S.fxModalShown && !window.__crosshairCapture && S.ready && (!fx.installed || !fx.cliReady)) {
    S.fxModalShown = true;
    S.fxModalMode = "setup";
    modal.classList.remove("hidden");
  }
  const isOpen = !modal.classList.contains("hidden");
  if (!isOpen) return;
  const mode = S.fxModalMode || "setup";
  const repairFailed = /failed|could not|not installed|too old/i.test(fx.message || "");
  if (mode === "repair") {
    // The worker's "FxSound reinstalled." notice is the success toast; the modal
    // only has to get out of the way once the staged run has finished cleanly.
    if (!fx.busy && S.fxRepairArmed && !repairFailed) {
      S.fxRepairArmed = false;
      modal.classList.add("hidden");
      return;
    }
  } else if (!fx.busy && ((mode === "update" && !fx.updateAvailable) || (mode === "setup" && fx.installed && fx.cliReady))) {
    modal.classList.add("hidden");
    return;
  }
  const title = $("#fxModalTitle");
  const message = $("#fxModalMessage");
  if (title) title.textContent = mode === "repair" ? "Reinstall FxSound" : mode === "update" ? "Update FxSound" : fx.installed ? "Update FxSound" : "Set up FxSound";
  if (message) {
    if (mode === "repair") {
      message.textContent = repairFailed && S.fxRepairArmed && !fx.busy
        ? "FxSound could not be reinstalled. Check the log, then retry."
        : "Crosshair+ closes FxSound, backs up its settings, uninstalls and reinstalls the current version, then restores your settings and starts it again. Sound is unavailable while this runs.";
    } else {
      message.textContent = mode === "update"
        ? `FxSound ${fx.latestVersion || ""} is available.`
        : fx.installed
          ? `FxSound ${fx.version || ""} needs updating for live control.`
          : "Install FxSound to enable live sound controls.";
    }
  }
  $("#fxProgress")?.classList.toggle("hidden", !fx.busy);
  const task = $("#fxTaskText");
  if (task) {
    task.textContent = fx.busy ? (fx.task || "Installing FxSound…") : "";
    task.classList.toggle("hidden", !fx.busy);
  }
  const install = $("#fxInstallButton");
  if (install) {
    install.textContent = fx.busy ? (fx.task || "Working…") : mode === "repair" ? "Reinstall FxSound" : mode === "update" ? "Update FxSound" : "Install FxSound";
    install.dataset.action = mode === "repair" ? "repair-run" : "setup-audio";
    install.disabled = !!fx.busy;
  }
  const retry = $("#fxRetryButton");
  if (retry) {
    retry.dataset.action = mode === "repair" ? "repair-run" : "setup-audio";
    retry.classList.toggle("hidden", fx.busy || !/failed/i.test(fx.message || ""));
    retry.disabled = !!fx.busy;
  }
  $$("#fxModal [data-action=open-fx-download]").forEach((button) => { button.disabled = !!fx.busy; });
  $$("[data-action=repair-audio], [data-action=open-fxsound]").forEach((button) => { button.disabled = !!fx.busy; });
}

function renderAudio() {
  if (!S.config) return;
  const audio = S.config.audio;
  renderPresetToolbar("audio");
  const device = $("#audioDevice");
  if (device) {
    const options = [`<option value="">System default</option>`].concat(
      S.devices.map((d) => `<option value="${esc(d.name)}" ${audio.device === d.name ? "selected" : ""}>${esc(d.name)}${d.isDefault ? " · Default" : ""}</option>`)
    );
    if (audio.device && !S.devices.some((d) => d.name === audio.device)) {
      options.push(`<option value="${esc(audio.device)}" selected>${esc(audio.device)}</option>`);
    }
    device.innerHTML = options.join("");
    device.value = audio.device || "";
  }
  const grid = $("#bandGrid");
  if (grid && !grid.querySelector(".fader")) {
    grid.innerHTML = FADERS.map(([emoji, label], i) => `
      <div class="fader" data-fader="${i}">
        <span class="fader-value" aria-hidden="true">0.0</span>
        <div class="fader-track"><input type="range" min="-12" max="12" step="0.5" data-band="${i}" aria-label="${label} (${FREQS[i]} Hz)"></div>
        <span class="fader-emoji" aria-hidden="true">${emoji}</span>
        <span class="fader-label">${label}</span>
      </div>`).join("");
  }
  $$("[data-band]").forEach((el) => {
    const i = Number(el.dataset.band);
    if (document.activeElement !== el) el.value = audio.bands[i] ?? 0;
    setRangeFill(el);
  });
  const importStatus = $("#importStatus");
  const importName = $("#importName");
  if (importStatus && importName) {
    importStatus.classList.toggle("hidden", !audio.imported);
    importName.textContent = audio.imported ? `${audio.imported.name} · imported base` : "No imported profile";
  }
  const fx = S.fx || {};
  const fxState = $("#fxState");
  if (fxState) {
    fxState.textContent = fx.busy
      ? "WORKING"
      : !fx.installed ? "NOT INSTALLED" : !fx.cliReady ? "UPDATE" : fx.running ? (fx.power ? "READY" : "MUTED") : "STOPPED";
  }
  const needsSetup = !fx.installed || !fx.cliReady;
  const setup = $("#setupAudio");
  if (setup) {
    setup.classList.toggle("hidden", !needsSetup);
    setup.textContent = fx.installed ? "⚡ Update FxSound" : "⚡ Install FxSound";
    setup.disabled = !!fx.busy;
  }
  const launch = $("#launchFx");
  if (launch) {
    launch.classList.toggle("hidden", needsSetup || !!fx.running || !!fx.busy);
    launch.disabled = !!fx.busy;
  }
  $$(".update-badge").forEach((badge) => badge.classList.toggle("hidden", !fx.updateAvailable));
  const fxMessage = $("#fxMessage");
  if (fxMessage) {
    const message = fx.busy
      ? (fx.task || "Working…")
      : needsSetup
        ? "Set up FxSound to enable sound presets."
        : !fx.running
          ? "FxSound is installed but not running. Launch it to enable live sound control."
          : (fx.message && fx.message !== "FxSound is ready." ? fx.message : "FxSound is ready. Every slider applies live.");
    fxMessage.textContent = message;
    fxMessage.classList.toggle("error", /failed|could not|not installed|too old/i.test(message));
  }
  syncFxModal();
  updateMeter(S.meterPeak, S.meterHeadroom);
  updateRangeFills();
  drawEQ();
}

function updateMeter(peak, headroom) {
  const db = peak > 0.00001 ? 20 * Math.log10(peak) : -60;
  const pct = Math.max(0, Math.min(100, (db + 60) / 60 * 100));
  const fill = $("#meterFill");
  if (fill) fill.style.clipPath = `inset(0 ${100 - pct}% 0 0)`;
}

function renderSlots() {
  if (!S.config) return;
  const slots = S.config.slots.list;
  const count = $("#slotCount");
  if (count) count.textContent = `${slots.length} / 6`;
  const list = $("#slotList");
  if (list) {
    list.innerHTML = slots.map((slot) => {
      const bits = ([slot.crosshair.on ? "Aim" : null, slot.display.on ? "Screen" : null, slot.audio.on ? "Sound" : null].filter(Boolean).join(" · ") || "No modules") + (S.config.slots.active === slot.id ? " · Active" : "");
      const selected = S.editing?.id === slot.id;
      return `<button class="slot-card ${selected ? "selected" : ""} ${S.config.slots.active === slot.id ? "active" : ""}" data-action="select-slot" data-id="${esc(slot.id)}">
        <span class="slot-emoji">${esc(slot.emoji)}</span>
        <span><span class="slot-name">${esc(slot.name)}</span><span class="slot-sub">${esc(bits)}</span></span>
        <span class="slot-hotkey">${slot.hotkey ? esc(slot.hotkey) : "—"}</span>
        <span class="slot-state"></span>
      </button>`;
    }).join("");
  }
  if (!S.editing && slots.length) {
    const active = slots.find((s) => s.id === S.selectedSlot) || slots.find((s) => s.id === S.config.slots.active) || slots[0];
    S.editing = clone(active);
    S.editingDirty = false;
  }
  if (!S.slotEditorFocused) renderSlotEditor();
}

function renderSlotEditor() {
  const editor = $("#slotEditor");
  if (!editor) return;
  if (!S.editing) {
    editor.innerHTML = `<div class="slot-empty"><strong>No slot selected</strong><span>Create a slot to bundle your settings.</span></div>`;
    return;
  }
  const slot = S.editing;
  const used = new Set(S.config.slots.list.filter((s) => s.id !== slot.id).map((s) => s.emoji));
  const optionRows = S.runningApps.map((app) => {
    const icon = app.icon
      ? `<img class="app-icon" src="${esc(app.icon)}" alt="">`
      : `<span class="app-icon placeholder">📦</span>`;
    return `<button class="custom-option ${slot.targetExe === app.exe ? "active" : ""}" data-action="custom-pick" data-select="target" data-value="${esc(app.exe)}">${icon}<span class="app-copy"><span class="app-title">${esc(app.title || app.exe)}</span><span class="app-exe">${esc(app.exe)}</span></span>${slot.targetExe === app.exe ? '<span class="check">✓</span>' : ""}</button>`;
  }).join("");
  const currentRow = slot.targetExe && !S.runningApps.some((app) => app.exe === slot.targetExe)
    && !(S.installedGames || []).some((game) => game.exe === slot.targetExe)
    ? `<button class="custom-option active" data-action="custom-pick" data-select="target" data-value="${esc(slot.targetExe)}"><span class="app-icon placeholder">📦</span><span class="app-copy"><span class="app-title">Current target</span><span class="app-exe">${esc(slot.targetExe)}</span></span><span class="check">✓</span></button>` : "";
  const moduleCard = (key, label) => {
    const m = slot[key] || { on: false, preset: null };
    const presets = key === "crosshair" ? S.crosshairPresets : key === "display" ? S.displayPresets : S.audioPresets;
    const selected = presets.find((preset) => preset.id === m.preset);
    const menu = [`<button class="custom-option ${!m.preset ? "active" : ""}" data-action="custom-pick" data-select="module:${key}" data-value="">Default</button>`].concat(
      presets.map((preset) => `<button class="custom-option ${m.preset === preset.id ? "active" : ""}" data-action="custom-pick" data-select="module:${key}" data-value="${esc(preset.id)}"><span>${esc(stripPresetEmoji(preset.name))}</span>${m.preset === preset.id ? '<span class="check">✓</span>' : ""}</button>`)
    ).join("");
    const open = S.customSelect === `module:${key}`;
    return `<div class="module-card ${m.on ? "" : "off"}">
      <div class="row-toggle"><span>${label}</span><label class="switch"><input type="checkbox" data-slot-module="${key}" ${m.on ? "checked" : ""}><i></i></label></div>
      <div class="custom-select">
        <button class="custom-trigger" data-action="toggle-select" data-select="module:${key}" aria-haspopup="listbox" aria-expanded="${open}" ${m.on ? "" : "disabled"}><span>${esc(selected ? stripPresetEmoji(selected.name) : "Default")}</span><b>▾</b></button>
        <div class="custom-menu ${open ? "" : "hidden"}" role="listbox">${menu}</div>
      </div>
    </div>`;
  };
  const knownTarget = slot.targetExe
    ? S.runningApps.find((app) => app.exe === slot.targetExe) || (S.installedGames || []).find((game) => game.exe === slot.targetExe)
    : null;
  const targetLabel = slot.targetExe ? (knownTarget?.title || knownTarget?.name || slot.targetExe) : "Manual only";
  const targetOpen = S.customSelect === "target";
  editor.innerHTML = `
    <div class="card slot-top">
      <button class="slot-icon-button" data-action="open-emoji" aria-label="Change slot icon">${esc(slot.emoji)}</button>
      <div class="slot-identity"><span>Name</span><input id="slotName" maxlength="24" value="${esc(slot.name)}" placeholder="Slot name"><span>${slot.name.length}/24</span></div>
      <div class="slot-identity"><span>Hotkey</span><div class="hotkey-field" id="hotkeyField" tabindex="0">${esc(slot.hotkey || "Click to record")}</div><span class="hotkey-help">Esc cancels · Backspace clears</span></div>
    </div>
    <div class="card">
      <div class="card-heading"><h2>Modules</h2></div>
      <div class="module-grid">${moduleCard("crosshair", "🎯 Aim")}${moduleCard("display", "🖥️ Screen")}${moduleCard("audio", "🔊 Sound")}</div>
    </div>
    <div class="card">
      <div class="card-heading"><h2>Auto-launch</h2></div>
      <div class="slot-meta">
        <div class="custom-select slot-target-custom">
          <button class="custom-trigger" id="slotTarget" data-action="toggle-select" data-select="target" aria-haspopup="listbox" aria-expanded="${targetOpen}" aria-label="Game target"><span>${esc(targetLabel)}</span><b>▾</b></button>
          <div class="custom-menu grid ${S.runningApps.length > 6 ? "grid-3" : ""} ${targetOpen ? "" : "hidden"}" role="listbox">
            <button class="custom-option ${!slot.targetExe ? "active" : ""}" data-action="custom-pick" data-select="target" data-value=""><span class="app-icon placeholder">—</span><span class="app-copy"><span class="app-title">Manual only</span><span class="app-exe">No automatic switching</span></span></button>
            ${currentRow}${optionRows}
            <button class="custom-option" data-action="open-games"><span class="app-icon placeholder">🎮</span><span class="app-copy"><span class="app-title">Installed games…</span><span class="app-exe">${S.gamesScanning ? "Scanning Steam, Epic and Xbox folders…" : (S.installedGames || []).length ? `${S.installedGames.length} game${S.installedGames.length === 1 ? "" : "s"} found` : "Scan Steam, Epic and Xbox folders"}</span></span></button>
            <button class="custom-option" data-action="custom-pick" data-select="target" data-value="__browse__"><span class="app-icon placeholder">＋</span><span class="app-copy"><span class="app-title">Browse .exe…</span><span class="app-exe">Choose any installed app</span></span></button>
          </div>
        </div>
        <div class="slot-target-line"><span>${slot.targetExe ? esc(slot.targetExe) : "No game target selected."}</span></div>
      </div>
    </div>
    <div class="slot-actions">
      <span class="help-copy">Click a slot card to activate or deactivate it.</span>
      <button class="button ghost" data-action="delete-slot">Delete</button>
    </div>`;
  const nameInput = $("#slotName");
  nameInput?.addEventListener("input", () => {
    slot.name = nameInput.value;
    markSlotDirty();
  });
  $("#hotkeyField")?.addEventListener("keydown", (event) => {
    event.preventDefault();
    if (event.key === "Escape") { event.target.blur(); return; }
    if (event.key === "Backspace" || event.key === "Delete") {
      slot.hotkey = null;
      event.target.textContent = "Click to record";
      markSlotDirty();
      return;
    }
    if (["Control", "Shift", "Alt", "Meta"].includes(event.key)) return;
    const parts = [];
    if (event.ctrlKey) parts.push("Ctrl");
    if (event.shiftKey) parts.push("Shift");
    if (event.altKey) parts.push("Alt");
    if (event.metaKey) parts.push("Win");
    let key = event.key;
    if (key.length === 1) key = key.toUpperCase();
    else if (key === " ") key = "Space";
    else if (key.startsWith("Arrow")) key = key.slice(5);
    else key = key.charAt(0).toUpperCase() + key.slice(1);
    parts.push(key);
    slot.hotkey = parts.join("+");
    event.target.textContent = slot.hotkey;
    markSlotDirty();
  });
  $$("[data-slot-module]").forEach((el) => el.addEventListener("change", () => {
    const key = el.dataset.slotModule;
    slot[key] = slot[key] || { on: false, preset: null };
    slot[key].on = el.checked;
    const card = el.closest(".module-card");
    card?.classList.toggle("off", !el.checked);
    const select = card?.querySelector(".custom-trigger");
    if (select) select.disabled = !el.checked;
    markSlotDirty();
  }));
  editor.dataset.usedEmojis = JSON.stringify([...used]);
}

function markSlotDirty() {
  S.editingDirty = true;
  clearTimeout(S.slotSaveTimer);
  S.slotSaveTimer = setTimeout(flushSlotSave, 380);
}
function flushSlotSave() {
  clearTimeout(S.slotSaveTimer);
  if (!S.editing || !S.editingDirty) return;
  S.slotSavePending = true;
  send("SaveSlot", { slot: S.editing });
}
function selectSlot(id, toggle = false) {
  const slot = S.config?.slots?.list.find((s) => s.id === id);
  if (!slot) return;
  if (toggle && S.slotSwitchPending) return;
  const save = S.editingDirty && S.editing ? clone(S.editing) : null;
  clearTimeout(S.slotSaveTimer);
  S.editing = clone(slot);
  S.editingDirty = false;
  S.selectedSlot = id;
  renderSlots();
  if (toggle) {
    const deactivate = S.config.slots.active === id;
    const payload = { id: deactivate ? null : id };
    if (save) payload.slot = save;
    S.slotSwitchPending = id;
    clearTimeout(S.slotSwitchTimer);
    S.slotSwitchTimer = setTimeout(() => {
      if (S.slotSwitchPending === id) {
        S.slotSwitchPending = null;
        showToast("Slot switch timed out. Settings were not left half-applied.", true);
      }
    }, 4000);
    window.__crosshairSlotSwitches = (window.__crosshairSlotSwitches || 0) + 1;
    window.__crosshairLastActivation = deactivate ? null : id;
    // One transaction: save the edited card and activate in the same command.
    send("ActivateSlot", payload);
  } else {
    // Selection-only is used when opening the Slots tab; it must not activate.
    if (save) {
      S.slotSavePending = true;
      send("SaveSlot", { slot: save });
    }
    send("SelectSlot", { id });
  }
}

function clearSlotSwitch() {
  clearTimeout(S.slotSwitchTimer);
  S.slotSwitchPending = null;
}

function closeCustomSelects() {
  S.customSelect = null;
  $$(".custom-select").forEach((select) => {
    select.querySelector(".custom-menu")?.classList.add("hidden");
    select.querySelector(".custom-trigger")?.setAttribute("aria-expanded", "false");
  });
}

function renderEmojiPicker(slot) {
  const used = new Set(S.config.slots.list.filter((s) => s.id !== slot.id).map((s) => s.emoji));
  const grid = $("#emojiGrid");
  grid.innerHTML = EMOJIS.map((emoji) => `<button class="emoji-button ${slot.emoji === emoji ? "active" : ""}" data-action="select-emoji" data-value="${emoji}" ${used.has(emoji) ? "disabled" : ""}>${emoji}</button>`).join("");
}

function applyPreset(module, id) {
  const list = module === "crosshair" ? S.crosshairPresets : module === "display" ? S.displayPresets : S.audioPresets;
  const preset = list.find((p) => p.id === id);
  if (!preset) return;
  if (module === "crosshair") {
    const old = S.config.crosshair;
    const merged = clone(preset.config);
    ["enabled", "image", "imageName", "imageScale", "imageRotation", "imageInvert", "imageSolid", "imageTint", "imageSolidColor", "gifSpeed", "gifPlaying", "monitor", "hideInMenus", "hideOnRmb", "aimVisibility"].forEach((key) => { merged[key] = old[key]; });
    S.config.crosshair = merged;
    S.config.crosshair.activePreset = preset.id;
    send("UpdateCrosshair", { crosshair: S.config.crosshair });
    renderAim(); syncControls(); refreshPreview();
  } else if (module === "display") {
    // Presets own the seven calibration fields, never the panel type.
    S.config.display.global = S.config.display.global || {};
    DISPLAY_PRESET_FIELDS.forEach((key) => { S.config.display.global[key] = preset.values[key]; });
    if (S.config.display.customizeEach) {
      const monitor = displayMonitor();
      if (monitor) {
        const values = S.config.display.monitors[monitor.id] || clone(S.config.display.global);
        DISPLAY_PRESET_FIELDS.forEach((key) => { values[key] = preset.values[key]; });
        S.config.display.monitors[monitor.id] = values;
      }
    }
    S.config.display.activePreset = preset.id;
    S.config.display.enabled = true;
    send("UpdateDisplay", { display: S.config.display });
    renderDisplay(); syncControls(); renderHeader(); drawLut();
  } else {
    S.config.audio.bands = clone(preset.bands);
    S.config.audio.bass = preset.bass;
    S.config.audio.treble = preset.treble;
    S.config.audio.outputLevel = preset.outputLevel;
    S.config.audio.echo = preset.echo;
    S.config.audio.surround = preset.surround ?? 0;
    S.config.audio.dynamicBoost = preset.dynamicBoost ?? 0;
    S.config.audio.volumeLeveling = preset.volumeLeveling ?? 0;
    S.config.audio.balance = preset.balance ?? 0;
    S.config.audio.filterQ = preset.filterQ ?? 1;
    S.config.audio.volumeGuard = preset.volumeGuard;
    S.config.audio.activePreset = preset.id;
    send("UpdateSound", { audio: S.config.audio });
    renderAudio(); syncControls();
  }
  closePresetMenus();
}

function updateScene() {
  const src = S.day ? "/assets/scene_day.png" : "/assets/scene_night.png";
  const scene = $("#sceneImage");
  if (scene && !scene.src.endsWith(src)) scene.src = src;
  const img = new Image();
  img.onload = () => { S.sceneImage = img; drawLut(); };
  img.src = src;
}

function drawLut() {
  const canvas = $("#lutCanvas");
  if (!canvas || !S.sceneImage) return;
  const ctx = canvas.getContext("2d");
  const width = canvas.width;
  const height = canvas.height;
  ctx.clearRect(0, 0, width, height);
  ctx.drawImage(S.sceneImage, 0, 0, width, height);
  mirrorLutCanvas(canvas);
  if (!S.lut) return;
  const data = ctx.getImageData(0, 0, width, height);
  const r = S.lut.r || [];
  const g = S.lut.g || [];
  const b = S.lut.b || [];
  for (let i = 0; i < data.data.length; i += 4) {
    data.data[i] = r[data.data[i]] ?? data.data[i];
    data.data[i + 1] = g[data.data[i + 1]] ?? data.data[i + 1];
    data.data[i + 2] = b[data.data[i + 2]] ?? data.data[i + 2];
  }
  ctx.putImageData(data, 0, 0);
  mirrorLutCanvas(canvas);
}

function mirrorLutCanvas(canvas) {
  const image = $("#lutPreview");
  if (!image || !canvas.toBlob) return;
  const token = ++S.lutMirrorToken;
  canvas.toBlob((blob) => {
    if (!blob || token !== S.lutMirrorToken) return;
    if (S.lutObjectUrl) URL.revokeObjectURL(S.lutObjectUrl);
    S.lutObjectUrl = URL.createObjectURL(blob);
    image.src = S.lutObjectUrl;
  }, "image/png");
}

// Monotone cubic (PCHIP) interpolation in log-frequency. Linear interpolation
// leaves visible corners; PCHIP follows the faders without overshooting them.
function pchipAt(points, freq) {
  if (!points || !points.length) return 0;
  const pts = points
    .map(([f, g]) => [Math.log(Math.max(1, Number(f))), Number(g)])
    .filter(([x, g]) => Number.isFinite(x) && Number.isFinite(g))
    .sort((a, b) => a[0] - b[0]);
  if (!pts.length) return 0;
  if (pts.length === 1) return pts[0][1];
  const x = Math.log(Math.max(1, freq));
  const n = pts.length;
  const h = [];
  const delta = [];
  for (let i = 0; i < n - 1; i++) {
    h[i] = Math.max(1e-6, pts[i + 1][0] - pts[i][0]);
    delta[i] = (pts[i + 1][1] - pts[i][1]) / h[i];
  }
  const d = new Array(n).fill(0);
  d[0] = delta[0];
  d[n - 1] = delta[n - 2];
  for (let i = 1; i < n - 1; i++) {
    if (delta[i - 1] * delta[i] <= 0) {
      d[i] = 0;
    } else {
      const w1 = 2 * h[i] + h[i - 1];
      const w2 = h[i] + 2 * h[i - 1];
      d[i] = (w1 + w2) / (w1 / delta[i - 1] + w2 / delta[i]);
    }
  }
  // Evaluate the first/last segment beyond the fader range too. This keeps the
  // endpoint value and slope continuous, so the curve reaches the preview edge
  // without a flat 20–31 Hz or 16–20 kHz shelf.
  let i = 0;
  if (x < pts[0][0]) {
    i = 0;
  } else if (x > pts[n - 1][0]) {
    i = n - 2;
  } else {
    while (i < n - 2 && x > pts[i + 1][0]) i++;
  }
  const t = (x - pts[i][0]) / h[i];
  const t2 = t * t;
  const t3 = t2 * t;
  return (2 * t3 - 3 * t2 + 1) * pts[i][1]
    + (t3 - 2 * t2 + t) * h[i] * d[i]
    + (-2 * t3 + 3 * t2) * pts[i + 1][1]
    + (t3 - t2) * h[i] * d[i + 1];
}

function manualPoints(audio) {
  return FREQS.map((freq, i) => [freq, Number(audio.bands?.[i] || 0)]);
}

function curveAt(audio, freq) {
  let db = pchipAt(manualPoints(audio), freq);
  if (audio.imported) {
    const imported = (audio.imported.bands || []).map((band) => [band.freq, band.gain]);
    db += pchipAt(imported, freq) + Number(audio.imported.preamp || 0);
  }
  return Math.max(-12, Math.min(12, db));
}

function drawEQ() {
  const canvas = $("#eqCanvas");
  if (!canvas || !S.config) return;
  const ctx = canvas.getContext("2d");
  const w = canvas.width, h = canvas.height;
  const audio = S.config.audio;
  const accent = getComputedStyle(document.body).getPropertyValue("--accent").trim() || "#5CE693";
  ctx.clearRect(0, 0, w, h);
  const points = 220;
  const coords = [];
  for (let i = 0; i <= points; i++) {
    const f = 20 * Math.pow(1000, i / points);
    const db = curveAt(audio, f);
    coords.push([(i / points) * w, (12 - db) / 24 * h]);
  }
  ctx.beginPath();
  ctx.moveTo(coords[0][0], h);
  coords.forEach(([x, y]) => ctx.lineTo(x, y));
  ctx.lineTo(w, h);
  ctx.closePath();
  ctx.globalAlpha = 0.08;
  ctx.fillStyle = accent;
  ctx.fill();
  ctx.globalAlpha = 1;
  ctx.beginPath();
  coords.forEach(([x, y], index) => index === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y));
  ctx.strokeStyle = accent;
  const cssWidth = canvas.getBoundingClientRect().width || w;
  const backingScale = w / Math.max(1, cssWidth);
  ctx.lineWidth = 4 * backingScale;
  window.__crosshairEqStroke = 4;
  ctx.lineJoin = "round";
  ctx.lineCap = "round";
  ctx.stroke();
}

function openSettings() {
  $("#settingsModal").classList.remove("hidden");
  renderSettingsValues();
}
function closeSettings() { $("#settingsModal").classList.add("hidden"); }
function renderSettingsValues() {
  if (!S.config) return;
  const s = S.config.settings;
  const setHotkey = (id, value) => {
    const field = $(`#${id}`);
    if (field) field.textContent = value || "Set key";
  };
  setHotkey("setAimHotkey", s.aimHotkey);
  setHotkey("setDisplayHotkey", s.displayHotkey);
  setHotkey("setAudioHotkey", s.audioHotkey);
  setHotkey("setRestoreHotkey", s.restoreHotkey);
  setHotkey("setCycleHotkey", s.cycleHotkey);
  $("#setStartWindows").checked = !!s.startWithWindows;
  $("#setStartMinimized").checked = !!s.startMinimized;
  $("#setMinimizeTray").checked = !!s.minimizeToTray;
  $("#setRestoreDisplay").checked = !!s.restoreDisplayOnExit;
  $("#setHotkeys").checked = !!s.hotkeysEnabled;
  $("#setAutoRevert").checked = !!s.autoRevertSlots;
  $("#setOsdToast").checked = s.osdToast !== false;
  $("#versionLabel").textContent = `v${S.appVersion || "1.0.0"}`;
}
function saveSettings() {
  if (!S.config) return;
  const settings = clone(S.config.settings);
  settings.startWithWindows = $("#setStartWindows").checked;
  settings.startMinimized = $("#setStartMinimized").checked;
  settings.minimizeToTray = $("#setMinimizeTray").checked;
  settings.restoreDisplayOnExit = $("#setRestoreDisplay").checked;
  settings.hotkeysEnabled = $("#setHotkeys").checked;
  settings.autoRevertSlots = $("#setAutoRevert").checked;
  settings.osdToast = $("#setOsdToast").checked;
  settings.previewDay = S.config.settings.previewDay !== false;
  S.config.settings = settings;
  send("SettingsChanged", { settings });
  closeSettings();
}

function setCompareActive(active) {
  const next = !!active;
  if (S.compareActive === next) return;
  S.compareActive = next;
  const button = $("#compareDisplay");
  if (button) {
    button.classList.toggle("active", next);
    button.setAttribute("aria-pressed", String(next));
  }
  $$("#view-display input[type=range]").forEach((el) => { el.disabled = next; });
  send("CompareDisplay", { active: next });
}

function updateDisplayValue(field, value) {
  const values = displayValues();
  if (!values) return;
  values[field] = value;
  if (field === "warmth" && value > 0) S.lastWarmth = value;
  S.config.display.activePreset = null;
  scheduleSend("display");
  const readout = $(`[data-display-readout="${field}"]`);
  if (readout) readout.textContent = field === "gamma" ? Number(value).toFixed(2) : String(Math.round(value * 10) / 10);
  const el = document.querySelector(`[data-display="${field}"]`);
  if (el) setRangeFill(el);
}

function renderAll() {
  if (!S.config) return;
  S.day = S.config.settings.previewDay !== false;
  document.body.dataset.tab = S.tab;
  renderHeader();
  renderAim();
  renderDisplay();
  renderAudio();
  renderSlots();
  syncControls();
  updateScene();
  drawLut();
  drawEQ();
  renderSettingsValues();
}

function closeModals(except) {
  ["presetModal", "emojiModal", "importModal"].forEach((id) => {
    if (id !== except) $(`#${id}`)?.classList.add("hidden");
  });
}

// Installed games come from Steam, Epic and Xbox folders (a worker thread in
// Rust). Their own modal keeps a 200-title list usable instead of clipping a
// dropdown that also has to hold running apps.
function renderGamesPicker() {
  const list = $("#gamesList");
  if (!list) return;
  const games = S.installedGames || [];
  const hint = $("#gamesHint");
  if (hint) {
    hint.textContent = "Games found in Steam, Epic and Xbox install folders. Launchers that only run through their store, like Minecraft, appear under Running now while they are open.";
  }
  const status = $("#gamesStatus");
  if (status) status.textContent = games.length ? `${games.length} game${games.length === 1 ? "" : "s"}` : "";
  if (!games.length) {
    list.innerHTML = `<div class="slot-empty"><strong>${S.gamesScanning ? "Scanning…" : (S.gamesTimedOut ? "Scan took too long" : "No games found")}</strong><span>${S.gamesScanning ? "Reading Steam, Epic and Xbox folders." : (S.gamesTimedOut ? "A library folder did not answer in time. Use Rescan, or browse to the game's .exe." : "Use Rescan, or browse to the game's .exe.")}</span></div>`;
    return;
  }
  list.innerHTML = games.map((game) => {
    const active = S.editing?.targetExe === game.exe;
    const icon = game.icon
      ? `<img class="app-icon" src="${esc(game.icon)}" alt="">`
      : `<span class="app-icon placeholder">🎮</span>`;
    return `<button class="custom-option ${active ? "active" : ""}" data-action="pick-game" data-value="${esc(game.exe)}" role="option" aria-selected="${active}">${icon}<span class="app-copy"><span class="app-title">${esc(game.name)}</span><span class="app-exe">${esc(game.source)} · ${esc(game.exe)}</span></span>${active ? '<span class="check">✓</span>' : ""}</button>`;
  }).join("");
}

document.addEventListener("input", (event) => {
  const el = event.target;
  if (el.matches("[data-bind]")) handleBind(el);
  if (el.matches("[data-display]")) updateDisplayValue(el.dataset.display, Number(el.value));
  if (el.matches("[data-band]")) {
    const index = Number(el.dataset.band);
    S.config.audio.bands[index] = Number(el.value);
    S.config.audio.activePreset = null;
    setRangeFill(el);
    drawEQ();
    scheduleSend("audio");
  }
  if (el.id === "backlightRange") {
    const monitor = displayMonitor();
    if (!monitor) return;
    S.config.display.backlight = S.config.display.backlight || {};
    S.config.display.backlight[monitor.id] = Number(el.value);
    const out = $("#backlightReadout");
    if (out) out.textContent = el.value;
    scheduleSend("display");
  }
});

document.addEventListener("change", (event) => {
  const el = event.target;
  if (el.matches("[data-bind]") && (el.type === "checkbox" || el.tagName === "SELECT")) handleBind(el);
  if (el.id === "panelSelect") {
    const values = displayValues();
    if (!values) return;
    values.panel = el.value;
    S.config.display.activePreset = null;
    send("UpdateDisplay", { display: S.config.display });
    renderDisplay();
  }
});

document.addEventListener("click", (event) => {
  const el = event.target.closest("[data-action]");
  const clickedToolbar = event.target.closest(".preset-toolbar");
  if (!clickedToolbar && !event.target.closest(".preset-menu")) closePresetMenus();
  if (!event.target.closest(".custom-select")) closeCustomSelects();
  if (!el || el.tagName === "INPUT") return;
  const action = el.dataset.action;
  switch (action) {
    case "confirm-ok": settleConfirm(true); break;
    case "confirm-cancel": settleConfirm(false); break;
    case "tab": setTab(el.dataset.tab); break;
    case "open-slots": setTab("slots"); break;
    case "settings": openSettings(); break;
    case "close-settings": closeSettings(); break;
    case "save-settings": saveSettings(); break;
    case "day-night": {
      S.day = el.dataset.value === "day";
      S.config.settings.previewDay = S.day;
      send("SettingsChanged", { settings: S.config.settings });
      updateScene();
      renderAim(); renderDisplay();
      break;
    }
    case "aim-visibility": {
      const visibility = el.dataset.value;
      if (!["always", "hide", "only"].includes(visibility)) break;
      S.config.crosshair.aimVisibility = visibility;
      S.config.crosshair.hideOnRmb = visibility === "hide";
      S.config.crosshair.activePreset = null;
      send("UpdateCrosshair", { crosshair: S.config.crosshair });
      renderAim(); refreshPreview();
      break;
    }
    case "blue-light": {
      const values = displayValues();
      if (!values) break;
      const current = Number(values.warmth || 0);
      if (current > 0) S.lastWarmth = current;
      values.warmth = current > 0 ? 0 : (Number(S.lastWarmth) > 0 ? Number(S.lastWarmth) : 40);
      S.config.display.activePreset = null;
      send("UpdateDisplay", { display: S.config.display });
      renderDisplay();
      break;
    }
    case "open-preset": openPresetMenu(el.dataset.module); break;
    case "apply-preset": applyPreset(el.dataset.module, el.dataset.id); break;
    case "save-preset":
      S.savePresetModule = el.dataset.module;
      $("#presetModalTitle").textContent = `Save ${el.dataset.module === "crosshair" ? "shape" : el.dataset.module} preset`;
      $("#presetName").value = "";
      $("#presetModal").classList.remove("hidden");
      $("#presetName").focus();
      break;
    case "close-preset-modal": $("#presetModal").classList.add("hidden"); break;
    case "submit-preset": {
      const name = $("#presetName").value.trim();
      if (!name) { showToast("Give the preset a name.", true); return; }
      send("SavePreset", { module: S.savePresetModule, name });
      $("#presetModal").classList.add("hidden");
      break;
    }
    case "delete-active-preset": {
      const module = el.dataset.module;
      const preset = currentPreset(module);
      if (!preset || preset.builtin) { showToast("Built-in presets cannot be deleted.", true); return; }
      confirmThen({
        title: `Delete “${preset.name}”?`,
        message: "The preset is removed from the list. This cannot be undone.",
        confirm: "Delete preset",
        danger: true,
      }, () => send("DeletePreset", { module, id: preset.id }));
      break;
    }
    case "aim-mode":
      S.aimMode = el.dataset.value;
      renderAim();
      break;
    case "style":
      S.config.crosshair.style = el.dataset.value;
      S.config.crosshair.activePreset = null;
      send("UpdateCrosshair", { crosshair: S.config.crosshair });
      renderAim(); syncControls(); refreshPreview();
      break;
    case "color":
      S.colorHsv = null;
      applyCustomColor(el.dataset.value);
      break;
    case "close-color": openColorPopover(false); break;
    case "image-mode": {
      const mode = el.dataset.value;
      S.config.crosshair.imageSolid = mode === "solid";
      S.config.crosshair.imageTint = mode === "tint";
      S.config.crosshair.activePreset = null;
      send("UpdateCrosshair", { crosshair: S.config.crosshair });
      syncAimConditionals(); syncControls(); refreshPreview();
      break;
    }
    case "import-image": send("ImportImage", { path: null }); break;
    case "remove-image":
      S.config.crosshair.image = null;
      S.config.crosshair.imageName = null;
      send("RemoveImage", {});
      break;
    case "display-mode":
      S.config.display.mode = el.dataset.value;
      send("UpdateDisplay", { display: S.config.display });
      renderDisplay(); syncControls();
      break;
    case "select-monitor":
      S.config.display.selectedMonitor = el.dataset.id;
      send("UpdateDisplay", { display: S.config.display });
      renderDisplay(); syncControls();
      break;
    case "reset-display": send("ResetDisplay", {}); break;
    case "unstick-audio": send("UnstickAudio", {}); break;
    case "open-windows-display":
    case "fix-display": send("OpenWindowsDisplaySettings", {}); break;
    case "setup-audio": send("SetupAudio", {}); break;
    case "open-fx-update": openFxModal(S.fx?.updateAvailable ? "update" : "setup"); break;
    case "repair-audio": openFxModal("repair"); break;
    case "repair-run":
      S.fxRepairArmed = true;
      send("RepairAudio", {});
      break;
    case "open-games":
      closeCustomSelects();
      $("#gamesModal")?.classList.remove("hidden");
      renderGamesPicker();
      if (!(S.installedGames || []).length && !S.gamesScanning) {
        S.gamesScanning = true;
        S.gamesTimedOut = false;
        send("ScanGames", {});
        renderGamesPicker();
      }
      break;
    case "close-games": $("#gamesModal")?.classList.add("hidden"); break;
    case "scan-games":
      S.gamesScanning = true;
      S.gamesTimedOut = false;
      send("ScanGames", {});
      renderGamesPicker();
      renderSlotEditor();
      break;
    case "pick-game": {
      if (!S.editing) return;
      S.editing.targetExe = el.dataset.value || null;
      markSlotDirty();
      $("#gamesModal")?.classList.add("hidden");
      renderSlotEditor();
      break;
    }
    case "close-fx-modal": closeFxModal(); break;
    case "open-fx-download": send("OpenFxSoundDownload", {}); break;
    case "step": {
      const input = document.querySelector(`[data-bind="${el.dataset.target}"]`);
      if (!input) return;
      const min = Number(input.min || -300);
      const max = Number(input.max || 300);
      const next = Math.max(min, Math.min(max, Number(input.value || 0) + Number(el.dataset.step || 1)));
      input.value = String(next);
      handleBind(input);
      break;
    }
    case "toggle-select": {
      const key = el.dataset.select;
      const willOpen = S.customSelect !== key;
      closeCustomSelects();
      if (willOpen) {
        S.customSelect = key;
        el.setAttribute("aria-expanded", "true");
        el.closest(".custom-select")?.querySelector(".custom-menu")?.classList.remove("hidden");
      }
      break;
    }
    case "custom-pick": {
      if (!S.editing) return;
      const select = el.dataset.select;
      const value = el.dataset.value || "";
      if (select.startsWith("module:")) {
        const key = select.slice(7);
        S.editing[key] = S.editing[key] || { on: false, preset: null };
        S.editing[key].preset = value || null;
        markSlotDirty();
      } else if (select === "target") {
        if (value === "__browse__") {
          S.awaitingBrowseSlot = S.editing.id;
          send("BrowseSlotExe", { slotId: S.editing.id });
        } else {
          S.editing.targetExe = value || null;
          markSlotDirty();
        }
      }
      closeCustomSelects();
      renderSlotEditor();
      break;
    }
    case "reset-audio": {
      S.config.audio.bands = Array(10).fill(0);
      S.config.audio.bass = 0;
      S.config.audio.treble = 0;
      S.config.audio.echo = 0;
      S.config.audio.surround = 0;
      S.config.audio.dynamicBoost = 0;
      S.config.audio.outputLevel = 0;
      S.config.audio.volumeLeveling = 0;
      S.config.audio.balance = 0;
      S.config.audio.filterQ = 1;
      S.config.audio.volumeGuard = true;
      S.config.audio.activePreset = null;
      send("UpdateSound", { audio: S.config.audio });
      renderAudio(); syncControls();
      break;
    }
    case "open-import": $("#importModal").classList.remove("hidden"); break;
    case "close-import-modal": $("#importModal").classList.add("hidden"); break;
    case "import-eq-file":
      $("#importModal").classList.add("hidden");
      send("ImportEq", { path: null, text: null });
      break;
    case "submit-eq-text": {
      const text = $("#eqText").value;
      if (text.trim()) send("ImportEq", { path: null, text, name: "Pasted profile" });
      $("#importModal").classList.add("hidden");
      break;
    }
    case "flatten-eq":
      confirmThen({
        title: "Apply imported curve to the ten sliders?",
        message: "This is a 10-band approximation and removes the imported profile layer.",
        confirm: "Apply to sliders",
      }, () => send("FlattenEqImport", {}));
      break;
    case "clear-eq": send("ClearEqImport", {}); break;
    case "open-data": send("OpenDataFolder", {}); break;
    case "open-backup":
      S.backupPreview = null;
      S.backupStatus = "";
      renderBackupSummary(null);
      const backupStatus = $("#backupStatus");
      if (backupStatus) backupStatus.textContent = "";
      renderBackupUndo();
      $("#backupModal")?.classList.remove("hidden");
      send("BackupModalOpen", { open: true });
      break;
    case "close-backup-modal": closeBackupModal(); break;
    case "backup-create": send("CreateBackup", {}); break;
    case "backup-browse": send("BrowseBackup", {}); break;
    case "backup-restore":
      if (S.backupPreview) {
        confirmThen({
          title: "Restore this backup?",
          message: "Current custom presets, slots, imported crosshair images and the imported EQ profile will be replaced. General settings and current display/audio calibration stay as they are.",
          confirm: "Restore backup",
          danger: true,
        }, () => send("RestoreBackup", {}));
      }
      break;
    case "backup-undo":
      confirmThen({
        title: "Undo the last restore?",
        message: "The presets, slots, images and EQ profile from before that restore come back, replacing what the restore brought in.",
        confirm: "Undo restore",
        danger: true,
      }, () => send("UndoRestore", {}));
      break;
    case "open-log": send("OpenLog", {}); break;
    case "repair-audio": send("RepairAudio", {}); break;
    case "open-fxsound": send("OpenFxSound", {}); break;
    case "factory-reset":
      confirmThen({
        title: "Reset everything to a clean install?",
        message: "This wipes settings, slots, presets and imported images. The diagnostics log and the FxSound safety backup are kept.",
        confirm: "Factory reset",
        danger: true,
      }, () => send("FactoryReset", {}));
      break;
    case "new-slot": {
      flushSlotSave();
      const used = new Set(S.config.slots.list.map((s) => s.emoji));
      S.editing = {
        id: `slot-${Date.now()}`, name: "New Slot", emoji: EMOJIS.find((e) => !used.has(e)) || "🎯",
        hotkey: null, targetExe: null,
        crosshair: { on: false, preset: null },
        display: { on: false, preset: null },
        audio: { on: false, preset: null },
      };
      S.selectedSlot = S.editing.id;
      S.slotEditorFocused = false;
      S.editingDirty = true;
      renderSlots();
      renderSlotEditor();
      markSlotDirty();
      break;
    }
    case "select-slot": selectSlot(el.dataset.id, true); break;
    case "open-emoji":
      if (!S.editing) return;
      S.emojiSlot = S.editing.id;
      renderEmojiPicker(S.editing);
      $("#emojiModal").classList.remove("hidden");
      break;
    case "close-emoji-modal": $("#emojiModal").classList.add("hidden"); break;
    case "select-emoji":
      if (!S.editing || el.disabled) return;
      S.editing.emoji = el.dataset.value;
      markSlotDirty();
      $("#emojiModal").classList.add("hidden");
      renderSlots();
      renderSlotEditor();
      break;
    case "delete-slot":
      if (S.editing) {
        confirmThen({
          title: "Delete this slot?",
          message: "The slot, its hotkey and its game target are removed.",
          confirm: "Delete slot",
          danger: true,
        }, () => {
          const id = S.editing.id;
          S.editing = null;
          S.selectedSlot = null;
          send("DeleteSlot", { slotId: id });
        });
      }
      break;
    default: break;
  }
});

$("#settingsModal")?.addEventListener("click", (event) => {
  if (event.target.classList.contains("modal-backdrop")) closeSettings();
});
$("#presetModal")?.addEventListener("click", (event) => {
  if (event.target.classList.contains("modal-backdrop")) $("#presetModal").classList.add("hidden");
});
$("#emojiModal")?.addEventListener("click", (event) => {
  if (event.target.classList.contains("modal-backdrop")) $("#emojiModal").classList.add("hidden");
});
$("#importModal")?.addEventListener("click", (event) => {
  if (event.target.classList.contains("modal-backdrop")) $("#importModal").classList.add("hidden");
});
$("#backupModal")?.addEventListener("click", (event) => {
  if (event.target.classList.contains("modal-backdrop")) closeBackupModal();
});

document.querySelectorAll("input[data-action=master]").forEach((el) => {
  el.addEventListener("change", () => send("ToggleMaster", { module: el.dataset.module }));
});
document.querySelectorAll("button.quick-toggle").forEach((el) => {
  el.addEventListener("click", () => send("ToggleMaster", { module: el.dataset.module }));
});
$("#audioDevice")?.addEventListener("change", (event) => {
  S.config.audio.device = event.target.value || null;
  send("UpdateSound", { audio: S.config.audio });
});
$("#toast")?.addEventListener("click", (event) => {
  const button = event.target.closest?.("[data-toast-action]");
  if (!button) return;
  const action = button.dataset.toastAction;
  if (action === "repair-managed-preset") send("RepairManagedPreset", {});
  else if (action === "open-fxsound-presets") send("OpenFxSoundPresets", {});
  else if (action === "use-default-output") {
    S.config.audio.device = null;
    send("UpdateSound", { audio: S.config.audio });
  } else if (action === "choose-output") {
    setTab("audio");
    $("#audioDevice")?.focus();
  } else if (action === "unstick-audio") {
    send("UnstickAudio", {});
  }
  hideToast();
});
$("#colorTrigger")?.addEventListener("click", (event) => {
  event.stopPropagation();
  openColorPopover($("#colorPopover")?.classList.contains("hidden"));
});
$("#colorSv")?.addEventListener("pointerdown", (event) => {
  event.preventDefault();
  event.target.setPointerCapture?.(event.pointerId);
  handleColorSv(event);
  const move = (moveEvent) => handleColorSv(moveEvent);
  const up = () => window.removeEventListener("pointermove", move);
  window.addEventListener("pointermove", move);
  window.addEventListener("pointerup", up, { once: true });
});
$("#colorHue")?.addEventListener("input", (event) => {
  const rgb = hexToRgb(colorHexValue()) || { r: 78, g: 168, b: 255 };
  const hsv = rgbToHsv(rgb);
  setHsv({ h: Number(event.target.value), s: hsv.s, v: hsv.v });
});
$("#colorHex")?.addEventListener("change", (event) => {
  const hex = hexToRgb(event.target.value);
  if (hex) {
    S.colorHsv = rgbToHsv(hex);
    applyCustomColor(rgbToHex(hex));
  } else {
    event.target.value = colorHexValue().toUpperCase();
    showToast("Use a hex colour like #4EA8FF.", true);
  }
});
document.addEventListener("pointerdown", (event) => {
  if (!$("#colorPopover")?.classList.contains("hidden") && !event.target.closest(".color-control")) openColorPopover(false);
});

// Hold to Compare is pointer-captured so dragging off the button and letting
// go still restores the calibration. pointercancel/lostpointercapture and a
// window blur cover the cases where the up event never reaches the button.
const compareButton = $("#compareDisplay");
compareButton?.addEventListener("pointerdown", (event) => {
  if (event.button !== 0) return;
  event.preventDefault();
  try {
    compareButton.setPointerCapture?.(event.pointerId);
  } catch (error) {
    // Synthetic pointer events (the UI self-test) have no active pointer.
  }
  setCompareActive(true);
});
["pointerup", "pointercancel", "lostpointercapture"].forEach((type) => {
  compareButton?.addEventListener(type, () => setCompareActive(false));
});
window.addEventListener("blur", () => setCompareActive(false));
$("#dropzone")?.addEventListener("keydown", (event) => {
  if (event.key === "Enter" || event.key === " ") { event.preventDefault(); send("ImportImage", { path: null }); }
});
$$("[data-module-hotkey]").forEach((field) => {
  field.addEventListener("focus", () => {
    if (!field.textContent || field.textContent === "Set key") field.textContent = "Press key…";
  });
  field.addEventListener("blur", () => {
    if (field.textContent === "Press key…") field.textContent = "Set key";
  });
});
$("#slotEditor")?.addEventListener("focusin", () => { S.slotEditorFocused = true; });
$("#slotEditor")?.addEventListener("focusout", (event) => {
  if (!$("#slotEditor").contains(event.relatedTarget)) {
    // Do not send SaveSlot here. A click on a slot sends one ActivateSlot
    // transaction (with the edited card included), and the debounce still
    // saves ordinary focus changes.
    S.slotEditorFocused = false;
  }
});
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && !$("#confirmModal")?.classList.contains("hidden")) {
    event.preventDefault();
    settleConfirm(false);
    return;
  }
  const field = event.target.closest?.("[data-module-hotkey]");
  if (field) {
    event.preventDefault();
    event.stopPropagation();
    if (event.key === "Escape") { field.blur(); return; }
    const module = field.dataset.moduleHotkey;
    if (event.key === "Backspace" || event.key === "Delete") {
      field.textContent = "Set key";
      send("ClearModuleHotkey", { module });
      return;
    }
    if (["Control", "Shift", "Alt", "Meta"].includes(event.key)) return;
    const parts = [];
    if (event.ctrlKey) parts.push("Ctrl");
    if (event.shiftKey) parts.push("Shift");
    if (event.altKey) parts.push("Alt");
    if (event.metaKey) parts.push("Win");
    let key = event.key;
    if (key.length === 1) key = key.toUpperCase();
    else if (key === " ") key = "Space";
    else if (key.startsWith("Arrow")) key = key.slice(5);
    else key = key.charAt(0).toUpperCase() + key.slice(1);
    parts.push(key);
    const value = parts.join("+");
    field.textContent = value;
    send("AssignModuleHotkey", { module, hotkey: value });
    return;
  }
  if (event.key === "Escape") {
    if (!$("#presetModal").classList.contains("hidden")) $("#presetModal").classList.add("hidden");
    else if (!$("#emojiModal").classList.contains("hidden")) $("#emojiModal").classList.add("hidden");
    else if (!$("#importModal").classList.contains("hidden")) $("#importModal").classList.add("hidden");
    else if (!$("#backupModal").classList.contains("hidden")) closeBackupModal();
    else if (!$("#fxModal").classList.contains("hidden")) closeFxModal();
    else if (!$("#gamesModal").classList.contains("hidden")) $("#gamesModal").classList.add("hidden");
    else if (!$("#colorPopover")?.classList.contains("hidden")) openColorPopover(false);
    else if (S.customSelect || S.presetMenu) { closeCustomSelects(); closePresetMenus(); }
    else if (!$("#settingsModal").classList.contains("hidden")) closeSettings();
  }
});

function handleMessage(message) {
  if (!message || message.v !== 1) return;
  const type = message.type;
  const payload = message.payload || {};
  if (type === "FullState") {
    clearSlotSwitch();
    S.config = payload.config;
    S.monitors = payload.monitors || [];
    S.fx = payload.fxSound;
    S.devices = payload.audioDevices || [];
    S.runningApps = payload.runningApps || [];
    S.installedGames = payload.installedGames || [];
    // Scanning and its timeout are owned by the InstalledGames event, which
    // now reports every scan outcome; a FullState must not mask it.
    if (!$("#gamesModal")?.classList.contains("hidden")) renderGamesPicker();
    S.crosshairPresets = payload.crosshairPresets || [];
    S.displayPresets = payload.displayPresets || [];
    S.audioPresets = payload.audioPresets || [];
    S.selectedSlot = payload.selectedSlot;
    S.appVersion = payload.appVersion || "1.0.0";
    S.canUndoRestore = !!payload.canUndoRestore;
    renderBackupUndo();
    S.ready = true;
    if (S.awaitingBrowseSlot) {
      const updated = S.config.slots.list.find((s) => s.id === S.awaitingBrowseSlot);
      if (updated && S.editing?.id === updated.id) {
        S.editing.targetExe = updated.targetExe;
        const line = document.querySelector("#slotEditor .slot-target-line span");
        if (line) line.textContent = updated.targetExe || "No game target selected.";
      }
      S.awaitingBrowseSlot = null;
    }
    if (S.slotSavePending && S.editing) {
      const saved = S.config.slots.list.find((s) => s.id === S.editing.id);
      if (saved) {
        S.slotSavePending = false;
        S.editingDirty = false;
        if (!S.slotEditorFocused) S.editing = clone(saved);
      }
    }
    if (S.editing && !S.editingDirty && S.config.slots.list.some((s) => s.id === S.editing.id)) {
      S.editing = clone(S.config.slots.list.find((s) => s.id === S.editing.id));
    }
    if (S.editing && !S.slotSavePending && !S.config.slots.list.some((s) => s.id === S.editing.id)) {
      S.editing = null;
    }
    renderAll();
    refreshPreview(true);
    window.__crosshairReady = true;
  } else if (type === "PreviewTick") {
    S.previewTick = payload.v || 0;
    paintPreview();
  } else if (type === "DisplayLut") {
    S.lut = { r: payload.r, g: payload.g, b: payload.b };
    S.applied = !!payload.applied;
    S.warning = payload.warning || null;
    drawLut();
    renderDisplay();
  } else if (type === "SoundMeter") {
    S.meterPeak = payload.peak || 0;
    S.meterHeadroom = payload.headroomDb ?? -60;
    updateMeter(S.meterPeak, S.meterHeadroom);
  } else if (type === "MonitorsUpdated") {
    S.monitors = payload.monitors || [];
    renderAim(); renderDisplay();
  } else if (type === "FxSoundStatus") {
    S.fx = payload;
    if (payload && /could not|failed/i.test(payload.message || "")) {
      showToast(payload.message, true);
    }
    renderAudio();
  } else if (type === "BackupPreview") {
    S.backupPreview = payload.preview;
    renderBackupSummary(payload.preview);
    send("BackupModalOpen", { open: true });
    $("#backupModal")?.classList.remove("hidden");
  } else if (type === "BackupResult") {
    if (payload.message) showToast(payload.message, !payload.ok);
    if (payload.ok && /restored|undone/i.test(payload.message || "")) {
      closeBackupModal();
    }
    const status = $("#backupStatus");
    if (status) status.textContent = payload.message || "";
    renderBackupSummary(S.backupPreview);
  } else if (type === "InstalledGames") {
    S.installedGames = payload.games || [];
    S.gamesScanning = false;
    S.gamesTimedOut = !!payload.timedOut;
    if (!$("#gamesModal")?.classList.contains("hidden")) renderGamesPicker();
    else if (S.tab === "slots") renderSlotEditor();
  } else if (type === "HotkeyConflict") {
    showToast(`Hotkey ${payload.key} is already used by ${payload.owner || "another app"}.`, true);
  } else if (type === "DiagnosticBanner") {
    if (payload.code === "slot") {
      S.slotSavePending = false;
      clearSlotSwitch();
      S.editingDirty = true;
    }
    if (payload.code === "hotkey") renderSettingsValues();
    // Install and reinstall notices reuse this channel; only failures are errors.
    const failed = payload.code !== "audio_notice" || /fail|could not|not installed|too old/i.test(payload.message || "");
    showToast(payload.message || payload.code || "Diagnostic warning", failed);
  } else if (type === "AudioRecovery") {
    const error = payload.code !== "audio_output_fallback";
    showToast(payload.message || "FxSound needs attention.", error, payload.actions || []);
  }
}

window.__crosshair_onMessage = handleMessage;
window.__crosshair_setTab = (tab) => setTab(tab);
window.__crosshairCurveAt = curveAt;
window.__crosshairFlushAudio = () => flushSend("audio");
window.__crosshairSend = send;
window.__crosshairState = S;
document.addEventListener("DOMContentLoaded", () => {
  updateScene();
  send("Ready", {});
});
window.addEventListener("beforeunload", () => {
  if (S.editing && S.editingDirty) {
    S.slotSavePending = true;
    send("SaveSlot", { slot: S.editing });
  }
});
