/**
 * A stand-in Tauri backend for looking at the UI in a browser with realistic
 * data. Development only: loaded by demo.html / demo-switcher.html, never by
 * the app's own entry points, so nothing here ships.
 *
 * Query parameters:
 *   scenario = 2x2 (default) | 1x3 | 3x4 | 1x5 | empty | merge
 *   lang     = zh-TW | en
 *   theme    = light | dark
 */

type Platform = "windows" | "mac";
interface Fingerprint { manufacturer_id: string; product_code: string; serial_number: string | null }
interface DemoDisplay { key: string; name: string; fp: Fingerprint; width: number; height: number; inputs: number[] }
interface DemoPeer { id: string; name: string; platform: Platform; address: string; mac: string }

const params = new URLSearchParams(location.search);
const scenario = params.get("scenario") ?? "2x2";
/** Demo names follow the requested language, so an English screenshot shows no Chinese. */
const isEnglish = params.get("lang") === "en";
try {
  const lang = params.get("lang");
  if (lang === "zh-TW" || lang === "en") localStorage.setItem("muxsu.locale", lang);
  const theme = params.get("theme");
  if (theme === "light" || theme === "dark") localStorage.setItem("muxsu.theme", theme);
  if (!params.has("view")) localStorage.removeItem("muxsu.switchView");
  else localStorage.setItem("muxsu.switchView", params.get("view") ?? "stage");
} catch { /* Storage may be unavailable; the defaults still render. */ }

const HDMI1 = 0x11, HDMI2 = 0x12, DP = 0x0f, TYPEC = 0x1b, DVI = 0x03, VGA = 0x01;
const inputNames: Record<number, string> = { [VGA]: "VGA", [DVI]: "DVI", [DP]: "DP", [HDMI1]: "HDMI 1", [HDMI2]: "HDMI 2", [TYPEC]: "Type-C" };

const displays: DemoDisplay[] = [
  { key: "demo-uw34", name: "DEMO Ultrawide 34", fp: { manufacturer_id: "DMO", product_code: "3410", serial_number: "DEMO-001" }, width: 3440, height: 1440, inputs: [HDMI1, HDMI2, DP, TYPEC, DVI, VGA] },
  { key: "demo-st27", name: "DEMO Studio 27", fp: { manufacturer_id: "DMO", product_code: "2720", serial_number: "DEMO-002" }, width: 3840, height: 2160, inputs: [HDMI1, DP, TYPEC, HDMI2] },
  { key: "demo-u24", name: "Dell U2421E", fp: { manufacturer_id: "DEL", product_code: "A1F4", serial_number: "DEMO-003" }, width: 1920, height: 1200, inputs: [DP, HDMI1, HDMI2, TYPEC] },
];
const allPeers: DemoPeer[] = [
  { id: "peer-macmini", name: isEnglish ? "Work Mac mini" : "工作用 Mac mini", platform: "mac", address: "192.168.1.20", mac: "AA:BB:CC:00:00:01" },
  { id: "peer-nuc", name: isEnglish ? "Render PC" : "算圖主機", platform: "windows", address: "192.168.1.31", mac: "AA:BB:CC:00:00:02" },
  { id: "peer-air", name: "MacBook Air", platform: "mac", address: "192.168.1.42", mac: "" },
  { id: "peer-thinkpad", name: "ThinkPad", platform: "windows", address: "192.168.1.55", mac: "AA:BB:CC:00:00:04" },
];
/** Which input each host uses, per display, in host order (local first). */
const portPlan: Record<string, number[]> = {
  "demo-uw34": [HDMI1, DP, HDMI2, TYPEC, DVI],
  "demo-st27": [DP, HDMI1, -1, TYPEC, HDMI2],
  "demo-u24": [DP, HDMI1, HDMI2, -1, TYPEC],
};

const shape = {
  "2x2": { displays: 2, peers: 1, active: ["local", "peer-macmini"] },
  "1x3": { displays: 1, peers: 2, active: ["peer-macmini"] },
  "3x4": { displays: 3, peers: 3, active: ["local", "peer-macmini", "peer-nuc"] },
  "1x5": { displays: 1, peers: 4, active: ["peer-nuc"] },
  "empty": { displays: 0, peers: 1, active: [] },
  // Studio 27 reports a different identity in another display mode.
  "merge": { displays: 2, peers: 1, active: ["local", "local"] },
}[scenario] ?? { displays: 2, peers: 1, active: ["local", "peer-macmini"] };

const shared = displays.slice(0, shape.displays);
const peers = allPeers.slice(0, shape.peers);
const routeIds = ["local", ...peers.map((peer) => peer.id)];
const activeRoute: Record<string, string> = Object.fromEntries(shared.map((display, index) => [display.key, shape.active[index] ?? "local"]));
const hostNames: Record<string, string> = { local: isEnglish ? "This Windows PC" : "這台 Windows PC" };
const inputLabels: Record<string, Record<number, string>> = { "demo-uw34": { [TYPEC]: isEnglish ? "USB-C dock" : "USB-C 擴充座" } };
/** Custom looks by route id, as `get_host_appearances` returns them. */
const hostAppearances: Record<string, { icon: string | null; color: string | null }> =
  scenario === "3x4" ? { "peer-nuc": { icon: "server", color: "green" } } : {};
let hostOrder = [...routeIds];

function inputFor(displayKey: string, routeId: string): number | null {
  const value = portPlan[displayKey]?.[routeIds.indexOf(routeId)];
  return value == null || value < 0 ? null : value;
}

function identities(): Record<string, string> {
  return Object.fromEntries(displays.map((display) => [JSON.stringify(display.fp), display.key]));
}

function monitorDescriptor(display: DemoDisplay) {
  return {
    id: display.key, name: display.name, active: true, builtIn: false, fingerprint: display.fp,
    maxResolution: { width: display.width, height: display.height }, resolutionSource: "windowsDisplayMode",
    connection: { hostOutput: "hdmi", sinkInterface: "hdmi", sharesUsbData: false, signalConversion: false, ddcRisk: "low" },
  };
}

/** Studio 27 as it reports itself at 1920×1080: same panel, another product code. */
const studioAtLowRes: DemoDisplay = {
  key: "demo-st27-1080", name: "DEMO Studio 27", fp: { manufacturer_id: "DMO", product_code: "7270", serial_number: null },
  width: 1920, height: 1080, inputs: [],
};

function dashboardState() {
  const present = scenario === "empty" ? displays.slice(0, 2)
    : scenario === "merge" ? [displays[0], studioAtLowRes]
    : displays.slice(0, Math.max(shape.displays, 2));
  return {
    platform: "windows", localHost: "windows", agentConfigured: true,
    monitors: present.map(monitorDescriptor),
    uncontrollableMonitors: [],
    shared: shared.map((display) => ({
      monitorKey: display.key, fingerprint: display.fp, name: display.name, ddcAvailable: activeRoute[display.key] === "local",
      displayState: activeRoute[display.key] === "local" ? "ready" : "onOtherHost",
      statusText: "", connection: null, connectionInputConflict: false,
    })),
    selectionNotices: [],
    monitorIdentityClaims: scenario === "merge"
      ? [{ aliasKey: "DMO/3411/", aliasLabel: "DEMO Ultrawide 34 (DMO/3411)", primaryKey: "demo-uw34", primaryLabel: "DEMO Ultrawide 34" }]
      : [],
    // What the backend's curated table of multi-identity models says about the
    // display that just turned up: this Studio 27 at 1080p is the shared one at
    // 4K. The real table carries only hardware someone has confirmed, so the
    // demo's fictional model is named here rather than shipped in it.
    mergeSuggestions: scenario === "merge"
      ? [{ monitorId: studioAtLowRes.key, primaryKey: displays[1].key, primaryLabel: "DMO / 2720" }]
      : [],
    resolvedMonitorIdentities: { ...identities(), [JSON.stringify(studioAtLowRes.fp)]: studioAtLowRes.key },
    localHostName: "DESKTOP-DEMO",
  };
}

function appSettings() {
  return {
    localHost: "windows",
    sharedMonitors: shared.map((display) => ({
      name: display.name, fingerprint: display.fp, maxResolution: { width: display.width, height: display.height },
      resolutionSource: "windowsDisplayMode", localInput: inputFor(display.key, "local"), supportedInputs: display.inputs,
      activeRoute: activeRoute[display.key],
    })),
    peers: peers.map((peer) => ({
      id: peer.id, name: peer.name, platform: peer.platform, address: peer.address, port: 47821, macAddress: peer.mac,
      inputs: shared.flatMap((display) => {
        const input = inputFor(display.key, peer.id);
        return input == null ? [] : [{ monitor: display.fp, input }];
      }),
    })),
    broadcastIp: "255.255.255.255", wakePort: 9, sharedKey: "demo-pairing-password", waitSeconds: 45,
    autostart: true, checkUpdates: true, onboardingCompleted: true, hostSwitcherEnabled: true,
    hostSwitcherShortcut: "CommandOrControl+Alt+Space", diagnosticsEnabled: false, diagnosticsAsked: true,
  };
}

function inputOptions(displayKey: string) {
  const display = displays.find((item) => item.key === displayKey);
  return (display?.inputs ?? []).map((value) => {
    const label = inputLabels[displayKey]?.[value];
    const baseName = inputNames[value] ?? String(value);
    return { value, name: label ? `${baseName} (${label})` : baseName, baseName, label: label ?? null };
  });
}

function routeName(routeId: string): string {
  return hostNames[routeId] ?? peers.find((peer) => peer.id === routeId)?.name ?? routeId;
}

function switcherState() {
  return {
    monitors: shared.map((display) => ({
      monitorKey: display.key,
      name: display.name,
      hosts: hostOrder.map((routeId) => {
        const input = inputFor(display.key, routeId);
        const peer = peers.find((item) => item.id === routeId);
        return {
          id: routeId, name: routeName(routeId), platform: peer?.platform ?? "windows",
          inputName: input == null ? null : inputNames[input], isLocal: routeId === "local",
          available: input != null, isActive: activeRoute[display.key] === routeId,
          icon: hostAppearances[routeId]?.icon ?? null, color: hostAppearances[routeId]?.color ?? null,
        };
      }),
    })),
  };
}

const delay = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));
type Args = Record<string, unknown> & { onEvent?: { onmessage?: (message: unknown) => void } };

async function handle(command: string, args: Args): Promise<unknown> {
  switch (command) {
    case "get_dashboard_state": return dashboardState();
    case "get_settings": return appSettings();
    case "get_input_options": return inputOptions(String(args.monitorId));
    case "get_host_order": return hostOrder;
    case "set_host_order": hostOrder = (args.routeIds as string[]) ?? hostOrder; return hostOrder;
    case "get_host_names": return hostNames;
    case "set_host_name": {
      const name = String(args.name ?? "");
      if (name) hostNames[String(args.routeId)] = name; else delete hostNames[String(args.routeId)];
      return hostNames;
    }
    case "set_input_label": {
      const key = String(args.monitorId);
      inputLabels[key] = { ...inputLabels[key], [Number(args.input)]: String(args.label ?? "") };
      return inputOptions(key);
    }
    case "get_host_appearances": return hostAppearances;
    case "set_host_appearance": {
      const routeId = String(args.routeId);
      const icon = String(args.icon ?? ""), color = String(args.color ?? "");
      if (icon || color) hostAppearances[routeId] = { icon: icon || null, color: color || null };
      else delete hostAppearances[routeId];
      return hostAppearances;
    }
    case "discover_peers": return scenario === "2x2" ? [{ id: "peer-studio", name: "Studio PC", platform: "windows", address: "192.168.1.60", port: 47821, macAddress: null }] : [];
    case "get_host_switcher_state": return switcherState();
    case "switch_host": {
      const target = String(args.targetId);
      const post = (message: unknown) => args.onEvent?.onmessage?.(message);
      if (target !== "local") { post({ event: "checking", peerName: routeName(target) }); await delay(500); }
      post({ event: "switching" });
      await delay(600);
      activeRoute[String(args.monitorId)] = target;
      return { title: "已切換", detail: `已切換至 ${routeName(target)}`, peerWoken: false, warning: false };
    }
    case "check_for_update": return { available: false, currentVersion: "0.6.0", version: null, notes: null };
    case "check_host_switcher_shortcut": return { available: true, message: "快捷鍵可使用" };
    case "diagnostics_status": return { uploadAvailable: false };
    case "complete_onboarding": case "add_shared_monitor": case "remove_shared_monitor": case "set_monitor_identity_link":
    case "set_local_input": case "select_peer": case "remove_peer": case "reset_settings": case "set_diagnostics_consent":
      return appSettings();
    case "probe_peer": case "wake_peer": case "save_settings":
      return { title: "完成", detail: "（示範資料）", peerWoken: false, warning: false };
    case "plugin:app|version": return "0.6.0";
    case "plugin:event|listen": return Math.floor(Math.random() * 1e6);
    case "hide_host_switcher": return null;
    default:
      // Commands that only confirm, and plugin calls such as set_theme.
      return null;
  }
}

let nextCallbackId = 1;
const callbacks = new Map<number, (payload: unknown) => void>();

(window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
  metadata: { currentWindow: { label: "main" }, currentWebview: { windowLabel: "main", label: "main" } },
  invoke: (command: string, args: Args = {}) => handle(command, args),
  transformCallback: (callback: (payload: unknown) => void) => {
    const id = nextCallbackId++;
    callbacks.set(id, callback);
    return id;
  },
  unregisterCallback: (id: number) => callbacks.delete(id),
  convertFileSrc: (path: string) => path,
};
(window as unknown as Record<string, unknown>).__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => undefined };
