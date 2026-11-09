import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

const inputDeviceSelect = document.getElementById(
  "input-device-select",
) as HTMLSelectElement;
const inputChannelSelect = document.getElementById(
  "input-channel-select",
) as HTMLSelectElement;
const outputDeviceSelect = document.getElementById(
  "output-device-select",
) as HTMLSelectElement;
const outputChannelSelect = document.getElementById(
  "output-channel-select",
) as HTMLSelectElement;
const midiInSelect = document.getElementById(
  "midi-in-select",
) as HTMLSelectElement;
const midiOutSelect = document.getElementById(
  "midi-out-select",
) as HTMLSelectElement;
const profileSelect = document.getElementById(
  "profile-select",
) as HTMLSelectElement;
const profileHint = document.getElementById("profile-hint") as HTMLElement;
const simulateCheck = document.getElementById(
  "simulate-check",
) as HTMLInputElement;
const mscDeviceSelect = document.getElementById(
  "msc-device-select",
) as HTMLSelectElement;
const mscFormatSelect = document.getElementById(
  "msc-format-select",
) as HTMLSelectElement;
const txLevelInput = document.getElementById(
  "tx-level-input",
) as HTMLInputElement;
const linkKeyInput = document.getElementById(
  "link-key-input",
) as HTMLInputElement;
const mscAcceptSelect = document.getElementById(
  "msc-accept-select",
) as HTMLSelectElement;
const failsafeBurstCheck = document.getElementById(
  "failsafe-burst-check",
) as HTMLInputElement;
const linkWrongKeyEl = document.getElementById(
  "link-wrong-key",
) as HTMLElement;
const setupWarnings = document.getElementById(
  "setup-warnings",
) as HTMLUListElement;

const refreshBtn = document.getElementById(
  "refresh-devices-btn",
) as HTMLButtonElement;
const saveSettingsBtn = document.getElementById(
  "save-settings-btn",
) as HTMLButtonElement;
const selfTestBtn = document.getElementById(
  "self-test-btn",
) as HTMLButtonElement;
const selfTestResults = document.getElementById(
  "self-test-results",
) as HTMLElement;
const panicBtn = document.getElementById("panic-btn") as HTMLButtonElement;
const decoderPanicBtn = document.getElementById(
  "decoder-panic-btn",
) as HTMLButtonElement;
const txQueuedEl = document.getElementById("tx-queued") as HTMLElement;
const txDroppedEl = document.getElementById("tx-dropped") as HTMLElement;
const txQueueLatencyEl = document.getElementById(
  "tx-queue-latency",
) as HTMLElement;

const startListenBtn = document.getElementById(
  "start-listening-btn",
) as HTMLButtonElement;
const stopListenBtn = document.getElementById(
  "stop-listening-btn",
) as HTMLButtonElement;
const startEncodeBtn = document.getElementById(
  "start-encode-btn",
) as HTMLButtonElement;
const stopEncodeBtn = document.getElementById(
  "stop-encode-btn",
) as HTMLButtonElement;

const decodedDisplay = document.getElementById(
  "decoded-display",
) as HTMLDivElement;
const inputMeter = document.getElementById("input-meter") as HTMLDivElement;
const inputLevelReadout = document.getElementById(
  "input-level-readout",
) as HTMLElement;
const logContainer = document.getElementById("log-container") as HTMLDivElement;
const alarmCard = document.getElementById("alarm-card") as HTMLDivElement;
const alarmList = document.getElementById("alarm-list") as HTMLDivElement;
const ackAllBtn = document.getElementById("ack-all-btn") as HTMLButtonElement;
const decodeCard = document.getElementById("decode-card") as HTMLElement;
const encodeCard = document.getElementById("encode-card") as HTMLElement;

const linkLightEl = document.getElementById("link-light") as HTMLElement;
const linkLightDetailEl = document.getElementById(
  "link-light-detail",
) as HTMLElement;
const linkHintEl = document.getElementById("link-hint") as HTMLElement;
const linkStateEl = document.getElementById("link-state") as HTMLElement;
const linkLastPacketEl = document.getElementById(
  "link-last-packet",
) as HTMLElement;
const linkConfidenceEl = document.getElementById(
  "link-confidence",
) as HTMLElement;
const linkLatencyEl = document.getElementById("link-latency") as HTMLElement;
const linkCrcEl = document.getElementById("link-crc") as HTMLElement;
const linkCrcFailRateEl = document.getElementById(
  "link-crc-fail-rate",
) as HTMLElement;
const linkFecEl = document.getElementById("link-fec") as HTMLElement;
const linkFecRateEl = document.getElementById("link-fec-rate") as HTMLElement;
const linkLocksEl = document.getElementById("link-locks") as HTMLElement;
const linkSyncLostEl = document.getElementById(
  "link-sync-lost",
) as HTMLElement;
const linkDropsEl = document.getElementById("link-drops") as HTMLElement;
const linkSeqEl = document.getElementById("link-seq") as HTMLElement;
const linkLastEventEl = document.getElementById(
  "link-last-event",
) as HTMLElement;
const confidenceMeter = document.getElementById(
  "confidence-meter",
) as HTMLDivElement;
const confidenceGood = document.getElementById(
  "confidence-good",
) as HTMLDivElement;
const confidenceReadout = document.getElementById(
  "confidence-readout",
) as HTMLElement;

/** Devices are stored by name (they survive restarts and re-plugging). */
interface AppSettings {
  profile: string;
  mode: string;
  audio_in: string | null;
  audio_out: string | null;
  midi_in: string | null;
  midi_out: string | null;
  msc_device_id: number;
  msc_command_format: number;
  /** 1-based device channels. */
  audio_in_channel: number;
  audio_out_channel: number;
  /** Encoder transmit level, peak dBFS. */
  tx_level_db: number;
  /** Link key as typed (`0x1234` or a passphrase); empty = unkeyed. */
  link_key: string;
  /** Encoder MSC filter: device ID whose cues are carried (null = any). */
  msc_accept_device: number | null;
  /** Decoder: send the fail-safe burst on link-dead / Panic / stop. */
  fail_safe_burst: boolean;
}

/** One profile of the catalog (`get_profiles`). */
interface ProfileEntry {
  name: string;
  display: string;
  tier: "standard" | "advanced";
  is_default: boolean;
  coded: boolean;
  aliases: string[];
  line: string;
  processing: string;
  latency: string;
  use_when: string;
  airtime_ms: number;
  min_confidence_db: number;
}

const DEFAULT_PROFILE = "analog-safe";
const DEFAULT_TX_LEVEL_DB = -12;
/** Confidence (dB) from which a packet is comfortably decoded. */
const CONFIDENCE_GOOD_DB = 6;
const CONFIDENCE_SCALE_DB = 20;
/** Meter range in dBFS. */
const METER_MIN_DBFS = -60;
/** Below this the input is silence or noise, not a Cricket signal. */
const NO_SIGNAL_DBFS = -50;
/** Peak hold of the level readout. */
const LEVEL_HOLD_MS = 1500;
/** Window of the per-minute rates. */
const RATE_WINDOW_MS = 60_000;

let profiles: ProfileEntry[] = [];
/** The saved profile has been applied to the selector (once per session). */
let profileRestored = false;

const hex = (v: number) =>
  `0x${v.toString(16).toUpperCase().padStart(2, "0")}`;

/** Fill the MSC selects once: device IDs and the common command formats. */
function populateMscSelects() {
  if (mscDeviceSelect.options.length > 0) return;
  const add = (select: HTMLSelectElement, value: number, text: string) => {
    const o = document.createElement("option");
    o.value = String(value);
    o.textContent = text;
    select.appendChild(o);
  };
  add(mscDeviceSelect, 0x7f, "All-call (0x7F)");
  // The encoder filter: any device (no filter), or one device / group
  // (all-call cues always pass).
  const any = document.createElement("option");
  any.value = "";
  any.textContent = "Any device (no filter)";
  mscAcceptSelect.appendChild(any);
  for (let id = 0; id <= 0x6f; id++) {
    add(mscDeviceSelect, id, `${id} (${hex(id)})`);
    add(mscAcceptSelect, id, `${id} (${hex(id)}) + all-call`);
  }
  for (let g = 0x70; g <= 0x7e; g++) {
    add(mscDeviceSelect, g, `Group ${g - 0x6f} (${hex(g)})`);
    add(mscAcceptSelect, g, `Group ${g - 0x6f} (${hex(g)}) + all-call`);
  }
  const formats: [number, string][] = [
    [0x01, "Lighting (General)"],
    [0x02, "Moving Lights"],
    [0x10, "Sound (General)"],
    [0x20, "Machinery (General)"],
    [0x30, "Video (General)"],
    [0x40, "Projection (General)"],
    [0x50, "Process Control (General)"],
    [0x60, "Pyro (General)"],
    [0x7f, "All-types"],
  ];
  for (const [v, name] of formats) {
    add(mscFormatSelect, v, `${name} (${hex(v)})`);
  }
}

/** Select `value` (or `fallback`), adding an option for an unlisted value. */
function selectMsc(
  select: HTMLSelectElement,
  value: number | undefined,
  fallback: number,
) {
  const v = String(value ?? fallback);
  if (!Array.from(select.options).some((o) => o.value === v)) {
    const o = document.createElement("option");
    o.value = v;
    o.textContent = hex(Number(v));
    select.appendChild(o);
  }
  select.value = v;
}

function log(msg: string) {
  const p = document.createElement("div");
  p.textContent = `[${new Date().toLocaleTimeString()}] ${msg}`;
  logContainer.appendChild(p);
  logContainer.scrollTop = logContainer.scrollHeight;
}

// --- alarm list -------------------------------------------------------------

interface AlarmRow {
  key: string;
  kind: string;
  title: string;
  message: string;
  count: number;
  first: Date;
  last: Date;
  el: HTMLElement;
}

/** Active (not yet acknowledged or cleared) alarms by key. */
const alarmRows = new Map<string, AlarmRow>();

/** Kinds that are warnings about the setup rather than live faults. */
const WARNING_KINDS = new Set(["device_missing", "profile_unknown"]);

function alarmKey(kind: string, message: string): string {
  // A rejected message is about one message: different cues get their own
  // row. Every other kind is one condition with a counter.
  return kind === "message_rejected" ? `${kind}|${message}` : kind;
}

function renderAlarm(row: AlarmRow) {
  row.el.innerHTML = "";
  row.el.className = `alarm-row${WARNING_KINDS.has(row.kind) ? " warning" : ""}`;
  const time = document.createElement("span");
  time.className = "alarm-time";
  time.textContent =
    row.count > 1
      ? `${row.first.toLocaleTimeString()} – ${row.last.toLocaleTimeString()}`
      : row.first.toLocaleTimeString();
  const text = document.createElement("span");
  text.className = "alarm-text";
  const title = document.createElement("strong");
  title.textContent = row.title;
  text.appendChild(title);
  text.appendChild(document.createTextNode(` — ${row.message}`));
  const count = document.createElement("span");
  count.className = "alarm-count";
  count.textContent = row.count > 1 ? `×${row.count}` : "";
  const ack = document.createElement("button");
  ack.className = "small";
  ack.textContent = "Ack";
  ack.title = "Acknowledge: remove this alarm from the list";
  ack.addEventListener("click", () => ackAlarm(row.key));
  row.el.append(time, text, count, ack);
}

function updateAlarmCard() {
  alarmCard.classList.toggle("visible", alarmRows.size > 0);
}

/** Raise (or count up) an alarm. */
function raiseAlarm(kind: string, title: string, message: string) {
  const key = alarmKey(kind, message);
  const now = new Date();
  const existing = alarmRows.get(key);
  if (existing) {
    existing.count += 1;
    existing.last = now;
    existing.message = message;
    renderAlarm(existing);
  } else {
    const el = document.createElement("div");
    const row: AlarmRow = {
      key,
      kind,
      title,
      message,
      count: 1,
      first: now,
      last: now,
      el,
    };
    alarmRows.set(key, row);
    renderAlarm(row);
    alarmList.prepend(el);
  }
  updateAlarmCard();
}

/** The condition behind every alarm of `kind` has ended. */
function clearAlarmKind(kind: string) {
  for (const [key, row] of Array.from(alarmRows.entries())) {
    if (row.kind === kind) {
      row.el.remove();
      alarmRows.delete(key);
    }
  }
  updateAlarmCard();
}

function ackAlarm(key: string) {
  const row = alarmRows.get(key);
  if (!row) return;
  row.el.remove();
  alarmRows.delete(key);
  updateAlarmCard();
}

ackAllBtn.addEventListener("click", () => {
  for (const key of Array.from(alarmRows.keys())) ackAlarm(key);
});

/** Selected device name (option values are device names), or null. */
function selectedName(select: HTMLSelectElement): string | null {
  return select.value === "" ? null : select.value;
}

/** Label of the "no MIDI" entry of each MIDI select (value ""). */
const MIDI_IN_NONE = "None (heartbeats only)";
const MIDI_OUT_NONE = "None (no MIDI output)";

/**
 * Fill `select` with device names and select `preferred` by name. If the
 * preferred device is not connected, it stays selected as a marked
 * placeholder (starting with it fails in the engine) instead of silently
 * selecting another device; returns a description of it for the warning.
 */
function populateSelect(
  select: HTMLSelectElement,
  devices: string[],
  preferred: string | null,
  label: string,
  noneLabel?: string,
): string | null {
  select.innerHTML = "";
  // Optional first entry meaning "no device"; selected when nothing is preferred.
  if (noneLabel !== undefined) {
    const opt = document.createElement("option");
    opt.value = "";
    opt.text = noneLabel;
    select.add(opt);
  }
  devices.forEach((name) => {
    const opt = document.createElement("option");
    opt.value = name;
    opt.text = name;
    select.add(opt);
  });
  if (preferred && !devices.includes(preferred)) {
    const opt = document.createElement("option");
    opt.value = preferred;
    opt.text = `${preferred} (not connected)`;
    select.add(opt, noneLabel !== undefined ? 1 : 0);
    select.value = preferred;
    return `${label} "${preferred}"`;
  }
  if (select.options.length === 0) {
    const opt = document.createElement("option");
    opt.text = "No devices found";
    opt.value = "";
    select.add(opt);
    return null;
  }
  if (preferred) select.value = preferred;
  return null;
}

/**
 * Fill a channel select with 1..n (from the device's channel count) and
 * select `preferred` if the device has it, else channel 1.
 */
async function populateChannels(
  select: HTMLSelectElement,
  device: string | null,
  input: boolean,
  preferred: number,
) {
  let count = 1;
  try {
    count = await invoke<number>("get_device_channels", {
      name: device,
      input,
    });
  } catch (e) {
    // No device (or none connected): one channel to choose from.
    count = Math.max(1, preferred);
  }
  select.innerHTML = "";
  for (let ch = 1; ch <= Math.max(1, count); ch++) {
    const opt = document.createElement("option");
    opt.value = String(ch);
    opt.text = `${ch}`;
    select.add(opt);
  }
  select.value = String(preferred >= 1 && preferred <= count ? preferred : 1);
}

function selectedChannel(select: HTMLSelectElement): number {
  const v = Number(select.value);
  return Number.isFinite(v) && v >= 1 ? v : 1;
}

/** The encoder's MSC device filter: null = any device. */
function mscAcceptDevice(): number | null {
  return mscAcceptSelect.value === "" ? null : Number(mscAcceptSelect.value);
}

function txLevelDb(): number {
  const v = Number(txLevelInput.value);
  if (!Number.isFinite(v)) return DEFAULT_TX_LEVEL_DB;
  return Math.min(0, Math.max(-60, v));
}

function currentMode(): string {
  const el = document.querySelector(
    'input[name="mode"]:checked',
  ) as HTMLInputElement | null;
  return el?.value ?? "both";
}

function applyModeVisibility() {
  const mode = currentMode();
  decodeCard.style.display = mode === "encode" ? "none" : "";
  encodeCard.style.display = mode === "decode" ? "none" : "";
}

// --- profiles ---------------------------------------------------------------

/** The short latency figure of a catalog entry ("~0.3 s", "< 1 ms"). */
function shortLatency(p: ProfileEntry): string {
  const cut = p.latency.indexOf(" (");
  const short = cut > 0 ? p.latency.slice(0, cut) : p.latency;
  return short.startsWith("not measured")
    ? `${p.airtime_ms.toFixed(0)} ms airtime`
    : short;
}

/** Fill the profile selector from the catalog: standard tiers, then the
 * advanced group. Option texts carry the measured latency (or the airtime
 * where nothing was measured), never a first-copy airtime dressed up as
 * latency. */
async function loadProfiles() {
  profiles = await invoke<ProfileEntry[]>("get_profiles");
  profileSelect.innerHTML = "";
  const groups: [string, ProfileEntry["tier"]][] = [
    ["Standard (one per kind of line)", "standard"],
    ["Advanced: no error correction", "advanced"],
  ];
  for (const [label, tier] of groups) {
    const group = document.createElement("optgroup");
    group.label = label;
    for (const p of profiles.filter((p) => p.tier === tier)) {
      const opt = document.createElement("option");
      opt.value = p.name;
      opt.text = `${p.display}${p.is_default ? " (default)" : ""} · ${shortLatency(p)}`;
      group.appendChild(opt);
    }
    profileSelect.appendChild(group);
  }
}

function currentProfile(): ProfileEntry | undefined {
  return profiles.find((p) => p.name === profileSelect.value);
}

function capitalize(s: string): string {
  return s.length > 0 ? s[0].toUpperCase() + s.slice(1) : s;
}

/** One line of guidance for the selected profile. */
function refreshProfileHint() {
  const p = currentProfile();
  if (!p) {
    profileHint.textContent = "";
    return;
  }
  const also = p.aliases.length > 0 ? ` Also known as ${p.aliases.join(", ")}.` : "";
  profileHint.textContent =
    `${p.display}: ${capitalize(p.line)}. Processing on the bus: ${p.processing}. ` +
    `Latency ${p.latency}; airtime ${p.airtime_ms.toFixed(1)} ms per message. ` +
    `Use when ${p.use_when}. (Latencies are measured in-process with an ideal ` +
    `audio device; real interfaces add their own buffering.)${also}`;
  // The confidence meter's "good" window starts where the receiver accepts.
  const goodFrom = Math.max(p.min_confidence_db, CONFIDENCE_GOOD_DB);
  const left = Math.min(95, (goodFrom / CONFIDENCE_SCALE_DB) * 100);
  confidenceGood.style.left = `${left}%`;
  confidenceGood.style.width = `${100 - left}%`;
}

/**
 * Select the saved profile. Older files hold wire-format names
 * (`robust`, `ltc_fec`): the backend maps them. A name it does not know is
 * reported and replaced by the default, never by a guess.
 */
async function selectSavedProfile(saved: string) {
  const wanted = (saved || DEFAULT_PROFILE).trim();
  try {
    const info = await invoke<{ canonical: string }>("get_profile_info", {
      profile: wanted,
    });
    if (profiles.some((p) => p.name === info.canonical)) {
      profileSelect.value = info.canonical;
      clearAlarmKind("profile_unknown");
      return;
    }
  } catch (e) {
    // fall through: unknown
  }
  profileSelect.value = DEFAULT_PROFILE;
  const msg = `Saved profile "${wanted}" is not known to this version; Analog Safe is selected instead. Check that both ends use the same profile.`;
  log(`WARNING: ${msg}`);
  raiseAlarm("profile_unknown", "Unknown profile", msg);
}

// --- setup warnings ---------------------------------------------------------

async function refreshSetupWarnings() {
  const mode = currentMode();
  try {
    const warnings = await invoke<string[]>("check_setup", {
      profile: profileSelect.value,
      audioIn: selectedName(inputDeviceSelect),
      audioOut: selectedName(outputDeviceSelect),
      checkIn: mode !== "encode",
      checkOut: mode !== "decode",
    });
    setupWarnings.innerHTML = "";
    for (const w of warnings) {
      const li = document.createElement("li");
      li.textContent = `⚠ ${w}`;
      setupWarnings.appendChild(li);
    }
  } catch (e) {
    setupWarnings.innerHTML = "";
  }
}

function clearTxQueueDisplay() {
  txQueuedEl.textContent = "—";
  txQueueLatencyEl.textContent = "—";
}

async function loadDevices() {
  try {
    log("Loading devices...");
    const [audioIn, audioOut, midiIn, midiOut, settings] = await Promise.all([
      invoke<string[]>("get_input_devices"),
      invoke<string[]>("get_output_devices"),
      invoke<string[]>("get_midi_inputs"),
      invoke<string[]>("get_midi_outputs"),
      invoke<AppSettings>("get_settings"),
    ]);
    if (profiles.length === 0) await loadProfiles();

    // Keep the current selection on refresh; otherwise restore the saved one.
    const pick = (select: HTMLSelectElement, saved: string | null) =>
      selectedName(select) ?? saved;
    // MIDI selects: "None" (null) is a real choice once the select is filled;
    // a saved null (or no saved choice) restores to "None".
    const pickMidi = (select: HTMLSelectElement, saved: string | null) =>
      select.options.length > 0 ? selectedName(select) : saved;
    const missing = [
      populateSelect(
        inputDeviceSelect,
        audioIn,
        pick(inputDeviceSelect, settings.audio_in),
        "Audio in",
      ),
      populateSelect(
        outputDeviceSelect,
        audioOut,
        pick(outputDeviceSelect, settings.audio_out),
        "Audio out",
      ),
      populateSelect(
        midiInSelect,
        midiIn,
        pickMidi(midiInSelect, settings.midi_in),
        "MIDI in",
        MIDI_IN_NONE,
      ),
      populateSelect(
        midiOutSelect,
        midiOut,
        pickMidi(midiOutSelect, settings.midi_out),
        "MIDI out",
        MIDI_OUT_NONE,
      ),
    ].filter((m): m is string => m !== null);
    if (missing.length > 0) {
      const msg =
        `Saved device not connected: ${missing.join(", ")}. ` +
        "Reconnect it and press Refresh Devices, or choose another device.";
      log(`WARNING: ${msg}`);
      raiseAlarm("device_missing", "Saved device not connected", msg);
    } else {
      clearAlarmKind("device_missing");
    }

    const pickChannel = (select: HTMLSelectElement, saved: number) =>
      select.options.length > 0 ? selectedChannel(select) : saved;
    await Promise.all([
      populateChannels(
        inputChannelSelect,
        selectedName(inputDeviceSelect),
        true,
        pickChannel(inputChannelSelect, settings.audio_in_channel ?? 1),
      ),
      populateChannels(
        outputChannelSelect,
        selectedName(outputDeviceSelect),
        false,
        pickChannel(outputChannelSelect, settings.audio_out_channel ?? 1),
      ),
    ]);

    if (!profileRestored) {
      profileRestored = true;
      await selectSavedProfile(settings.profile);
    }
    if (txLevelInput.dataset.loaded !== "1") {
      txLevelInput.value = String(settings.tx_level_db ?? DEFAULT_TX_LEVEL_DB);
      txLevelInput.dataset.loaded = "1";
    }
    populateMscSelects();
    selectMsc(mscDeviceSelect, settings.msc_device_id, 0x7f);
    selectMsc(mscFormatSelect, settings.msc_command_format, 0x01);
    mscAcceptSelect.value =
      settings.msc_accept_device === null || settings.msc_accept_device === undefined
        ? ""
        : String(settings.msc_accept_device);
    if (linkKeyInput.dataset.loaded !== "1") {
      linkKeyInput.value = settings.link_key ?? "";
      failsafeBurstCheck.checked = settings.fail_safe_burst ?? true;
      linkKeyInput.dataset.loaded = "1";
    }
    const modeRadio = document.querySelector(
      `input[name="mode"][value="${settings.mode || "both"}"]`,
    ) as HTMLInputElement | null;
    if (modeRadio) modeRadio.checked = true;
    applyModeVisibility();
    refreshProfileHint();
    await refreshSetupWarnings();

    log(
      `Audio in/out ${audioIn.length}/${audioOut.length}, MIDI in/out ${midiIn.length}/${midiOut.length}`,
    );
  } catch (error) {
    log(`Error loading devices: ${error}`);
  }
}

async function persistSettings() {
  const settings: AppSettings = {
    profile: profileSelect.value || DEFAULT_PROFILE,
    mode: currentMode(),
    audio_in: selectedName(inputDeviceSelect),
    audio_out: selectedName(outputDeviceSelect),
    midi_in: selectedName(midiInSelect),
    midi_out: selectedName(midiOutSelect),
    msc_device_id: Number(mscDeviceSelect.value || 0x7f),
    msc_command_format: Number(mscFormatSelect.value || 0x01),
    audio_in_channel: selectedChannel(inputChannelSelect),
    audio_out_channel: selectedChannel(outputChannelSelect),
    tx_level_db: txLevelDb(),
    link_key: linkKeyInput.value.trim(),
    msc_accept_device: mscAcceptDevice(),
    fail_safe_burst: failsafeBurstCheck.checked,
  };
  await invoke("set_settings", { settings });
  log("Settings saved.");
}

refreshBtn.addEventListener("click", loadDevices);
saveSettingsBtn.addEventListener("click", () => {
  persistSettings().catch((e) => log(`Save failed: ${e}`));
});
profileSelect.addEventListener("change", () => {
  refreshProfileHint();
  refreshSetupWarnings();
});
inputDeviceSelect.addEventListener("change", () => {
  populateChannels(
    inputChannelSelect,
    selectedName(inputDeviceSelect),
    true,
    selectedChannel(inputChannelSelect),
  );
  refreshSetupWarnings();
});
outputDeviceSelect.addEventListener("change", () => {
  populateChannels(
    outputChannelSelect,
    selectedName(outputDeviceSelect),
    false,
    selectedChannel(outputChannelSelect),
  );
  refreshSetupWarnings();
});
txLevelInput.addEventListener("change", () => {
  txLevelInput.value = String(txLevelDb());
});
document.querySelectorAll('input[name="mode"]').forEach((el) => {
  el.addEventListener("change", () => {
    applyModeVisibility();
    refreshSetupWarnings();
  });
});

selfTestBtn.addEventListener("click", async () => {
  selfTestBtn.disabled = true;
  selfTestResults.textContent = "Measuring loopback latency (paced)…";
  try {
    const lines = await invoke<string[]>("run_self_test");
    for (const line of lines) log(line);
    selfTestResults.textContent = lines.join(" · ");
  } catch (e) {
    selfTestResults.textContent = "";
    log(`Self-test failed: ${e}`);
  } finally {
    selfTestBtn.disabled = false;
  }
});

type Role = "decoder" | "encoder";

/** Payload of the `worker_stopped` event. */
interface WorkerStopped {
  role: Role;
  run_id: number;
}

/**
 * Run id returned by the start command of the worker the UI currently shows
 * as running (null = shown as stopped), and the highest run id reported by
 * `worker_stopped` per role. Run ids increase, so a `worker_stopped` that
 * arrives before its start command's reply is still matched afterwards, and
 * a late one from an earlier run is ignored.
 */
const currentRun: Record<Role, number | null> = {
  decoder: null,
  encoder: null,
};
const lastStoppedRun: Record<Role, number> = { decoder: 0, encoder: 0 };

/** Lock or unlock a role's controls (buttons and the selects it uses). */
function setRunningUi(role: Role, running: boolean) {
  if (role === "decoder") {
    startListenBtn.disabled = running;
    stopListenBtn.disabled = !running;
    decoderPanicBtn.disabled = !running;
    inputDeviceSelect.disabled = running;
    inputChannelSelect.disabled = running;
    midiOutSelect.disabled = running;
    if (!running) resetLinkDisplay();
  } else {
    startEncodeBtn.disabled = running;
    stopEncodeBtn.disabled = !running;
    panicBtn.disabled = !running;
    outputDeviceSelect.disabled = running;
    outputChannelSelect.disabled = running;
    midiInSelect.disabled = running;
    simulateCheck.disabled = running;
    txLevelInput.disabled = running;
    linkKeyInput.disabled = running;
    mscAcceptSelect.disabled = running;
    failsafeBurstCheck.disabled = running;
    if (!running) clearTxQueueDisplay();
  }
}

/** Show `role` as stopped; `reason` is shown in the decoder display. */
function markStopped(role: Role, reason: string) {
  currentRun[role] = null;
  setRunningUi(role, false);
  if (role === "decoder") {
    decodedDisplay.innerHTML = "";
    const span = document.createElement("span");
    span.style.color = "#555";
    span.textContent = reason;
    decodedDisplay.appendChild(span);
  }
}

/** The worker of `role` ended without the user pressing Stop. */
function workerDied(role: Role) {
  const name = role === "decoder" ? "Decoder" : "Encoder";
  markStopped(role, "Stopped (worker ended — see log)");
  log(`${name} stopped unexpectedly (see the log above for the reason).`);
}

/** Record a started run; returns false if it has already stopped. */
function trackRun(role: Role, runId: number): boolean {
  if (lastStoppedRun[role] >= runId) {
    workerDied(role);
    return false;
  }
  currentRun[role] = runId;
  setRunningUi(role, true);
  return true;
}

// --- link status: traffic light, meters, rates -------------------------------

interface LinkStatsPayload {
  state: string;
  crc_pass: number;
  crc_fail: number;
  /** CRC failures with a packet's structure: another link's key. */
  wrong_key_suspects: number;
  fec_corrected: number;
  confidence_db: number;
  last_seq: number | null;
  sync_lost: number;
  crc_pass_rate: number;
  est_latency_ms: number;
  link_alive: boolean;
  buffer_drops: number;
}

let lastStats: LinkStatsPayload | null = null;
/** Input level (dBFS, peak hold) and when it was last raised. */
let levelDb = -120;
let levelAt = 0;
/** Time of the last valid packet (heartbeats included), ms since epoch. */
let lastPacketAt: number | null = null;
/** Samples of (time, crc_fail, fec_corrected) for the per-minute rates. */
const rateHistory: { t: number; crcFail: number; fec: number }[] = [];

type Light = "stopped" | "ok" | "nosignal" | "nolock" | "dead";

/** Same rule as the engine's `LinkLight::from_stats`. */
function linkLight(): Light {
  if (currentRun.decoder === null) return "stopped";
  if (lastStats?.link_alive) return "ok";
  if (lastStats?.state === "Dead") return "dead";
  if (levelDb < NO_SIGNAL_DBFS) return "nosignal";
  return "nolock";
}

const LIGHT_LABEL: Record<Light, string> = {
  stopped: "STOPPED",
  ok: "LINK OK",
  nosignal: "NO SIGNAL",
  nolock: "SIGNAL, NO LOCK",
  dead: "LINK DEAD",
};

const LIGHT_HINT: Record<Light, string> = {
  stopped: "",
  ok: "",
  nosignal:
    "No signal: check the cable, the input channel, and that the far end is encoding.",
  nolock:
    "Signal present, no lock: profile mismatch, or EQ / dynamics on the bus? Both ends must use the same profile.",
  dead: "Link dead: Sustain off + All Notes Off were sent. It recovers by itself with the next valid packet.",
};

function ago(t: number | null): string {
  if (t === null) return "no packet yet";
  return `${((Date.now() - t) / 1000).toFixed(1)} s ago`;
}

/** Rate per minute of `pick` over the last minute (or since the decoder
 * started, once there are 5 s of data). */
function perMinute(pick: (s: { crcFail: number; fec: number }) => number): string {
  if (rateHistory.length < 2) return "—";
  const newest = rateHistory[rateHistory.length - 1];
  const oldest = rateHistory[0];
  const dt = newest.t - oldest.t;
  if (dt < 5000) return "—";
  const rate = ((pick(newest) - pick(oldest)) * 60_000) / dt;
  return rate < 10 ? rate.toFixed(1) : rate.toFixed(0);
}

function updateLight() {
  const light = linkLight();
  linkLightEl.textContent = LIGHT_LABEL[light];
  linkLightEl.className = `light ${light === "stopped" ? "" : light}`.trim();
  if (light === "stopped") {
    linkLightDetailEl.textContent = "Decoder not running.";
  } else {
    linkLightDetailEl.textContent = `Last heartbeat / packet: ${ago(lastPacketAt)}`;
  }
  linkHintEl.textContent = LIGHT_HINT[light];
  linkLastPacketEl.textContent = currentRun.decoder === null ? "—" : ago(lastPacketAt);
  // Peak hold of the level readout.
  if (levelAt > 0 && Date.now() - levelAt > LEVEL_HOLD_MS && light !== "stopped") {
    // Let the hold decay to the current meter position on the next event.
    levelAt = 0;
  }
}

function resetLinkDisplay() {
  lastStats = null;
  lastPacketAt = null;
  levelDb = -120;
  levelAt = 0;
  rateHistory.length = 0;
  inputMeter.style.width = "0%";
  inputLevelReadout.textContent = "— dBFS";
  confidenceMeter.style.width = "0%";
  confidenceReadout.textContent = "— dB";
  linkStateEl.textContent = "—";
  linkStateEl.style.color = "";
  updateLight();
}

function showLevel(rms: number) {
  // Full-scale sine = 0 dBFS (a console's peak meter on a steady tone); the
  // default transmit level then reads -12 dBFS.
  const db = rms > 0 ? Math.max(-120, 20 * Math.log10(rms * Math.SQRT2)) : -120;
  if (db >= levelDb || levelAt === 0) {
    levelDb = db;
    levelAt = Date.now();
  }
  const percent = Math.min(100, Math.max(0, ((levelDb - METER_MIN_DBFS) / -METER_MIN_DBFS) * 100));
  inputMeter.style.width = `${percent}%`;
  inputMeter.style.backgroundColor =
    levelDb > -1 ? "#d64545" : levelDb >= -30 ? "#3d9a7a" : "#d4a017";
  inputLevelReadout.textContent =
    levelDb <= -99.5 ? "< −99 dBFS" : `${levelDb.toFixed(1).replace("-", "−")} dBFS`;
}

startListenBtn.addEventListener("click", async () => {
  // Cleared before starting so an alarm raised by the new worker stays up.
  clearAlarmKind("link_dead");
  clearAlarmKind("audio_input_stalled");
  clearAlarmKind("midi_output_error");
  try {
    const runId = await invoke<number>("start_decoder", {
      audioIn: selectedName(inputDeviceSelect),
      audioInChannel: selectedChannel(inputChannelSelect),
      midiOut: selectedName(midiOutSelect),
      profile: profileSelect.value,
      mscDeviceId: Number(mscDeviceSelect.value || 0x7f),
      mscCommandFormat: Number(mscFormatSelect.value || 0x01),
      linkKey: linkKeyInput.value.trim(),
      failSafeBurst: failsafeBurstCheck.checked,
    });
    if (trackRun("decoder", runId)) {
      decodedDisplay.innerHTML =
        '<span style="color: #3d9a7a">Listening…</span>';
      resetLinkDisplay();
    }
    await persistSettings();
  } catch (error) {
    log(`Error starting decoder: ${error}`);
  }
});

stopListenBtn.addEventListener("click", async () => {
  // Forget the run first so its own worker_stopped is not taken as a crash.
  markStopped("decoder", "Stopped (All Notes Off sent)");
  try {
    await invoke("stop_decoder");
    log("Decoder stopped.");
  } catch (error) {
    log(`Error stopping decoder: ${error}`);
  }
});

decoderPanicBtn.addEventListener("click", async () => {
  try {
    await invoke("decoder_panic");
    log("Decoder panic: Sustain off + All Notes Off sent on all channels.");
  } catch (error) {
    log(`Panic failed: ${error}`);
  }
});

startEncodeBtn.addEventListener("click", async () => {
  try {
    const runId = await invoke<number>("start_encoder", {
      audioOut: selectedName(outputDeviceSelect),
      audioOutChannel: selectedChannel(outputChannelSelect),
      midiIn: selectedName(midiInSelect),
      profile: profileSelect.value,
      // The test scale is an explicit opt-in; without MIDI In the encoder
      // sends heartbeats only.
      simulate: simulateCheck.checked,
      txLevelDb: txLevelDb(),
      linkKey: linkKeyInput.value.trim(),
      mscAcceptDevice: mscAcceptDevice(),
    });
    trackRun("encoder", runId);
    await persistSettings();
  } catch (error) {
    log(`Error starting encoder: ${error}`);
  }
});

stopEncodeBtn.addEventListener("click", async () => {
  markStopped("encoder", "Stopped");
  try {
    await invoke("stop_encoder");
    log("Encoder stopped.");
  } catch (error) {
    log(`Error stopping encoder: ${error}`);
  }
});

panicBtn.addEventListener("click", async () => {
  try {
    await invoke("encoder_panic");
    log("Encoder panic sent (All Notes Off over the link).");
  } catch (error) {
    log(`Panic failed: ${error}`);
  }
});

async function bindEvents() {
  // A worker ended (missing/failed device, panic, or a deliberate stop).
  // Only the run the UI shows as running is reset; see `currentRun`.
  await listen<WorkerStopped>("worker_stopped", (event) => {
    const { role, run_id } = event.payload;
    lastStoppedRun[role] = Math.max(lastStoppedRun[role], run_id);
    if (currentRun[role] === run_id) workerDied(role);
  });

  await listen<{ message: string }>("log", (event) => {
    log(event.payload.message);
  });

  await listen<{
    frame_type: string;
    payload: string;
    is_heartbeat: boolean;
  }>("decoded_frame", (event) => {
    const { frame_type, payload, is_heartbeat } = event.payload;
    lastPacketAt = Date.now();
    if (is_heartbeat) return;
    const text = payload ? `${frame_type}: ${payload}` : frame_type;
    decodedDisplay.textContent = text;
    linkLastEventEl.textContent = text;
    decodedDisplay.style.backgroundColor = "#1a2220";
    setTimeout(() => (decodedDisplay.style.backgroundColor = "#0d0f12"), 100);
  });

  await listen<number>("input_level", (event) => {
    showLevel(event.payload);
  });

  await listen<LinkStatsPayload>("link_stats", (event) => {
    const s = event.payload;
    lastStats = s;
    const now = Date.now();
    rateHistory.push({ t: now, crcFail: s.crc_fail, fec: s.fec_corrected });
    while (rateHistory.length > 2 && now - rateHistory[0].t > RATE_WINDOW_MS) {
      rateHistory.shift();
    }
    linkStateEl.textContent = s.state;
    linkConfidenceEl.textContent = s.confidence_db.toFixed(1);
    linkCrcEl.textContent = `${s.crc_pass} / ${s.crc_fail}`;
    linkWrongKeyEl.textContent =
      s.wrong_key_suspects > 0
        ? `(${s.wrong_key_suspects} from another link key?)`
        : "";
    linkCrcFailRateEl.textContent = perMinute((h) => h.crcFail);
    linkFecEl.textContent = String(s.fec_corrected);
    linkFecRateEl.textContent = perMinute((h) => h.fec);
    const locks = s.crc_pass + s.crc_fail + s.sync_lost;
    linkLocksEl.textContent = `${locks} / ${s.crc_fail}`;
    linkSyncLostEl.textContent = String(s.sync_lost);
    linkDropsEl.textContent = String(s.buffer_drops);
    linkSeqEl.textContent = s.last_seq === null ? "—" : String(s.last_seq);
    linkLatencyEl.textContent = s.est_latency_ms.toFixed(1);
    const conf = Math.min(Math.max(s.confidence_db, 0), CONFIDENCE_SCALE_DB);
    confidenceMeter.style.width = `${(conf / CONFIDENCE_SCALE_DB) * 100}%`;
    confidenceMeter.style.backgroundColor =
      s.confidence_db >= CONFIDENCE_GOOD_DB ? "#3d9a7a" : "#d4a017";
    confidenceReadout.textContent = `${s.confidence_db.toFixed(1)} dB`;
    if (s.state === "Locked") linkStateEl.style.color = "#3d9a7a";
    else if (s.state === "Dead") linkStateEl.style.color = "#d64545";
    else linkStateEl.style.color = "#d4a017";
    updateLight();
  });

  await listen<{ kind: string; title: string; message: string }>(
    "alarm",
    (event) => {
      const { kind, title, message } = event.payload;
      raiseAlarm(kind, title, message);
      log(`ALARM: ${title} — ${message}`);
    },
  );

  // The engine reports when an alarm's condition has ended (audio input
  // resumed, the link came back, MIDI out works again): drop its rows.
  await listen<{ kind: string; title: string; message: string }>(
    "alarm_cleared",
    (event) => {
      clearAlarmKind(event.payload.kind);
      log(event.payload.message);
    },
  );

  await listen<{ queued: number; latency_ms: number; dropped: number }>(
    "tx_status",
    (event) => {
      const { queued, latency_ms, dropped } = event.payload;
      txQueuedEl.textContent = String(queued);
      txQueueLatencyEl.textContent = latency_ms.toFixed(0);
      txDroppedEl.textContent = String(dropped);
      txDroppedEl.style.color = dropped > 0 ? "#d64545" : "";
    },
  );
}

setInterval(updateLight, 250);

bindEvents()
  .then(() => loadDevices())
  .catch((e) => console.error(e));

// The version shown in the header comes from the Cargo/Tauri package version
// (one source of truth in the workspace Cargo.toml), not from a string here.
getVersion()
  .then((v) => {
    const el = document.getElementById("app-version");
    if (el) el.textContent = `v${v}`;
  })
  .catch(() => {});
