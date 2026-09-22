import { createIcons } from "lucide";
import { Channel, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { hostIconSet, hostLook } from "./host-look";
import { locale, t } from "./i18n";
import { initializeTheme } from "./theme";
import "./styles/tokens.css";
import "./styles/base.css";
import "./host-switcher.css";

type Platform = "windows" | "mac";
type SwitchProgressEvent =
  | { event: "waking"; peerName: string }
  | { event: "checking"; peerName: string }
  | { event: "waiting"; peerName: string; seconds: number }
  | { event: "switching" }
  | { event: "remoteFallback"; peerName: string };

interface HostOption {
  id: string;
  name: string;
  platform: Platform;
  inputName: string | null;
  isLocal: boolean;
  available: boolean;
  /** The host the display is already showing. */
  isActive: boolean;
  /** Custom icon and colour names; absent or null keeps the default. */
  icon?: string | null;
  color?: string | null;
}

interface HostSwitcherMonitor {
  monitorKey: string;
  name: string;
  hosts: HostOption[];
}

interface HostSwitcherState {
  monitors: HostSwitcherMonitor[];
}

/** What the backend's last check said about one paired host. */
interface HostPresence {
  peerId: string;
  online: boolean;
  checkedAtMs: number;
  lastSeenAtMs: number;
  /** `monitorKey` of every shared display that host says it can see; null when
   *  it did not say, which is never the same as "it sees none". */
  attachedMonitors: string[] | null;
  detail: string;
}

interface OperationResult {
  title: string;
  detail: string;
}

/** "all", or the key of one shared display. */
type Target = string;
const ALL_DISPLAYS = "all";
/** Digit keys reach this many hosts. */
const MAX_NUMBERED_HOSTS = 9;

/** One host as this window lists it, for the display (or displays) targeted. */
interface HostRow {
  id: string;
  name: string;
  platform: Platform;
  /** Lucide icon and colour name, as the main window draws this host. */
  icon: string;
  color: string;
  detail: string;
  /** Displays among the target this host is on. */
  showingCount: number;
  /** Displays among the target a switch would move to this host. */
  switchable: HostSwitcherMonitor[];
}

const root = document.querySelector<HTMLElement>("#host-switcher-app")!;
if (!root) throw new Error("MuxSU host switcher root was not found");
initializeTheme();

let state: HostSwitcherState = { monitors: [] };
let target: Target = ALL_DISPLAYS;
let rows: HostRow[] = [];
let selectedIndex = 0;
let switching = false;
/** What the last check said about each paired host, keyed by peer id. */
let hostPresence: Record<string, HostPresence> = {};
/** Counts switch batches. Opening the window again supersedes one still
 *  running, so a stale batch neither carries on nor closes the new window. */
let batch = 0;
/** Emitted by the backend when this or a paired host saves a new host card order. */
const HOST_ORDER_CHANGED_EVENT = "host-order-changed";
/** Emitted by the backend when this or a paired host renames a host. */
const HOST_NAMES_CHANGED_EVENT = "host-names-changed";
/** Emitted by the backend when this or a paired host changes an input note. */
const INPUT_LABELS_CHANGED_EVENT = "input-labels-changed";
/** Which host a shared display is showing; this window says so on every row. */
const ACTIVE_ROUTE_CHANGED_EVENT = "active-route-changed";
/** A paired host's port, which this window shows beside that host. */
const PEER_INPUTS_CHANGED_EVENT = "peer-inputs-changed";
/** A merge changes which displays this window lists. */
const MONITOR_IDENTITIES_CHANGED_EVENT = "monitor-identities-changed";

function escapeHtml(value: string): string {
  return value.replace(/[&<>'"]/g, (character) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", "\"": "&quot;",
  })[character] ?? character);
}

function platformLabel(platform: Platform): string {
  return platform === "mac" ? "macOS" : "Windows";
}

type PresenceState = "online" | "offline" | "unknown";

/** Whether a host answers right now. This computer always does. */
function presenceState(hostId: string): PresenceState {
  if (hostId === "local") return "online";
  const presence = hostPresence[hostId];
  if (!presence || !presence.checkedAtMs) return "unknown";
  return presence.online ? "online" : "offline";
}

function presenceLabel(hostId: string): string {
  const state = presenceState(hostId);
  if (state === "online") return t("presence.online");
  return state === "offline" ? t("presence.offline") : t("presence.unknown");
}

/** What a dot means, as a sentence: the overlay is read at a glance, and a
 *  word like "not checked yet" explains nothing on its own. */
function presenceHelp(hostId: string): string {
  const state = presenceState(hostId);
  if (state === "online") return t("presence.onlineHelp");
  if (state === "unknown") return t("presence.unknownHelp");
  return `${hostPresence[hostId]?.detail || t("presence.offlinePlain")} ${t("presence.offlineHelp")}`;
}

function presenceDotHtml(hostId: string): string {
  const label = escapeHtml(presenceHelp(hostId));
  return `<span class="presence-dot is-${presenceState(hostId)}" role="img" aria-label="${label}" title="${label}"></span>`;
}

/** Whether a host can see the one display this window is aimed at. Only asked
 *  for a single display: aimed at all of them, a host is either up or not. */
function seesTargetedDisplay(hostId: string, monitors: HostSwitcherMonitor[]): boolean | null {
  const [only] = monitors;
  if (monitors.length !== 1 || !only || hostId === "local") return null;
  const presence = hostPresence[hostId];
  if (!presence?.online || !presence.attachedMonitors) return null;
  return presence.attachedMonitors.includes(only.monitorKey);
}

/** The targets Tab cycles through: every display at once, then each one. */
function targets(): Target[] {
  const keys = state.monitors.map((monitor) => monitor.monitorKey);
  return keys.length > 1 ? [ALL_DISPLAYS, ...keys] : keys;
}

function targetedMonitors(): HostSwitcherMonitor[] {
  return target === ALL_DISPLAYS ? state.monitors : state.monitors.filter((monitor) => monitor.monitorKey === target);
}

function computeRows(): HostRow[] {
  const monitors = targetedMonitors();
  // Every display lists the same hosts in the same saved order.
  const order = state.monitors[0]?.hosts ?? [];
  return order.map((host, index) => {
    const entries = monitors.map((monitor) => ({ monitor, option: monitor.hosts.find((item) => item.id === host.id) }));
    const showingCount = entries.filter(({ option }) => option?.isActive).length;
    // The host a display already shows is not somewhere to switch it to; the
    // backend would only answer that the display is already on that input.
    const switchable = entries.filter(({ option }) => option?.available && !option.isActive).map(({ monitor }) => monitor);
    const single = monitors.length === 1 ? entries[0]?.option : undefined;
    const standing = host.isLocal ? "" : presenceLabel(host.id);
    const blind = seesTargetedDisplay(host.id, monitors) === false ? t("presence.blindShort") : "";
    const detail = [
      platformLabel(host.platform),
      single ? single.inputName ?? t("switcher.inputUnset") : "",
      standing,
      blind,
    ].filter(Boolean).join(" · ");
    const look = hostLook({ icon: host.icon, color: host.color }, host.platform, index);
    return { id: host.id, name: host.name, platform: host.platform, icon: look.lucide, color: look.color, detail, showingCount, switchable };
  });
}

function isSelectable(row: HostRow | undefined): row is HostRow {
  return Boolean(row && row.switchable.length);
}

function rowState(row: HostRow, isFocused: boolean): string {
  const isAll = targetedMonitors().length > 1;
  if (isFocused && isSelectable(row)) return `↩ ${isAll ? t("switcher.switchAll") : t("action.switchShort")}`;
  if (row.showingCount && isAll) return t("switcher.showingCount", { count: row.showingCount });
  if (row.showingCount) return t("switcher.showing");
  if (!isSelectable(row)) return t("switcher.inputUnset");
  return "";
}

function targetLabel(key: Target): string {
  if (key === ALL_DISPLAYS) return t("switcher.allDisplays", { count: state.monitors.length });
  return state.monitors.find((monitor) => monitor.monitorKey === key)?.name ?? key;
}

function render(message?: { title: string; detail: string; error?: boolean }): void {
  const allTargets = targets();
  const numbered = Math.min(rows.length, MAX_NUMBERED_HOSTS);
  root.innerHTML = `
    <main class="hud" aria-labelledby="switcher-title">
      <header class="hud-head">
        <h1 id="switcher-title">${t("switcher.title")}</h1>
        ${allTargets.length > 1 ? `<span class="caption">${t("switcher.targetHint")}</span>` : `<span class="caption">${escapeHtml(state.monitors[0]?.name ?? t("switcher.noDisplay"))}</span>`}
      </header>
      ${allTargets.length > 1 ? `<nav class="targets" role="tablist">
        ${allTargets.map((key) => `<button type="button" role="tab" class="${key === target ? "is-active" : ""}" aria-selected="${key === target}" data-target="${escapeHtml(key)}" ${switching ? "disabled" : ""}>${escapeHtml(targetLabel(key))}</button>`).join("")}
      </nav>` : ""}
      <section class="hud-list" role="listbox" aria-label="${t("switcher.hostListAria")}">
        ${rows.map((row, index) => {
          const isFocused = index === selectedIndex && isSelectable(row);
          const isShowing = row.showingCount > 0 && row.showingCount === targetedMonitors().length;
          return `<button type="button" class="hud-row ${isFocused ? "tint is-focused" : ""} ${isShowing ? "is-showing" : ""}" data-color="${row.color}"
            data-row-index="${index}" role="option" aria-selected="${isFocused}" ${isSelectable(row) && !switching ? "" : "disabled"}>
            <kbd>${index < MAX_NUMBERED_HOSTS ? index + 1 : ""}</kbd>
            <span class="host-chip"><i data-lucide="${row.icon}"></i>${presenceDotHtml(row.id)}</span>
            <span class="hud-copy"><b>${escapeHtml(row.name)}</b><small>${escapeHtml(row.detail)}</small></span>
            <span class="hud-state">${escapeHtml(rowState(row, isFocused))}</span>
          </button>`;
        }).join("") || `<p class="empty-note">${t("switcher.noHosts")}</p>`}
      </section>
      ${message ? `<div class="switch-message ${message.error ? "is-error" : ""}" role="status"><strong>${escapeHtml(message.title)}</strong><span>${escapeHtml(message.detail)}</span></div>` : ""}
      <footer class="hud-foot">
        <span>${numbered > 1 ? `${t("switcher.numberHint", { count: numbered })} · ` : ""}${t("switcher.navigationHint")}</span>
        <span>${t("switcher.closeHint")}</span>
      </footer>
    </main>`;
  createIcons({ icons: hostIconSet });
}

function refreshRows(keepHostId?: string): void {
  rows = computeRows();
  const kept = keepHostId ? rows.findIndex((row) => row.id === keepHostId && isSelectable(row)) : -1;
  selectedIndex = kept !== -1 ? kept : Math.max(0, rows.findIndex((row) => isSelectable(row)));
}

function nextAvailableIndex(direction: 1 | -1): number {
  if (!rows.some((row) => isSelectable(row))) return selectedIndex;
  let candidate = selectedIndex;
  do {
    candidate = (candidate + direction + rows.length) % rows.length;
  } while (!isSelectable(rows[candidate]));
  return candidate;
}

function selectIndex(index: number): void {
  if (!isSelectable(rows[index]) || switching) return;
  selectedIndex = index;
  render();
  document.querySelector<HTMLElement>(`[data-row-index="${index}"]`)?.focus();
}

function selectTarget(next: Target): void {
  if (switching || next === target) return;
  const keep = rows[selectedIndex]?.id;
  target = next;
  refreshRows(keep);
  render();
}

function cycleTarget(direction: 1 | -1): void {
  const all = targets();
  if (all.length < 2) return;
  const index = all.indexOf(target);
  selectTarget(all[(index + direction + all.length) % all.length]);
}

async function hideSwitcher(): Promise<void> {
  try { await invoke("hide_host_switcher"); } catch { window.close(); }
}

function progressDetail(event: SwitchProgressEvent): string {
  if (event.event === "waking") return t("switcher.waking", { name: event.peerName });
  if (event.event === "waiting") return t("switcher.waiting", { name: event.peerName, seconds: event.seconds });
  if (event.event === "remoteFallback") return t("switcher.remoteFallback", { name: event.peerName });
  return t("switcher.switching");
}

/**
 * Switches every targeted display that is not already on the chosen host, one
 * at a time: the first switch wakes a sleeping host, so later ones find it
 * ready. A display that fails does not stop the rest.
 */
async function switchToSelected(): Promise<void> {
  const row = rows[selectedIndex];
  if (!isSelectable(row) || switching) return;
  switching = true;
  const run = ++batch;
  render({ title: t("switcher.preparing"), detail: t("switcher.preparingDetail") });
  const results: OperationResult[] = [];
  const failures: string[] = [];
  for (const monitor of row.switchable) {
    const onEvent = new Channel<SwitchProgressEvent>();
    const prefix = row.switchable.length > 1 ? `${monitor.name} · ` : "";
    onEvent.onmessage = (event) => {
      if (run === batch) render({ title: t("switcher.preparing"), detail: `${prefix}${progressDetail(event)}` });
    };
    try {
      results.push(await invoke<OperationResult>("switch_host", { monitorId: monitor.monitorKey, targetId: row.id, onEvent }));
    } catch (error) {
      failures.push(`${monitor.name}: ${String(error)}`);
    }
    if (run !== batch) return;
  }
  if (failures.length) {
    switching = false;
    // Displays that did switch have moved; show where everything is now, so
    // a second try only goes after the ones that failed.
    await loadState();
    render({ title: t("switcher.failed"), detail: failures.join(" "), error: true });
    return;
  }
  const last = results[results.length - 1];
  render({ title: last?.title ?? "", detail: last?.detail ?? "" });
  window.setTimeout(() => {
    if (run !== batch) return;
    // Cleared here and not left to the window closing: this window is hidden
    // rather than destroyed, so a flag left set survives into the next time
    // it opens — and every control, every key and the refresh itself are all
    // gated on it. It would never accept anything again.
    switching = false;
    void hideSwitcher();
  }, 450);
}

root.addEventListener("click", (event) => {
  const element = event.target as HTMLElement;
  const tab = element.closest<HTMLButtonElement>("[data-target]");
  if (tab?.dataset.target) {
    selectTarget(tab.dataset.target);
    return;
  }
  const option = element.closest<HTMLButtonElement>("[data-row-index]");
  if (!option) return;
  const index = Number(option.dataset.rowIndex);
  if (Number.isInteger(index) && isSelectable(rows[index])) {
    selectedIndex = index;
    void switchToSelected();
  }
});

document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") {
    event.preventDefault();
    void hideSwitcher();
    return;
  }
  if (switching) return;
  if (event.key === "Tab" && targets().length > 1) {
    event.preventDefault();
    cycleTarget(event.shiftKey ? -1 : 1);
  } else if (event.key === "Tab") {
    // With one display there is no other target, so Tab keeps moving between hosts.
    event.preventDefault();
    selectIndex(nextAvailableIndex(event.shiftKey ? -1 : 1));
  } else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    selectIndex(nextAvailableIndex(event.key === "ArrowUp" ? -1 : 1));
  } else if (event.key === "Enter") {
    event.preventDefault();
    void switchToSelected();
  } else if (/^[1-9]$/.test(event.key) && !event.metaKey && !event.ctrlKey && !event.altKey) {
    const index = Number(event.key) - 1;
    if (!isSelectable(rows[index])) return;
    event.preventDefault();
    selectedIndex = index;
    void switchToSelected();
  }
});

async function initialize(): Promise<void> {
  try {
    await invoke("set_locale", { locale });
    state = await invoke<HostSwitcherState>("get_host_switcher_state");
  } catch {
    state = {
      monitors: [{
        monitorKey: "preview",
        name: t("switcher.previewDisplay"),
        hosts: [
          { id: "local", name: t("switcher.previewWindows"), platform: "windows", inputName: "HDMI 1", isLocal: true, available: true, isActive: true },
          { id: "peer", name: t("switcher.previewMac"), platform: "mac", inputName: "DisplayPort", isLocal: false, available: true, isActive: false },
        ],
      }],
    };
  }
  target = targets()[0] ?? ALL_DISPLAYS;
  refreshRows();
  render();
  void loadHostPresence(false).then(() => loadHostPresence(true));
  try {
    await listen(HOST_ORDER_CHANGED_EVENT, () => void reloadState());
    await listen(HOST_NAMES_CHANGED_EVENT, () => void reloadState());
    await listen(INPUT_LABELS_CHANGED_EVENT, () => void reloadState());
    // This window opens over whatever the user is doing and is read at a
    // glance, so anything that changes a row has to reach it too. It showed a
    // stale "currently showing" when a paired host switched a display, and
    // stale ports when one reported its own.
    await listen(ACTIVE_ROUTE_CHANGED_EVENT, () => void reloadState());
    await listen(PEER_INPUTS_CHANGED_EVENT, () => void reloadState());
    await listen(MONITOR_IDENTITIES_CHANGED_EVENT, () => void reloadState());
  } catch {
    // Preview mode has no Tauri backend; the static preview order never changes.
    return;
  }
  // The window is hidden rather than closed, so re-read hosts each time it opens.
  window.addEventListener("focus", () => {
    // A switch that never finished must not leave the window inert the next
    // time it is summoned; whatever it was still doing is superseded.
    batch += 1;
    switching = false;
    void reloadState();
  });
}

/** Re-reads hosts and their order, keeping the same target and host selected. */
async function reloadState(): Promise<void> {
  if (switching) return;
  if (await loadState()) render();
}

/** Reads what is known about the paired hosts, and asks them in the
 *  background. This window is summoned over whatever the user is doing, so it
 *  draws with the last answers at once and never waits for new ones. */
async function loadHostPresence(check: boolean): Promise<void> {
  try {
    const known = await invoke<HostPresence[]>(check ? "refresh_host_presence" : "get_host_presence");
    hostPresence = Object.fromEntries(known.map((presence) => [presence.peerId, presence]));
  } catch {
    // Preview mode, or one failed round: keep whatever was known.
    return;
  }
  if (!switching) {
    refreshRows(rows[selectedIndex]?.id);
    render();
  }
}

/** Fetches the latest hosts into the rows without drawing them; false when
 *  the backend could not answer. */
async function loadState(): Promise<boolean> {
  const keep = rows[selectedIndex]?.id;
  try {
    state = await invoke<HostSwitcherState>("get_host_switcher_state");
  } catch {
    // Keep showing the previous hosts; the next time the switcher opens it retries.
    return false;
  }
  void loadHostPresence(false).then(() => loadHostPresence(true));
  if (!targets().includes(target)) target = targets()[0] ?? ALL_DISPLAYS;
  refreshRows(keep);
  return true;
}

void initialize();
