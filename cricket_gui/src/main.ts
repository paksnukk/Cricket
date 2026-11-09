import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

const inputDeviceSelect = document.getElementById(
  "input-device-select",
) as HTMLSelectElement;
const outputDeviceSelect = document.getElementById(
  "output-device-select",
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
const txQueuedEl = document.getElementById("tx-queued") as HTMLElement;
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
const logContainer = document.getElementById("log-container") as HTMLDivElement;
const alarmBox = document.getElementById("alarm-box") as HTMLDivElement;
const decodeCard = document.getElementById("decode-card") as HTMLElement;
const encodeCard = document.getElementById("encode-card") as HTMLElement;

const linkStateEl = document.getElementById("link-state") as HTMLElement;
const linkAliveEl = document.getElementById("link-alive") as HTMLElement;
const linkConfidenceEl = document.getElementById(
  "link-confidence",
) as HTMLElement;
const linkLatencyEl = document.getElementById("link-latency") as HTMLElement;
const linkCrcEl = document.getElementById("link-crc") as HTMLElement;
const linkCrcRateEl = document.getElementById("link-crc-rate") as HTMLElement;
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
}

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
  for (let id = 0; id <= 0x6f; id++) {
    add(mscDeviceSelect, id, `${id} (${hex(id)})`);
  }
  for (let g = 0x70; g <= 0x7e; g++) {
    add(mscDeviceSelect, g, `Group ${g - 0x6f} (${hex(g)})`);
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

/** Kind of the alarm the banner currently shows (null when hidden). */
let alarmKind: string | null = null;

function showAlarm(kind: string, message: string) {
  alarmKind = kind;
  alarmBox.textContent = message;
  alarmBox.classList.add("visible");
}

function clearAlarm() {
  alarmKind = null;
  alarmBox.classList.remove("visible");
}

/** Selected device name (option values are device names), or null. */
function selectedName(select: HTMLSelectElement): string | null {
  return select.value === "" ? null : select.value;
}

/** Label of the "no MIDI" entry of each MIDI select (value ""). */
const MIDI_IN_NONE = "None (encoder sends simulated test scale)";
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

/** Short operator-facing description of a profile (both ends must match). */
function profileDescription(profile: string): string {
  switch (profile) {
    case "robust_repeat":
      return "Robust repeat — uncoded, every message sent twice. For lines with glitches or dropouts of ~50 ms or more.";
    case "fast":
      return "Fast — notes on a clean dedicated line (no EQ, compression, or limiter).";
    case "ltc":
    case "ltc2400":
    case "ltc9600":
      return "LTC — biphase-mark code like timecode. Dedicated clean line only: no EQ, compression, limiter or dropouts.";
    case "ltc_fec":
    case "ltc2400_fec":
    case "ltc9600_fec":
      return "LTC FEC — biphase-mark code with error correction; survives short dropouts and clicks. Dedicated line, no EQ (9600: digital lines only).";
    case "ltc14400":
    case "ltc14400_fec":
      return "LTC 14400 — fastest biphase-mark profile, for digital lines only (Dante, MADI, AES at 48 kHz or more): needs ≥ 16 kHz of bandwidth and ≥ 15 dB SNR. The FEC variant also survives dropouts up to ~0.5 ms.";
    default:
      return "Robust (default) — error-corrected, one copy per message; cues and notes over a chain you don't fully trust.";
  }
}

async function refreshProfileHint() {
  try {
    const info = await invoke<{ est_latency_ms: number }>("get_profile_info", {
      profile: profileSelect.value,
    });
    const airtime = `≈ ${info.est_latency_ms.toFixed(0)} ms first-copy airtime.`;
    profileHint.textContent = `${profileDescription(profileSelect.value)} ${airtime}`;
  } catch (e) {
    log(`Profile info error: ${e}`);
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
      showAlarm("device_missing", msg);
    } else if (alarmKind === "device_missing") {
      clearAlarm();
    }

    // `robust_fec` is the former name of `robust`; unknown names mean robust.
    const saved = (settings.profile || "robust").toLowerCase().replace(/-/g, "_");
    const known = Array.from(profileSelect.options).some((o) => o.value === saved);
    profileSelect.value = known ? saved : "robust";
    populateMscSelects();
    selectMsc(mscDeviceSelect, settings.msc_device_id, 0x7f);
    selectMsc(mscFormatSelect, settings.msc_command_format, 0x01);
    const modeRadio = document.querySelector(
      `input[name="mode"][value="${settings.mode || "both"}"]`,
    ) as HTMLInputElement | null;
    if (modeRadio) modeRadio.checked = true;
    applyModeVisibility();
    await refreshProfileHint();

    log(
      `Audio in/out ${audioIn.length}/${audioOut.length}, MIDI in/out ${midiIn.length}/${midiOut.length}`,
    );
  } catch (error) {
    log(`Error loading devices: ${error}`);
  }
}

async function persistSettings() {
  const settings: AppSettings = {
    profile: profileSelect.value,
    mode: currentMode(),
    audio_in: selectedName(inputDeviceSelect),
    audio_out: selectedName(outputDeviceSelect),
    midi_in: selectedName(midiInSelect),
    midi_out: selectedName(midiOutSelect),
    msc_device_id: Number(mscDeviceSelect.value || 0x7f),
    msc_command_format: Number(mscFormatSelect.value || 0x01),
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
});
document.querySelectorAll('input[name="mode"]').forEach((el) => {
  el.addEventListener("change", applyModeVisibility);
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
    inputDeviceSelect.disabled = running;
    midiOutSelect.disabled = running;
  } else {
    startEncodeBtn.disabled = running;
    stopEncodeBtn.disabled = !running;
    panicBtn.disabled = !running;
    outputDeviceSelect.disabled = running;
    midiInSelect.disabled = running;
    simulateCheck.disabled = running;
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

startListenBtn.addEventListener("click", async () => {
  // Cleared before starting so an alarm raised by the new worker stays up.
  clearAlarm();
  try {
    const runId = await invoke<number>("start_decoder", {
      audioIn: selectedName(inputDeviceSelect),
      midiOut: selectedName(midiOutSelect),
      profile: profileSelect.value,
      mscDeviceId: Number(mscDeviceSelect.value || 0x7f),
      mscCommandFormat: Number(mscFormatSelect.value || 0x01),
    });
    if (trackRun("decoder", runId)) {
      decodedDisplay.innerHTML =
        '<span style="color: #3d9a7a">Listening…</span>';
    }
    await persistSettings();
  } catch (error) {
    log(`Error starting decoder: ${error}`);
  }
});

stopListenBtn.addEventListener("click", async () => {
  // Forget the run first so its own worker_stopped is not taken as a crash.
  markStopped("decoder", "Stopped");
  try {
    await invoke("stop_decoder");
    log("Decoder stopped.");
  } catch (error) {
    log(`Error stopping decoder: ${error}`);
  }
});

startEncodeBtn.addEventListener("click", async () => {
  try {
    const midiIn = selectedName(midiInSelect);
    const runId = await invoke<number>("start_encoder", {
      audioOut: selectedName(outputDeviceSelect),
      midiIn,
      profile: profileSelect.value,
      // With MIDI In = None the encoder always sends the test scale.
      simulate: simulateCheck.checked || midiIn === null,
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
    log("Encoder panic sent (All Notes Off).");
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
    if (is_heartbeat) {
      linkAliveEl.textContent = "yes";
      linkAliveEl.style.color = "#3d9a7a";
      return;
    }
    const text = payload ? `${frame_type}: ${payload}` : frame_type;
    decodedDisplay.textContent = text;
    linkLastEventEl.textContent = text;
    decodedDisplay.style.backgroundColor = "#1a2220";
    setTimeout(() => (decodedDisplay.style.backgroundColor = "#0d0f12"), 100);
  });

  await listen<number>("input_level", (event) => {
    const percent = Math.min(event.payload * 400, 100);
    inputMeter.style.width = `${percent}%`;
    inputMeter.style.backgroundColor = percent > 90 ? "#d64545" : "#3d9a7a";
  });

  await listen<{
    state: string;
    crc_pass: number;
    crc_fail: number;
    confidence_db: number;
    last_seq: number | null;
    sync_lost: number;
    crc_pass_rate: number;
    est_latency_ms: number;
    link_alive: boolean;
    buffer_drops: number;
  }>("link_stats", (event) => {
    const s = event.payload;
    linkStateEl.textContent = s.state;
    linkConfidenceEl.textContent = s.confidence_db.toFixed(1);
    linkCrcEl.textContent = `${s.crc_pass} / ${s.crc_fail}`;
    linkCrcRateEl.textContent = s.crc_pass_rate.toFixed(0);
    linkSyncLostEl.textContent = String(s.sync_lost);
    linkDropsEl.textContent = String(s.buffer_drops);
    linkSeqEl.textContent = s.last_seq === null ? "—" : String(s.last_seq);
    linkLatencyEl.textContent = s.est_latency_ms.toFixed(0);
    linkAliveEl.textContent = s.link_alive ? "yes" : "no";
    linkAliveEl.style.color = s.link_alive ? "#3d9a7a" : "#9aa3ad";
    confidenceMeter.style.width = `${Math.min(Math.max(s.confidence_db, 0) * 10, 100)}%`;
    if (s.state === "Locked") linkStateEl.style.color = "#3d9a7a";
    else if (s.state === "Dead") linkStateEl.style.color = "#d64545";
    else linkStateEl.style.color = "#d4a017";
  });

  await listen<{ kind: string; message: string }>("alarm", (event) => {
    showAlarm(event.payload.kind, event.payload.message);
    log(`ALARM: ${event.payload.message}`);
  });

  // The engine reports when an alarm's condition has ended (e.g. audio input
  // resumed after a stall): drop the banner if it still shows that alarm.
  await listen<{ kind: string; message: string }>("alarm_cleared", (event) => {
    if (alarmKind === event.payload.kind) clearAlarm();
    log(event.payload.message);
  });

  await listen<{ queued: number; latency_ms: number }>("tx_status", (event) => {
    const { queued, latency_ms } = event.payload;
    txQueuedEl.textContent = String(queued);
    txQueueLatencyEl.textContent = latency_ms.toFixed(0);
  });
}

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
