import {
  Activity, AlertCircle, ChevronDown, ChevronUp, CircleHelp, createIcons, Download, ExternalLink, FlaskConical, Github, Info,
  GripVertical, KeyRound, Keyboard, Languages, LayoutGrid, Link, Monitor, MonitorDot, MonitorOff, Network, Pencil, PlugZap,
  Plus, Power, RefreshCw, RotateCcw, Save, Search, SunMoon, Trash2, TriangleAlert, UserRound, Zap,
} from "lucide";
import { getVersion } from "@tauri-apps/api/app";
import { Channel, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import packageMetadata from "../package.json";
import { locale, localePreference, setLocalePreference, t, type MessageKey } from "./i18n";
import { initDiagnostics } from "./diagnostics";
import { HOST_COLORS, HOST_ICONS, hostIconSet, hostLook, type CustomLook } from "./host-look";
import { appShellHtml, SETTINGS_TABS, type SettingsTab } from "./layout";
import { initializeTheme, setThemePreference } from "./theme";
import "./styles/tokens.css";
import "./styles/base.css";
import "./styles/shell.css";
import "./styles/switch-center.css";
import "./styles/settings.css";
import "./styles/overlays.css";

type Platform = "windows" | "mac";
type ResolutionSource = "edid" | "coreGraphicsDisplayMode" | "windowsDisplayMode";

interface MonitorResolution {
  width: number;
  height: number;
}

interface Fingerprint {
  manufacturer_id: string;
  product_code: string;
  serial_number: string | null;
}

type HostOutput = "hdmi" | "displayPort" | "usbC" | "thunderbolt" | "dvi" | "vga" | "indirect";
type SinkInterface = "hdmi" | "displayPort" | "dvi" | "vga" | "unknownDigital";
type DdcRisk = "low" | "elevated" | "unsupported";

interface MonitorConnection {
  hostOutput: HostOutput | null;
  hostPort?: string | null;
  sinkInterface: SinkInterface | null;
  sharesUsbData: boolean;
  signalConversion: boolean;
  ddcRisk: DdcRisk | null;
}

interface MonitorDescriptor {
  id: string;
  name: string;
  active: boolean;
  builtIn: boolean;
  fingerprint: Fingerprint;
  maxResolution?: MonitorResolution | null;
  resolutionSource?: ResolutionSource | null;
  connection?: MonitorConnection | null;
}

interface SelectedMonitor {
  name: string;
  fingerprint: Fingerprint;
  maxResolution?: MonitorResolution | null;
  resolutionSource?: ResolutionSource | null;
  localInput: number | null;
  supportedInputs: number[] | null;
  activeRoute?: string | null;
}

interface MonitorInputAssignment {
  monitor: Fingerprint;
  input: number;
}

interface HostRoute {
  id: string;
  name: string;
  platform: Platform;
  address: string;
  port: number;
  macAddress: string;
  inputs: MonitorInputAssignment[];
}

interface AppSettings {
  localHost: Platform;
  sharedMonitors: SelectedMonitor[];
  peers: HostRoute[];
  broadcastIp: string;
  wakePort: number;
  sharedKey: string;
  waitSeconds: number;
  autostart: boolean;
  checkUpdates: boolean;
  onboardingCompleted: boolean;
  hostSwitcherEnabled: boolean;
  hostSwitcherShortcut: string;
  diagnosticsEnabled?: boolean;
  diagnosticsAsked?: boolean;
}

interface SharedMonitorStatus {
  monitorKey: string;
  fingerprint: Fingerprint;
  name: string;
  ddcAvailable: boolean;
  /** "onOtherHost": unreadable because the display is showing a paired host, which is expected. */
  displayState: "ready" | "onOtherHost" | "unavailable";
  statusText: string;
  connection: MonitorConnection | null;
  connectionInputConflict: boolean;
}

interface MonitorIdentityClaim {
  aliasKey: string;
  aliasLabel: string;
  primaryKey: string;
  primaryLabel: string;
}

/** "This display present right now is probably another mode of that shared
 *  display", from the backend's curated table of multi-identity models. It only
 *  preselects the merge below; the declaration is still the user's. */
interface MergeSuggestion {
  monitorId: string;
  primaryKey: string;
  primaryLabel: string;
}

interface DashboardState {
  platform: string;
  localHost: Platform;
  agentConfigured: boolean;
  monitors: MonitorDescriptor[];
  uncontrollableMonitors: MonitorDescriptor[];
  shared: SharedMonitorStatus[];
  selectionNotices: string[];
  monitorIdentityClaims?: MonitorIdentityClaim[];
  mergeSuggestions?: MergeSuggestion[];
  resolvedMonitorIdentities: Record<string, string>;
  localHostName?: string;
}

/** What the backend's last check said about one paired host. */
interface HostPresence {
  peerId: string;
  online: boolean;
  /** Unix ms of the last completed check; 0 when nothing has asked yet. */
  checkedAtMs: number;
  /** Unix ms that host last answered; 0 when it never has. */
  lastSeenAtMs: number;
  /** `monitorKey` of every shared display that host says it can see, or null
   *  when it did not say — which is never the same as "it sees none". */
  attachedMonitors: string[] | null;
  /** Why the last check failed; empty while online. */
  detail: string;
}

interface DiscoveredPeer {
  id: string;
  name: string;
  platform: Platform;
  address: string;
  port: number;
  macAddress: string | null;
}

/** `name` includes the user's note; `baseName` and `label` are its parts. */
interface InputOption { value: number; name: string; baseName?: string; label?: string; }
interface OperationResult { title: string; detail: string; peerWoken: boolean; warning: boolean; }
interface ShortcutCheckResult { available: boolean; message: string; }
interface UpdateInfo { available: boolean; currentVersion: string; version: string | null; notes: string | null; }
interface ReleaseHistoryItem { date: string; version: string; url: string; }
interface GitHubRelease {
  tag_name?: unknown;
  published_at?: unknown;
  draft?: unknown;
  prerelease?: unknown;
}
type SwitchProgressEvent =
  | { event: "waking"; peerName: string }
  | { event: "checking"; peerName: string }
  | { event: "waiting"; peerName: string; seconds: number }
  | { event: "switching" }
  | { event: "remoteFallback"; peerName: string };
type UpdateDownloadEvent =
  | { event: "started"; contentLength: number | null }
  | { event: "progress"; downloaded: number; contentLength: number | null }
  | { event: "finished" };

const standardInputs: InputOption[] = [
  [0x01, "VGA"], [0x03, "DVI"], [0x0f, "DP"],
  [0x11, "HDMI 1"], [0x12, "HDMI 2"], [0x1b, "Type-C"],
].map(([value, name]) => ({ value: value as number, name: name as string }));
const MIN_SHARED_KEY_LENGTH = 15;
/** With this many shared displays, a stage for each no longer fits side by side. */
const MATRIX_MIN_DISPLAYS = 3;
/** With this many hosts, a row of host keys no longer fits under one display. */
const MATRIX_MIN_HOSTS = 4;

const previewSettings: AppSettings = {
  localHost: "windows", sharedMonitors: [], peers: [],
  broadcastIp: "255.255.255.255", wakePort: 9, sharedKey: "", waitSeconds: 45, autostart: true, checkUpdates: true,
  onboardingCompleted: false, hostSwitcherEnabled: false, hostSwitcherShortcut: "CommandOrControl+Alt+Space",
};
const previewDashboard: DashboardState = {
  platform: "windows", localHost: "windows", agentConfigured: false,
  monitors: [], uncontrollableMonitors: [], shared: [], selectionNotices: [],
  resolvedMonitorIdentities: {},
};

let settings = previewSettings;
let dashboard = previewDashboard;
let discoveredPeers: DiscoveredPeer[] = [];
let inputOptionsByMonitor: Record<string, InputOption[]> = {};
let isPreview = false;
let pendingUpdate: UpdateInfo | null = null;
let isRecordingShortcut = false;
let shortcutStatus: { kind: "checking" | "available" | "conflict"; text: string } | null = null;

const releaseHistoryFallback = [
  { date: "2026-09-22", version: "v0.8.1", url: "https://github.com/OmarHung/MuxSU/releases/tag/v0.8.1" },
  { date: "2026-09-22", version: "v0.8.0", url: "https://github.com/OmarHung/MuxSU/releases/tag/v0.8.0" },
  { date: "2026-09-20", version: "v0.7.2", url: "https://github.com/OmarHung/MuxSU/releases/tag/v0.7.2" },
  { date: "2026-09-20", version: "v0.7.1", url: "https://github.com/OmarHung/MuxSU/releases/tag/v0.7.1" },
  { date: "2026-09-19", version: "v0.7.0", url: "https://github.com/OmarHung/MuxSU/releases/tag/v0.7.0" },
  { date: "2026-09-19", version: "v0.6.0", url: "https://github.com/OmarHung/MuxSU/releases/tag/v0.6.0" },
  { date: "2026-09-19", version: "v0.5.0", url: "https://github.com/OmarHung/MuxSU/releases/tag/v0.5.0" },
  { date: "2026-09-18", version: "v0.4.1", url: "https://github.com/OmarHung/MuxSU/releases/tag/v0.4.1" },
  { date: "2026-09-18", version: "v0.4.0", url: "https://github.com/OmarHung/MuxSU/releases/tag/v0.4.0" },
  { date: "2026-09-18", version: "v0.3.0", url: "https://github.com/OmarHung/MuxSU/releases/tag/v0.3.0" },
  { date: "2026-09-17", version: "v0.2.0", url: "https://github.com/OmarHung/MuxSU/releases/tag/v0.2.0" },
  { date: "2026-09-17", version: "v0.1.11", url: "https://github.com/OmarHung/MuxSU/releases/tag/v0.1.11" },
  { date: "2026-09-16", version: "v0.1.10", url: "https://github.com/OmarHung/MuxSU/releases/tag/v0.1.10" },
  { date: "2026-09-16", version: "v0.1.9", url: "https://github.com/OmarHung/MuxSU/releases/tag/v0.1.9" },
  { date: "2026-09-16", version: "v0.1.8", url: "https://github.com/OmarHung/MuxSU/releases/tag/v0.1.8" },
  { date: "2026-09-15", version: "v0.1.7", url: "https://github.com/OmarHung/MuxSU/releases/tag/v0.1.7" },
  { date: "2026-09-14", version: "v0.1.6", url: "https://github.com/HenryHsu/DisplayMux/releases/tag/v0.1.6" },
  { date: "2026-09-14", version: "v0.1.5", url: "https://github.com/HenryHsu/DisplayMux/releases/tag/v0.1.5" },
  { date: "2026-09-14", version: "v0.1.4", url: "https://github.com/HenryHsu/DisplayMux/releases/tag/v0.1.4" },
  { date: "2026-09-13", version: "v0.1.3", url: "https://github.com/HenryHsu/DisplayMux/releases/tag/v0.1.3" },
  { date: "2026-09-12", version: "v0.1.2", url: "https://github.com/HenryHsu/DisplayMux/releases/tag/v0.1.2" },
  { date: "2026-09-11", version: "v0.1.1", url: "https://github.com/HenryHsu/DisplayMux/releases/tag/v0.1.1" },
  { date: "2026-09-11", version: "v0.1.0", url: "https://github.com/HenryHsu/DisplayMux/releases/tag/v0.1.0" },
] satisfies ReleaseHistoryItem[];

const releaseUrl = (version: string) => `https://github.com/OmarHung/MuxSU/releases/tag/${version}`;

const onboardingSteps: readonly {
  label: string; title: string; body: string; page: string; tab?: SettingsTab; target: string; placement: string;
}[] = [
  {
    label: t("onboarding.stepWelcome"),
    title: t("onboarding.dashboardTitle"),
    body: t("onboarding.dashboardBody"),
    page: "dashboard",
    target: "#switch-panel > :first-child",
    placement: "bottom",
  },
  {
    label: t("onboarding.stepSettings"),
    title: t("onboarding.settingsTitle"),
    body: t("onboarding.settingsBody"),
    page: "dashboard",
    target: '[data-page="settings"]',
    placement: "bottom",
  },
  {
    label: t("onboarding.stepDisplay"),
    title: t("onboarding.displayTitle"),
    body: t("onboarding.displayBody"),
    page: "settings",
    tab: "displays",
    target: ".form-section.first",
    placement: "bottom",
  },
  {
    label: t("onboarding.stepPairing"),
    title: t("onboarding.pairingTitle"),
    body: t("onboarding.pairingBody"),
    page: "settings",
    tab: "hosts",
    target: ".pairing-section",
    placement: "bottom",
  },
  {
    label: t("onboarding.stepFinish"),
    title: t("onboarding.finishTitle"),
    body: t("onboarding.finishBody"),
    page: "settings",
    tab: "hosts",
    // The whole section, not the password row: a row sits inside a clipped
    // glass card, which would cut off the highlight and keep it under the scrim.
    target: ".pairing-key-section",
    placement: "top",
  },
];

let onboardingStep = 0;

const app = document.querySelector<HTMLDivElement>("#app");
if (!app) throw new Error(t("app.rootMissing"));
document.documentElement.lang = locale;
const themePreference = initializeTheme();

app.innerHTML = appShellHtml({ releaseRows: releaseHistoryRows(releaseHistoryFallback), minSharedKeyLength: MIN_SHARED_KEY_LENGTH });

const iconSet = {
  Activity, AlertCircle, ChevronDown, ChevronUp, CircleHelp, Download, ExternalLink, FlaskConical, Github, GripVertical, Info, KeyRound, Keyboard,
  Languages, LayoutGrid, Link, Monitor, MonitorDot, MonitorOff, Network, Pencil, PlugZap, Plus, Power, RefreshCw, Save, Search,
  SunMoon, RotateCcw, Trash2, TriangleAlert, UserRound, Zap, ...hostIconSet,
};
const refreshIcons = () => createIcons({ icons: iconSet });
refreshIcons();

document.querySelectorAll<HTMLButtonElement>("[data-page]").forEach((button) => button.addEventListener("click", () => showPage(button.dataset.page ?? "dashboard")));
document.querySelectorAll<HTMLButtonElement>("[data-settings-tab]").forEach((button) => button.addEventListener("click", () => {
  const tab = SETTINGS_TABS.find((item) => item === button.dataset.settingsTab);
  if (tab) showSettingsTab(tab);
}));
document.querySelector("#view-toggle")?.addEventListener("click", (event) => {
  const view = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-switch-view]")?.dataset.switchView;
  if (view === "stage" || view === "matrix") setSwitchView(view);
});
document.querySelector<HTMLButtonElement>("#refresh-button")?.addEventListener("click", () => void refresh());
document.querySelector<HTMLButtonElement>("#update-button")?.addEventListener("click", () => pendingUpdate ? showUpdateDialog(pendingUpdate) : void checkForUpdates(true));
document.querySelector<HTMLButtonElement>("#update-cancel")?.addEventListener("click", hideUpdateDialog);
document.querySelector<HTMLButtonElement>("#update-install")?.addEventListener("click", () => void installUpdate());
document.querySelector<HTMLButtonElement>("#onboarding-restart")?.addEventListener("click", () => showOnboarding(0));
document.querySelector<HTMLButtonElement>("#onboarding-previous")?.addEventListener("click", () => {
  if (onboardingStep > 0) showOnboarding(onboardingStep - 1);
});
document.querySelector<HTMLButtonElement>("#onboarding-next")?.addEventListener("click", () => {
  if (onboardingStep < onboardingSteps.length - 1) showOnboarding(onboardingStep + 1);
  else void completeOnboarding(true);
});
document.querySelector<HTMLButtonElement>("#onboarding-skip")?.addEventListener("click", () => void completeOnboarding(false));
document.querySelector<HTMLButtonElement>('[data-page="settings"]')?.addEventListener("click", () => {
  if (document.querySelector("#onboarding-overlay")?.classList.contains("is-visible") && onboardingStep === 1) {
    showOnboarding(2);
  }
});
window.addEventListener("resize", () => positionOnboardingTooltip());
document.querySelector(".workspace")?.addEventListener("scroll", () => positionOnboardingTooltip());
document.querySelector<HTMLButtonElement>("#scan-button")?.addEventListener("click", () => void scanPeers());
document.addEventListener("click", (event) => {
  const link = (event.target as HTMLElement).closest<HTMLAnchorElement>("a[data-external-url]");
  if (!link) return;
  event.preventDefault();
  void openExternalUrl(link.href);
});
const languageSelect = document.querySelector<HTMLSelectElement>("#language-select");
if (languageSelect) {
  languageSelect.value = localePreference;
  languageSelect.addEventListener("change", () => {
    if (setLocalePreference(languageSelect.value)) window.location.reload();
  });
}

const themeSelect = document.querySelector<HTMLSelectElement>("#theme-select");
if (themeSelect) {
  themeSelect.value = themePreference;
  themeSelect.addEventListener("change", () => {
    if (!setThemePreference(themeSelect.value)) themeSelect.value = themePreference;
  });
}
/** The controls that wait for the save button. Everything else on the settings
 *  page has its own command and is saved as it is changed. */
const SAVE_ON_SUBMIT_FIELDS = [
  "#shared-key",
  "#wait-seconds",
  "#autostart",
  "#check-updates",
  "#host-switcher-enabled",
] as const;

const diagnostics = initDiagnostics({
  settings: () => settings,
  adoptSettings: (next: AppSettings) => { settings = next; },
  isPreview: () => isPreview,
  notify: showToast,
  withBusyButton,
});
document.querySelector<HTMLFormElement>("#settings-form")?.addEventListener("submit", (event) => void saveSettings(event));
// The form spans every settings tab, and the browser cannot point at a field
// on a hidden one: saving from another tab would just do nothing. Opening the
// field's tab first lets the browser show which one to fix.
document.querySelector<HTMLFormElement>("#settings-form")?.addEventListener("invalid", (event) => {
  const panel = (event.target as HTMLElement).closest<HTMLElement>("[data-settings-panel]");
  const tab = SETTINGS_TABS.find((item) => item === panel?.dataset.settingsPanel);
  if (tab && !panel?.classList.contains("is-active")) showSettingsTab(tab);
}, true);
// Most of this page saves as it is changed; these few do not, and nothing said
// so. Named one by one rather than watching the whole form, so a control that
// saves itself never raises a warning that its change is waiting.
for (const selector of SAVE_ON_SUBMIT_FIELDS) {
  document.querySelector(selector)?.addEventListener("change", markUnsaved);
  document.querySelector(selector)?.addEventListener("input", markUnsaved);
}
document.querySelector("#discard-button")?.addEventListener("click", discardUnsavedChanges);
document.querySelector<HTMLInputElement>("#host-switcher-enabled")?.addEventListener("change", () => {
  renderShortcutSetting();
  if (document.querySelector<HTMLInputElement>("#host-switcher-enabled")?.checked) {
    void checkShortcutConflict(settings.hostSwitcherShortcut);
  }
});
document.querySelector<HTMLButtonElement>("#shortcut-recorder")?.addEventListener("click", beginShortcutRecording);
document.addEventListener("keydown", captureShortcut, true);
document.querySelector("#monitor-picker")?.addEventListener("click", (event) => {
  const button = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-monitor-id]");
  const monitorId = button?.dataset.monitorId;
  const busyKey = button?.dataset.busyKey;
  if (!button || !monitorId || !busyKey) return;
  const selected = button.dataset.monitorSelected === "true";
  void withBusyDisplay(busyKey, () => (selected ? removeSharedMonitor(monitorId) : addSharedMonitor(monitorId)));
});
document.querySelector("#display-maintenance")?.addEventListener("click", (event) => {
  const button = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-maintain]");
  const monitorKey = button?.dataset.monitorKey;
  if (!button || !monitorKey) return;
  void withBusyButton(button, () => maintainDisplay(button.dataset.maintain ?? "", monitorKey));
});
document.querySelector("#local-input-summary")?.addEventListener("change", (event) => {
  const field = (event.target as HTMLElement).closest<HTMLSelectElement>("[data-local-input]");
  if (field?.dataset.localInput) void commitLocalInput(field.dataset.localInput, field.value);
});
document.querySelector("#settings-form")?.addEventListener("click", (event) => {
  const button = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-reset-scope]");
  if (button?.dataset.resetScope) requestReset(button.dataset.resetScope, button);
});
document.querySelector("#monitor-merge")?.addEventListener("click", (event) => {
  const target = event.target as HTMLElement;
  const undo = target.closest<HTMLButtonElement>("[data-unmerge-alias]");
  const undoAlias = undo?.dataset.unmergeAlias;
  if (undo && undoAlias) {
    void withBusyButton(undo, () => unmergeSharedMonitor(undoAlias));
    return;
  }
  const button = target.closest<HTMLButtonElement>("[data-merge-alias]");
  const aliasId = button?.dataset.mergeAlias;
  if (!button || !aliasId) return;
  const select = document.querySelector<HTMLSelectElement>(`[data-merge-target="${CSS.escape(aliasId)}"]`);
  const primaryId = select?.value;
  if (primaryId) void withBusyButton(button, () => mergeSharedMonitor(aliasId, primaryId));
});
document.querySelector("#peer-list")?.addEventListener("click", (event) => {
  const button = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-add-peer]");
  if (button?.dataset.addPeer) void addPeer(button.dataset.addPeer);
});
const inputLabels = document.querySelector<HTMLElement>("#input-labels");
inputLabels?.addEventListener("change", (event) => {
  const field = (event.target as HTMLElement).closest<HTMLInputElement>("[data-label-input]");
  if (field) void commitInputLabel(field);
});
inputLabels?.addEventListener("keydown", (event) => {
  const field = (event.target as HTMLElement).closest<HTMLInputElement>("[data-label-input]");
  if (!field) return;
  if (event.key === "Enter") {
    // Inside the settings form, Enter would otherwise submit every setting.
    event.preventDefault();
    field.blur();
  } else if (event.key === "Escape") {
    field.value = inputOptionsByMonitor[field.dataset.labelMonitor ?? ""]?.find((option) => option.value === Number(field.dataset.labelInput))?.label ?? "";
    field.blur();
  }
});
inputLabels?.addEventListener("focusout", () => {
  window.setTimeout(() => {
    if (inputNamesReloadPending && !inputLabels.contains(document.activeElement)) {
      renderInputNames();
    }
  }, 0);
});
/** Hosts are named, ordered and diagnosed from one list on the hosts tab. */
const hostList = document.querySelector<HTMLElement>("#host-list");
hostList?.addEventListener("click", (event) => {
  const target = event.target as HTMLElement;
  const lookButton = target.closest<HTMLButtonElement>("[data-edit-look]");
  if (lookButton?.dataset.editLook) {
    toggleLookEditor(lookButton.dataset.editLook);
    return;
  }
  if (target.closest("[data-close-look]")) {
    toggleLookEditor(null);
    return;
  }
  const lookChoice = target.closest<HTMLButtonElement>("[data-look-route]");
  const lookRoute = lookChoice?.dataset.lookRoute;
  if (lookChoice && lookRoute) {
    if (lookChoice.dataset.lookIcon) void setHostLook(lookRoute, { icon: lookChoice.dataset.lookIcon });
    else if (lookChoice.dataset.lookColor) void setHostLook(lookRoute, { color: lookChoice.dataset.lookColor });
    else if (lookChoice.hasAttribute("data-look-reset")) void setHostLook(lookRoute, "reset");
    return;
  }
  const moveButton = target.closest<HTMLButtonElement>("[data-move-route]");
  if (moveButton?.dataset.moveRoute) moveRouteBy(moveButton.dataset.moveRoute, Number(moveButton.dataset.moveOffset));
  const renameButton = target.closest<HTMLButtonElement>("[data-rename-route]");
  if (renameButton?.dataset.renameRoute) startRenaming(renameButton.dataset.renameRoute);
  const peerButton = target.closest<HTMLButtonElement>("[data-remove-peer], [data-probe-id], [data-wake-id]");
  if (peerButton?.dataset.removePeer) void removePeer(peerButton.dataset.removePeer);
  if (peerButton?.dataset.probeId) void peerCommand("probe_peer", peerButton.dataset.probeId);
  if (peerButton?.dataset.wakeId) void peerCommand("wake_peer", peerButton.dataset.wakeId);
});
hostList?.addEventListener("input", (event) => {
  const field = event.target as HTMLInputElement;
  if (field.dataset.renameInput && renaming?.routeId === field.dataset.renameInput) {
    renaming = { ...renaming, draft: field.value };
  }
});
hostList?.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && editingLook && (event.target as HTMLElement).closest(".look-editor, [data-edit-look]")) {
    event.preventDefault();
    const routeId = editingLook;
    editingLook = null;
    renderHostList();
    refreshIcons();
    document.querySelector<HTMLButtonElement>(`[data-edit-look="${cssEscape(routeId)}"]`)?.focus();
    return;
  }
  const field = event.target as HTMLInputElement;
  if (!field.dataset.renameInput) return;
  if (event.key === "Enter") {
    // Inside the settings form, Enter would otherwise submit every setting.
    event.preventDefault();
    void commitRename(field.dataset.renameInput);
  } else if (event.key === "Escape") {
    event.preventDefault();
    stopRenaming(field.dataset.renameInput);
  }
});
hostList?.addEventListener("focusout", (event) => {
  const field = event.target as HTMLInputElement;
  if (field.dataset.renameInput) void commitRename(field.dataset.renameInput);
});
hostList?.addEventListener("pointerdown", (event) => {
  const card = (event.target as HTMLElement).closest<HTMLElement>("[data-route-card]");
  if (card) card.draggable = Boolean((event.target as HTMLElement).closest("[data-drag-handle]"));
});
document.addEventListener("pointerup", () => {
  hostList?.querySelectorAll<HTMLElement>("[data-route-card]").forEach((card) => { card.draggable = false; });
});
hostList?.addEventListener("dragstart", (event) => {
  const card = (event.target as HTMLElement).closest<HTMLElement>("[data-route-card]");
  // Only a card the grip armed reorders. Dragging selected text also raises
  // this, and that must not quietly move a host.
  if (!card?.draggable || !card.dataset.routeCard || !event.dataTransfer) return;
  draggedRouteId = card.dataset.routeCard;
  event.dataTransfer.effectAllowed = "move";
  event.dataTransfer.setData("text/plain", draggedRouteId);
  card.classList.add("is-dragging");
});
hostList?.addEventListener("dragover", (event) => {
  const card = (event.target as HTMLElement).closest<HTMLElement>("[data-route-card]");
  if (!draggedRouteId || !card) return;
  event.preventDefault();
  if (event.dataTransfer) event.dataTransfer.dropEffect = "move";
  hostList.querySelectorAll(".is-drop-target").forEach((item) => item.classList.remove("is-drop-target"));
  if (card.dataset.routeCard !== draggedRouteId) card.classList.add("is-drop-target");
});
hostList?.addEventListener("drop", (event) => {
  const card = (event.target as HTMLElement).closest<HTMLElement>("[data-route-card]");
  if (!draggedRouteId || !card?.dataset.routeCard) return;
  event.preventDefault();
  const order = currentRouteIds();
  const next = movedRoute(order, draggedRouteId, order.indexOf(card.dataset.routeCard));
  if (next.join() !== order.join()) void saveRouteOrder(next);
});
hostList?.addEventListener("dragend", () => {
  draggedRouteId = null;
  hostList.querySelectorAll<HTMLElement>("[data-route-card]").forEach((card) => { card.draggable = false; });
  hostList.querySelectorAll(".is-dragging, .is-drop-target").forEach((item) => item.classList.remove("is-dragging", "is-drop-target"));
});
const switchAllClick = (event: Event) => {
  const button = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-switch-all-id]");
  if (button?.dataset.switchAllId) void switchAllToHost(button.dataset.switchAllId);
};
document.querySelector("#switch-all-bar")?.addEventListener("click", switchAllClick);
const switchPanel = document.querySelector<HTMLElement>("#switch-panel");
switchPanel?.addEventListener("click", switchAllClick);
switchPanel?.addEventListener("click", (event) => {
  const target = event.target as HTMLElement;
  if (target.closest("[data-open-displays]")) {
    showPage("settings");
    showSettingsTab("displays");
    return;
  }
  const button = target.closest<HTMLButtonElement>("[data-switch-id]");
  if (!button?.dataset.switchId) return;
  const card = button.closest<HTMLElement>("[data-monitor-key]");
  if (card?.dataset.monitorKey) void switchHost(card.dataset.monitorKey, button.dataset.switchId);
});

function showPage(page: string): void {
  document.querySelectorAll(".page").forEach((item) => item.classList.remove("is-active"));
  document.querySelector(`#${page}-page`)?.classList.add("is-active");
  document.querySelectorAll("[data-page]").forEach((item) => item.classList.toggle("is-active", (item as HTMLElement).dataset.page === page));
  refreshIcons();
}

function showSettingsTab(tab: SettingsTab): void {
  document.querySelectorAll<HTMLElement>("[data-settings-panel]").forEach((panel) => panel.classList.toggle("is-active", panel.dataset.settingsPanel === tab));
  document.querySelectorAll<HTMLElement>("[data-settings-tab]").forEach((button) => {
    const isActive = button.dataset.settingsTab === tab;
    button.classList.toggle("is-active", isActive);
    if (isActive) button.setAttribute("aria-current", "page"); else button.removeAttribute("aria-current");
  });
  document.querySelector(".workspace")?.scrollTo({ top: 0 });
}

type SwitchView = "stage" | "matrix";
const switchViewStorageKey = "muxsu.switchView";

/** The layout the user picked, if any. Without one, the switch center picks
 *  the matrix once a stage per display would no longer fit side by side. */
function storedSwitchView(): SwitchView | null {
  try {
    const stored = localStorage.getItem(switchViewStorageKey);
    return stored === "stage" || stored === "matrix" ? stored : null;
  } catch {
    return null;
  }
}

function currentSwitchView(): SwitchView {
  const stored = storedSwitchView();
  if (stored) return stored;
  const hostCount = settings.peers.length + 1;
  return dashboard.shared.length >= MATRIX_MIN_DISPLAYS || hostCount >= MATRIX_MIN_HOSTS ? "matrix" : "stage";
}

function setSwitchView(view: SwitchView): void {
  try { localStorage.setItem(switchViewStorageKey, view); } catch { /* The choice just won't outlive this window. */ }
  renderSwitchPanel();
  refreshIcons();
}

function releaseHistoryRows(releases: ReleaseHistoryItem[]): string {
  return releases.map((release) => `
    <tr>
      <td>${escapeHtml(release.date)}</td>
      <td><code>${escapeHtml(release.version)}</code></td>
      <td><a href="${escapeHtml(release.url)}" data-external-url>${t("help.viewRelease")}<i data-lucide="external-link"></i></a></td>
    </tr>
  `).join("");
}

async function refreshReleaseHistory(): Promise<void> {
  try {
    const response = await fetch("https://api.github.com/repos/OmarHung/MuxSU/releases?per_page=30", {
      headers: { Accept: "application/vnd.github+json" },
    });
    if (!response.ok) throw new Error(`GitHub Releases API returned ${response.status}`);
    const payload: unknown = await response.json();
    if (!Array.isArray(payload)) throw new Error("GitHub Releases API returned an invalid response");

    const seen = new Set<string>();
    const releases = (payload as GitHubRelease[]).flatMap((release): ReleaseHistoryItem[] => {
      const version = typeof release.tag_name === "string" ? release.tag_name : "";
      const publishedAt = typeof release.published_at === "string" ? release.published_at : "";
      if (release.draft === true || release.prerelease === true || !/^v\d+\.\d+\.\d+$/.test(version) || !/^\d{4}-\d{2}-\d{2}T/.test(publishedAt) || seen.has(version)) return [];
      seen.add(version);
      return [{ date: publishedAt.slice(0, 10), version, url: releaseUrl(version) }];
    });
    releases.sort((left, right) => compareReleaseVersions(right.version, left.version));
    if (!releases.length) return;

    const body = document.querySelector<HTMLTableSectionElement>("#release-history-body");
    if (body) {
      body.innerHTML = releaseHistoryRows(releases);
      refreshIcons();
    }
  } catch {
    // Keep the bundled history available when GitHub is unreachable or rate-limited.
  }
}

function compareReleaseVersions(left: string, right: string): number {
  const leftParts = left.slice(1).split(".").map(Number);
  const rightParts = right.slice(1).split(".").map(Number);
  for (let index = 0; index < 3; index += 1) {
    const difference = (leftParts[index] ?? 0) - (rightParts[index] ?? 0);
    if (difference !== 0) return difference;
  }
  return 0;
}

async function openExternalUrl(value: string): Promise<void> {
  try {
    const url = new URL(value);
    const repositoryPaths = ["/OmarHung/MuxSU", "/HenryHsu/DisplayMux"];
    const isAllowedPath = repositoryPaths.some((path) => url.pathname === path || url.pathname.startsWith(`${path}/`));
    if (url.protocol !== "https:" || url.hostname !== "github.com" || !isAllowedPath) {
      throw new Error("unsupported external URL");
    }
    if (isPreview) {
      window.open(url.href, "_blank", "noopener,noreferrer");
    } else {
      await openUrl(url.href);
    }
  } catch (error) {
    showToast(t("toast.openLinkFailed"), String(error), true);
  }
}

async function loadInputOptionsByMonitor(monitorKeys: string[]): Promise<Record<string, InputOption[]>> {
  const entries = await Promise.all(monitorKeys.map(async (monitorKey) =>
    [monitorKey, await invoke<InputOption[]>("get_input_options", { monitorId: monitorKey })] as const,
  ));
  return Object.fromEntries(entries);
}

/** Minimum gap between automatic rescans; each one issues DDC/CI reads. */
const FOCUS_REFRESH_INTERVAL_MS = 10_000;
/** How often the window re-scans on its own. A scan reads capabilities and
 *  retries DDC, so this trades freshness against talking to the displays
 *  constantly; `FOCUS_REFRESH_INTERVAL_MS` still floors the actual rate. */
const PERIODIC_REFRESH_INTERVAL_MS = 15_000;
/** Emitted by the backend when a paired host reports a switch. */
const ACTIVE_ROUTE_CHANGED_EVENT = "active-route-changed";
/** Emitted by the backend when this or a paired host saves a new host card order. */
const HOST_ORDER_CHANGED_EVENT = "host-order-changed";
/** Route ids ("local" and peer ids) in the saved host card order. */
let routeOrder: string[] = [];
/** Emitted by the backend when this or a paired host renames a host. */
const HOST_NAMES_CHANGED_EVENT = "host-names-changed";
/** Emitted by the backend when this or a paired host changes an input note. */
const INPUT_LABELS_CHANGED_EVENT = "input-labels-changed";
/** How often the window asks every paired host whether it is still up. Each
 *  check waits out a sleeping host's connect timeout, so this is deliberately
 *  slow: often enough to notice a host going down, quiet enough to leave the
 *  network alone. */
const PRESENCE_REFRESH_INTERVAL_MS = 30_000;
/** What the last check said about each paired host, keyed by peer id. */
let hostPresence: Record<string, HostPresence> = {};
/** Emitted by the backend when a paired host reports the port it occupies. */
const PEER_INPUTS_CHANGED_EVENT = "peer-inputs-changed";
/** A paired host declared two display identities to be one display. */
const MONITOR_IDENTITIES_CHANGED_EVENT = "monitor-identities-changed";
/** Longest input note the backend accepts, in characters. */
const MAX_INPUT_LABEL_CHARS = 24;
/** Longest custom host name the backend accepts, in characters. */
const MAX_HOST_NAME_CHARS = 32;
/** Custom host names by route id; hosts using their default name are absent. */
let hostNames: Record<string, string> = {};
/** Custom icons and colours by route id; hosts at their default are absent. */
let hostAppearances: Record<string, CustomLook> = {};
/** The host whose icon and colour picker is open. */
let editingLook: string | null = null;
/** Emitted by the backend when this or a paired host changes a host's icon or colour. */
const HOST_APPEARANCES_CHANGED_EVENT = "host-appearances-changed";
/** The host card whose name is being edited, and the unsaved text. */
let renaming: { routeId: string; draft: string } | null = null;
let isRefreshing = false;
let lastRefreshAt = 0;
let inputNamesReloadPending = false;

async function refresh(): Promise<void> {
  isRefreshing = true;
  lastRefreshAt = Date.now();
  document.querySelector("#refresh-button svg")?.classList.add("is-spinning");
  try {
    dashboard = await invoke<DashboardState>("get_dashboard_state");
    [settings, inputOptionsByMonitor, routeOrder, hostNames, hostAppearances] = await Promise.all([
      invoke<AppSettings>("get_settings"),
      loadInputOptionsByMonitor(dashboard.shared.map((shared) => shared.monitorKey)),
      invoke<string[]>("get_host_order"),
      invoke<Record<string, string>>("get_host_names"),
      invoke<Record<string, CustomLook>>("get_host_appearances"),
    ]);
    try { discoveredPeers = await invoke<DiscoveredPeer[]>("discover_peers"); } catch { discoveredPeers = []; }
    isPreview = false;
    // Catch up on host names and order changed while a paired host was offline.
    // Throttled and run in the background by the backend; results arrive as events.
    void invoke("exchange_host_layout").catch((error: unknown) => showToast(t("toast.hostNameFailed"), String(error), true));
    // Draws with what is already known, then asks the hosts in the background.
    void loadHostPresence(false).then(() => loadHostPresence(true));
  } catch {
    dashboard = previewDashboard; settings = previewSettings; inputOptionsByMonitor = {}; discoveredPeers = []; isPreview = true;
  } finally {
    isRefreshing = false;
    document.querySelector("#refresh-button svg")?.classList.remove("is-spinning");
  }
  renderState();
  if (!isPreview && dashboard.selectionNotices.length) {
    showToast(t("toast.selectionUpdated"), dashboard.selectionNotices.join(" "));
  }
}

/**
 * Rescans when the window comes back into view, so a switch made from the
 * display's own buttons shows up without pressing refresh. Only the dashboard
 * rescans: a full render would discard unsaved edits on the settings page.
 */
/** Says that a change is waiting for the save button, since the same page
 *  saves most things without one and the difference is invisible otherwise. */
function markUnsaved(): void {
  setUnsavedVisible(true);
}

function setUnsavedVisible(visible: boolean): void {
  const actions = document.querySelector<HTMLElement>("#form-actions");
  if (actions) actions.hidden = !visible;
  if (visible) refreshIcons();
}

/** Puts the controls that wait for the save button back to what was saved, so
 *  a change can be taken back without knowing what it used to be. */
function discardUnsavedChanges(): void {
  setInput("#shared-key", settings.sharedKey);
  setInput("#wait-seconds", String(settings.waitSeconds));
  const autostart = document.querySelector<HTMLInputElement>("#autostart");
  if (autostart) autostart.checked = settings.autostart;
  const checkUpdates = document.querySelector<HTMLInputElement>("#check-updates");
  if (checkUpdates) checkUpdates.checked = settings.checkUpdates;
  const hostSwitcherEnabled = document.querySelector<HTMLInputElement>("#host-switcher-enabled");
  if (hostSwitcherEnabled) hostSwitcherEnabled.checked = settings.hostSwitcherEnabled;
  renderShortcutSetting();
  setUnsavedVisible(false);
}

function refreshOnReturn(): void {
  if (isPreview || isRefreshing || document.visibilityState !== "visible") return;
  if (Date.now() - lastRefreshAt < FOCUS_REFRESH_INTERVAL_MS) return;
  // Never while a field is being edited: a re-render would take the text with
  // it. The next tick picks it up once the field is left.
  if (isEditingAField()) return;
  void refresh();
}

/** Whether the user is part-way through typing something a re-render would
 *  discard — a host name, an input note, or a pairing password. */
function isEditingAField(): boolean {
  const active = document.activeElement;
  return active instanceof HTMLInputElement || active instanceof HTMLSelectElement;
}

/** Displays change without the app being told: a cable is moved, a display
 *  sleeps, a display mode is switched, a paired host takes one over. None of
 *  that raises an event here, so what the window shows is only ever as fresh
 *  as the last thing the user did. */
function startPeriodicRefresh(): void {
  window.setInterval(refreshOnReturn, PERIODIC_REFRESH_INTERVAL_MS);
}

/** Re-reads only which host is active; no DDC scan and no settings form re-render. */
async function reloadActiveRoutes(): Promise<void> {
  try {
    const latest = await invoke<AppSettings>("get_settings");
    settings = { ...settings, sharedMonitors: latest.sharedMonitors };
    renderSwitchPanel();
    refreshIcons();
  } catch (error) {
    showToast(t("toast.activeHostSyncFailed"), String(error), true);
  }
  scheduleSettledRescans();
}

/** Re-reads the saved peer inputs after a paired host reported the port it
 *  occupies. */
async function reloadPeerInputs(): Promise<void> {
  try {
    const latest = await invoke<AppSettings>("get_settings");
    settings = { ...settings, peers: latest.peers };
    renderHostList();
    renderInputNames();
    refreshIcons();
  } catch (error) {
    showToast(t("toast.peerInputSyncFailed"), String(error), true);
  }
}

/** Reads what is known about every paired host, asking them first when
 *  `check` is set.
 *
 *  Never awaited by anything the user is waiting on: a host that is asleep
 *  answers only once its connect timeout runs out.
 */
async function loadHostPresence(check: boolean): Promise<void> {
  if (isPreview) return;
  try {
    const known = await invoke<HostPresence[]>(check ? "refresh_host_presence" : "get_host_presence");
    hostPresence = Object.fromEntries(known.map((presence) => [presence.peerId, presence]));
  } catch (error) {
    // One failed round says nothing about the hosts, so the last answers stay.
    // It is still worth saying: every host would otherwise sit at "not checked
    // yet" for the rest of the session with nothing to explain why.
    if (check) showToast(t("toast.presenceFailed"), String(error), true);
    return;
  }
  renderPresence();
}

/** Redraws only what a presence answer changes. The host list is left alone
 *  while a field is being edited there, since a re-render would take the text
 *  with it. */
function renderPresence(): void {
  renderSwitchPanel();
  if (!isEditingAField()) renderHostList();
  refreshIcons();
}

/** Hosts go down without telling anybody, so their standing is only ever as
 *  fresh as the last check. Checks run only while the window is on screen: a
 *  window in the tray has nobody to show an answer to. */
function startPresenceChecks(): void {
  window.setInterval(() => {
    if (isPreview || document.visibilityState !== "visible") return;
    void loadHostPresence(true);
  }, PRESENCE_REFRESH_INTERVAL_MS);
}

/** The line under the title: how many displays and hosts, and whether a
 *  sleeping host can be woken. Also the counts beside the settings tabs. */
function renderSummary(): void {
  const hostCount = settings.peers.length + 1;
  const parts = [t("dashboard.summary", { displays: dashboard.shared.length, hosts: hostCount })];
  if (settings.peers.length) {
    const wake = settings.peers.some((peer) => peer.macAddress) ? t("dashboard.wakeNormal") : t("dashboard.noMac");
    parts.push(`${t("dashboard.wakeLabel")}${locale === "en" ? " " : ""}${wake}`);
  }
  setText("#dashboard-summary", dashboard.shared.length ? parts.join(" · ") : t("dashboard.notSelected"));
  setText("#count-displays", dashboard.shared.length ? String(dashboard.shared.length) : "");
  setText("#count-hosts", settings.peers.length ? String(settings.peers.length) : "");
}

/**
 * When to rescan after a switch. Displays take a few seconds to change input
 * and keep answering (or not answering) DDC/CI as before until they do, so an
 * immediate scan shows the old readiness.
 */
const SETTLED_RESCAN_DELAYS_MS = [3_000, 8_000];
let settledRescanTimers: number[] = [];

function scheduleSettledRescans(): void {
  settledRescanTimers.forEach((timer) => window.clearTimeout(timer));
  settledRescanTimers = SETTLED_RESCAN_DELAYS_MS.map((delay) => window.setTimeout(() => void rescanDisplays(), delay));
}

/**
 * Rescans displays and redraws only the dashboard's display cards, so unsaved
 * edits on the settings page survive.
 */
async function rescanDisplays(): Promise<void> {
  if (isPreview || isRefreshing) return;
  isRefreshing = true;
  lastRefreshAt = Date.now();
  try {
    // The scan can move the active host, so read settings after it.
    dashboard = await invoke<DashboardState>("get_dashboard_state");
    const latest = await invoke<AppSettings>("get_settings");
    settings = { ...settings, sharedMonitors: latest.sharedMonitors };
    renderSummary();
    renderSwitchPanel();
    refreshIcons();
  } catch (error) {
    showToast(t("toast.activeHostSyncFailed"), String(error), true);
  } finally {
    isRefreshing = false;
  }
}

function selectedMonitorFor(shared: SharedMonitorStatus): SelectedMonitor | undefined {
  return settings.sharedMonitors.find((sm) => sameDisplay(sm.fingerprint, shared.fingerprint));
}

/** A display's resolution belongs to the display mode it is in right now, not
 *  to the snapshot taken when it was selected, so a live reading wins. The
 *  stored value is only a last-known fallback for a display nothing can see:
 *  it is refreshed solely while the display answers DDC/CI, so it outlives the
 *  mode — and, on displays that change identity with the mode, the reading it
 *  was taken for. Displays showing another host are included, since they are
 *  still enumerated even though they cannot be read. */
function resolutionFor(shared: SharedMonitorStatus): { resolution: MonitorResolution | null; source: ResolutionSource | null } {
  const live = [...dashboard.monitors, ...(dashboard.uncontrollableMonitors ?? [])]
    .find((monitor) => sameDisplay(monitor.fingerprint, shared.fingerprint));
  if (live?.maxResolution) {
    return { resolution: live.maxResolution, source: live.resolutionSource ?? null };
  }
  const selectedMonitor = selectedMonitorFor(shared);
  return {
    resolution: selectedMonitor?.maxResolution ?? null,
    source: selectedMonitor?.resolutionSource ?? null,
  };
}

/** Aspect ratio and resolution, as the switch center labels a display. */
function specText(resolution: MonitorResolution | null, isUltrawide: boolean): string {
  const ratio = isUltrawide ? "21:9" : "16:9";
  return resolution ? `${ratio} · ${resolution.width}×${resolution.height}` : ratio;
}

interface SwitchRoute {
  id: string; name: string; platform: Platform; local: boolean;
  /** Lucide icon and colour name it wears everywhere. */
  icon: string; color: string;
  customIcon: string | null; customColor: string | null;
}

function routePlatform(routeId: string): Platform {
  return routeId === "local" ? dashboard.localHost : settings.peers.find((peer) => peer.id === routeId)?.platform ?? "windows";
}

/** Every host in the saved order, each with the icon and colour it wears
 *  everywhere: its own if the user chose one, else a default by platform and
 *  by its place in the host order, which paired hosts share. */
function switchRoutes(): SwitchRoute[] {
  return currentRouteIds().map((id, index) => {
    const platform = routePlatform(id);
    const look = hostLook(hostAppearances[id], platform, index);
    return {
      id, name: routeDisplayName(id), platform, local: id === "local",
      icon: look.lucide, color: look.color, customIcon: look.customIcon, customColor: look.customColor,
    };
  });
}

function routeColor(routeId: string): string {
  return hostLook(hostAppearances[routeId], routePlatform(routeId), currentRouteIds().indexOf(routeId)).color;
}

function platformIcon(platform: Platform): string {
  return platform === "mac" ? "laptop" : "computer";
}

function activeRouteFor(shared: SharedMonitorStatus): string {
  return selectedMonitorFor(shared)?.activeRoute ?? "local";
}

/** Whether a switch to this host could run at all: it needs the port the host
 *  is on, and either this computer's DDC/CI or the network Agent. */
function canSwitch(shared: SharedMonitorStatus, routeId: string): boolean {
  return routeInputFor(shared, routeId) != null && (shared.ddcAvailable || dashboard.agentConfigured);
}

type PresenceState = "online" | "offline" | "unknown";
/** Whether a host can see one shared display, and when it cannot be said, why
 *  not. Each reason reads differently to somebody about to switch: a host
 *  nothing has asked yet is nothing to worry about, one that cannot be reached
 *  is, and "up but this display is not on it" is the one worth acting on. */
type DisplayAttachment = "sees" | "blind" | "unchecked" | "unreachable" | "unreported";

/** Whether a host answers right now. This computer always does. */
function presenceState(routeId: string): PresenceState {
  if (routeId === "local") return "online";
  const presence = hostPresence[routeId];
  if (!presence || !presence.checkedAtMs) return "unknown";
  return presence.online ? "online" : "offline";
}

function presenceLabel(routeId: string): string {
  const state = presenceState(routeId);
  if (state === "online") return t("presence.online");
  return state === "offline" ? t("presence.offline") : t("presence.unknown");
}

/** A host's standing in one line: offline since when, or simply online. */
function presenceSummary(routeId: string): string {
  if (presenceState(routeId) !== "offline") return presenceLabel(routeId);
  const presence = hostPresence[routeId];
  const lastSeen = presence?.lastSeenAtMs
    ? t("presence.lastSeen", { time: formatClock(presence.lastSeenAtMs) })
    : t("presence.neverSeen");
  return `${t("presence.offline")} · ${lastSeen}`;
}

/** What a standing means, in one short sentence. A row has space for a word,
 *  and "not checked yet" explains nothing on its own.
 *
 *  Deliberately does not repeat what the row already shows — when it was last
 *  up, or what the last attempt ran into. A tooltip that restates the line it
 *  hangs off is a wall of text nobody reads twice. */
function presenceHelp(routeId: string): string {
  const state = presenceState(routeId);
  if (state === "online") return t("presence.onlineHelp");
  return state === "unknown" ? t("presence.unknownHelp") : t("presence.offlineHelp");
}

/** The reason a host could not be reached, for the line under its row, in the
 *  words the attempt itself reported. Empty unless it is offline. */
function presenceProblem(routeId: string): string {
  if (presenceState(routeId) !== "offline") return "";
  return hostPresence[routeId]?.detail || t("presence.offlinePlain");
}

function formatClock(milliseconds: number): string {
  return new Date(milliseconds).toLocaleTimeString(locale, { hour: "2-digit", minute: "2-digit" });
}

function presenceDotHtml(routeId: string): string {
  const label = escapeHtml(presenceHelp(routeId));
  return `<span class="presence-dot is-${presenceState(routeId)}" role="img" aria-label="${label}" title="${label}"></span>`;
}

/** Whether a host can see one shared display. This computer answers from its
 *  own scan; a paired host answers from what its last reply reported. */
function attachmentFor(routeId: string, shared: SharedMonitorStatus): DisplayAttachment {
  if (routeId === "local") {
    const seen = [...dashboard.monitors, ...(dashboard.uncontrollableMonitors ?? [])]
      .some((monitor) => sameDisplay(monitor.fingerprint, shared.fingerprint));
    return seen ? "sees" : "blind";
  }
  const state = presenceState(routeId);
  if (state === "unknown") return "unchecked";
  if (state === "offline") return "unreachable";
  const attached = hostPresence[routeId]?.attachedMonitors;
  if (!attached) return "unreported";
  return attached.includes(shared.monitorKey) ? "sees" : "blind";
}

function attachmentLabel(attachment: DisplayAttachment): string {
  if (attachment === "sees") return t("presence.seesShort");
  if (attachment === "blind") return t("presence.blindShort");
  if (attachment === "unreachable") return t("presence.unreachableShort");
  return attachment === "unreported" ? t("presence.unreportedShort") : t("presence.unknown");
}

/** What a display's state on one host means, in full.
 *
 *  A display drops its link on hosts it is not showing about as often as a
 *  cable is actually out, so "cannot see it" names both rather than accusing
 *  the cable; and each way of not knowing says which one it is.
 */
function attachmentTitle(routeId: string, name: string, shared: SharedMonitorStatus): string {
  const attachment = attachmentFor(routeId, shared);
  if (attachment === "sees") {
    return routeId === "local" ? t("presence.seesLocal") : t("presence.sees", { name });
  }
  if (attachment === "blind") {
    return routeId === "local" ? t("presence.blindLocal") : t("presence.blind", { name });
  }
  if (attachment === "unreachable") return t("presence.attachmentOffline", { name });
  if (attachment === "unreported") return t("presence.attachmentUnreported", { name });
  return t("presence.attachmentUnchecked", { name });
}

/** The short line a host's key carries about one display, or "" when there is
 *  nothing to say.
 *
 *  Only trouble speaks up: a host that is up and sees the display is the
 *  normal case, and a host nothing has asked yet — every host for the first
 *  seconds after the window opens — would otherwise cover the key's own label
 *  with a caveat that resolves itself. The dot carries that state instead.
 */
function attachmentNote(routeId: string, shared: SharedMonitorStatus): string {
  const attachment = attachmentFor(routeId, shared);
  if (attachment === "unreachable") return t("presence.offline");
  return attachment === "blind" ? t("presence.blindShort") : "";
}

/** Whether what a display is set to can still be believed.
 *
 *  The active route is a remembered value: it outlives the display being
 *  unplugged, put to sleep, or switched by its own buttons, and the window
 *  would go on lighting up a host as "showing" a display that is not there.
 *  Only evidence against it demotes it — the host it names answered and does
 *  not see the display, or, for this computer, its own scan cannot find it.
 *  A paired host showing a display this computer cannot read is the normal
 *  case and stays confirmed.
 */
function activeRouteUnconfirmed(shared: SharedMonitorStatus): boolean {
  return attachmentFor(activeRouteFor(shared), shared) === "blind";
}

function displayBadge(shared: SharedMonitorStatus): string {
  if (shared.displayState === "ready") return `<span class="badge is-ok">${t("dashboard.ddcReady")}</span>`;
  if (shared.displayState === "onOtherHost") return `<span class="badge is-muted">${t("dashboard.onOtherHost")}</span>`;
  return `<span class="badge is-muted">${t("dashboard.notReady")}</span>`;
}

function renderSwitchPanel(): void {
  renderSwitchAllBar();
  const container = document.querySelector<HTMLElement>("#switch-panel");
  if (!container) return;
  const toggle = document.querySelector<HTMLElement>("#view-toggle");
  if (!dashboard.shared.length) {
    if (toggle) toggle.hidden = true;
    container.innerHTML = emptySwitchPanelHtml();
    return;
  }
  const view = currentSwitchView();
  if (toggle) {
    toggle.hidden = false;
    toggle.querySelectorAll<HTMLButtonElement>("[data-switch-view]").forEach((button) => {
      const isActive = button.dataset.switchView === view;
      button.classList.toggle("is-active", isActive);
      button.setAttribute("aria-pressed", String(isActive));
    });
  }
  const routes = switchRoutes();
  if (view === "matrix") {
    container.innerHTML = matrixHtml(routes);
    // A custom property set through the CSSOM, since the CSP refuses style attributes.
    container.querySelector<HTMLElement>(".matrix")?.style.setProperty("--hosts", String(routes.length));
    return;
  }
  container.innerHTML = `<div class="stage ${dashboard.shared.length === 1 ? "is-single" : ""}">
    ${dashboard.shared.map((shared) => stagePanelHtml(shared, routes)).join("")}
  </div>`;
}

/** A drawn monitor. `model` goes on the bottom bezel, where a real one wears its badge. */
function displayDrawingHtml(isUltrawide: boolean, label: string, model?: string): string {
  const badge = model ? `<span class="display-model">${escapeHtml(model)}</span>` : "";
  return `<div class="display-frame">
    <span class="display-glow"></span>
    <div class="display-screen ${isUltrawide ? "is-ultrawide" : ""} ${model ? "has-model" : ""}"><div class="display-inner"><div class="display-label">${label}</div></div>${badge}</div>
    <div class="display-neck"></div><div class="display-base"></div>
  </div>`;
}

function emptySwitchPanelHtml(): string {
  const idle = displayDrawingHtml(false, `<strong>${t("dashboard.notSelected")}</strong>`).replace("display-screen ", "display-screen is-idle ");
  return `<article class="monitor-panel glass is-empty">
    <header class="panel-head"><h2>${t("dashboard.sharedDisplay")}</h2></header>
    <div class="display-visual">${idle}</div>
    <div class="empty-copy">
      <p>${isPreview ? t("preview.monitorStatus") : t("dashboard.noSharedBody")}</p>
      <button type="button" class="button primary" data-open-displays><i data-lucide="monitor"></i>${t("action.chooseDisplays")}</button>
    </div>
  </article>`;
}

function stagePanelHtml(shared: SharedMonitorStatus, routes: SwitchRoute[]): string {
  const { resolution, source } = resolutionFor(shared);
  const isUltrawide = Boolean(resolution && isUltrawideResolution(resolution));
  const activeId = activeRouteFor(shared);
  const active = routes.find((route) => route.id === activeId);
  const activeInput = routeInputFor(shared, activeId);
  const unconfirmed = activeRouteUnconfirmed(shared);
  const label = `<span>${unconfirmed ? t("dashboard.lastShown") : t("dashboard.nowShowing")}</span>
    <strong>${escapeHtml(active?.name ?? routeDisplayName(activeId))}</strong>
    ${activeInput != null ? `<span class="display-port">${escapeHtml(inputName(activeInput, shared.monitorKey))}</span>` : ""}`;
  return `<article class="monitor-panel glass" data-monitor-key="${escapeHtml(shared.monitorKey)}">
    <header class="panel-head">
      <h2>${escapeHtml(shared.name)}</h2>
      <span class="spec" title="${escapeHtml(resolutionSourceName(source))}">${escapeHtml(specText(resolution, isUltrawide))}</span>
      ${displayBadge(shared)}
    </header>
    ${shared.statusText ? `<p class="panel-status">${escapeHtml(shared.statusText)}</p>` : ""}
    <div class="display-visual" ${active && !unconfirmed ? `data-color="${active.color}"` : ""} ${unconfirmed ? `title="${escapeHtml(unconfirmedTitle(shared))}"` : ""}>${displayDrawingHtml(isUltrawide, label, shared.name)}</div>
    <div class="source-keys glass-flat ${routes.length >= MATRIX_MIN_HOSTS ? "is-stacked" : ""}">
      ${routes.map((route) => sourceKeyHtml(shared, route, route.id === activeId, unconfirmed)).join("")}
    </div>
  </article>`;
}

/** Why a display's host cannot be confirmed, for the drawing's tooltip. */
function unconfirmedTitle(shared: SharedMonitorStatus): string {
  const activeId = activeRouteFor(shared);
  const name = routeDisplayName(activeId);
  return `${t("dashboard.lastShownHint", { name })} ${attachmentTitle(activeId, name, shared)}`;
}

/** One host's key under a display: lit when that host is on screen, and only
 *  while that can still be believed. */
function sourceKeyHtml(shared: SharedMonitorStatus, route: SwitchRoute, isActive: boolean, unconfirmed = false): string {
  const input = routeInputFor(shared, route.id);
  const port = input == null ? t("dashboard.inputUnset") : inputName(input, shared.monitorKey);
  const note = attachmentNote(route.id, shared);
  const noteTitle = escapeHtml(attachmentTitle(route.id, route.name, shared));
  const copy = `<span class="host-chip"><i data-lucide="${route.icon}"></i>${presenceDotHtml(route.id)}</span>
    <span class="source-copy"><b>${escapeHtml(route.name)}</b><small>${escapeHtml(port)}</small>${
      note ? `<small class="source-note" title="${noteTitle}">${escapeHtml(note)}</small>` : ""
    }</span>`;
  if (isActive && unconfirmed) {
    // Named, but not lit: the display cannot be seen to be on this host, and
    // the filled key is how the window says it is.
    return `<div class="source-key is-unconfirmed" data-color="${route.color}" aria-current="true" title="${escapeHtml(unconfirmedTitle(shared))}">${copy}<span class="source-action">${t("dashboard.lastShown")}</span></div>`;
  }
  if (isActive) {
    return `<div class="source-key tint is-active" data-color="${route.color}" aria-current="true">${copy}<span class="source-action">${t("dashboard.currentlyDisplayed")}</span></div>`;
  }
  const label = escapeHtml(`${t("action.switchHost")}: ${route.name}`);
  return `<button type="button" class="source-key" data-color="${route.color}" data-switch-id="${escapeHtml(route.id)}" aria-label="${label}" title="${label}" ${canSwitch(shared, route.id) ? "" : "disabled"}>
    ${copy}<span class="source-action">${t("action.switchShort")}</span>
  </button>`;
}

/** Displays down the side, hosts across the top; a cell switches one display. */
function matrixHtml(routes: SwitchRoute[]): string {
  const heads = routes.map((route) => {
    const name = escapeHtml(route.name);
    const isAllOnThisHost = dashboard.shared.every((shared) => activeRouteFor(shared) === route.id);
    const allUnconfirmed = isAllOnThisHost && dashboard.shared.some(activeRouteUnconfirmed);
    const allButton = isAllOnThisHost
      ? `<span class="switch-all-host ${allUnconfirmed ? "is-unconfirmed" : "tint is-showing"}">${allUnconfirmed ? t("dashboard.lastShown") : t("dashboard.allDisplayed")}</span>`
      : `<button type="button" class="switch-all-host glass" data-switch-all-id="${escapeHtml(route.id)}" aria-label="${escapeHtml(t("action.switchAllToHost", { name: route.name }))}" ${switchAllTargets(route.id).length ? "" : "disabled"}>${t("switcher.switchAll")}</button>`;
    const standing = route.local
      ? t("dashboard.localBadge")
      : `${platformName(route.platform)} · ${presenceLabel(route.id)}`;
    return `<div class="matrix-host" data-color="${route.color}">
      <span class="host-chip"><i data-lucide="${route.icon}"></i>${presenceDotHtml(route.id)}</span>
      <b title="${name}">${name}</b>
      <small class="presence-line" title="${escapeHtml(presenceHelp(route.id))}">${escapeHtml(standing)}</small>
      ${allButton}
    </div>`;
  }).join("");
  const rows = dashboard.shared.map((shared) => {
    const { resolution } = resolutionFor(shared);
    const isUltrawide = Boolean(resolution && isUltrawideResolution(resolution));
    const activeId = activeRouteFor(shared);
    const unconfirmed = activeRouteUnconfirmed(shared);
    const activeColor = routes.find((route) => route.id === activeId)?.color;
    const name = escapeHtml(shared.name);
    return `<div class="matrix-display">
        <span class="display-thumb ${isUltrawide ? "is-ultrawide" : ""}" ${activeColor && !unconfirmed ? `data-color="${activeColor}"` : ""}><i></i></span>
        <div><b title="${name}">${name}</b><small>${escapeHtml(specText(resolution, isUltrawide))}</small></div>
      </div>
      ${routes.map((route) => matrixCellHtml(shared, route, route.id === activeId, unconfirmed)).join("")}`;
  }).join("");
  return `<section class="matrix-panel glass">
    <div class="matrix">
      <div class="matrix-corner"><span class="caption">${t("dashboard.matrixCorner")}</span></div>
      ${heads}${rows}
    </div>
    <div class="matrix-legend">
      <span><i class="legend-active"></i>${t("dashboard.legendActive")}</span>
      <span><i class="legend-ready"></i>${t("dashboard.legendReady")}</span>
      <span><i class="legend-unset"></i>${t("dashboard.legendUnset")}</span>
    </div>
  </section>`;
}

function matrixCellHtml(shared: SharedMonitorStatus, route: SwitchRoute, isActive: boolean, unconfirmed = false): string {
  const input = routeInputFor(shared, route.id);
  const port = input == null ? "" : escapeHtml(inputName(input, shared.monitorKey));
  if (isActive && unconfirmed) {
    return `<div class="matrix-cell is-unconfirmed" data-color="${route.color}" aria-current="true" title="${escapeHtml(unconfirmedTitle(shared))}">${port}<small>${t("dashboard.lastShown")}</small></div>`;
  }
  if (isActive) {
    return `<div class="matrix-cell tint is-active" data-color="${route.color}" aria-current="true">${port}<small>${t("dashboard.currentlyDisplayed")}</small></div>`;
  }
  if (input == null) return `<div class="matrix-cell is-unset">${t("dashboard.inputUnset")}</div>`;
  // The cell is where a display meets a host, so it is where "that host cannot
  // see this display" belongs. It still switches: a host that is asleep, or a
  // display that dropped the link, both come back once it is on screen.
  const note = attachmentNote(route.id, shared);
  const label = escapeHtml(
    note ? `${shared.name} → ${route.name} · ${attachmentTitle(route.id, route.name, shared)}` : `${shared.name} → ${route.name}`,
  );
  return `<button type="button" class="matrix-cell ${note ? "is-unconfirmed" : ""}" data-color="${route.color}" data-monitor-key="${escapeHtml(shared.monitorKey)}" data-switch-id="${escapeHtml(route.id)}" aria-label="${label}" title="${label}" ${canSwitch(shared, route.id) ? "" : "disabled"}>
    ${port}<small>${escapeHtml(note || t("action.switchShort"))}</small>
  </button>`;
}

/** Full-width closing punctuation that leaves its right half of the em empty. */
const TRAILING_FULL_WIDTH = /[）〉》」』】〕｝。、，；：]$/u;

/** Sets the status pill's label, trimming the empty half em a full-width
    closing bracket would otherwise add to the capsule's right side. */
function setAgentPillText(target: Element, label: string): void {
  target.textContent = label;
  target.classList.toggle("trim-trailing-em", TRAILING_FULL_WIDTH.test(label));
}

function renderState(): void {
  renderSummary();
  const pill = document.querySelector("#agent-pill");
  pill?.classList.toggle("is-ready", dashboard.agentConfigured);
  if (pill) setAgentPillText(pill.querySelector("span:last-child")!, isPreview ? t("dashboard.preview") : dashboard.agentConfigured ? t("dashboard.agentReady") : t("dashboard.agentMissing"));
  setInput("#shared-key", settings.sharedKey);
  setInput("#wait-seconds", String(settings.waitSeconds));
  const autostart = document.querySelector<HTMLInputElement>("#autostart");
  if (autostart) autostart.checked = settings.autostart;
  const checkUpdates = document.querySelector<HTMLInputElement>("#check-updates");
  if (checkUpdates) checkUpdates.checked = settings.checkUpdates;
  const hostSwitcherEnabled = document.querySelector<HTMLInputElement>("#host-switcher-enabled");
  if (hostSwitcherEnabled) hostSwitcherEnabled.checked = settings.hostSwitcherEnabled;
  renderShortcutSetting();
  renderSwitchPanel();
  diagnostics.render();
  renderMonitors(); renderMonitorMerge(); renderPeerList(); renderHostList(); renderLocalInputSummary(); renderInputLabels(); renderDisplayMaintenance(); refreshIcons();
  // The tray menu lists the same displays and hosts. Nothing here can do
  // anything about a menu that failed to rebuild; the backend logs it.
  if (!isPreview) void invoke("refresh_tray").catch(() => undefined);
}

function shortcutDisplay(value: string): string {
  const isMac = dashboard.localHost === "mac";
  return value.split("+").map((part) => {
    const key = part.toLowerCase();
    if (key === "commandorcontrol") return isMac ? "Command" : "Ctrl";
    if (key === "super") return isMac ? "Command" : "Win";
    if (key === "alt") return isMac ? "Option" : "Alt";
    if (key.startsWith("key")) return part.slice(3).toUpperCase();
    if (key.startsWith("digit")) return part.slice(5);
    return part;
  }).join(" + ");
}

function renderShortcutSetting(): void {
  const enabled = document.querySelector<HTMLInputElement>("#host-switcher-enabled")?.checked ?? false;
  const button = document.querySelector<HTMLButtonElement>("#shortcut-recorder");
  const value = document.querySelector<HTMLElement>("#shortcut-value");
  const status = document.querySelector<HTMLElement>("#shortcut-status");
  if (button) button.disabled = !enabled;
  if (value) value.textContent = isRecordingShortcut ? t("settings.recordingShortcut") : shortcutDisplay(settings.hostSwitcherShortcut);
  if (status) {
    status.textContent = enabled ? shortcutStatus?.text ?? "" : "";
    status.className = `shortcut-status${shortcutStatus ? ` is-${shortcutStatus.kind}` : ""}`;
  }
}

function beginShortcutRecording(): void {
  if (document.querySelector<HTMLButtonElement>("#shortcut-recorder")?.disabled) return;
  isRecordingShortcut = true;
  shortcutStatus = null;
  renderShortcutSetting();
}

function shortcutFromEvent(event: KeyboardEvent): string | null {
  if (["Control", "Shift", "Alt", "Meta"].includes(event.key)) return null;
  const parts: string[] = [];
  const isMac = dashboard.localHost === "mac";
  if (event.ctrlKey) parts.push(isMac ? "Control" : "CommandOrControl");
  if (event.altKey) parts.push("Alt");
  if (event.shiftKey) parts.push("Shift");
  if (event.metaKey) parts.push(isMac ? "CommandOrControl" : "Super");
  if (!parts.some((part) => part !== "Shift")) return null;
  parts.push(event.code);
  return parts.join("+");
}

function captureShortcut(event: KeyboardEvent): void {
  if (!isRecordingShortcut) return;
  event.preventDefault();
  event.stopImmediatePropagation();
  if (event.key === "Escape") {
    isRecordingShortcut = false;
    renderShortcutSetting();
    return;
  }
  const shortcut = shortcutFromEvent(event);
  if (!shortcut) return;
  settings.hostSwitcherShortcut = shortcut;
  isRecordingShortcut = false;
  renderShortcutSetting();
  void checkShortcutConflict(shortcut);
}

async function checkShortcutConflict(shortcut: string): Promise<void> {
  if (isCommonApplicationShortcut(shortcut)) {
    shortcutStatus = { kind: "conflict", text: t("settings.shortcutCommonConflict") };
    renderShortcutSetting();
    return;
  }
  shortcutStatus = { kind: "checking", text: t("settings.shortcutChecking") };
  renderShortcutSetting();
  if (isPreview) {
    shortcutStatus = { kind: "available", text: t("settings.shortcutAvailable") };
    renderShortcutSetting();
    return;
  }
  try {
    const result = await invoke<ShortcutCheckResult>("check_host_switcher_shortcut", { shortcut });
    shortcutStatus = {
      kind: result.available ? "available" : "conflict",
      text: result.message,
    };
  } catch (error) {
    shortcutStatus = { kind: "conflict", text: String(error) };
  }
  renderShortcutSetting();
}

function isCommonApplicationShortcut(shortcut: string): boolean {
  const parts = shortcut.toLowerCase().split("+");
  const key = (parts.pop() ?? "").replace(/^key/, "");
  const modifiers = new Set(parts);
  const primary = modifiers.has("commandorcontrol") ||
    (dashboard.localHost === "mac" ? modifiers.has("super") : modifiers.has("control"));
  if (!primary) return false;
  const additionalModifiers = [...modifiers].filter((modifier) =>
    !["commandorcontrol", dashboard.localHost === "mac" ? "super" : "control"].includes(modifier)
  );
  const primaryOnly = additionalModifiers.length === 0;
  const primaryWithShift = additionalModifiers.length === 1 && additionalModifiers[0] === "shift";
  return (primaryOnly && new Set([
    "a", "c", "f", "h", "l", "m", "n", "o", "p", "q", "r", "s", "t", "v", "w", "x", "y", "z", "tab", "f4",
  ]).has(key)) || (primaryWithShift && new Set(["n", "p", "r", "s", "t", "w"]).has(key));
}

function renderMonitors(): void {
  const container = document.querySelector("#monitor-picker");
  if (!container) return;
  // A shared display showing another host can't be read from here, but it is
  // still this computer's shared display, so list it with the controllable ones.
  const isOnOtherHost = (monitor: MonitorDescriptor) => dashboard.shared.some((shared) =>
    shared.displayState === "onOtherHost" && sameDisplay(shared.fingerprint, monitor.fingerprint));
  const uncontrollable = dashboard.uncontrollableMonitors ?? [];
  const elsewhere = uncontrollable.filter(isOnOtherHost);
  const unreachable = uncontrollable.filter((monitor) => !isOnOtherHost(monitor));
  if (!dashboard.monitors.length && !uncontrollable.length) {
    container.innerHTML = `<p class="empty-note">${t("settings.noMonitors")}</p>`; return;
  }
  // A shared display this computer cannot see at all is listed from the saved
  // selection, because it is exactly the one the user may need to remove and
  // nothing enumerates a row for it.
  const absent = dashboard.shared.filter((shared) =>
    ![...dashboard.monitors, ...uncontrollable].some((monitor) => sameDisplay(monitor.fingerprint, shared.fingerprint)));
  container.innerHTML = [
    ...dashboard.monitors.map((monitor) => selectableMonitorRow(monitor, t("settings.ddcControllable"))),
    ...elsewhere.map((monitor) => selectableMonitorRow(monitor, t("dashboard.onOtherHost"))),
    ...absent.map((shared) => `<div class="row has-icon">
      <span class="app-icon is-accent"><i data-lucide="monitor-off"></i></span>
      <div>
        <div class="row-title">${escapeHtml(shared.name)}</div>
        <div class="row-sub mono">${escapeHtml(shared.fingerprint.manufacturer_id)} / ${escapeHtml(shared.fingerprint.product_code)} / ${escapeHtml(shared.fingerprint.serial_number ?? t("settings.noSerial"))} · ${escapeHtml(t("settings.notDetected"))}</div>
      </div>
      ${shareToggleHtml(shared.monitorKey, shared.monitorKey, true, shared.name)}
    </div>`),
    ...unreachable.map((monitor) => selectableMonitorRow(monitor, t("settings.ddcUnreachable"), true)),
  ].join("");
}

/** Sharing a display runs a command, so the switch is a button that only
 *  flips once the command answers, not a checkbox that flips first. */
function shareToggleHtml(monitorId: string, busyKey: string, isSelected: boolean, name: string): string {
  const isBusy = busyDisplays.has(busyKey);
  const label = escapeHtml(`${isSelected ? t("action.removeShared") : t("action.selectShared")}: ${name}`);
  return `<button type="button" class="toggle-button ${isBusy ? "is-busy" : ""}" role="switch" aria-checked="${isSelected}" aria-label="${label}" title="${label}"
    data-monitor-id="${escapeHtml(monitorId)}" data-busy-key="${escapeHtml(busyKey)}" data-monitor-selected="${isSelected}"${isBusy ? " disabled" : ""}></button>`;
}

/** Offers to merge a display that is present but belongs to no shared display
 *  into one that is. A display that reports a different identity per display
 *  mode shows up as an unfamiliar new display while the shared one it really
 *  is goes unreadable, and only the user can say they are one panel. */
function renderMonitorMerge(): void {
  const container = document.querySelector("#monitor-merge");
  if (!container) return;
  const present = [...dashboard.monitors, ...(dashboard.uncontrollableMonitors ?? [])];
  const strangers = present.filter((monitor) => !isSharedDisplay(monitor.fingerprint));
  // Only a shared display that has gone missing can be the other identity of a
  // display that just turned up. With every shared display accounted for there
  // is nothing to merge, and offering it anyway invites merging two displays
  // that are genuinely different — which is not something a user can undo by
  // looking at the screen.
  const targets = dashboard.shared.filter((shared) =>
    !present.some((monitor) => sameDisplay(monitor.fingerprint, shared.fingerprint)));
  const claims = dashboard.monitorIdentityClaims ?? [];
  const suggestions = dashboard.mergeSuggestions ?? [];
  const canMerge = strangers.length > 0 && targets.length > 0;
  if (!canMerge && !claims.length) { container.innerHTML = ""; return; }

  const describeMonitor = (monitor: MonitorDescriptor) =>
    `${monitor.name} (${monitor.fingerprint.manufacturer_id}/${monitor.fingerprint.product_code})`;
  const describeShared = (shared: SharedMonitorStatus) =>
    `${shared.name} (${shared.fingerprint.manufacturer_id}/${shared.fingerprint.product_code})`;
  container.innerHTML = `
    <h3 class="group-title">${t("settings.mergeTitle")}</h3>
    <p class="section-hint">${t("settings.mergeIntro")}</p>
    <div class="list">
      ${claims.map((claim) => `<div class="row has-icon">
        <span class="app-icon is-accent"><i data-lucide="link"></i></span>
        <div>
          <div class="row-title">${escapeHtml(claim.aliasLabel)} → ${escapeHtml(claim.primaryLabel)}</div>
          <div class="row-sub">${escapeHtml(t("settings.mergedInto", { name: claim.primaryLabel }))}</div>
        </div>
        <button type="button" class="button small" data-unmerge-alias="${escapeHtml(claim.aliasKey)}">${t("settings.mergeUndo")}</button>
      </div>`).join("")}
      ${(canMerge ? strangers : []).map((monitor) => {
        // A display model known to publish this second identity points the
        // dropdown at the display it belongs to. Without one the row reads
        // exactly as it did before, with nothing preselected.
        const suggested = suggestions.find((suggestion) => suggestion.monitorId === monitor.id
          && targets.some((target) => target.monitorKey === suggestion.primaryKey));
        return `<div class="row has-icon">
        <span class="app-icon${suggested ? " is-accent" : ""}"><i data-lucide="monitor-dot"></i></span>
        <div>
          <div class="row-title">${escapeHtml(describeMonitor(monitor))}</div>
          <div class="row-sub">${escapeHtml(suggested
            ? t("settings.mergeSuggested", { name: suggested.primaryLabel })
            : t("settings.mergeUnidentified"))}</div>
        </div>
        <div class="row-actions">
          <select class="field-select plain-font" data-merge-target="${escapeHtml(monitor.id)}" aria-label="${escapeHtml(t("settings.mergeSelect"))}">
            ${targets.map((target) => `<option value="${escapeHtml(target.monitorKey)}"${target.monitorKey === suggested?.primaryKey ? " selected" : ""}>${escapeHtml(describeShared(target))}</option>`).join("")}
          </select>
          <button type="button" class="button small" data-merge-alias="${escapeHtml(monitor.id)}">${t("settings.mergeAction")}</button>
        </div>
      </div>`;
      }).join("")}
    </div>`;
}

function selectableMonitorRow(monitor: MonitorDescriptor, statusLabel: string, isDimmed = false): string {
  const shared = dashboard.shared.find((item) => sameDisplay(item.fingerprint, monitor.fingerprint));
  const isSelected = Boolean(shared);
  const fp = monitor.fingerprint;
  const res = monitor.maxResolution;
  const resText = res
    ? ` · ${res.width}×${res.height} ${isUltrawideResolution(res) ? "21:9" : "16:9"} (${resolutionSourceName(monitor.resolutionSource ?? null)})`
    : "";
  return `<div class="row has-icon ${isDimmed && !isSelected ? "is-dimmed" : ""}">
    <span class="app-icon ${isSelected ? "is-accent" : ""}"><i data-lucide="monitor"></i></span>
    <div>
      <div class="row-title">${escapeHtml(monitor.name)}</div>
      <div class="row-sub mono">${escapeHtml(fp.manufacturer_id)} / ${escapeHtml(fp.product_code)} / ${escapeHtml(fp.serial_number ?? t("settings.noSerial"))}${escapeHtml(resText)} · ${escapeHtml(statusLabel)}</div>
      ${renderConnection(monitor.connection ?? null)}
    </div>
    ${shareToggleHtml(shared?.monitorKey ?? monitor.id, monitor.id, isSelected, monitor.name)}
  </div>`;
}

const hostOutputKeys = {
  hdmi: "connection.host.hdmi", displayPort: "connection.host.displayPort", usbC: "connection.host.usbC",
  thunderbolt: "connection.host.thunderbolt", dvi: "connection.host.dvi", vga: "connection.host.vga",
  indirect: "connection.host.indirect",
} as const satisfies Record<HostOutput, MessageKey>;

const sinkInterfaceKeys = {
  hdmi: "connection.sink.hdmi", displayPort: "connection.sink.displayPort", dvi: "connection.sink.dvi",
  vga: "connection.sink.vga", unknownDigital: "connection.sink.unknownDigital",
} as const satisfies Record<SinkInterface, MessageKey>;

function renderConnection(connection: MonitorConnection | null): string {
  if (!connection || (!connection.hostOutput && !connection.sinkInterface)) return "";
  const host = connection.hostOutput ? t(hostOutputKeys[connection.hostOutput]) : t("connection.unknown");
  const sink = connection.sinkInterface ? t(sinkInterfaceKeys[connection.sinkInterface]) : t("connection.unknown");
  const traits = [
    connection.signalConversion ? t("connection.conversion") : null,
    connection.sharesUsbData ? t("connection.sharesUsb") : null,
  ].filter((value): value is string => value !== null);
  const risk = connection.ddcRisk === "elevated" ? t("connection.riskElevated")
    : connection.ddcRisk === "unsupported" ? t("connection.riskUnsupported") : null;
  const title = connection.hostPort ? ` title="${escapeHtml(connection.hostPort)}"` : "";
  return `<span class="monitor-connection"${title}>${escapeHtml(t("connection.summary", { host, sink }))}${traits.length ? ` · ${escapeHtml(traits.join(" · "))}` : ""}</span>
    ${risk ? `<span class="monitor-connection-risk">${escapeHtml(risk)}</span>` : ""}`;
}

function renderLocalInputSummary(): void {
  const container = document.querySelector("#local-input-summary");
  if (!container) return;
  if (!dashboard.shared.length) {
    container.innerHTML = `<p class="empty-note">${t("settings.noMonitors")}</p>`;
    return;
  }
  container.innerHTML = dashboard.shared.map((shared) => {
    const value = selectedMonitorFor(shared)?.localInput ?? null;
    const conflict = shared.connectionInputConflict ? `<p class="row-warning">${t("settings.inputConflict")}</p>` : "";
    return `<div class="row">
      <div><div class="row-title">${escapeHtml(shared.name)}</div>${conflict}</div>
      <select class="field-select" data-local-input="${escapeHtml(shared.monitorKey)}" aria-label="${escapeHtml(t("settings.localInputAria", { monitor: shared.name }))}"${displayInputsKnown(shared.monitorKey) ? "" : " disabled"}>
        ${renderInputOptions("local", shared.monitorKey, value)}
      </select>
    </div>`;
  }).join("");
}

/** Marks the two actions that write to a display nobody has verified this on. */
const experimental = (hint: string) => `${t("settings.experimentalTag")} ${hint}`;

/**
 * The recoveries for one shared display, one button each.
 *
 * They are separate buttons because they cost the user different things: a
 * re-detection only reads, a power cycle blanks the panel for a moment, and
 * re-seating the signal sends the display out through another input and back.
 */
function renderDisplayMaintenance(): void {
  const container = document.querySelector("#display-maintenance");
  if (!container) return;
  if (!dashboard.shared.length) {
    container.innerHTML = `<p class="empty-note">${t("settings.noMonitors")}</p>`;
    return;
  }
  container.innerHTML = dashboard.shared.map((shared) => {
    // Re-seating takes two hosts. This computer can send the display away —
    // it is the one on screen — but it cannot bring it back, because a display
    // answers DDC/CI only on the input it is showing. So it needs a paired
    // host that has a port on this display and is not known to be offline;
    // the backend confirms that host answers before anything moves.
    const partner = settings.peers.find((peer) =>
      peer.inputs.some((assignment) => sameDisplay(assignment.monitor, shared.fingerprint)
        && assignment.input !== selectedMonitorFor(shared)?.localInput));
    const canResync = selectedMonitorFor(shared)?.localInput != null
      && partner != null && presenceState(partner.id) !== "offline";
    const key = escapeHtml(shared.monitorKey);
    // A display whose USB rides the same cable has a hub or KVM of its own,
    // and that binding follows the active input rather than the panel: an
    // MSI MPG 274U came back from a power cycle with its USB still detached.
    const usbNote = shared.connection?.sharesUsbData
      ? `<span class="row-hint is-warn">${escapeHtml(t("settings.powerCycleUsbNote"))}</span>` : "";
    return `<div class="row">
      <div><div class="row-title">${escapeHtml(shared.name)}</div><span class="row-hint">${escapeHtml(shared.statusText)}</span>${usbNote}</div>
      <div class="row-actions">
        <button type="button" class="button small" data-maintain="redetect" data-monitor-key="${key}" title="${escapeHtml(t("settings.redetectHint"))}"><i data-lucide="search"></i>${t("action.redetectDisplay")}</button>
        <button type="button" class="button small" data-maintain="resync" data-monitor-key="${key}" title="${escapeHtml(experimental(canResync ? t("settings.resyncHint") : t("settings.resyncUnavailable")))}"${canResync ? "" : " disabled"}><i data-lucide="plug-zap"></i>${t("action.resyncInput")}</button>
        <button type="button" class="button small" data-maintain="power" data-monitor-key="${key}" title="${escapeHtml(experimental(t("settings.powerCycleHint")))}"><i data-lucide="power"></i>${t("action.powerCycle")}</button>
      </div>
    </div>`;
  }).join("");
}

/** Hosts whose saved input for `shared` is `value`, by route id. */
function inputUserRoutes(shared: SharedMonitorStatus, value: number): string[] {
  const localUser = selectedMonitorFor(shared)?.localInput === value ? ["local"] : [];
  const peerUsers = settings.peers
    .filter((peer) => peer.inputs.some((assignment) => assignment.input === value && sameDisplay(assignment.monitor, shared.fingerprint)))
    .map((peer) => peer.id);
  return [...localUser, ...peerUsers];
}

/**
 * One tile per input of each shared display: who uses it, and a note field.
 * Left alone while a field has focus, so a sync from a paired host never
 * replaces what is being typed.
 */
function renderInputLabels(): void {
  const container = document.querySelector<HTMLElement>("#input-labels");
  if (!container) return;
  if (container.contains(document.activeElement)) {
    inputNamesReloadPending = true;
    return;
  }
  inputNamesReloadPending = false;
  if (!dashboard.shared.length) {
    container.innerHTML = `<p class="empty-note">${t("settings.noMonitors")}</p>`;
    return;
  }
  const listFormat = new Intl.ListFormat(locale, { type: "conjunction" });
  container.innerHTML = dashboard.shared.map((shared) => {
    const options = inputOptionsByMonitor[shared.monitorKey] ?? [];
    const tiles = options.map((option) => {
      const baseName = option.baseName ?? option.name;
      const users = inputUserRoutes(shared, option.value);
      const names = users.map(routeDisplayName);
      return `<label class="port-tile ${users.length ? "is-used" : ""}" ${users.length ? `data-color="${routeColor(users[0])}"` : ""}
        ${names.length ? `title="${escapeHtml(t("settings.inputUsedBy", { hosts: listFormat.format(names) }))}"` : ""}>
        <strong>${escapeHtml(baseName)}</strong>
        <span class="port-users">${users.map((routeId) => `<span class="port-user" data-color="${routeColor(routeId)}">${escapeHtml(routeDisplayName(routeId))}</span>`).join("")}</span>
        <input class="input-label-field" data-label-monitor="${escapeHtml(shared.monitorKey)}" data-label-input="${option.value}" value="${escapeHtml(option.label ?? "")}" placeholder="${escapeHtml(t("settings.inputLabelPlaceholder"))}" maxlength="${MAX_INPUT_LABEL_CHARS}" aria-label="${escapeHtml(t("settings.inputLabelAria", { monitor: shared.name, input: baseName }))}" />
      </label>`;
    }).join("");
    const discovery = selectedMonitorFor(shared)?.supportedInputs?.length
      ? t("settings.capabilitiesDetected", { count: (inputOptionsByMonitor[shared.monitorKey] ?? standardInputs).length })
      : t("settings.capabilitiesFallback");
    return `<div class="port-group">
      <div class="port-group-title"><strong>${escapeHtml(shared.name)}</strong></div>
      ${tiles ? `<div class="port-grid">${tiles}</div>` : ""}
      <p class="input-discovery-note">${escapeHtml(discovery)}</p>
    </div>`;
  }).join("");
}

/** Redraws every place that shows input names. */
function renderInputNames(): void {
  renderLocalInputSummary();
  renderHostList();
  renderInputLabels();
  renderDisplayMaintenance();
  renderSwitchPanel();
  refreshIcons();
}

async function reloadInputOptions(): Promise<void> {
  try {
    inputOptionsByMonitor = await loadInputOptionsByMonitor(dashboard.shared.map((shared) => shared.monitorKey));
    renderInputNames();
  } catch (error) {
    showToast(t("toast.inputLabelFailed"), String(error), true);
  }
}

async function commitInputLabel(field: HTMLInputElement): Promise<void> {
  const monitorKey = field.dataset.labelMonitor ?? "";
  // The field commits on blur, which is also what clicking anything else does,
  // so it can fire for a display that stopped being shared while it was open —
  // after a reset, or after the display was removed. A note for a display that
  // is no longer shared means nothing; reporting it as a failure to save does.
  if (!dashboard.shared.some((shared) => shared.monitorKey === monitorKey)) return;
  const input = Number(field.dataset.labelInput);
  const saved = inputOptionsByMonitor[monitorKey]?.find((option) => option.value === input)?.label ?? "";
  const label = field.value.trim();
  if (label === saved) {
    field.value = saved;
    return;
  }
  try {
    const options = await invoke<InputOption[]>("set_input_label", { monitorId: monitorKey, input, label });
    inputOptionsByMonitor = { ...inputOptionsByMonitor, [monitorKey]: options };
    field.value = options.find((option) => option.value === input)?.label ?? "";
    renderInputNames();
  } catch (error) {
    field.value = saved;
    showToast(t("toast.inputLabelFailed"), String(error), true);
  }
}

function renderPeerList(): void {
  const list = document.querySelector("#peer-list");
  if (!list) return;
  const available = discoveredPeers.filter((peer) => !settings.peers.some((item) => item.id === peer.id));
  list.innerHTML = available.length ? available.map((peer) => `<div class="row has-icon">
    <span class="app-icon is-accent"><i data-lucide="${platformIcon(peer.platform)}"></i></span>
    <div>
      <div class="row-title">${escapeHtml(hostNames[peer.id] ?? peer.name)}</div>
      <div class="row-sub">${platformName(peer.platform)} · ${t("settings.networkAuto")}</div>
    </div>
    <button type="button" class="button small primary" data-add-peer="${escapeHtml(peer.id)}"><i data-lucide="plus"></i>${t("action.add")}</button>
  </div>`).join("") : `<p class="empty-note">${t("settings.noAvailableHosts")}</p>`;
}

function peerInputValue(peerId: string, monitorKey: string): number | null {
  const shared = dashboard.shared.find((item) => item.monitorKey === monitorKey);
  const peer = settings.peers.find((item) => item.id === peerId);
  if (!shared || !peer) return null;
  return peer.inputs.find((assignment) => sameDisplay(assignment.monitor, shared.fingerprint))?.input ?? null;
}

/** The name a host falls back to when it has no custom one. */
function defaultRouteName(routeId: string): string {
  if (routeId === "local") {
    return dashboard.localHostName
      || (dashboard.localHost === "windows" ? t("dashboard.localWindows") : t("dashboard.localMac"));
  }
  return settings.peers.find((peer) => peer.id === routeId)?.name ?? routeId;
}

/** Every host, this one included, in the order the switch center shows them:
 *  named, reordered and diagnosed here. */
function renderHostList(): void {
  const container = document.querySelector("#host-list");
  if (!container) return;
  const routes = switchRoutes();
  container.innerHTML = routes.map((route, index) => hostRowHtml(route, index, routes.length)).join("");
}

function hostRowHtml(route: SwitchRoute, index: number, total: number): string {
  const peer = settings.peers.find((item) => item.id === route.id);
  const id = escapeHtml(route.id);
  const name = route.name;
  const isRenaming = renaming?.routeId === route.id;
  const title = isRenaming
    ? `<input class="field-input host-name-input" data-rename-input="${id}" value="${escapeHtml(renaming?.draft ?? name)}" placeholder="${escapeHtml(defaultRouteName(route.id))}" maxlength="${MAX_HOST_NAME_CHARS}" aria-label="${escapeHtml(t("dashboard.hostNameLabel"))}" />`
    : `<span class="host-title">${escapeHtml(name)}</span>${route.local ? `<span class="badge">${t("dashboard.localBadge")}</span>` : ""}`;
  const standing = `<span class="presence-line" title="${escapeHtml(presenceHelp(route.id))}">${presenceDotHtml(route.id)}${escapeHtml(presenceSummary(route.id))}</span>`;
  const sub = peer
    ? `${platformName(peer.platform)} · ${escapeHtml(peer.address)} · ${standing}`
    : platformName(route.platform);
  // A host that cannot be reached says so in the open: the reason is what
  // decides whether this is a sleeping host, the wrong network, or a pairing
  // password that no longer matches.
  const problem = peer ? presenceProblem(route.id) : "";
  const canWake = Boolean(peer?.macAddress.trim());
  const peerActions = peer ? `
    <button class="button small" type="button" data-probe-id="${id}" title="${escapeHtml(t("action.testConnectionHint", { name }))}">${t("action.testConnection")}</button>
    <button class="button small" type="button" data-wake-id="${id}" title="${escapeHtml(canWake ? t("action.sendWakeHint", { name }) : t("action.sendWakeUnavailable", { name }))}" ${canWake ? "" : "disabled"}>${t("action.sendWake")}</button>
    <button class="icon-button" type="button" data-remove-peer="${id}" aria-label="${escapeHtml(`${t("action.remove")}: ${name}`)}" title="${t("action.remove")}"><i data-lucide="trash-2"></i></button>` : "";
  const inputs = dashboard.shared.map((shared) => {
    // Each host reports its own port; it is set on that computer, not here.
    const value = routeInputFor(shared, route.id);
    const shown = value == null
      ? (peer ? t("settings.peerInputUnreported") : t("dashboard.inputUnset"))
      : inputName(value, shared.monitorKey);
    // A port is what the host is wired to; this is whether the display is
    // there right now. They answer different questions and both belong here.
    const attachment = attachmentFor(route.id, shared);
    const attachmentText = attachmentLabel(attachment);
    return `<div class="sub-row"><span>${escapeHtml(shared.name)}</span>
      <span class="sub-values">
        <output ${peer ? `title="${escapeHtml(t("settings.peerInputOwnHost"))}"` : ""}>${escapeHtml(shown)}</output>
        <span class="sub-tag is-${attachment}" title="${escapeHtml(attachmentTitle(route.id, route.name, shared))}">${escapeHtml(attachmentText)}</span>
      </span></div>`;
  }).join("");
  return `<div class="row has-icon host-row" data-route-card="${id}">
      <span class="drag-handle ${isRenaming ? "is-disabled" : ""}" ${isRenaming ? "" : "data-drag-handle"} role="img" aria-label="${escapeHtml(t("action.dragHandle"))}" title="${escapeHtml(t("action.dragHandle"))}"><i data-lucide="grip-vertical"></i></span>
      <button type="button" class="app-icon look-button ${editingLook === route.id ? "is-open" : ""}" data-color="${route.color}" data-edit-look="${id}"
        aria-expanded="${editingLook === route.id}" aria-label="${escapeHtml(t("hostLook.edit", { name }))}" title="${escapeHtml(t("hostLook.edit", { name }))}"><i data-lucide="${route.icon}"></i></button>
      <div>
        <div class="row-title">${title}</div>
        <div class="row-sub">${sub}</div>
        ${problem ? `<span class="row-hint is-warn">${escapeHtml(problem)}</span>` : ""}
      </div>
      <div class="row-actions" aria-label="${escapeHtml(t("settings.diagnosticAria", { name }))}">
        ${peerActions}
        <button type="button" class="icon-button" data-rename-route="${id}" aria-label="${escapeHtml(t("action.renameHost", { name }))}" title="${escapeHtml(t("action.renameHost", { name }))}"><i data-lucide="pencil"></i></button>
        <span class="order-buttons">
          <button type="button" class="icon-button" data-move-route="${id}" data-move-offset="-1" aria-label="${escapeHtml(t("action.moveHostEarlier", { name }))}" title="${escapeHtml(t("action.moveHostEarlier", { name }))}" ${index === 0 ? "disabled" : ""}><i data-lucide="chevron-up"></i></button>
          <button type="button" class="icon-button" data-move-route="${id}" data-move-offset="1" aria-label="${escapeHtml(t("action.moveHostLater", { name }))}" title="${escapeHtml(t("action.moveHostLater", { name }))}" ${index === total - 1 ? "disabled" : ""}><i data-lucide="chevron-down"></i></button>
        </span>
      </div>
    </div>
    ${editingLook === route.id ? lookEditorHtml(route) : ""}
    ${inputs ? `<div class="sub-rows">${inputs}</div>` : ""}`;
}

/** Icon and colour choices for one host. Every choice saves at once and
 *  reaches paired hosts, like a rename. */
function lookEditorHtml(route: SwitchRoute): string {
  const id = escapeHtml(route.id);
  const icons = HOST_ICONS.map((icon) => {
    const isChosen = route.icon === icon.lucide;
    return `<button type="button" class="look-icon ${isChosen ? "is-chosen" : ""}" data-color="${route.color}" data-look-route="${id}" data-look-icon="${icon.key}"
      aria-pressed="${isChosen}" aria-label="${escapeHtml(t(icon.label))}" title="${escapeHtml(t(icon.label))}"><i data-lucide="${icon.lucide}"></i></button>`;
  }).join("");
  const colors = HOST_COLORS.map((color) => {
    const isChosen = route.color === color.key;
    return `<button type="button" class="look-swatch ${isChosen ? "is-chosen" : ""}" data-color="${color.key}" data-look-route="${id}" data-look-color="${color.key}"
      aria-pressed="${isChosen}" aria-label="${escapeHtml(t(color.label))}" title="${escapeHtml(t(color.label))}"></button>`;
  }).join("");
  const isDefault = route.customIcon == null && route.customColor == null;
  return `<div class="look-editor" role="group" aria-label="${escapeHtml(t("hostLook.edit", { name: route.name }))}">
    <div class="look-section"><span class="caption">${t("hostLook.icon")}</span><div class="look-icons">${icons}</div></div>
    <div class="look-section"><span class="caption">${t("hostLook.color")}</span><div class="look-swatches">${colors}</div></div>
    <div class="look-actions">
      <button type="button" class="button small" data-look-route="${id}" data-look-reset ${isDefault ? "disabled" : ""}><i data-lucide="rotate-ccw"></i>${t("hostLook.reset")}</button>
      <button type="button" class="button small primary" data-close-look>${t("hostLook.done")}</button>
    </div>
  </div>`;
}

/** Whether a display has told some host which inputs it has. Until one of them
 *  has read it, the app knows only the standard MCCS codes, which describe no
 *  particular display — this one's only input in use is the vendor-specific
 *  value it calls 8, so every standard code on offer would be wrong for it. */
function displayInputsKnown(monitorKey: string): boolean {
  const shared = dashboard.shared.find((item) => item.monitorKey === monitorKey);
  return Boolean(shared && selectedMonitorFor(shared)?.supportedInputs?.length);
}

function renderInputOptions(routeId: string, monitorKey: string, current: number | null): string {
  // Offering a guess invites a choice that cannot be right, and a wrong port
  // sends a switch somewhere the display has nothing on. Whatever is already
  // set stays visible, since it may have come from the host that could read it.
  if (!displayInputsKnown(monitorKey)) {
    return current == null
      ? `<option value="" selected>${t("settings.inputsUnknown")}</option>`
      : `<option value="${current}" selected>${escapeHtml(inputName(current, monitorKey))}</option>`;
  }
  const assignedElsewhere = new Set<number>();
  const shared = dashboard.shared.find((item) => item.monitorKey === monitorKey);
  const selectedMonitor = shared ? selectedMonitorFor(shared) : undefined;
  if (routeId !== "local" && selectedMonitor?.localInput != null) assignedElsewhere.add(selectedMonitor.localInput);
  for (const peer of settings.peers) {
    if (peer.id === routeId) continue;
    const peerValue = peerInputValue(peer.id, monitorKey);
    if (peerValue != null) assignedElsewhere.add(peerValue);
  }
  const monitorOptions = inputOptionsByMonitor[monitorKey] ?? standardInputs;
  const options = monitorOptions
    .filter((item) => !assignedElsewhere.has(item.value) || item.value === current)
    .map((item) => `<option value="${item.value}" ${item.value === current ? "selected" : ""}>${escapeHtml(item.name)}</option>`)
    .join("");
  return `<option value="" ${current == null ? "selected" : ""}>${t("settings.selectInput")}</option>${options}`;
}

/** Position of a route in the saved order; routes not yet ordered sort last. */
function routeRank(routeId: string): number {
  const rank = routeOrder.indexOf(routeId);
  return rank === -1 ? Number.MAX_SAFE_INTEGER : rank;
}

/** Every current route id in display order, as the backend expects it. */
function currentRouteIds(): string[] {
  return ["local", ...settings.peers.map((peer) => peer.id)]
    .sort((left, right) => routeRank(left) - routeRank(right));
}

function movedRoute(order: string[], routeId: string, targetIndex: number): string[] {
  const without = order.filter((id) => id !== routeId);
  const clamped = Math.max(0, Math.min(targetIndex, without.length));
  return [...without.slice(0, clamped), routeId, ...without.slice(clamped)];
}

/** Redraws everything that shows host names, order or colour. */
function renderHostViews(): void {
  renderSwitchPanel();
  renderHostList();
  renderInputLabels();
}

async function saveRouteOrder(next: string[]): Promise<void> {
  const previous = routeOrder;
  routeOrder = next;
  renderHostViews();
  refreshIcons();
  try {
    routeOrder = await invoke<string[]>("set_host_order", { routeIds: next });
  } catch (error) {
    routeOrder = previous;
    showToast(t("toast.hostOrderFailed"), String(error), true);
  }
  renderHostViews();
  refreshIcons();
}

async function reloadHostOrder(): Promise<void> {
  try {
    routeOrder = await invoke<string[]>("get_host_order");
    renderHostViews();
    refreshIcons();
  } catch (error) {
    showToast(t("toast.hostOrderFailed"), String(error), true);
  }
}

function moveRouteBy(routeId: string, offset: number): void {
  const order = currentRouteIds();
  const index = order.indexOf(routeId);
  if (index === -1) return;
  const next = movedRoute(order, routeId, index + offset);
  if (next.join() === order.join()) return;
  void saveRouteOrder(next).then(() => {
    document.querySelector<HTMLButtonElement>(`[data-move-route="${cssEscape(routeId)}"][data-move-offset="${offset}"]:not(:disabled)`)?.focus();
  });
}

let draggedRouteId: string | null = null;

async function reloadHostNames(): Promise<void> {
  try {
    hostNames = await invoke<Record<string, string>>("get_host_names");
    renderHostViews();
    refreshIcons();
  } catch (error) {
    showToast(t("toast.hostNameFailed"), String(error), true);
  }
}

async function reloadHostAppearances(): Promise<void> {
  try {
    hostAppearances = await invoke<Record<string, CustomLook>>("get_host_appearances");
    renderHostViews();
    refreshIcons();
  } catch (error) {
    showToast(t("hostLook.failed"), String(error), true);
  }
}

/** Saves one part of a host's look, keeping the other part as it was chosen
 *  (an unchosen part stays at its default rather than freezing the default). */
async function setHostLook(routeId: string, change: { icon?: string; color?: string } | "reset"): Promise<void> {
  const current = hostAppearances[routeId];
  const icon = change === "reset" ? "" : change.icon ?? current?.icon ?? "";
  const color = change === "reset" ? "" : change.color ?? current?.color ?? "";
  if (isPreview) {
    const rest = Object.fromEntries(Object.entries(hostAppearances).filter(([key]) => key !== routeId));
    hostAppearances = icon || color ? { ...rest, [routeId]: { icon: icon || null, color: color || null } } : rest;
  } else {
    try {
      hostAppearances = await invoke<Record<string, CustomLook>>("set_host_appearance", { routeId, icon, color });
    } catch (error) {
      showToast(t("hostLook.failed"), String(error), true);
      return;
    }
  }
  renderHostViews();
  refreshIcons();
}

function toggleLookEditor(routeId: string | null): void {
  editingLook = editingLook === routeId ? null : routeId;
  renderHostList();
  refreshIcons();
  if (routeId) document.querySelector<HTMLButtonElement>(`[data-edit-look="${cssEscape(routeId)}"]`)?.focus();
}

function startRenaming(routeId: string): void {
  const shownTitle = document.querySelector<HTMLElement>(`[data-route-card="${cssEscape(routeId)}"] .host-title`);
  renaming = { routeId, draft: shownTitle?.textContent ?? hostNames[routeId] ?? "" };
  renderHostViews();
  refreshIcons();
  const field = document.querySelector<HTMLInputElement>(`[data-rename-input="${cssEscape(routeId)}"]`);
  field?.focus();
  field?.select();
}

function stopRenaming(routeId: string): void {
  if (renaming?.routeId !== routeId) return;
  renaming = null;
  renderHostViews();
  refreshIcons();
  document.querySelector<HTMLButtonElement>(`[data-rename-route="${cssEscape(routeId)}"]`)?.focus();
}

async function commitRename(routeId: string): Promise<void> {
  if (renaming?.routeId !== routeId) return;
  const { draft } = renaming;
  const defaultName = document.querySelector<HTMLInputElement>(`[data-rename-input="${cssEscape(routeId)}"]`)?.placeholder ?? "";
  const currentName = hostNames[routeId] ?? defaultName;
  stopRenaming(routeId);
  const name = draft.trim();
  if (name === currentName) return;
  try {
    // Typing the default name back is the same as clearing the custom one.
    hostNames = await invoke<Record<string, string>>("set_host_name", { routeId, name: name === defaultName ? "" : name });
    renderHostViews();
    refreshIcons();
  } catch (error) {
    showToast(t("toast.hostNameFailed"), String(error), true);
  }
}

/** A host's name as its card shows it: the custom name, else the default. */
function routeDisplayName(routeId: string): string {
  return hostNames[routeId] || defaultRouteName(routeId);
}

/**
 * One button per host that switches every shared display at once. Sits beside
 * the dashboard title because it acts on all displays, not on one of them. The
 * matrix has the same buttons in its column heads, so it hides this bar.
 */
function renderSwitchAllBar(): void {
  const container = document.querySelector<HTMLElement>("#switch-all-bar");
  if (!container) return;
  const isShown = dashboard.shared.length > 1 && currentSwitchView() === "stage";
  container.hidden = !isShown;
  if (!isShown) {
    container.innerHTML = "";
    return;
  }
  const buttons = switchRoutes().map((route) => {
    const name = escapeHtml(route.name);
    const isAllOnThisHost = dashboard.shared.every((shared) => activeRouteFor(shared) === route.id);
    if (isAllOnThisHost) {
      const unconfirmed = dashboard.shared.some(activeRouteUnconfirmed);
      const standing = unconfirmed ? t("dashboard.lastShown") : t("dashboard.allDisplayed");
      return `<span class="switch-all-host ${unconfirmed ? "is-unconfirmed" : "tint is-showing"}" data-color="${route.color}" title="${escapeHtml(`${route.name} · ${standing}`)}"><span class="swatch"></span>${name}</span>`;
    }
    const label = escapeHtml(t("action.switchAllToHost", { name: route.name }));
    return `<button type="button" class="switch-all-host glass" data-color="${route.color}" data-switch-all-id="${escapeHtml(route.id)}" aria-label="${label}" title="${label}" ${switchAllTargets(route.id).length === 0 ? "disabled" : ""}>
      <span class="swatch"></span>${name}
    </button>`;
  }).join("");
  container.innerHTML = `<span class="caption">${t("switcher.switchAll")}</span>${buttons}`;
}

async function scanPeers(): Promise<void> {
  const button = document.querySelector<HTMLButtonElement>("#scan-button");
  const label = button?.querySelector("span");
  if (button) button.disabled = true;
  if (label) label.textContent = t("action.searching");
  try {
    discoveredPeers = isPreview ? [] : await invoke<DiscoveredPeer[]>("discover_peers");
    renderPeerList(); refreshIcons();
    if (!discoveredPeers.length) showToast(t("toast.noPeersTitle"), t("toast.noPeersBody"), true);
  } catch (error) { showToast(t("toast.scanFailed"), String(error), true); }
  finally {
    if (button) button.disabled = false;
    if (label) label.textContent = t("action.searchAgain");
  }
}

/** Displays with a command in flight, by the id their row is keyed on.
 *
 *  Held here rather than on the button, because any refresh rebuilds the list
 *  and takes the button with it: two displays selected in quick succession had
 *  the first one's refresh drop the second back to its idle label while its own
 *  command was still running. Rendering from this survives that. */
const busyDisplays = new Set<string>();

/** Runs a display command, marking that display as working for as long as it
 *  takes, however often the list is rebuilt meanwhile. */
async function withBusyDisplay(key: string, run: () => Promise<void>): Promise<void> {
  if (busyDisplays.has(key)) return;
  busyDisplays.add(key);
  renderMonitors();
  refreshIcons();
  try {
    await run();
  } finally {
    busyDisplays.delete(key);
    renderMonitors();
    refreshIcons();
  }
}

/** Marks a button as working until its command settles. Re-rendering replaces
 *  the element, so restoring it afterwards is harmless when that happens. */
async function withBusyButton(button: HTMLButtonElement, run: () => Promise<void>): Promise<void> {
  if (button.disabled) return;
  button.disabled = true;
  button.classList.add("is-busy");
  try {
    await run();
  } finally {
    button.disabled = false;
    button.classList.remove("is-busy");
  }
}

async function addSharedMonitor(monitorId: string): Promise<void> {
  const monitor = dashboard.monitors.find((item) => item.id === monitorId);
  try {
    settings = await invoke<AppSettings>("add_shared_monitor", { monitorId });
    await refresh();
    showToast(t("toast.monitorSelected"), t("toast.monitorSelectedBody", { name: monitor?.name ?? t("dashboard.sharedDisplay") }));
  } catch (error) { showToast(t("toast.monitorSelectFailed"), String(error), true); }
}

async function removeSharedMonitor(monitorId: string): Promise<void> {
  try {
    settings = await invoke<AppSettings>("remove_shared_monitor", { monitorId });
    await refresh();
  } catch (error) { showToast(t("toast.monitorSelectFailed"), String(error), true); }
}

async function mergeSharedMonitor(aliasId: string, primaryId: string): Promise<void> {
  const primary = dashboard.shared.find((shared) => shared.monitorKey === primaryId);
  try {
    settings = await invoke<AppSettings>("set_monitor_identity_link", { aliasId, primaryId });
    await refresh();
    showToast(
      t("settings.mergeAction"),
      t("settings.mergedInto", { name: primary?.name ?? t("dashboard.sharedDisplay") }),
    );
  } catch (error) { showToast(t("toast.monitorSelectFailed"), String(error), true); }
}

/** Saves which input this computer occupies on a shared display. Announced to
 *  paired hosts like a detected one, so a correction here corrects where every
 *  other computer switches to. */
const MAINTENANCE_COMMANDS: Record<string, string> = {
  redetect: "redetect_display",
  resync: "resync_display_input",
  power: "power_cycle_display",
};

async function maintainDisplay(action: string, monitorKey: string): Promise<void> {
  const command = MAINTENANCE_COMMANDS[action];
  if (!command) return;
  try {
    const result = await invoke<OperationResult>(command, { monitorId: monitorKey });
    showToast(result.title, result.detail, result.warning);
    // A re-detection replaces the input list every other field here is drawn
    // from, and may have confirmed this computer's own input along the way.
    if (action === "redetect") await refresh();
  } catch (error) {
    showToast(t("toast.maintenanceFailed"), String(error), true);
  }
}

async function commitLocalInput(monitorKey: string, value: string): Promise<void> {
  const input = value === "" ? null : Number(value);
  try {
    settings = await invoke<AppSettings>("set_local_input", { monitorId: monitorKey, input });
    renderState();
    refreshIcons();
  } catch (error) {
    showToast(t("toast.inputLabelFailed"), String(error), true);
    renderLocalInputSummary();
  }
}

let pendingReset: { scope: string; button: HTMLButtonElement; timer: number } | null = null;

/** A reset button's own description, which arming replaces and disarming puts
 *  back. Re-derived from the scope rather than remembered from the element, so
 *  it cannot be restored to whatever the button happened to say last. */
function resetHint(scope: string): string {
  return scope === "everything"
    ? t("settings.resetEverythingHint")
    : t("settings.resetDisplaysHint");
}

/** Puts an armed button back the way it was. The reset section is part of the
 *  page built once at startup, so nothing re-renders it — a description left
 *  saying "press again" would say that until the app restarts. */
function disarmReset(): void {
  if (!pendingReset) return;
  window.clearTimeout(pendingReset.timer);
  const label = pendingReset.button.querySelector("small");
  if (label) label.textContent = resetHint(pendingReset.scope);
  pendingReset.button.classList.remove("is-confirming");
  pendingReset = null;
}

/** Asks once, then performs. The second press within ten seconds confirms; any
 *  other press, or the timeout, puts the button back. */
function requestReset(scope: string, button: HTMLButtonElement): void {
  const confirmed = pendingReset?.scope === scope;
  disarmReset();
  if (confirmed) {
    // Resetting restarts the agent and rewrites the settings file, so it is
    // not instant; without this the confirmed press looked like no press.
    void withBusyButton(button, () => performReset(scope));
    return;
  }
  const label = button.querySelector("small");
  if (label) label.textContent = t("settings.resetConfirm");
  button.classList.add("is-confirming");
  pendingReset = { scope, button, timer: window.setTimeout(disarmReset, 10_000) };
}

async function performReset(scope: string): Promise<void> {
  try {
    settings = await invoke<AppSettings>("reset_settings", { scope });
    await refresh();
    renderState();
    showToast(t("settings.resetTitle"), t("settings.resetDone"));
  } catch (error) { showToast(t("settings.resetTitle"), String(error), true); }
}

async function unmergeSharedMonitor(aliasId: string): Promise<void> {
  try {
    settings = await invoke<AppSettings>("set_monitor_identity_link", { aliasId, primaryId: null });
    await refresh();
    showToast(t("settings.mergeUndo"), t("settings.mergeUndone"));
  } catch (error) { showToast(t("toast.monitorSelectFailed"), String(error), true); }
}

async function addPeer(peerId: string): Promise<void> {
  try {
    const sharedKey = document.querySelector<HTMLInputElement>("#shared-key")?.value ?? "";
    settings = await invoke<AppSettings>("select_peer", { peerId, sharedKey });
    const added = settings.peers.find((peer) => peer.id === peerId);
    renderState();
    const detectedPorts = (added?.inputs ?? []).map((assignment) => {
      const shared = dashboard.shared.find((item) => sameDisplay(item.fingerprint, assignment.monitor));
      return shared ? `${shared.name}: ${inputName(assignment.input, shared.monitorKey)}` : String(assignment.input);
    });
    showToast(t("toast.peerAdded"), detectedPorts.length ? t("toast.peerPortDetected", { port: detectedPorts.join(", ") }) : t("toast.peerAddedBody"));
    void loadHostPresence(true);
  }
  catch (error) { showToast(t("toast.peerAddFailed"), String(error), true); }
}

async function removePeer(peerId: string): Promise<void> {
  try { settings = await invoke<AppSettings>("remove_peer", { peerId }); renderState(); }
  catch (error) { showToast(t("toast.peerRemoveFailed"), String(error), true); }
}

async function saveSettings(event: SubmitEvent): Promise<void> {
  event.preventDefault();
  try {
    settings = {
      ...settings,
      sharedKey: document.querySelector<HTMLInputElement>("#shared-key")?.value ?? "",
      waitSeconds: Number(document.querySelector<HTMLInputElement>("#wait-seconds")?.value ?? 45),
      autostart: document.querySelector<HTMLInputElement>("#autostart")?.checked ?? true,
      checkUpdates: document.querySelector<HTMLInputElement>("#check-updates")?.checked ?? true,
      hostSwitcherEnabled: document.querySelector<HTMLInputElement>("#host-switcher-enabled")?.checked ?? false,
      hostSwitcherShortcut: settings.hostSwitcherShortcut,
    };
    const result = await invoke<OperationResult>("save_settings", { settings });
    setUnsavedVisible(false);
    showToast(result.title, result.detail); await refresh();
  } catch (error) { showToast(t("toast.settingsFailed"), String(error), true); }
}

function switchProgressText(event: SwitchProgressEvent): { title: string; detail: string } {
  if (event.event === "waking") return { title: t("operation.wakingTitle", { name: event.peerName }), detail: t("operation.wakingBody") };
  if (event.event === "checking") return { title: t("operation.checkingTitle", { name: event.peerName }), detail: t("operation.checkingBody") };
  if (event.event === "waiting") return { title: t("operation.waitingTitle", { name: event.peerName }), detail: t("operation.waitingBody", { seconds: event.seconds }) };
  if (event.event === "remoteFallback") return { title: t("operation.remoteTitle", { name: event.peerName }), detail: t("operation.remoteBody") };
  return { title: t("operation.switchingTitle"), detail: t("operation.switchingBody") };
}

/**
 * Runs one backend switch and mirrors its progress in the operation dialog.
 * With a `step` label, the dialog keeps that label as its title so a batch
 * shows which display it is on.
 */
async function requestSwitch(monitorKey: string, targetId: string, step?: string): Promise<OperationResult> {
  const onEvent = new Channel<SwitchProgressEvent>();
  onEvent.onmessage = (event) => {
    const { title, detail } = switchProgressText(event);
    if (step) showOperation(step, title);
    else showOperation(title, detail);
  };
  return invoke<OperationResult>("switch_host", { monitorId: monitorKey, targetId, onEvent });
}

async function switchHost(monitorKey: string, targetId: string): Promise<void> {
  showOperation(t("operation.preparingTitle"), t("operation.preparingBody"));
  try {
    const result = await requestSwitch(monitorKey, targetId);
    showToast(result.title, result.detail, result.warning);
    await refresh();
    scheduleSettledRescans();
  } catch (error) {
    showToast(t("toast.switchFailed"), String(error), true);
  } finally {
    hideOperation();
  }
}

/** The display input a host uses on a shared display, or null when it is not configured. */
function routeInputFor(shared: SharedMonitorStatus, routeId: string): number | null {
  if (routeId === "local") return selectedMonitorFor(shared)?.localInput ?? null;
  const peer = settings.peers.find((item) => item.id === routeId);
  return peer?.inputs.find((assignment) => sameDisplay(assignment.monitor, shared.fingerprint))?.input ?? null;
}

/** Shared displays that switching everything to this host would change. */
function switchAllTargets(routeId: string): SharedMonitorStatus[] {
  return dashboard.shared.filter((shared) =>
    (selectedMonitorFor(shared)?.activeRoute ?? "local") !== routeId
    && routeInputFor(shared, routeId) != null
    && (shared.ddcAvailable || dashboard.agentConfigured));
}

/**
 * Switches every shared display to one host, one display at a time: the first
 * switch wakes a sleeping host, so later ones find it ready. A failed display
 * does not stop the rest.
 */
async function switchAllToHost(targetId: string): Promise<void> {
  const hostName = routeDisplayName(targetId);
  const targets = switchAllTargets(targetId);
  if (!targets.length) return;
  const problems: string[] = [];
  let switched = 0;
  showOperation(t("operation.preparingTitle"), t("operation.preparingBody"));
  try {
    for (const [index, shared] of targets.entries()) {
      const step = t("operation.switchAllStep", { current: index + 1, total: targets.length, name: shared.name });
      showOperation(step, t("operation.preparingBody"));
      try {
        const result = await requestSwitch(shared.monitorKey, targetId, step);
        switched += 1;
        if (result.warning) problems.push(`${shared.name}: ${result.detail}`);
      } catch (error) {
        problems.push(`${shared.name}: ${String(error)}`);
      }
    }
  } finally {
    hideOperation();
  }
  const title = switched === targets.length
    ? t("toast.switchAllDone", { count: switched, name: hostName })
    : t("toast.switchAllPartial", { count: switched, total: targets.length, name: hostName });
  showToast(switched === 0 ? t("toast.switchFailed") : title, problems.join(" "), problems.length > 0);
  await refresh();
  scheduleSettledRescans();
}

async function peerCommand(command: "probe_peer" | "wake_peer", peerId: string): Promise<void> {
  try {
    const result = await invoke<OperationResult>(command, { peerId });
    showToast(result.title, result.detail, result.warning);
    // A connection test adopts whatever input the host reported for itself.
    if (command === "probe_peer") await reloadPeerInputs();
  } catch (error) {
    showToast(command === "probe_peer" ? t("toast.probeFailed") : t("toast.wakeFailed"), String(error), true);
  }
}

async function checkForUpdates(manual: boolean): Promise<void> {
  if (isPreview) {
    if (manual) showToast(t("toast.updateUnavailable"), t("toast.updateUnavailableBody"), true);
    return;
  }
  const button = document.querySelector<HTMLButtonElement>("#update-button");
  button?.classList.add("is-checking");
  try {
    const update = await invoke<UpdateInfo>("check_for_update");
    pendingUpdate = update.available ? update : null;
    button?.classList.toggle("has-update", update.available);
    button?.setAttribute("title", update.available ? t("update.availableTooltip", { version: update.version ?? "" }) : t("action.checkUpdates"));
    if (update.available) {
      if (manual) showUpdateDialog(update);
      else showToast(t("update.availableTitle"), t("update.availableBody", { version: update.version ?? "" }));
    } else if (manual) {
      showToast(t("update.latestTitle"), t("update.latestBody", { version: update.currentVersion }));
    }
  } catch (error) {
    if (manual) showToast(t("toast.updateFailed"), String(error), true);
  } finally {
    button?.classList.remove("is-checking");
  }
}

function showUpdateDialog(update: UpdateInfo): void {
  setText("#update-title", `MuxSU ${update.version ?? ""}`);
  setText("#update-version", t("update.currentVersion", { version: update.currentVersion }));
  const notes = document.querySelector<HTMLElement>("#update-notes");
  if (notes) renderMarkdown(notes, update.notes?.trim() || t("update.noneNotes"));
  const overlay = document.querySelector("#update-overlay");
  overlay?.classList.add("is-visible");
  overlay?.setAttribute("aria-hidden", "false");
}

function showOnboarding(step: number): void {
  clearOnboardingTarget();
  onboardingStep = Math.max(0, Math.min(step, onboardingSteps.length - 1));
  const current = onboardingSteps[onboardingStep];
  showPage(current.page);
  if (current.tab) showSettingsTab(current.tab);
  setText("#onboarding-counter", t("onboarding.progress", { current: onboardingStep + 1, total: onboardingSteps.length }));
  setText("#onboarding-step-label", current.label);
  setText("#onboarding-title", current.title);
  setText("#onboarding-body", current.body);

  const status = document.querySelector<HTMLElement>("#onboarding-status");
  const statusCopy = onboardingStatus(onboardingStep);
  if (status) {
    status.hidden = statusCopy == null;
    status.classList.toggle("is-ready", statusCopy?.ready ?? false);
  }
  if (statusCopy) {
    setText("#onboarding-status-title", statusCopy.title);
    setText("#onboarding-status-detail", statusCopy.detail);
  }

  const previous = document.querySelector<HTMLButtonElement>("#onboarding-previous");
  if (previous) previous.hidden = onboardingStep === 0;
  setText("#onboarding-next", onboardingStep === onboardingSteps.length - 1 ? t("onboarding.finishTour") : t("onboarding.next"));

  const overlay = document.querySelector("#onboarding-overlay");
  const tooltip = document.querySelector("#onboarding-tooltip");
  overlay?.classList.add("is-visible");
  overlay?.setAttribute("aria-hidden", "false");
  tooltip?.classList.add("is-visible");
  tooltip?.setAttribute("aria-hidden", "false");

  const workspace = document.querySelector<HTMLElement>(".workspace");
  if (onboardingStep <= 2) workspace?.scrollTo({ top: 0, behavior: "auto" });
  window.requestAnimationFrame(() => {
    const target = document.querySelector<HTMLElement>(current.target);
    if (!target) return;
    target.scrollIntoView({ block: "center", inline: "nearest", behavior: "auto" });
    target.classList.add("onboarding-target");
    target.setAttribute("aria-describedby", "onboarding-title onboarding-body");
    positionOnboardingTooltip(target);
    document.querySelector<HTMLButtonElement>("#onboarding-next")?.focus();
  });
}

function onboardingStatus(step: number): { ready: boolean; title: string; detail: string } | null {
  if (step === 2) {
    if (settings.sharedMonitors.length > 0) {
      return { ready: true, title: t("onboarding.displaySelected"), detail: settings.sharedMonitors.map((sm) => sm.name).join(", ") };
    }
    if (dashboard.monitors.length > 0) {
      return { ready: true, title: t("onboarding.displaysDetected", { count: dashboard.monitors.length }), detail: t("onboarding.displaysDetectedDetail") };
    }
    return { ready: false, title: t("onboarding.noDisplayDetected"), detail: t("onboarding.noDisplayDetectedDetail") };
  }
  if (step === 3) {
    const ready = [...settings.sharedKey].length >= MIN_SHARED_KEY_LENGTH;
    return {
      ready,
      title: ready ? t("onboarding.pairingReady") : t("onboarding.pairingNotReady"),
      detail: ready ? t("onboarding.pairingReadyDetail") : t("onboarding.pairingNotReadyDetail"),
    };
  }
  return null;
}

function clearOnboardingTarget(): void {
  document.querySelectorAll<HTMLElement>(".onboarding-target").forEach((target) => {
    target.classList.remove("onboarding-target");
    target.removeAttribute("aria-describedby");
  });
}

function positionOnboardingTooltip(explicitTarget?: HTMLElement): void {
  const tooltip = document.querySelector<HTMLElement>("#onboarding-tooltip");
  if (!tooltip?.classList.contains("is-visible")) return;
  const target = explicitTarget ?? document.querySelector<HTMLElement>(".onboarding-target");
  if (!target) return;

  const targetRect = target.getBoundingClientRect();
  const tooltipRect = tooltip.getBoundingClientRect();
  const gap = 18;
  const edge = 16;
  const preferred = onboardingSteps[onboardingStep].placement;
  const placements = [preferred, "right", "left", "bottom", "top"]
    .filter((placement, index, all) => all.indexOf(placement) === index);

  const coordinates = (placement: string): { left: number; top: number } => {
    if (placement === "left") return { left: targetRect.left - tooltipRect.width - gap, top: targetRect.top + (targetRect.height - tooltipRect.height) / 2 };
    if (placement === "bottom") return { left: targetRect.left + (targetRect.width - tooltipRect.width) / 2, top: targetRect.bottom + gap };
    if (placement === "top") return { left: targetRect.left + (targetRect.width - tooltipRect.width) / 2, top: targetRect.top - tooltipRect.height - gap };
    return { left: targetRect.right + gap, top: targetRect.top + (targetRect.height - tooltipRect.height) / 2 };
  };

  let placement = placements[0];
  let position = coordinates(placement);
  for (const candidate of placements) {
    const next = coordinates(candidate);
    if (next.left >= edge && next.top >= edge && next.left + tooltipRect.width <= window.innerWidth - edge && next.top + tooltipRect.height <= window.innerHeight - edge) {
      placement = candidate;
      position = next;
      break;
    }
  }

  tooltip.dataset.placement = placement;
  tooltip.style.left = `${Math.min(Math.max(position.left, edge), window.innerWidth - tooltipRect.width - edge)}px`;
  tooltip.style.top = `${Math.min(Math.max(position.top, edge), window.innerHeight - tooltipRect.height - edge)}px`;
}

async function completeOnboarding(openSettings: boolean): Promise<void> {
  try {
    settings = isPreview
      ? { ...settings, onboardingCompleted: true }
      : await invoke<AppSettings>("complete_onboarding");
    const overlay = document.querySelector("#onboarding-overlay");
    const tooltip = document.querySelector("#onboarding-tooltip");
    clearOnboardingTarget();
    overlay?.classList.remove("is-visible");
    overlay?.setAttribute("aria-hidden", "true");
    tooltip?.classList.remove("is-visible");
    tooltip?.setAttribute("aria-hidden", "true");
    diagnostics.askIfUnasked();
    if (openSettings) {
      showPage("settings");
      showSettingsTab("displays");
      window.requestAnimationFrame(() => document.querySelector<HTMLButtonElement>("[data-monitor-id]:not(:disabled)")?.focus());
    }
  } catch (error) {
    showToast(t("toast.onboardingFailed"), String(error), true);
  }
}

function appendInlineMarkdown(parent: HTMLElement, source: string): void {
  const pattern = /(\[([^\]]+)\]\(([^)\s]+)\)|\*\*([^*]+)\*\*|`([^`]+)`|\*([^*]+)\*)/g;
  let cursor = 0;

  for (const match of source.matchAll(pattern)) {
    const index = match.index ?? 0;
    parent.append(document.createTextNode(source.slice(cursor, index)));
    if (match[2] && match[3]) {
      try {
        const url = new URL(match[3]);
        if (url.protocol !== "https:") throw new Error("unsupported Markdown link protocol");
        const link = document.createElement("a");
        link.href = url.href;
        link.target = "_blank";
        link.rel = "noopener noreferrer";
        link.textContent = match[2];
        parent.append(link);
      } catch {
        parent.append(document.createTextNode(match[0]));
      }
    } else if (match[4]) {
      const strong = document.createElement("strong");
      strong.textContent = match[4];
      parent.append(strong);
    } else if (match[5]) {
      const code = document.createElement("code");
      code.textContent = match[5];
      parent.append(code);
    } else if (match[6]) {
      const emphasis = document.createElement("em");
      emphasis.textContent = match[6];
      parent.append(emphasis);
    }
    cursor = index + match[0].length;
  }
  parent.append(document.createTextNode(source.slice(cursor)));
}

function renderMarkdown(container: HTMLElement, source: string): void {
  container.replaceChildren();
  let list: HTMLUListElement | HTMLOListElement | null = null;

  for (const rawLine of source.replace(/\r\n?/g, "\n").split("\n")) {
    const line = rawLine.trim();
    if (!line) {
      list = null;
      continue;
    }

    const heading = /^(#{1,6})\s+(.+)$/.exec(line);
    if (heading) {
      list = null;
      const element = document.createElement(`h${heading[1].length}`) as HTMLHeadingElement;
      appendInlineMarkdown(element, heading[2]);
      container.append(element);
      continue;
    }

    if (/^(?:-{3,}|\*{3,}|_{3,})$/.test(line)) {
      list = null;
      container.append(document.createElement("hr"));
      continue;
    }

    const listItem = /^(?:([-*+])|(\d+)\.)\s+(.+)$/.exec(line);
    if (listItem) {
      const tagName = listItem[2] ? "OL" : "UL";
      if (!list || list.tagName !== tagName) {
        list = document.createElement(tagName.toLowerCase()) as HTMLUListElement | HTMLOListElement;
        container.append(list);
      }
      const item = document.createElement("li");
      appendInlineMarkdown(item, listItem[3]);
      list.append(item);
      continue;
    }

    list = null;
    const quote = /^>\s?(.*)$/.exec(line);
    const element = document.createElement(quote ? "blockquote" : "p");
    appendInlineMarkdown(element, quote?.[1] ?? line);
    container.append(element);
  }
}

function hideUpdateDialog(): void {
  const overlay = document.querySelector("#update-overlay");
  overlay?.classList.remove("is-visible");
  overlay?.setAttribute("aria-hidden", "true");
}

async function installUpdate(): Promise<void> {
  const installButton = document.querySelector<HTMLButtonElement>("#update-install");
  const cancelButton = document.querySelector<HTMLButtonElement>("#update-cancel");
  const progress = document.querySelector<HTMLElement>("#update-progress");
  if (installButton) { installButton.disabled = true; installButton.textContent = t("update.preparing"); }
  if (cancelButton) cancelButton.disabled = true;
  if (progress) progress.hidden = false;
  const onEvent = new Channel<UpdateDownloadEvent>();
  onEvent.onmessage = (event) => {
    if (event.event === "started") {
      setText("#update-progress-label", t("update.downloadingSigned"));
    } else if (event.event === "progress") {
      const percent = event.contentLength ? Math.min(100, Math.round(event.downloaded / event.contentLength * 100)) : 0;
      const bar = document.querySelector<HTMLElement>("#update-progress-bar");
      if (bar) bar.style.width = event.contentLength ? `${percent}%` : "35%";
      setText("#update-progress-label", event.contentLength ? t("update.downloaded", { percent }) : t("update.downloading"));
    } else {
      setText("#update-progress-label", t("update.installing"));
    }
  };
  try {
    await invoke("install_update", { onEvent });
  } catch (error) {
    showToast(t("toast.installFailed"), String(error), true);
    if (installButton) { installButton.disabled = false; installButton.textContent = t("update.retry"); }
    if (cancelButton) cancelButton.disabled = false;
  }
}

function inputName(value: number, monitorKey: string): string {
  const known = (inputOptionsByMonitor[monitorKey] ?? standardInputs).find((item) => item.value === value);
  return known ? known.name : t("input.other");
}
function platformName(value: Platform): string { return value === "mac" ? "macOS" : "Windows"; }
function isUltrawideResolution(value: MonitorResolution): boolean { return value.height > 0 && value.width >= value.height * 2; }
function resolutionSourceName(value: ResolutionSource | null): string {
  if (value === "edid") return "EDID";
  if (value === "coreGraphicsDisplayMode") return t("resolution.coreGraphics");
  if (value === "windowsDisplayMode") return t("resolution.windows");
  return t("resolution.unknown");
}
/** Whether two fingerprints name one physical display. Rust resolves every
 *  fingerprint with the authoritative serial-number and merge rules; this
 *  side only compares the opaque identities returned with the dashboard. */
function sameDisplay(left: Fingerprint, right: Fingerprint): boolean {
  const leftIdentity = dashboard.resolvedMonitorIdentities[JSON.stringify(left)];
  const rightIdentity = dashboard.resolvedMonitorIdentities[JSON.stringify(right)];
  return leftIdentity != null && leftIdentity === rightIdentity;
}

/** Whether a display present right now is one of this computer's shared displays. */
function isSharedDisplay(fingerprint: Fingerprint): boolean {
  return settings.sharedMonitors.some((sm) => sameDisplay(sm.fingerprint, fingerprint));
}

function escapeHtml(value: string): string { return value.replace(/[&<>'"]/g, (char) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;" })[char] ?? char); }
function cssEscape(value: string): string { return typeof CSS !== "undefined" && CSS.escape ? CSS.escape(value) : value.replace(/["\\]/g, "\\$&"); }
function setText(selector: string, value: string): void { const element = document.querySelector(selector); if (element) element.textContent = value; }
function setInput(selector: string, value: string): void { const element = document.querySelector<HTMLInputElement>(selector); if (element) element.value = value; }
function showOperation(title: string, detail?: string): void {
  setText("#operation-title", title);
  if (detail) setText("#operation-detail", detail);
  document.querySelector("#operation-overlay")?.classList.add("is-visible");
}
function hideOperation(): void { document.querySelector("#operation-overlay")?.classList.remove("is-visible"); }
let toastTimer = 0;
function showToast(title: string, detail: string, warning = false): void {
  const toast = document.querySelector("#toast"); if (!toast) return;
  window.clearTimeout(toastTimer); setText("#toast-title", title); setText("#toast-detail", detail);
  toast.classList.toggle("is-warning", warning); toast.classList.add("is-visible");
  toastTimer = window.setTimeout(() => toast.classList.remove("is-visible"), 5200);
}

async function bootstrap(): Promise<void> {
  if (!isPreview) {
    try { await invoke("set_locale", { locale }); } catch { /* Preview mode has no Tauri backend. */ }
  }
  await Promise.all([refresh(), renderAppVersion()]);
  if (!isPreview) {
    try {
      await listen(ACTIVE_ROUTE_CHANGED_EVENT, () => void reloadActiveRoutes());
      await listen(HOST_ORDER_CHANGED_EVENT, () => void reloadHostOrder());
      await listen(HOST_NAMES_CHANGED_EVENT, () => void reloadHostNames());
      await listen(HOST_APPEARANCES_CHANGED_EVENT, () => void reloadHostAppearances());
      await listen(INPUT_LABELS_CHANGED_EVENT, () => void reloadInputOptions());
      await listen(PEER_INPUTS_CHANGED_EVENT, () => void reloadPeerInputs());
      // A merge changes which displays exist and what they are called, which
      // only a full scan can work out, so this reloads everything.
      await listen(MONITOR_IDENTITIES_CHANGED_EVENT, () => void refresh());
    } catch (error) {
      showToast(t("toast.activeHostSyncFailed"), String(error), true);
    }
    window.addEventListener("focus", refreshOnReturn);
    document.addEventListener("visibilitychange", refreshOnReturn);
    startPeriodicRefresh();
    startPresenceChecks();
  }
  void refreshReleaseHistory();
  if (!settings.onboardingCompleted) showOnboarding(0);
  else diagnostics.askIfUnasked();
  if (settings.checkUpdates && !isPreview) window.setTimeout(() => void checkForUpdates(false), 1800);
}

async function renderAppVersion(): Promise<void> {
  try {
    setText("#app-version", `v${await getVersion()}`);
  } catch {
    setText("#app-version", `v${packageMetadata.version}`);
  }
}

void bootstrap();
