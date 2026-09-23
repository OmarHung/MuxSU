mod diagnostics;
mod diagnostics_upload;
mod host_alias;
mod host_appearance;
mod host_order;
mod host_presence;
mod input_label;
mod known_identity_groups;
mod monitor_identity;
mod tray;

use std::{
    collections::HashMap,
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex as StdMutex, OnceLock, RwLock,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use host_presence::{Check as PresenceCheck, HostPresence};
use muxsu_core::{
    derive_pairing_key, AgentAction, AgentClient, AgentDisplayRoute, AgentHostInput, AgentResponse,
    AgentServer, DestinationHost, DiscoveredPeer, DisplayInput, DisplayMuxError, DisplayMuxProfile,
    DisplayMuxService, HostAlias, HostAppearance, InputLabel, LocalHostIdentity, MacAddress,
    MdnsPeerDiscovery, MonitorControl, MonitorDescriptor, MonitorFingerprint, MonitorId,
    MonitorIdentityLink, PeerDiscovery, PeerEndpoint, ResolutionSource, SwitchMode, SwitchOutcome,
    WakeTarget, AGENT_PROTOCOL_VERSION, DEFAULT_AGENT_PORT,
};
use serde::{Deserialize, Serialize};
use tauri::{ipc::Channel, AppHandle, Emitter, Manager, State};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt as AutostartManagerExt};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tauri_plugin_updater::UpdaterExt;
use tokio::{
    sync::Mutex,
    time::{sleep, timeout, Instant},
};

static NONCE_COUNTER: AtomicU64 = AtomicU64::new(1);
static UI_LOCALE: AtomicU64 = AtomicU64::new(0);
static PAIRING_KEY_CACHE: OnceLock<StdMutex<Option<(String, Arc<[u8]>)>>> = OnceLock::new();
const MIN_SHARED_KEY_LENGTH: usize = 15;
const DEFAULT_HOST_SWITCHER_SHORTCUT: &str = "CommandOrControl+Shift+Space";
/// Most port assignments one notice or response may carry: the largest host
/// order times a generous number of shared displays.
const MAX_SHARED_HOST_INPUTS: usize = 256;
/// How far ahead of this clock a paired host may date shared state. Agents
/// already refuse requests more than 30 seconds off; the rest is drift.
const MAX_REVISION_AHEAD_MS: u64 = 60_000;

/// Whether a revision from a paired host could have been written by now.
/// Shared state keeps the newest revision, so one dated far ahead would win
/// every later edit until this clock caught up.
fn is_plausible_revision(updated_at_ms: u64, now_ms: u64) -> bool {
    updated_at_ms <= now_ms.saturating_add(MAX_REVISION_AHEAD_MS)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiLocale {
    English,
    TraditionalChinese,
}

impl UiLocale {
    fn current() -> Self {
        if UI_LOCALE.load(Ordering::Relaxed) == 1 {
            Self::TraditionalChinese
        } else {
            Self::English
        }
    }
}

fn locale_from_tag(locale: &str) -> UiLocale {
    let normalized = locale.to_ascii_lowercase();
    if normalized.starts_with("zh-tw")
        || normalized.starts_with("zh-hant")
        || normalized.starts_with("zh-hk")
        || normalized.starts_with("zh-mo")
    {
        UiLocale::TraditionalChinese
    } else {
        UiLocale::English
    }
}

fn ui_text(zh_tw: &'static str, en: &'static str) -> &'static str {
    match UiLocale::current() {
        UiLocale::TraditionalChinese => zh_tw,
        UiLocale::English => en,
    }
}

fn input_label(vendor_indexed: bool, input: DisplayInput) -> String {
    if !vendor_indexed {
        return localized_input_name(input);
    }
    match UiLocale::current() {
        UiLocale::TraditionalChinese => format!("輸入 {}", input.value()),
        UiLocale::English => format!("Input {}", input.value()),
    }
}

/// An input's name with the user's note in front, e.g. "USB-C（輸入 8）".
fn noted_input_label(
    settings: &AppSettings,
    selected: &SelectedMonitor,
    input: DisplayInput,
) -> String {
    let base = input_label(selected.vendor_indexed_inputs, input);
    match input_label::label_for(&settings.input_labels, &selected.fingerprint, input) {
        Some(note) => match UiLocale::current() {
            UiLocale::TraditionalChinese => format!("{note}（{base}）"),
            UiLocale::English => format!("{note} ({base})"),
        },
        None => base,
    }
}

fn localized_input_name(input: DisplayInput) -> String {
    let standard_name = match (UiLocale::current(), input.value()) {
        (_, 0x01) => Some("VGA"),
        (_, 0x03) => Some("DVI"),
        (_, 0x0f) => Some("DP"),
        (_, 0x10) => Some("DP 2"),
        (_, 0x1b) => Some("Type-C"),
        (UiLocale::TraditionalChinese, 0x05) => Some("複合視訊 1"),
        (UiLocale::TraditionalChinese, 0x06) => Some("複合視訊 2"),
        (UiLocale::TraditionalChinese, 0x09) => Some("電視調諧器 1"),
        (UiLocale::TraditionalChinese, 0x0a) => Some("電視調諧器 2"),
        (UiLocale::TraditionalChinese, 0x0b) => Some("電視調諧器 3"),
        (UiLocale::TraditionalChinese, 0x0c) => Some("色差視訊 1"),
        (UiLocale::TraditionalChinese, 0x0d) => Some("色差視訊 2"),
        (UiLocale::TraditionalChinese, 0x0e) => Some("色差視訊 3"),
        _ => input.standard_name(),
    };
    match standard_name {
        Some(name) => name.to_owned(),
        None => match UiLocale::current() {
            UiLocale::TraditionalChinese => "其他輸入".to_owned(),
            UiLocale::English => "Other input".to_owned(),
        },
    }
}

#[tauri::command]
fn set_locale(locale: String, app: AppHandle) -> Result<(), String> {
    let selected = locale_from_tag(&locale);
    UI_LOCALE.store(
        u64::from(selected == UiLocale::TraditionalChinese),
        Ordering::Relaxed,
    );
    // The tray menu is written in the interface language.
    tray::refresh_menu(&app);
    Ok(())
}

/// Rebuilds the tray menu after the main window changed something it lists:
/// a shared display, a paired host, or a reset.
#[tauri::command]
fn refresh_tray(app: AppHandle) {
    tray::refresh_menu(&app);
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SelectedMonitor {
    name: String,
    fingerprint: MonitorFingerprint,
    #[serde(default)]
    max_resolution: Option<muxsu_core::MonitorResolution>,
    #[serde(default)]
    resolution_source: Option<ResolutionSource>,
    // Formerly top-level fields on `AppSettings`; each selected monitor now
    // carries its own live input state so N monitors can be tracked at once.
    #[serde(default)]
    local_input: Option<DisplayInput>,
    #[serde(default)]
    supported_inputs: Option<Vec<DisplayInput>>,
    // True when `supported_inputs` is the display's private 1..=max index
    // list rather than MCCS codes, because the advertised capabilities did
    // not even contain the input it was actually showing. Values are then
    // labelled "Input N" instead of by MCCS name.
    #[serde(default)]
    vendor_indexed_inputs: bool,
    // "local" or a peer id: whichever route was last confirmed by a switch,
    // a paired-host notice, or an unambiguous live DDC read. An unreadable or
    // ambiguous read keeps this value, since some displays cannot be read back
    // once switched away (see the macOS limits in product-facts.md).
    #[serde(default)]
    active_route: Option<String>,
    /// When a switch or a paired host's notice last confirmed `active_route`
    /// (Unix milliseconds). Live reads cannot override it until
    /// `ACTIVE_ROUTE_SETTLE_MS` later. In memory only.
    #[serde(default, skip_serializing)]
    active_route_confirmed_at_ms: u64,
}

impl From<&MonitorDescriptor> for SelectedMonitor {
    fn from(monitor: &MonitorDescriptor) -> Self {
        Self {
            name: monitor.name.clone(),
            fingerprint: monitor.fingerprint.clone(),
            max_resolution: monitor.max_resolution,
            resolution_source: monitor.resolution_source,
            local_input: None,
            supported_inputs: None,
            vendor_indexed_inputs: false,
            active_route: None,
            active_route_confirmed_at_ms: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MonitorInputAssignment {
    monitor: MonitorFingerprint,
    input: DisplayInput,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HostRoute {
    id: String,
    name: String,
    platform: DestinationHost,
    address: String,
    port: u16,
    mac_address: String,
    #[serde(default)]
    inputs: Vec<MonitorInputAssignment>,
}

/// One notice waiting for a particular peer to return a valid signed ACK.
/// Kept in `settings.json`, so quitting or restarting cannot lose a change
/// made while that peer was offline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingPeerNotice {
    id: String,
    peer_id: String,
    action: AgentAction,
    attempts: u32,
    next_attempt_at_ms: u64,
}

impl HostRoute {
    fn input_for(&self, fingerprint: &MonitorFingerprint) -> Option<DisplayInput> {
        self.inputs
            .iter()
            .find(|assignment| assignment.monitor.matches_exactly(fingerprint))
            .map(|assignment| assignment.input)
    }

    fn set_input_for(&mut self, fingerprint: &MonitorFingerprint, input: Option<DisplayInput>) {
        self.inputs
            .retain(|assignment| !assignment.monitor.matches_exactly(fingerprint));
        if let Some(input) = input {
            self.inputs.push(MonitorInputAssignment {
                monitor: fingerprint.clone(),
                input,
            });
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct AppSettings {
    local_host: DestinationHost,
    shared_monitors: Vec<SelectedMonitor>,
    peers: Vec<HostRoute>,
    broadcast_ip: String,
    wake_port: u16,
    shared_key: String,
    wait_seconds: u64,
    autostart: bool,
    check_updates: bool,
    onboarding_completed: bool,
    host_switcher_enabled: bool,
    host_switcher_shortcut: String,
    /// Host card order shared with paired hosts, as discovery ids (see
    /// `host_order`). Backend-owned: changed only through `set_host_order` or a
    /// paired host's notice, never by `save_settings`.
    host_order: Vec<String>,
    /// When `host_order` last changed (Unix milliseconds), so the newest order
    /// wins when several hosts reorder.
    host_order_updated_at_ms: u64,
    /// Custom host names shared with paired hosts (see `host_alias`).
    /// Backend-owned like `host_order`.
    host_aliases: Vec<HostAlias>,
    /// Custom host icons and colours shared with paired hosts (see
    /// `host_appearance`). Backend-owned like `host_order`.
    host_appearances: Vec<HostAppearance>,
    /// Notes for shared display inputs, shared with paired hosts (see
    /// `input_label`). Backend-owned like `host_order`.
    input_labels: Vec<InputLabel>,
    /// Which EDID identities the user declared to be the same physical display,
    /// shared with paired hosts (see `monitor_identity`). Backend-owned like
    /// `host_order`.
    monitor_identity_links: Vec<MonitorIdentityLink>,
    /// Last-write-wins display-port assignments, including clear tombstones.
    /// Backend-owned and exchanged in Ping responses for offline catch-up.
    host_inputs: Vec<AgentHostInput>,
    /// Durable per-peer delivery queue. A notice remains here until the peer
    /// returns a valid signed, ready response for that exact request.
    pending_peer_notices: Vec<PendingPeerNotice>,
    /// Whether the shared display list has ever been decided — by the user or
    /// by the one-time auto-select. An empty list means "none chosen" only
    /// until then; afterwards it means the user emptied it on purpose, and
    /// auto-select must not undo that.
    shared_monitors_chosen: bool,
    /// This computer's `LocalHostIdentity::id`, fixed the first time it is
    /// worked out. Peers store it to name this host in their pairings, host
    /// order and custom names, so it must never be re-derived: see
    /// `identifies_the_machine`. Backend-owned like `host_order`.
    local_host_id: String,
    /// Whether the user allowed diagnostic reports to be sent without asking
    /// each time — after a failed switch — and paired hosts to be answered
    /// with this computer's snapshot. Changed only through
    /// `set_diagnostics_consent`.
    diagnostics_enabled: bool,
    /// Whether the user has been asked about diagnostics, so the question is
    /// put once. Backend-owned like `diagnostics_enabled`.
    diagnostics_asked: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            local_host: local_host(),
            shared_monitors: Vec::new(),
            peers: Vec::new(),
            broadcast_ip: "255.255.255.255".to_owned(),
            wake_port: 9,
            shared_key: String::new(),
            wait_seconds: 45,
            autostart: true,
            check_updates: true,
            onboarding_completed: false,
            host_switcher_enabled: false,
            host_switcher_shortcut: DEFAULT_HOST_SWITCHER_SHORTCUT.to_owned(),
            host_order: Vec::new(),
            host_order_updated_at_ms: 0,
            host_aliases: Vec::new(),
            host_appearances: Vec::new(),
            input_labels: Vec::new(),
            monitor_identity_links: Vec::new(),
            host_inputs: Vec::new(),
            pending_peer_notices: Vec::new(),
            shared_monitors_chosen: false,
            local_host_id: String::new(),
            diagnostics_enabled: false,
            diagnostics_asked: false,
        }
    }
}

/// A stable, opaque, frontend-facing id for a monitor identity. Not
/// persisted — recomputed from the fingerprint on every call. Kept local to
/// this crate because `MonitorFingerprint::stable_key()` in `muxsu-core`
/// is `pub(crate)` there and not visible here.
fn monitor_key(fingerprint: &MonitorFingerprint) -> String {
    format!(
        "{}:{}:{}",
        fingerprint.manufacturer_id,
        fingerprint.product_code,
        fingerprint.serial_number.as_deref().unwrap_or("")
    )
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HostSwitcherOption {
    id: String,
    name: String,
    platform: DestinationHost,
    input_name: Option<String>,
    is_local: bool,
    available: bool,
    /// The host this display is already showing. The dashboard has always
    /// refused to switch to it; this window had no way to know which one it
    /// was.
    is_active: bool,
    /// The host's custom icon and colour (see `host_appearance`); `None`
    /// leaves the switcher to draw its default.
    icon: Option<String>,
    color: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HostSwitcherMonitor {
    monitor_key: String,
    name: String,
    hosts: Vec<HostSwitcherOption>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HostSwitcherState {
    monitors: Vec<HostSwitcherMonitor>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ShortcutCheckResult {
    available: bool,
    message: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct LegacySettings {
    local_host: DestinationHost,
    peer_id: String,
    peer_name: String,
    peer_ip: String,
    peer_port: u16,
    peer_mac: String,
    broadcast_ip: String,
    wake_port: u16,
    shared_key: String,
    wait_seconds: u64,
    autostart: bool,
    check_updates: bool,
}

impl Default for LegacySettings {
    fn default() -> Self {
        Self {
            local_host: local_host(),
            peer_id: String::new(),
            peer_name: String::new(),
            peer_ip: String::new(),
            peer_port: DEFAULT_AGENT_PORT,
            peer_mac: String::new(),
            broadcast_ip: "255.255.255.255".to_owned(),
            wake_port: 9,
            shared_key: String::new(),
            wait_seconds: 45,
            autostart: true,
            check_updates: true,
        }
    }
}

struct AppRuntime {
    settings: Arc<RwLock<AppSettings>>,
    settings_path: PathBuf,
    agent_task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    /// Filled in once mDNS has started, which is after this runtime is
    /// managed: starting it first delayed `manage` long enough for the first
    /// command from the webview to find no state and abort the process.
    discovery: OnceLock<MdnsPeerDiscovery>,
    /// This computer's discovery id, used to name it in the shared host order.
    local_host_id: String,
    /// The machine name paired hosts discover this computer by. Used as the
    /// default for its host card, so this computer reads the same on both
    /// sides until the user renames it.
    local_host_name: String,
    /// This computer's wake-on-LAN address, sent to paired hosts in signed
    /// `Ping` replies.
    local_mac_address: Option<String>,
    /// The input last announced to paired hosts per shared display, keyed by
    /// `monitor_key`, so a repeating scan announces a value only once.
    ///
    /// Emptied whenever a notice fails to reach a paired host: what was
    /// announced is only what arrived, and a host that was busy restarting had
    /// otherwise missed the one announcement that would ever be made.
    announced_inputs: Arc<std::sync::Mutex<HashMap<String, DisplayInput>>>,
    /// Which displays this host has already told paired hosts the inputs of,
    /// on the same terms.
    announced_input_lists: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Prevents the immediate sender and periodic retry loop from delivering
    /// the same durable queue entry concurrently.
    notices_in_flight: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Which displays this computer could last see, so a paired host's `Ping`
    /// can be answered without scanning displays inside the reply.
    attached_monitors: Arc<std::sync::Mutex<AttachedSnapshot>>,
    /// Whether a scan started to refresh `attached_monitors` is still running,
    /// so a host polling every second asks for one scan rather than one each
    /// time.
    attached_scan_running: Arc<AtomicBool>,
    /// What the last check said about each paired host, keyed by peer id.
    host_presence: Arc<std::sync::Mutex<HashMap<String, HostPresence>>>,
    /// Whether a round of presence checks is already in flight, so two windows
    /// asking at once cost one round of pings rather than two.
    presence_check_running: Arc<AtomicBool>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SharedMonitorStatus {
    monitor_key: String,
    fingerprint: MonitorFingerprint,
    name: String,
    ddc_available: bool,
    display_state: SharedDisplayState,
    status_text: String,
    connection: Option<muxsu_core::MonitorConnection>,
    connection_input_conflict: bool,
}

/// A live DDC read that moved a display to another configured route. The
/// fingerprint and input are enough for every paired host to resolve the same
/// route using its own settings.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ActiveInputUpdate {
    monitor: MonitorFingerprint,
    input: DisplayInput,
}

/// Whether this computer can use a shared display right now, and if not, why.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum SharedDisplayState {
    /// This computer reads the display's input over DDC/CI.
    Ready,
    /// The display is showing a paired host. Some displays (the MSI MPG 274U
    /// over USB-C, for one) stop answering DDC/CI on inputs they are not
    /// showing, so this is expected rather than a fault; switching back falls
    /// back to asking the paired host.
    OnOtherHost,
    /// Missing, or unreadable with no known reason.
    Unavailable,
}

/// Fills in the port of any shared display that has none, from the reading the
/// scan already took. Returns whether anything changed.
///
/// A port was only ever read when a display was selected or its metadata
/// changed, so a display added while it was showing another host — when its
/// reading says nothing about this host — stayed blank however many times the
/// user refreshed afterwards, including after switching the display here. The
/// scan reads every controllable display's input anyway, so this costs no DDC
/// traffic; it only stops throwing the answer away.
///
/// Only blank ports are filled. A port already set was either read when it
/// could be believed or chosen by the user, and neither should be overwritten
/// by a reading taken while another host is on screen.
fn fill_unset_local_inputs(settings: &mut AppSettings, inventory: &MonitorInventory) -> bool {
    let links = settings.monitor_identity_links.clone();
    let mut filled = false;
    for selected in &mut settings.shared_monitors {
        if selected.local_input.is_some() {
            continue;
        }
        let Some((monitor, input)) = inventory
            .controllable
            .iter()
            .find(|monitor| is_selected_display(&links, selected, monitor))
            .and_then(|monitor| Some((monitor, *inventory.current_inputs.get(&monitor.id)?)))
        else {
            continue;
        };
        if reading_is_this_host_port(monitor, input) {
            selected.local_input = Some(input);
            filled = true;
        }
    }
    filled
}

/// Whether a VCP 0x60 reading can be this host's own port.
///
/// The code reports the input the display is *showing*, not the one the reader
/// occupies, so a host that is off screen reads whoever is on screen. When the
/// reading contradicts the connector this host is plugged into — which the
/// platform reports independently of DDC/CI — it cannot be this host's port.
/// Leaving it unset says "not configured yet"; storing it sends a switch to the
/// wrong place.
fn reading_is_this_host_port(monitor: &MonitorDescriptor, input: DisplayInput) -> bool {
    let contradicts = monitor
        .connection
        .as_ref()
        .and_then(|connection| connection.sink_interface)
        .and_then(|sink| muxsu_core::input_matches_sink(sink, input))
        == Some(false);
    if contradicts {
        tracing::info!(
            monitor_id = monitor.id.as_str(),
            input = input.value(),
            "read input contradicts this host's connector; leaving the port unset"
        );
    }
    !contradicts
}

/// Whether `monitor`, present right now, is the display `selected` names — its
/// own identity or one the user merged into it. Deciding *which* display is
/// meant is separate from deciding what may be written to: every switch still
/// demands an exact fingerprint match (see `run_switch`).
fn is_selected_display(
    links: &[MonitorIdentityLink],
    selected: &SelectedMonitor,
    monitor: &MonitorDescriptor,
) -> bool {
    monitor_identity::is_same_display(links, &selected.fingerprint, &monitor.fingerprint)
}

fn shared_display_state(ddc_readable: bool, active_route: Option<&str>) -> SharedDisplayState {
    if ddc_readable {
        SharedDisplayState::Ready
    } else if active_route.is_some_and(|route| route != host_order::LOCAL_ROUTE_ID) {
        SharedDisplayState::OnOtherHost
    } else {
        SharedDisplayState::Unavailable
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DashboardState {
    platform: &'static str,
    local_host: DestinationHost,
    agent_configured: bool,
    monitors: Vec<MonitorDescriptor>,
    uncontrollable_monitors: Vec<MonitorDescriptor>,
    shared: Vec<SharedMonitorStatus>,
    selection_notices: Vec<String>,
    monitor_identity_claims: Vec<MonitorIdentityClaim>,
    /// For a display present right now, the one shared display a curated table
    /// of multi-identity models says it is another mode of. Only ever used to
    /// preselect the merge the settings page already offers; the user still
    /// declares it. See `known_identity_groups`.
    merge_suggestions: Vec<MergeSuggestion>,
    /// Maps the JSON representation of every fingerprint sent to the webview
    /// to the backend-resolved physical-display identity. The frontend only
    /// compares these opaque results; the serial-number and merge rules live
    /// exclusively in `monitor_identity`.
    resolved_monitor_identities: HashMap<String, String>,
    /// The name paired hosts discover this computer by, so its own card can
    /// default to it rather than to a generic "this Mac".
    local_host_name: String,
}

/// One "these two identities are the same display" claim, as the settings page
/// shows it. The keys go back to `set_monitor_identity_link`, so a claim can be
/// withdrawn even when neither identity is present or shared any more.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MonitorIdentityClaim {
    alias_key: String,
    alias_label: String,
    primary_key: String,
    primary_label: String,
}

/// `MANUFACTURER / PRODUCT`, enough to tell two identities of one display apart
/// where the name is identical because it is the same panel.
fn identity_label(fingerprint: &MonitorFingerprint) -> String {
    format!(
        "{} / {}",
        fingerprint.manufacturer_id, fingerprint.product_code
    )
}

/// "This display present right now is probably another mode of that shared
/// display." Carries the keys the settings page already uses to offer a merge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct MergeSuggestion {
    monitor_id: String,
    primary_key: String,
    primary_label: String,
}

/// The suggestion for each present display that a curated group ties to exactly
/// one shared display. Displays that are already part of a shared display are
/// skipped: they have nothing to merge into.
fn merge_suggestions(
    settings: &AppSettings,
    present: &[&MonitorDescriptor],
    shared: &[SharedMonitorStatus],
) -> Vec<MergeSuggestion> {
    let fingerprints = shared
        .iter()
        .map(|status| status.fingerprint.clone())
        .collect::<Vec<_>>();
    present
        .iter()
        .filter(|monitor| {
            !settings.shared_monitors.iter().any(|selected| {
                monitor_identity::is_same_display(
                    &settings.monitor_identity_links,
                    &selected.fingerprint,
                    &monitor.fingerprint,
                )
            })
        })
        .filter_map(|monitor| {
            let index =
                known_identity_groups::suggested_primary(&monitor.fingerprint, &fingerprints)?;
            Some(MergeSuggestion {
                monitor_id: monitor.id.as_str().to_owned(),
                primary_key: shared[index].monitor_key.clone(),
                primary_label: identity_label(&shared[index].fingerprint),
            })
        })
        .collect()
}

fn monitor_identity_claims(settings: &AppSettings) -> Vec<MonitorIdentityClaim> {
    settings
        .monitor_identity_links
        .iter()
        .filter_map(|link| {
            let primary = link.primary.as_ref()?;
            Some(MonitorIdentityClaim {
                alias_key: monitor_key(&link.alias),
                alias_label: identity_label(&link.alias),
                primary_key: monitor_key(primary),
                primary_label: identity_label(primary),
            })
        })
        .collect()
}

fn resolved_monitor_identities(
    settings: &AppSettings,
    monitors: &[MonitorDescriptor],
    uncontrollable_monitors: &[MonitorDescriptor],
) -> HashMap<String, String> {
    let mut fingerprints = Vec::<&MonitorFingerprint>::new();
    fingerprints.extend(
        settings
            .shared_monitors
            .iter()
            .map(|selected| &selected.fingerprint),
    );
    fingerprints.extend(
        settings
            .peers
            .iter()
            .flat_map(|peer| peer.inputs.iter().map(|assignment| &assignment.monitor)),
    );
    fingerprints.extend(monitors.iter().map(|monitor| &monitor.fingerprint));
    fingerprints.extend(
        uncontrollable_monitors
            .iter()
            .map(|monitor| &monitor.fingerprint),
    );
    for link in &settings.monitor_identity_links {
        fingerprints.push(&link.alias);
        if let Some(primary) = &link.primary {
            fingerprints.push(primary);
        }
    }

    // Resolved identities that name one display must share one key. The same
    // display resolves with or without a serial number depending on which
    // host made the claim, and the screen compares keys as plain strings, so
    // each resolved identity takes the key of the first one it is the same
    // display as. Shared displays come first and so give the key.
    let links = &settings.monitor_identity_links;
    let mut anchors = Vec::<&MonitorFingerprint>::new();
    fingerprints
        .into_iter()
        .filter_map(|fingerprint| {
            let serialized = serde_json::to_string(fingerprint).ok()?;
            let resolved = monitor_identity::primary_for(links, fingerprint);
            let anchor = match anchors
                .iter()
                .find(|anchor| monitor_identity::same_identity(anchor, resolved))
            {
                Some(anchor) => *anchor,
                None => {
                    anchors.push(resolved);
                    resolved
                }
            };
            Some((serialized, monitor_key(anchor)))
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MonitorSelectionChange {
    SelectedOnlyMonitor { name: String },
    RefreshedMetadata { name: String },
}

/// How long this computer's own view of which displays are attached stays
/// good enough to answer a paired host with. A cable is moved by hand, so a
/// reading this recent still describes it; an older one is reported as unknown
/// rather than passed off as current, since "no display" would otherwise read
/// as a missing cable.
const ATTACHED_SNAPSHOT_TTL_MS: u64 = 90_000;

/// The displays this computer could see when its last scan ran.
#[derive(Clone, Debug, Default)]
struct AttachedSnapshot {
    fingerprints: Vec<MonitorFingerprint>,
    /// Unix milliseconds of that scan; zero before the first one.
    taken_at_ms: u64,
}

struct MonitorInventory {
    detected: Vec<MonitorDescriptor>,
    controllable: Vec<MonitorDescriptor>,
    /// The input each controllable display reported while it was probed.
    current_inputs: HashMap<muxsu_core::MonitorId, DisplayInput>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InputOption {
    value: u32,
    /// The name to show, including the user's note when there is one.
    name: String,
    /// The name the display's input data gives, without the note.
    base_name: String,
    /// The user's note, or empty.
    label: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OperationResult {
    title: String,
    detail: String,
    peer_woken: bool,
    warning: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum SwitchProgress {
    Waking { peer_name: String },
    Checking { peer_name: String },
    Waiting { peer_name: String, seconds: u64 },
    Switching,
    RemoteFallback { peer_name: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NetworkPreparation {
    NotRequired,
    Ready { wake_sent: bool },
    Unavailable { wake_sent: bool, reason: String },
}

impl NetworkPreparation {
    fn peer_woken(&self) -> bool {
        matches!(
            self,
            Self::Ready { wake_sent: true }
                | Self::Unavailable {
                    wake_sent: true,
                    ..
                }
        )
    }

    fn warning(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateInfo {
    available: bool,
    current_version: String,
    version: Option<String>,
    notes: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum UpdateDownloadEvent {
    Started {
        content_length: Option<u64>,
    },
    Progress {
        downloaded: u64,
        content_length: Option<u64>,
    },
    Finished,
}

#[tauri::command]
async fn discover_peers(state: State<'_, AppRuntime>) -> Result<Vec<DiscoveredPeer>, String> {
    sleep(Duration::from_millis(700)).await;
    let discovery = state.discovery.get().ok_or_else(|| {
        ui_text(
            "無法啟動區域網路搜尋；請確認防火牆允許 MuxSU 使用私人網路",
            "Unable to start local network discovery. Allow MuxSU through the firewall on private networks.",
        )
        .to_owned()
    })?;
    let peers = discovery.peers().map_err(core_user_error)?;
    refresh_paired_endpoints(&state, &peers).await?;
    Ok(peers)
}

#[tauri::command]
async fn select_peer(
    peer_id: String,
    shared_key: Option<String>,
    state: State<'_, AppRuntime>,
) -> Result<AppSettings, String> {
    let discovery = state.discovery.get().ok_or_else(|| {
        ui_text(
            "區域網路搜尋目前不可用",
            "Local network discovery is unavailable",
        )
        .to_owned()
    })?;
    let peer = discovery
        .peers()
        .map_err(core_user_error)?
        .into_iter()
        .find(|peer| peer.id == peer_id)
        .ok_or_else(|| {
            ui_text(
                "這台主機已離線，請重新搜尋後再試一次",
                "This host is offline. Search again and retry.",
            )
            .to_owned()
        })?;
    let mut settings = read_settings(&state)?;
    upsert_discovered_peer(&mut settings, &peer);
    // A host just paired has been told none of it, whatever was said before it
    // existed. Forgetting what has been announced makes the next scan tell it
    // everything this computer knows.
    if let Ok(mut announced) = state.announced_inputs.lock() {
        announced.clear();
    }
    if let Ok(mut announced) = state.announced_input_lists.lock() {
        announced.clear();
    }
    let route = settings
        .peers
        .iter()
        .find(|route| route.id == peer.id)
        .cloned()
        .expect("peer was just inserted");
    let query_key = shared_key
        .filter(|value| has_valid_shared_key(value))
        .unwrap_or_else(|| settings.shared_key.clone());
    if has_valid_shared_key(&query_key) {
        let mut query_settings = settings.clone();
        query_settings.shared_key = query_key;
        if let Ok(response) = request_peer(&query_settings, &route, AgentAction::Ping).await {
            for display_route in agent_display_routes(&response) {
                let _ = apply_verified_peer_route(&mut settings, &route.id, display_route);
            }
            let _ = apply_host_input_updates(&mut settings, &response.host_inputs, unix_time_ms());
            let _ = adopt_peer_mac_address(&mut settings, &route.id, &response);
        }
    }
    ensure_host_input_history(&mut settings);
    let settings = store_settings(&state, settings)?;
    if !settings.host_inputs.is_empty() {
        broadcast_to_peers(
            &state,
            &AgentAction::HostInputsChanged {
                assignments: settings.host_inputs.clone(),
            },
        );
    }
    Ok(settings)
}

#[tauri::command]
fn remove_peer(peer_id: String, state: State<'_, AppRuntime>) -> Result<AppSettings, String> {
    let mut settings = read_settings(&state)?;
    forget_peer(&mut settings, &peer_id);
    store_settings(&state, settings)
}

/// Drops a peer with everything kept on its behalf: queued notices and its
/// port history, which would otherwise ride along in every Ping response.
fn forget_peer(settings: &mut AppSettings, peer_id: &str) {
    settings.peers.retain(|peer| peer.id != peer_id);
    settings
        .pending_peer_notices
        .retain(|notice| notice.peer_id != peer_id);
    settings
        .host_inputs
        .retain(|assignment| assignment.host_id != peer_id);
}

/// Runs blocking display work (enumeration, DDC/CI) off the main thread,
/// serialized with dashboard scans so they never talk to a display at once.
async fn run_display_task<T: Send + 'static>(
    app: AppHandle,
    task: impl FnOnce(&AppRuntime) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        task(&app.state::<AppRuntime>())
    })
    .await
    .map_err(user_error)?
}

fn display_not_found() -> String {
    ui_text(
        "找不到這台螢幕，請重新整理後再選擇",
        "This display was not found. Refresh and select it again.",
    )
    .to_owned()
}

#[tauri::command]
async fn add_shared_monitor(monitor_id: String, app: AppHandle) -> Result<AppSettings, String> {
    run_display_task(app, move |state| add_shared_monitor_now(state, &monitor_id)).await
}

fn add_shared_monitor_now(state: &AppRuntime, monitor_id: &str) -> Result<AppSettings, String> {
    let controller = platform_controller().map_err(core_user_error)?;
    let inventory = monitor_inventory(&controller).map_err(core_user_error)?;
    // Any detected display may be shared, not only one answering DDC/CI right
    // now. A display refuses DDC/CI while it is showing another host or asleep,
    // and that is exactly the display the user is trying to share — refusing it
    // left the display they came to add with no way to add it.
    let monitor = inventory
        .detected
        .iter()
        .find(|monitor| monitor.id.as_str() == monitor_id)
        .ok_or_else(display_not_found)?
        .clone();
    let readable = inventory
        .controllable
        .iter()
        .any(|candidate| candidate.id == monitor.id);
    let mut settings = read_settings(state)?;
    let links = settings.monitor_identity_links.clone();
    let already_selected = settings
        .shared_monitors
        .iter()
        .any(|selected| is_selected_display(&links, selected, &monitor));
    settings.shared_monitors_chosen = true;
    if !already_selected {
        settings
            .shared_monitors
            .push(SelectedMonitor::from(&monitor));
    }
    if let Some(selected) = settings
        .shared_monitors
        .iter_mut()
        .find(|selected| is_selected_display(&links, selected, &monitor))
    {
        // Its inputs are read when it answers again; until then the display is
        // reported as unavailable rather than dropped.
        if readable {
            refresh_selected_input_data(&controller, &monitor, selected)
                .map_err(core_user_error)?;
        }
    }
    store_settings(state, settings)
}

/// Removes a shared display, named either by its own key or by a display
/// present right now. Never enumerates: a display is removed precisely when
/// this computer cannot see it — asleep, showing another host, or reporting an
/// identity this host no longer recognises — and requiring it to be present
/// made those the only ones that could not be removed.
#[tauri::command]
async fn remove_shared_monitor(monitor_id: String, app: AppHandle) -> Result<AppSettings, String> {
    run_display_task(app, move |state| {
        remove_shared_monitor_now(state, &monitor_id)
    })
    .await
}

fn remove_shared_monitor_now(state: &AppRuntime, monitor_id: &str) -> Result<AppSettings, String> {
    let mut settings = read_settings(state)?;
    let target = find_shared_monitor(&settings, monitor_id)?
        .fingerprint
        .clone();
    settings.shared_monitors_chosen = true;
    unshare_monitor(&mut settings, &target);
    store_settings(state, settings)
}

/// Stops sharing `target` and every identity merged with it, forgetting the
/// ports every host had on it. Those would otherwise ride along in every Ping
/// until the next full reset.
fn unshare_monitor(settings: &mut AppSettings, target: &MonitorFingerprint) {
    let links = settings.monitor_identity_links.clone();
    let removed = settings
        .shared_monitors
        .iter()
        .filter(|selected| monitor_identity::is_same_display(&links, &selected.fingerprint, target))
        .map(|selected| selected.fingerprint.clone())
        .collect::<Vec<_>>();
    settings.shared_monitors.retain(|selected| {
        !monitor_identity::is_same_display(&links, &selected.fingerprint, target)
    });
    for peer in &mut settings.peers {
        peer.set_input_for(target, None);
        for fingerprint in &removed {
            peer.set_input_for(fingerprint, None);
        }
    }
    settings.host_inputs.retain(|assignment| {
        !assignment.monitor.matches_exactly(target)
            && !removed
                .iter()
                .any(|fingerprint| assignment.monitor.matches_exactly(fingerprint))
    });
}

#[tauri::command]
fn get_settings(state: State<'_, AppRuntime>) -> Result<AppSettings, String> {
    read_settings(&state)
}

#[tauri::command]
async fn get_host_switcher_state(app: AppHandle) -> Result<HostSwitcherState, String> {
    let event_app = app.clone();
    run_display_task(app, move |state| {
        let mut settings = read_settings(state)?;
        if let Ok(inventory) = enumerate_monitor_inventory() {
            remember_attached_monitors(state, &inventory);
            let ports_filled = fill_unset_local_inputs(&mut settings, &inventory);
            if ports_filled {
                ensure_host_input_history(&mut settings);
            }
            let updates =
                active_input_updates_from_live_inputs(&mut settings, &inventory, unix_time_ms());
            if ports_filled || !updates.is_empty() {
                store_settings(state, settings.clone())?;
            }
            publish_active_input_updates(state, &event_app, &updates);
            announce_confirmed_local_inputs(state, &settings);
        }
        Ok(build_host_switcher_state(state, &settings))
    })
    .await
}

fn build_host_switcher_state(state: &AppRuntime, settings: &AppSettings) -> HostSwitcherState {
    let route_order = ordered_routes(state, settings);
    let monitors = settings
        .shared_monitors
        .iter()
        .map(|selected| {
            let mut hosts = Vec::with_capacity(settings.peers.len() + 1);
            let look = |host_id: &str| {
                let (icon, color) =
                    host_appearance::appearance_for(&settings.host_appearances, host_id);
                (icon.map(str::to_owned), color.map(str::to_owned))
            };
            let (local_icon, local_color) = look(&state.local_host_id);
            hosts.push(HostSwitcherOption {
                id: "local".to_owned(),
                // Falls back to the name paired hosts discover this computer
                // by, so every surface calls it the same thing.
                name: host_alias::alias_for(&settings.host_aliases, &state.local_host_id)
                    .unwrap_or(if state.local_host_name.is_empty() {
                        ui_text("這台電腦", "This computer")
                    } else {
                        state.local_host_name.as_str()
                    })
                    .to_owned(),
                platform: settings.local_host,
                input_name: selected
                    .local_input
                    .map(|input| noted_input_label(settings, selected, input)),
                is_local: true,
                available: selected.local_input.is_some(),
                // Everywhere else an unset route means this computer: it is
                // the state before the display has ever been switched away.
                is_active: shows_this_host(selected),
                icon: local_icon,
                color: local_color,
            });
            hosts.extend(settings.peers.iter().map(|peer| {
                let input = peer.input_for(&selected.fingerprint);
                let (icon, color) = look(&peer.id);
                HostSwitcherOption {
                    id: peer.id.clone(),
                    name: host_alias::alias_for(&settings.host_aliases, &peer.id)
                        .unwrap_or(&peer.name)
                        .to_owned(),
                    platform: peer.platform,
                    input_name: input.map(|input| noted_input_label(settings, selected, input)),
                    is_local: false,
                    available: input.is_some(),
                    is_active: selected.active_route.as_deref() == Some(peer.id.as_str()),
                    icon,
                    color,
                }
            }));
            hosts.sort_by_key(|host| route_order.iter().position(|route| *route == host.id));
            HostSwitcherMonitor {
                monitor_key: monitor_key(&selected.fingerprint),
                name: selected.name.clone(),
                hosts,
            }
        })
        .collect();
    HostSwitcherState { monitors }
}

#[tauri::command]
fn hide_host_switcher(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("host-switcher") {
        window.hide().map_err(user_error)?;
    }
    Ok(())
}

/// What the window has to say about the last switch from the tray menu. The
/// window is shown before it has finished loading, so it reads the message
/// rather than only listening for it.
#[tauri::command]
fn get_switch_notice() -> Option<tray::SwitchNotice> {
    tray::last_switch_notice()
}

#[tauri::command]
fn hide_switch_notice(app: AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(tray::NOTICE_WINDOW) {
        window.hide().map_err(user_error)?;
    }
    Ok(())
}

#[tauri::command]
fn check_host_switcher_shortcut(
    shortcut: String,
    state: State<'_, AppRuntime>,
    app: AppHandle,
) -> Result<ShortcutCheckResult, String> {
    let candidate = validate_host_switcher_shortcut(&shortcut).map_err(core_user_error)?;
    let settings = read_settings(&state)?;
    let current = settings
        .host_switcher_enabled
        .then(|| Shortcut::from_str(&settings.host_switcher_shortcut).ok())
        .flatten();
    if current.is_some_and(|registered| {
        registered.id() == candidate.id() && app.global_shortcut().is_registered(registered)
    }) {
        return Ok(ShortcutCheckResult {
            available: true,
            message: ui_text("快捷鍵可使用", "Shortcut is available").to_owned(),
        });
    }
    if app.global_shortcut().is_registered(candidate) {
        return Ok(ShortcutCheckResult {
            available: false,
            message: ui_text(
                "此快捷鍵已由 MuxSU 的其他功能使用",
                "This shortcut is already used by another MuxSU feature.",
            )
            .to_owned(),
        });
    }
    match app.global_shortcut().register(candidate) {
        Ok(()) => {
            app.global_shortcut()
                .unregister(candidate)
                .map_err(user_error)?;
            Ok(ShortcutCheckResult {
                available: true,
                message: ui_text("快捷鍵可使用", "Shortcut is available").to_owned(),
            })
        }
        Err(_) => Ok(ShortcutCheckResult {
            available: false,
            message: ui_text(
                "快捷鍵發生衝突，可能已被其他程式使用",
                "Shortcut conflict detected. Another application may already be using it.",
            )
            .to_owned(),
        }),
    }
}

#[tauri::command]
fn complete_onboarding(state: State<'_, AppRuntime>) -> Result<AppSettings, String> {
    let mut settings = read_settings(&state)?;
    settings.onboarding_completed = true;
    store_settings(&state, settings)
}

#[tauri::command]
fn get_input_options(
    monitor_id: String,
    state: State<'_, AppRuntime>,
) -> Result<Vec<InputOption>, String> {
    let settings = read_settings(&state)?;
    input_options(&settings, &monitor_id)
}

fn input_options(settings: &AppSettings, monitor_id: &str) -> Result<Vec<InputOption>, String> {
    let selected = find_shared_monitor(settings, monitor_id)?;
    let inputs = selected
        .supported_inputs
        .clone()
        .filter(|inputs| !inputs.is_empty())
        .unwrap_or_else(common_input_sources);
    Ok(inputs
        .into_iter()
        .map(|input| InputOption {
            value: input.value(),
            name: noted_input_label(settings, selected, input),
            base_name: input_label(selected.vendor_indexed_inputs, input),
            label: input_label::label_for(&settings.input_labels, &selected.fingerprint, input)
                .unwrap_or_default()
                .to_owned(),
        })
        .collect())
}

fn find_shared_monitor<'a>(
    settings: &'a AppSettings,
    monitor_id: &str,
) -> Result<&'a SelectedMonitor, String> {
    settings
        .shared_monitors
        .iter()
        .find(|selected| monitor_key(&selected.fingerprint) == monitor_id)
        .ok_or_else(|| {
            ui_text(
                "找不到這台共用螢幕，請重新整理後再試一次",
                "This shared display was not found. Refresh and try again.",
            )
            .to_owned()
        })
}

/// `submitted` with everything the settings form does not own kept as saved.
/// Displays, discovered inputs and paired hosts are backend-owned: each host
/// reports its own port, so the form cannot assign one to a paired host.
fn settings_from_form(submitted: AppSettings, protected: &AppSettings) -> AppSettings {
    AppSettings {
        local_host: protected.local_host,
        peers: protected.peers.clone(),
        shared_monitors: protected.shared_monitors.clone(),
        onboarding_completed: protected.onboarding_completed,
        // A form opened before a paired host reordered must not revert it.
        host_order: protected.host_order.clone(),
        host_order_updated_at_ms: protected.host_order_updated_at_ms,
        host_aliases: protected.host_aliases.clone(),
        host_appearances: protected.host_appearances.clone(),
        input_labels: protected.input_labels.clone(),
        monitor_identity_links: protected.monitor_identity_links.clone(),
        host_inputs: protected.host_inputs.clone(),
        pending_peer_notices: protected.pending_peer_notices.clone(),
        shared_monitors_chosen: protected.shared_monitors_chosen,
        local_host_id: protected.local_host_id.clone(),
        diagnostics_enabled: protected.diagnostics_enabled,
        diagnostics_asked: protected.diagnostics_asked,
        ..settings_for_current_build(submitted)
    }
}

#[tauri::command]
async fn save_settings(
    settings: AppSettings,
    state: State<'_, AppRuntime>,
    app: AppHandle,
) -> Result<OperationResult, String> {
    let protected = read_settings(&state)?;
    let settings = settings_from_form(settings, &protected);
    validate_settings(&settings).map_err(core_user_error)?;
    let enable_autostart = settings.autostart;
    update_host_switcher_shortcut(&app, &protected, &settings)?;
    if let Err(error) = store_settings(&state, settings.clone()) {
        if let Err(rollback_error) = update_host_switcher_shortcut(&app, &settings, &protected) {
            tracing::warn!(error = %rollback_error, "unable to restore the previous host switcher shortcut");
        }
        return Err(error);
    }
    let autostart = app.autolaunch();
    let autostart_enabled = autostart.is_enabled().map_err(user_error)?;
    if enable_autostart != autostart_enabled {
        if enable_autostart {
            autostart.enable().map_err(user_error)?;
        } else {
            autostart.disable().map_err(user_error)?;
        }
    }
    restart_agent(&state, &app).await?;
    Ok(OperationResult {
        title: ui_text("設定已儲存", "Settings saved").to_owned(),
        detail: ui_text(
            "配對與一般設定已更新。",
            "The pairing and general settings were updated.",
        )
        .to_owned(),
        peer_woken: false,
        warning: false,
    })
}

/// How much of the saved setup a reset clears.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
enum ResetScope {
    /// The shared displays and everything keyed to them. Pairing survives, so
    /// this undoes a display setup without costing the user a re-pair on both
    /// computers.
    Displays,
    /// Everything a fresh install would not have, except this computer's host
    /// id: peers name this computer by it, so changing it here would strand
    /// them on the *other* computer, which a local reset has no business doing.
    Everything,
}

/// `settings` with `scope` cleared. Split out so the decision about what each
/// scope keeps is testable without a running app.
fn settings_after_reset(settings: &AppSettings, scope: ResetScope) -> AppSettings {
    match scope {
        ResetScope::Displays => AppSettings {
            shared_monitors: Vec::new(),
            // Emptied on purpose, by someone who is here. Auto-select exists
            // for a computer that has never chosen; treating a reset as "never
            // chosen" refills the list on the next refresh and makes the reset
            // look like it did nothing.
            shared_monitors_chosen: true,
            monitor_identity_links: Vec::new(),
            input_labels: Vec::new(),
            // Assignments name displays that no longer exist here.
            peers: settings
                .peers
                .iter()
                .map(|peer| HostRoute {
                    inputs: Vec::new(),
                    ..peer.clone()
                })
                .collect(),
            ..settings.clone()
        },
        ResetScope::Everything => AppSettings {
            local_host: settings.local_host,
            local_host_id: settings.local_host_id.clone(),
            // As above: a fresh install auto-selects because nobody is there to
            // choose, which is not the case after a reset.
            shared_monitors_chosen: true,
            ..AppSettings::default()
        },
    }
}

/// The port ledger after a display reset. The reset is this host's alone: its
/// own ports become tombstones for paired hosts to adopt, and what it knew of
/// theirs is forgotten here only, to be learned again once a display is shared.
fn host_inputs_after_display_reset(settings: &AppSettings, now_ms: u64) -> Vec<AgentHostInput> {
    let mut source = settings.clone();
    ensure_host_input_history(&mut source);
    source
        .host_inputs
        .into_iter()
        .filter(|assignment| assignment.host_id == settings.local_host_id)
        .map(|assignment| AgentHostInput {
            input: None,
            updated_at_ms: now_ms.max(assignment.updated_at_ms.saturating_add(1)),
            ..assignment
        })
        .collect()
}

/// Clears the saved setup. Destructive and not undoable, so the webview asks
/// before calling it.
#[tauri::command]
async fn reset_settings(
    scope: ResetScope,
    state: State<'_, AppRuntime>,
    app: AppHandle,
) -> Result<AppSettings, String> {
    let previous = read_settings(&state)?;
    let mut reset_source = previous.clone();
    if scope == ResetScope::Displays {
        reset_source.host_inputs = host_inputs_after_display_reset(&reset_source, unix_time_ms());
    }
    let settings = settings_for_current_build(settings_after_reset(&reset_source, scope));
    update_host_switcher_shortcut(&app, &previous, &settings)?;
    let settings = match store_settings(&state, settings.clone()) {
        Ok(settings) => settings,
        Err(error) => {
            if let Err(rollback) = update_host_switcher_shortcut(&app, &settings, &previous) {
                tracing::warn!(error = %rollback, "unable to restore the previous host switcher shortcut");
            }
            return Err(error);
        }
    };
    let autostart = app.autolaunch();
    if let Ok(enabled) = autostart.is_enabled() {
        if settings.autostart != enabled {
            let result = if settings.autostart {
                autostart.enable()
            } else {
                autostart.disable()
            };
            if let Err(error) = result {
                tracing::warn!(error = %error, "unable to apply the autostart setting after a reset");
            }
        }
    }
    // The agent is keyed to the pairing password, which a full reset clears.
    restart_agent(&state, &app).await?;
    for event in [
        HOST_ORDER_CHANGED_EVENT,
        HOST_NAMES_CHANGED_EVENT,
        HOST_APPEARANCES_CHANGED_EVENT,
        INPUT_LABELS_CHANGED_EVENT,
        MONITOR_IDENTITIES_CHANGED_EVENT,
    ] {
        if let Err(error) = app.emit(event, ()) {
            tracing::warn!(error = %error, event, "unable to notify windows of a reset");
        }
    }
    if scope == ResetScope::Displays {
        broadcast_to_peers(
            &state,
            &AgentAction::HostInputsChanged {
                assignments: settings.host_inputs.clone(),
            },
        );
    }
    Ok(settings)
}

#[tauri::command]
async fn check_for_update(app: AppHandle) -> Result<UpdateInfo, String> {
    let current_version = app.package_info().version.to_string();
    let update = app
        .updater()
        .map_err(update_error)?
        .check()
        .await
        .map_err(update_error)?;
    Ok(match update {
        Some(update) => UpdateInfo {
            available: true,
            current_version,
            version: Some(update.version),
            notes: update.body,
        },
        None => UpdateInfo {
            available: false,
            current_version,
            version: None,
            notes: None,
        },
    })
}

#[tauri::command]
async fn install_update(
    app: AppHandle,
    on_event: Channel<UpdateDownloadEvent>,
) -> Result<(), String> {
    let Some(update) = app
        .updater()
        .map_err(update_error)?
        .check()
        .await
        .map_err(update_error)?
    else {
        return Err(ui_text(
            "目前沒有可安裝的更新",
            "No update is currently available to install",
        )
        .to_owned());
    };

    let progress_events = on_event.clone();
    let finished_events = on_event;
    let mut downloaded = 0_u64;
    let mut started = false;
    update
        .download_and_install(
            move |chunk_length, content_length| {
                if !started {
                    let _ = progress_events.send(UpdateDownloadEvent::Started { content_length });
                    started = true;
                }
                downloaded = downloaded.saturating_add(chunk_length as u64);
                let _ = progress_events.send(UpdateDownloadEvent::Progress {
                    downloaded,
                    content_length,
                });
            },
            move || {
                let _ = finished_events.send(UpdateDownloadEvent::Finished);
            },
        )
        .await
        .map_err(update_install_error)?;

    tracing::info!(version = %update.version, "signed application update installed");
    app.restart()
}

/// Serializes dashboard scans. As a synchronous command the scan was implicitly
/// serialized on the main thread; overlapping refreshes would otherwise issue
/// concurrent DDC/CI requests and race on writing reconciled settings.
static DASHBOARD_SCAN: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tauri::command]
async fn get_dashboard_state(app: AppHandle) -> Result<DashboardState, String> {
    // Monitor enumeration and DDC/CI reads block for hundreds of milliseconds up
    // to seconds (capabilities strings, retries). Keep them off the main thread so
    // the window stays responsive while displays are scanned.
    let event_app = app.clone();
    run_display_task(app, move |state| build_dashboard_state(state, &event_app)).await
}

fn build_dashboard_state(state: &AppRuntime, app: &AppHandle) -> Result<DashboardState, String> {
    let mut settings = read_settings(state)?;
    let (monitors, uncontrollable_monitors, shared, selection_notices) =
        match enumerate_monitor_inventory() {
            Ok(inventory) => {
                diagnostics::remember_inventory(&inventory.detected, &inventory.current_inputs);
                remember_attached_monitors(state, &inventory);
                let changes = reconcile_monitor_selection(&mut settings, &inventory.controllable);
                // A display may be shared before it answers DDC/CI, and its
                // inputs cannot be read then. Nothing read them afterwards, so
                // it kept the generic list of standard inputs for good — on one
                // host a display offered "DP, HDMI 1, HDMI 2…" while the other
                // host knew its real ones. Read them the first time it answers.
                let inputs_unread = settings.shared_monitors.iter().any(|selected| {
                    selected.supported_inputs.is_none()
                        && inventory.controllable.iter().any(|monitor| {
                            is_selected_display(&settings.monitor_identity_links, selected, monitor)
                        })
                });
                if !changes.is_empty() || inputs_unread {
                    if let Ok(controller) = platform_controller() {
                        let links = settings.monitor_identity_links.clone();
                        for selected in &mut settings.shared_monitors {
                            if let Some(current) = inventory
                                .controllable
                                .iter()
                                .find(|monitor| is_selected_display(&links, selected, monitor))
                            {
                                if let Err(error) =
                                    refresh_selected_input_data(&controller, current, selected)
                                {
                                    tracing::warn!(
                                        monitor_id = current.id.as_str(),
                                        error = %error,
                                        "unable to record input data for automatically selected display"
                                    );
                                }
                            }
                        }
                    }
                }
                let ports_filled = fill_unset_local_inputs(&mut settings, &inventory);
                if ports_filled {
                    ensure_host_input_history(&mut settings);
                }
                let active_input_updates = active_input_updates_from_live_inputs(
                    &mut settings,
                    &inventory,
                    unix_time_ms(),
                );
                let routes_changed = !active_input_updates.is_empty();
                if !changes.is_empty() || inputs_unread || ports_filled || routes_changed {
                    store_settings(state, settings.clone())?;
                }
                publish_active_input_updates(state, app, &active_input_updates);
                announce_confirmed_local_inputs(state, &settings);
                announce_discovered_inputs(state, &settings);
                let selection_notices = changes
                    .into_iter()
                    .filter_map(selection_notice_text)
                    .collect();
                let shared: Vec<SharedMonitorStatus> = settings
                    .shared_monitors
                    .iter()
                    .map(|selected| {
                        let target_found = inventory.controllable.iter().any(|monitor| {
                            is_selected_display(&settings.monitor_identity_links, selected, monitor)
                        });
                        let detected = inventory.detected.iter().find(|monitor| {
                            is_selected_display(&settings.monitor_identity_links, selected, monitor)
                        });
                        let target_detected = detected.is_some();
                        let connection = detected.and_then(|monitor| monitor.connection.clone());
                        let display_state =
                            shared_display_state(target_found, selected.active_route.as_deref());
                        let showing_host = selected
                            .active_route
                            .as_deref()
                            .and_then(|route| settings.peers.iter().find(|peer| peer.id == route))
                            .map(|peer| {
                                host_alias::alias_for(&settings.host_aliases, &peer.id)
                                    .unwrap_or(&peer.name)
                            });
                        SharedMonitorStatus {
                            monitor_key: monitor_key(&selected.fingerprint),
                            fingerprint: selected.fingerprint.clone(),
                            name: selected.name.clone(),
                            ddc_available: target_found,
                            display_state,
                            status_text: shared_monitor_status_text(
                                &selected.name,
                                display_state,
                                target_detected,
                                showing_host,
                            ),
                            connection_input_conflict: connection_input_conflict(
                                selected,
                                connection.as_ref(),
                            ),
                            connection,
                        }
                    })
                    .collect();
                let uncontrollable = uncontrollable_monitors(&inventory);
                (
                    inventory.controllable,
                    uncontrollable,
                    shared,
                    selection_notices,
                )
            }
            Err(error) => {
                let message = core_user_error(error);
                let shared = settings
                    .shared_monitors
                    .iter()
                    .map(|selected| SharedMonitorStatus {
                        monitor_key: monitor_key(&selected.fingerprint),
                        fingerprint: selected.fingerprint.clone(),
                        name: selected.name.clone(),
                        ddc_available: false,
                        display_state: SharedDisplayState::Unavailable,
                        status_text: message.clone(),
                        connection: None,
                        connection_input_conflict: false,
                    })
                    .collect();
                (Vec::new(), Vec::new(), shared, Vec::new())
            }
        };
    let resolved_monitor_identities =
        resolved_monitor_identities(&settings, &monitors, &uncontrollable_monitors);
    let present = monitors
        .iter()
        .chain(uncontrollable_monitors.iter())
        .collect::<Vec<_>>();
    let merge_suggestions = merge_suggestions(&settings, &present, &shared);
    Ok(DashboardState {
        platform: std::env::consts::OS,
        local_host: settings.local_host,
        agent_configured: has_valid_shared_key(&settings.shared_key),
        monitors,
        uncontrollable_monitors,
        shared,
        selection_notices,
        monitor_identity_claims: monitor_identity_claims(&settings),
        merge_suggestions,
        resolved_monitor_identities,
        local_host_name: state.local_host_name.clone(),
    })
}

fn selection_notice_text(change: MonitorSelectionChange) -> Option<String> {
    match change {
        MonitorSelectionChange::SelectedOnlyMonitor { name } => Some(match UiLocale::current() {
            UiLocale::TraditionalChinese => {
                format!("已自動選取唯一可控制的 DDC/CI 螢幕：{name}")
            }
            UiLocale::English => {
                format!("Automatically selected the only controllable DDC/CI display: {name}")
            }
        }),
        MonitorSelectionChange::RefreshedMetadata { .. } => None,
    }
}

fn shared_monitor_status_text(
    name: &str,
    state: SharedDisplayState,
    target_detected: bool,
    showing_host: Option<&str>,
) -> String {
    if state == SharedDisplayState::OnOtherHost {
        let host = showing_host.unwrap_or(ui_text("另一台主機", "another host"));
        return match UiLocale::current() {
            UiLocale::TraditionalChinese => format!(
                "{name} 目前顯示 {host}；顯示其他主機時，這台螢幕不回應這台電腦的 DDC/CI"
            ),
            UiLocale::English => format!(
                "{name} is showing {host}. While it shows another host, it does not answer this computer's DDC/CI"
            ),
        };
    }
    if state == SharedDisplayState::Ready {
        return match UiLocale::current() {
            UiLocale::TraditionalChinese => format!("已鎖定共用螢幕：{name}"),
            UiLocale::English => format!("Shared display locked: {name}"),
        };
    }
    if target_detected {
        return match UiLocale::current() {
            UiLocale::TraditionalChinese => {
                format!("已偵測到 {name}，但目前無法讀取 DDC/CI 輸入")
            }
            UiLocale::English => {
                format!("{name} was detected, but its DDC/CI input cannot be read")
            }
        };
    }
    match UiLocale::current() {
        UiLocale::TraditionalChinese => format!("找不到先前選擇的共用螢幕：{name}"),
        UiLocale::English => format!("Previously selected shared display was not found: {name}"),
    }
}

/// What the last check said about each paired host, without asking anything
/// now. A window draws with this straight away and refreshes in the
/// background, so opening it never waits on a sleeping host's connect timeout.
#[tauri::command]
fn get_host_presence(state: State<'_, AppRuntime>) -> Result<Vec<HostPresence>, String> {
    let settings = read_settings(&state)?;
    Ok(known_host_presence(&state, &settings))
}

/// Asks every paired host at once whether it answers and which shared displays
/// it can see, then remembers the answers.
///
/// The hosts are asked in parallel: one asleep holds its check for the client's
/// connect timeout, and a row of those in turn would outlast the interval the
/// windows call this on.
#[tauri::command]
async fn refresh_host_presence(state: State<'_, AppRuntime>) -> Result<Vec<HostPresence>, String> {
    let settings = Arc::new(read_settings(&state)?);
    // One round at a time. The main window and the switcher overlay both ask,
    // and a round waiting on a sleeping host easily outlasts the gap between
    // two asks; without this they would each ping every host.
    if settings.peers.is_empty() {
        return Ok(known_host_presence(&state, &settings));
    }
    let Some(_round) = CheckRound::start(&state.presence_check_running) else {
        return Ok(known_host_presence(&state, &settings));
    };
    let checks: Vec<_> = settings
        .peers
        .iter()
        .cloned()
        .map(|peer| {
            let settings = Arc::clone(&settings);
            tauri::async_runtime::spawn(async move {
                let answer = request_peer(&settings, &peer, AgentAction::Ping).await;
                (peer.id, answer)
            })
        })
        .collect();
    let mut answers = Vec::with_capacity(checks.len());
    for check in checks {
        // A check that could not even be joined says nothing about its host,
        // so that host keeps whatever was last known about it.
        if let Ok(answer) = check.await {
            answers.push(answer);
        }
    }
    record_host_presence(&state, &settings, answers, unix_time_ms());
    Ok(known_host_presence(&state, &settings))
}

/// Holds the "a round is in flight" flag for as long as one is, and clears it
/// however the round ends. A window closed mid-round drops the command's
/// future, which would otherwise leave the flag set and every later round
/// reading the cache for the rest of the session.
struct CheckRound<'a>(&'a AtomicBool);

impl<'a> CheckRound<'a> {
    /// `None` when a round is already running.
    fn start(running: &'a AtomicBool) -> Option<Self> {
        (!running.swap(true, Ordering::SeqCst)).then_some(Self(running))
    }
}

impl Drop for CheckRound<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Every paired host, in the order they are stored, including those nothing
/// has asked yet. Hosts that are no longer paired are dropped as this reads.
fn known_host_presence(state: &AppRuntime, settings: &AppSettings) -> Vec<HostPresence> {
    let peer_ids: Vec<String> = settings.peers.iter().map(|peer| peer.id.clone()).collect();
    let Ok(mut known) = state.host_presence.lock() else {
        return peer_ids
            .iter()
            .map(|id| HostPresence::unknown(id))
            .collect();
    };
    host_presence::forget_unpaired(&mut known, &peer_ids);
    host_presence::listed(&known, &peer_ids)
}

/// Remembers how a round of checks ended.
fn record_host_presence(
    state: &AppRuntime,
    settings: &AppSettings,
    answers: Vec<(String, Result<AgentResponse, String>)>,
    now_ms: u64,
) {
    let Ok(mut known) = state.host_presence.lock() else {
        return;
    };
    for (peer_id, answer) in answers {
        let check = match answer {
            Ok(response) => PresenceCheck::Answered {
                attached: peer_attached_monitor_keys(settings, &response),
            },
            Err(detail) => PresenceCheck::Silent { detail },
        };
        let updated = host_presence::recorded(known.get(&peer_id), &peer_id, check, now_ms);
        known.insert(peer_id, updated);
    }
}

/// The shared displays a paired host reported seeing, as this computer's own
/// `monitor_key`s, so the windows can answer "is this display on that host"
/// display by display. A display it sees but nothing shares here has nowhere
/// to be shown and is dropped.
fn peer_attached_monitor_keys(
    settings: &AppSettings,
    response: &AgentResponse,
) -> Option<Vec<String>> {
    let attached = response.attached_monitors.as_ref()?;
    Some(
        attached
            .iter()
            .filter_map(|fingerprint| {
                let index = shared_monitor_index_for_peer(
                    &settings.shared_monitors,
                    &settings.monitor_identity_links,
                    fingerprint,
                )?;
                Some(monitor_key(&settings.shared_monitors[index].fingerprint))
            })
            .collect(),
    )
}

#[tauri::command]
async fn probe_peer(
    peer_id: String,
    state: State<'_, AppRuntime>,
) -> Result<OperationResult, String> {
    let settings = read_settings(&state)?;
    let peer = find_peer(&settings, &peer_id)?;
    let peer_name = peer.name.clone();
    // The button the user pressed is also a presence check: its answer belongs
    // in the host list beside it, whichever way it went.
    let answer = request_peer(&settings, peer, AgentAction::Ping).await;
    record_host_presence(
        &state,
        &settings,
        vec![(peer_id.clone(), answer.clone())],
        unix_time_ms(),
    );
    let response = answer?;
    let adoption = adopt_peer_routes(&state, &peer_id, &response)?;
    Ok(OperationResult {
        title: match UiLocale::current() {
            UiLocale::TraditionalChinese => format!("{peer_name} 已連線"),
            UiLocale::English => format!("{peer_name} connected"),
        },
        detail: adoption.detail,
        peer_woken: false,
        warning: adoption.warning,
    })
}

/// The outcome of adopting a paired host's reported inputs, phrased for a toast.
struct RouteAdoption {
    detail: String,
    warning: bool,
}

/// Applies every input a paired host reported for the displays shared here,
/// and describes what happened. A port that cannot be filled in says why,
/// rather than leaving the user with a field that silently stays empty.
fn adopt_peer_routes(
    state: &AppRuntime,
    peer_id: &str,
    response: &AgentResponse,
) -> Result<RouteAdoption, String> {
    let routes = agent_display_routes(response);
    let mut settings = read_settings(state)?;
    let host_inputs_update =
        apply_host_input_updates(&mut settings, &response.host_inputs, unix_time_ms());
    let host_inputs_accepted = host_inputs_update.is_some();
    let host_inputs_changed = host_inputs_update.unwrap_or(false);
    let mac_address_changed = adopt_peer_mac_address(&mut settings, peer_id, response);
    if routes.is_empty() {
        if host_inputs_accepted || mac_address_changed {
            store_settings(state, settings)?;
        }
        return Ok(RouteAdoption {
            detail: if host_inputs_changed {
                ui_text("已補齊遠端主機的輸入設定。", "The peer's input settings were synchronized.")
            } else {
                ui_text(
                    "Agent 已就緒，但這台主機沒有回報任何輸入值；請確認它也把同一台螢幕設為共用。",
                    "The agent is ready, but this host reported no input. Check that it shares the same display.",
                )
            }
            .to_owned(),
            warning: !host_inputs_changed,
        });
    }
    let mut notes: Vec<String> = Vec::new();
    let mut applied = host_inputs_changed;
    let mut warning = false;
    let chinese = matches!(UiLocale::current(), UiLocale::TraditionalChinese);
    for route in routes {
        let matched = shared_monitor_index_for_peer(
            &settings.shared_monitors,
            &settings.monitor_identity_links,
            &route.monitor,
        )
        .map(|index| settings.shared_monitors[index].clone());
        let label = matched.as_ref().map_or_else(
            || input_label(false, route.input),
            |selected| noted_input_label(&settings, selected, route.input),
        );
        let monitor = matched.map_or_else(String::new, |selected| selected.name);
        let outcome = apply_verified_peer_route(&mut settings, peer_id, route);
        applied |= outcome == PeerRouteOutcome::Applied;
        warning |= !matches!(
            outcome,
            PeerRouteOutcome::Applied | PeerRouteOutcome::Unchanged
        );
        notes.push(match (outcome, chinese) {
            (PeerRouteOutcome::Applied, true) => format!("{monitor} 已帶入 {label}。"),
            (PeerRouteOutcome::Applied, false) => format!("{monitor} set to {label}."),
            (PeerRouteOutcome::Unchanged, true) => format!("{monitor} 已經是 {label}。"),
            (PeerRouteOutcome::Unchanged, false) => format!("{monitor} was already {label}."),
            (PeerRouteOutcome::Kept, true) => format!(
                "{monitor} 回報 {label}，但這台主機目前不在螢幕上，讀到的可能是別台，因此保留原本的設定。"
            ),
            (PeerRouteOutcome::Kept, false) => format!(
                "{monitor} reported {label}, but that host is not on screen so the reading may be another host's; the current setting was kept."
            ),
            (PeerRouteOutcome::UnknownMonitor, true) => {
                "這台主機回報了一台這裡沒有共用的螢幕。".to_owned()
            }
            (PeerRouteOutcome::UnknownMonitor, false) => {
                "This host reported a display that is not shared here.".to_owned()
            }
            (PeerRouteOutcome::Unsupported, true) => {
                format!("{monitor} 沒有 {label} 這個輸入。")
            }
            (PeerRouteOutcome::Unsupported, false) => {
                format!("{monitor} has no {label} input.")
            }
            (PeerRouteOutcome::Taken, true) => format!(
                "{monitor} 的 {label} 已指派給其他主機；請確認兩台主機接在不同的 Port，或先切換到這台主機再試一次。"
            ),
            (PeerRouteOutcome::Taken, false) => format!(
                "{label} on {monitor} is already assigned to another host. Check that the hosts use different ports, or switch to this host and retry."
            ),
            (PeerRouteOutcome::UnknownPeer, true) => {
                "這台主機已不在已加入的清單中。".to_owned()
            }
            (PeerRouteOutcome::UnknownPeer, false) => {
                "This host is no longer in the added list.".to_owned()
            }
        });
    }
    if applied || host_inputs_accepted || mac_address_changed {
        store_settings(state, settings)?;
    }
    Ok(RouteAdoption {
        detail: notes.join(" "),
        warning,
    })
}

#[tauri::command]
async fn wake_peer(
    peer_id: String,
    state: State<'_, AppRuntime>,
) -> Result<OperationResult, String> {
    let settings = read_settings(&state)?;
    let peer = find_peer(&settings, &peer_id)?;
    wake_route(&settings, peer).await?;
    Ok(OperationResult {
        title: match UiLocale::current() {
            UiLocale::TraditionalChinese => format!("已送出喚醒訊號給 {}", peer.name),
            UiLocale::English => format!("Wake signal sent to {}", peer.name),
        },
        detail: ui_text(
            "主機是否能喚醒仍取決於電源與網路設定。",
            "Whether the host wakes still depends on its power and network settings.",
        )
        .to_owned(),
        peer_woken: true,
        warning: false,
    })
}

#[tauri::command]
async fn switch_host(
    monitor_id: String,
    target_id: String,
    on_event: Channel<SwitchProgress>,
    state: State<'_, AppRuntime>,
    app: AppHandle,
) -> Result<OperationResult, String> {
    let result = run_host_switch(monitor_id, target_id, on_event, &state).await;
    match &result {
        // Every window shows which host is active, and only the one that asked
        // for this switch knows it happened. The host switcher is one of them:
        // a switch made from it left the main window on the old host until the
        // next scan or a manual refresh.
        Ok(_) => {
            if let Err(error) = app.emit(ACTIVE_ROUTE_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify windows of a switch");
            }
        }
        Err(message) => report_failure_if_allowed(&app, message),
    }
    result
}

async fn run_host_switch(
    monitor_id: String,
    target_id: String,
    on_event: Channel<SwitchProgress>,
    state: &AppRuntime,
) -> Result<OperationResult, String> {
    let settings = read_settings(state)?;
    let selected = find_shared_monitor(&settings, &monitor_id)?.clone();
    let target = if target_id == "local" {
        None
    } else {
        Some(find_peer(&settings, &target_id)?)
    };
    let input = if let Some(peer) = target {
        peer.input_for(&selected.fingerprint).ok_or_else(|| {
            ui_text(
                "尚未設定這台主機使用的螢幕輸入",
                "The display input for this host is not configured",
            )
            .to_owned()
        })?
    } else {
        selected.local_input.ok_or_else(|| {
            ui_text(
                "尚未設定這台主機使用的螢幕輸入",
                "The display input for this host is not configured",
            )
            .to_owned()
        })?
    };
    let preparation = match target {
        Some(peer) => prepare_automatic_switch(&settings, peer, &on_event).await,
        None => NetworkPreparation::NotRequired,
    };
    let _ = on_event.send(SwitchProgress::Switching);
    let identities =
        monitor_identity::identities_for(&settings.monitor_identity_links, &selected.fingerprint);
    match run_switch(identities, input) {
        Ok(outcome) => {
            record_active_route(state, &selected.fingerprint, &target_id)?;
            announce_active_input(state, &selected.fingerprint, input);
            Ok(outcome_result(outcome, &preparation, |input| {
                noted_input_label(&settings, &selected, input)
            }))
        }
        Err(local_error) => {
            let local_error = core_user_error(local_error);
            let executor = if target_id == "local" {
                (settings.peers.len() == 1).then(|| &settings.peers[0])
            } else {
                settings.peers.iter().find(|peer| peer.id == target_id)
            }
            .ok_or_else(|| match UiLocale::current() {
                UiLocale::TraditionalChinese => {
                    format!("本機無法切換，而且沒有其他已配對主機可代為執行：{local_error}")
                }
                UiLocale::English => format!(
                    "Local switching failed and no other paired host can perform it: {local_error}"
                ),
            })?;
            let _ = on_event.send(SwitchProgress::RemoteFallback {
                peer_name: executor.name.clone(),
            });
            let remote_result: Result<AgentResponse, String> = async {
                let monitor_field =
                    resolve_switch_monitor_field(&settings, executor, &selected.fingerprint)
                        .await?;
                request_peer(
                    &settings,
                    executor,
                    AgentAction::SwitchInput {
                        monitor: monitor_field,
                        input,
                    },
                )
                .await
            }
            .await;
            remote_result.map_err(|remote_error| match UiLocale::current() {
                UiLocale::TraditionalChinese => format!(
                    "本機與 {} 都無法切換。本機：{}；遠端：{}",
                    executor.name, local_error, remote_error
                ),
                UiLocale::English => format!(
                    "Neither this computer nor {} could switch. Local: {}; remote: {}",
                    executor.name, local_error, remote_error
                ),
            })?;
            record_active_route(state, &selected.fingerprint, &target_id)?;
            announce_active_input(state, &selected.fingerprint, input);
            Ok(OperationResult {
                title: match UiLocale::current() {
                    UiLocale::TraditionalChinese => format!("已由 {} 執行切換", executor.name),
                    UiLocale::English => format!("Switch performed by {}", executor.name),
                },
                detail: match UiLocale::current() {
                    UiLocale::TraditionalChinese => format!(
                        "遠端主機已切換至 {}。",
                        noted_input_label(&settings, &selected, input)
                    ),
                    UiLocale::English => format!(
                        "The remote host switched to {}.",
                        noted_input_label(&settings, &selected, input)
                    ),
                },
                peer_woken: preparation.peer_woken(),
                warning: false,
            })
        }
    }
}

/// Confirms the peer speaks the current authenticated protocol before sending
/// a monitor-specific switch. `request_peer` rejects every other version.
async fn resolve_switch_monitor_field(
    settings: &AppSettings,
    peer: &HostRoute,
    target_fingerprint: &MonitorFingerprint,
) -> Result<Option<MonitorFingerprint>, String> {
    request_peer(settings, peer, AgentAction::Ping).await?;
    Ok(Some(target_fingerprint.clone()))
}

fn outcome_result(
    outcome: SwitchOutcome,
    preparation: &NetworkPreparation,
    label: impl Fn(DisplayInput) -> String,
) -> OperationResult {
    let mut result = match outcome {
        SwitchOutcome::DryRun { .. } => OperationResult {
            title: ui_text("檢查完成", "Check complete").to_owned(),
            detail: ui_text("未變更螢幕輸入。", "The display input was not changed.").to_owned(),
            peer_woken: preparation.peer_woken(),
            warning: preparation.warning(),
        },
        SwitchOutcome::AlreadySelected { target, input } => OperationResult {
            title: ui_text("已在指定輸入", "Already on the assigned input").to_owned(),
            detail: match UiLocale::current() {
                UiLocale::TraditionalChinese => {
                    format!("{} 已使用 {}。", target.name, label(input))
                }
                UiLocale::English => {
                    format!("{} is already using {}.", target.name, label(input))
                }
            },
            peer_woken: preparation.peer_woken(),
            warning: preparation.warning(),
        },
        SwitchOutcome::Switched {
            target,
            previous,
            selected,
        } => OperationResult {
            title: ui_text("共用螢幕已切換", "Shared display switched").to_owned(),
            detail: match UiLocale::current() {
                UiLocale::TraditionalChinese => format!(
                    "{} 已由 {} 切換至 {}。",
                    target.name,
                    label(previous),
                    label(selected)
                ),
                UiLocale::English => format!(
                    "{} switched from {} to {}.",
                    target.name,
                    label(previous),
                    label(selected)
                ),
            },
            peer_woken: preparation.peer_woken(),
            warning: preparation.warning(),
        },
    };
    if let NetworkPreparation::Unavailable {
        wake_sent, reason, ..
    } = preparation
    {
        let wake_detail = if *wake_sent {
            ui_text("已先送出喚醒訊號，但", "A wake signal was sent, but ")
        } else {
            ui_text(
                "無法送出喚醒訊號，且",
                "A wake signal could not be sent, and ",
            )
        };
        result.detail.push_str(&match UiLocale::current() {
            UiLocale::TraditionalChinese => format!(" {wake_detail}無法透過區域網路確認目標主機（{reason}）；已自動改用本機 DDC/CI。若目標主機尚未就緒，螢幕可能暫時黑畫面。"),
            UiLocale::English => format!(" {wake_detail}the target host could not be confirmed over the local network ({reason}); local DDC/CI was selected automatically. The display may be temporarily blank if the target host is not ready."),
        });
    }
    result
}

async fn prepare_automatic_switch(
    settings: &AppSettings,
    peer: &HostRoute,
    on_event: &Channel<SwitchProgress>,
) -> NetworkPreparation {
    let _ = on_event.send(SwitchProgress::Waking {
        peer_name: peer.name.clone(),
    });
    let wake_result = wake_route(settings, peer).await;
    let wake_sent = wake_result.is_ok();

    let _ = on_event.send(SwitchProgress::Checking {
        peer_name: peer.name.clone(),
    });
    if !has_valid_shared_key(&settings.shared_key) {
        return NetworkPreparation::Unavailable {
            wake_sent,
            reason: match UiLocale::current() {
                UiLocale::TraditionalChinese => format!("網路 Agent 尚未設定至少 {MIN_SHARED_KEY_LENGTH} 個字元的配對密碼"),
                UiLocale::English => format!("The network Agent does not have a pairing password of at least {MIN_SHARED_KEY_LENGTH} characters"),
            },
        };
    }
    if request_peer(settings, peer, AgentAction::Ping)
        .await
        .is_ok()
    {
        return NetworkPreparation::Ready { wake_sent };
    }

    if let Err(wake_error) = wake_result {
        return NetworkPreparation::Unavailable {
            wake_sent: false,
            reason: match UiLocale::current() {
                UiLocale::TraditionalChinese => format!("{}，且 Agent 目前沒有回應", wake_error),
                UiLocale::English => format!("{wake_error}, and the Agent is not responding"),
            },
        };
    }

    let _ = on_event.send(SwitchProgress::Waiting {
        peer_name: peer.name.clone(),
        seconds: settings.wait_seconds.clamp(5, 120),
    });
    match wait_until_peer_ready(settings, peer).await {
        Ok(()) => NetworkPreparation::Ready { wake_sent: true },
        Err(reason) => NetworkPreparation::Unavailable {
            wake_sent: true,
            reason,
        },
    }
}

/// How often a waking host is polled while the wait lasts.
const PEER_READY_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Waits for a waking host to answer, for at most the user's wait budget.
///
/// The budget is a deadline, not an attempt count: a host that is unreachable
/// rather than merely asleep leaves every poll hanging until its own connect
/// and read timeouts expire, so counting attempts would keep the waiting
/// dialog up for several times the number of seconds it promises.
async fn wait_until_peer_ready(settings: &AppSettings, peer: &HostRoute) -> Result<(), String> {
    let budget = settings.wait_seconds.clamp(5, 120);
    let deadline = Instant::now() + Duration::from_secs(budget);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        sleep(PEER_READY_POLL_INTERVAL.min(remaining)).await;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        if matches!(
            timeout(remaining, request_peer(settings, peer, AgentAction::Ping)).await,
            Ok(Ok(_))
        ) {
            return Ok(());
        }
    }
    Err(match UiLocale::current() {
        UiLocale::TraditionalChinese => {
            format!("{} 在送出喚醒訊號後 {} 秒內仍沒有回應", peer.name, budget)
        }
        UiLocale::English => format!(
            "{} did not respond within {} seconds after the wake signal",
            peer.name, budget
        ),
    })
}

async fn request_peer(
    settings: &AppSettings,
    peer: &HostRoute,
    action: AgentAction,
) -> Result<AgentResponse, String> {
    let endpoint = route_endpoint(peer).map_err(core_user_error)?;
    if !has_valid_shared_key(&settings.shared_key) {
        return Err(match UiLocale::current() {
            UiLocale::TraditionalChinese => {
                format!("請先設定至少 {MIN_SHARED_KEY_LENGTH} 個字元的配對密碼")
            }
            UiLocale::English => format!(
                "Configure a pairing password of at least {MIN_SHARED_KEY_LENGTH} characters first"
            ),
        });
    }
    let response = AgentClient::new(endpoint, stretched_pairing_key(&settings.shared_key))
        .request(action, next_nonce())
        .await
        .map_err(core_user_error)?;
    if response.ready {
        Ok(response)
    } else {
        Err(response.message)
    }
}

async fn wake_route(settings: &AppSettings, peer: &HostRoute) -> Result<(), String> {
    if peer.mac_address.trim().is_empty() {
        return Err(match UiLocale::current() {
            UiLocale::TraditionalChinese => format!("{} 沒有提供可用的 MAC 位址，因此無法使用 Wake-on-LAN；主機醒著時仍可切換", peer.name),
            UiLocale::English => format!("{} has no usable MAC address, so Wake-on-LAN is unavailable; switching still works while the host is awake", peer.name),
        });
    }
    let mac_address = MacAddress::from_str(&peer.mac_address).map_err(core_user_error)?;
    let broadcast_address = wake_broadcast_address(settings).map_err(core_user_error)?;
    WakeTarget {
        mac_address,
        broadcast_address,
        port: settings.wake_port,
    }
    .wake()
    .await
    .map_err(core_user_error)
}

/// Where wake packets are sent: the all-hosts broadcast or an address on a
/// local network, like everything else MuxSU sends. A wake packet carries the
/// target's MAC address, so it is never sent anywhere beyond.
fn wake_broadcast_address(settings: &AppSettings) -> Result<Ipv4Addr, DisplayMuxError> {
    let address = Ipv4Addr::from_str(&settings.broadcast_ip).map_err(|_| {
        DisplayMuxError::WakeFailed(
            ui_text("廣播位址格式無效", "Invalid broadcast address").to_owned(),
        )
    })?;
    if !address.is_broadcast() && !muxsu_core::is_local_network_address(IpAddr::V4(address)) {
        return Err(DisplayMuxError::WakeFailed(
            ui_text(
                "廣播位址必須是區域網路位址或 255.255.255.255",
                "The broadcast address must be on a local network or 255.255.255.255",
            )
            .to_owned(),
        ));
    }
    Ok(address)
}

fn route_endpoint(peer: &HostRoute) -> Result<PeerEndpoint, DisplayMuxError> {
    let address = IpAddr::from_str(&peer.address).map_err(|_| {
        DisplayMuxError::PeerUnavailable(match UiLocale::current() {
            UiLocale::TraditionalChinese => format!("{} 的 IP 位址無效", peer.name),
            UiLocale::English => format!("{} has an invalid IP address", peer.name),
        })
    })?;
    if !muxsu_core::is_local_network_address(address) {
        return Err(DisplayMuxError::PeerUnavailable(
            match UiLocale::current() {
                UiLocale::TraditionalChinese => {
                    format!("{} 不在區域網路內，MuxSU 不會連線", peer.name)
                }
                UiLocale::English => {
                    format!(
                        "{} is not on a local network, so MuxSU will not connect to it",
                        peer.name
                    )
                }
            },
        ));
    }
    Ok(PeerEndpoint {
        address,
        port: peer.port,
    })
}

fn find_peer<'a>(settings: &'a AppSettings, peer_id: &str) -> Result<&'a HostRoute, String> {
    settings
        .peers
        .iter()
        .find(|peer| peer.id == peer_id)
        .ok_or_else(|| {
            ui_text(
                "找不到這台已配對主機，請重新搜尋並加入",
                "This paired host was not found. Search for it and add it again.",
            )
            .to_owned()
        })
}

fn validate_settings(settings: &AppSettings) -> Result<(), DisplayMuxError> {
    if settings.local_host != local_host() {
        return Err(DisplayMuxError::Rejected {
            zh: "這台電腦的主機類型必須由作業系統自動判定".to_owned(),
            en: "This computer's host type must be determined by the operating system".to_owned(),
        });
    }
    if settings.host_switcher_enabled {
        validate_host_switcher_shortcut(&settings.host_switcher_shortcut)?;
    }
    let invalid_input = |input: DisplayInput| DisplayInput::new(input.value()).is_err();
    let has_invalid_input = settings
        .shared_monitors
        .iter()
        .any(|selected| selected.local_input.is_some_and(invalid_input))
        || settings.peers.iter().any(|peer| {
            peer.inputs
                .iter()
                .any(|assignment| invalid_input(assignment.input))
        });
    if has_invalid_input {
        return Err(DisplayMuxError::Rejected {
            zh: "請選擇有效的螢幕輸入 Port".to_owned(),
            en: "Select a valid display input port".to_owned(),
        });
    }
    for peer in &settings.peers {
        for assignment in &peer.inputs {
            let known_monitor = settings
                .shared_monitors
                .iter()
                .any(|selected| selected.fingerprint.matches_exactly(&assignment.monitor));
            if !known_monitor {
                return Err(DisplayMuxError::Rejected {
                    zh: "輸入設定對應到不存在的共用螢幕".to_owned(),
                    en: "The input assignment refers to a shared display that is not selected"
                        .to_owned(),
                });
            }
        }
    }
    for selected in &settings.shared_monitors {
        let assigned_inputs = selected
            .local_input
            .into_iter()
            .chain(
                settings
                    .peers
                    .iter()
                    .filter_map(|peer| peer.input_for(&selected.fingerprint)),
            )
            .collect::<Vec<_>>();
        let unique_inputs = assigned_inputs
            .iter()
            .map(|input| input.value())
            .collect::<std::collections::HashSet<_>>();
        if unique_inputs.len() != assigned_inputs.len() {
            return Err(DisplayMuxError::Rejected {
                zh: "每個主機必須使用不同的螢幕輸入 Port".to_owned(),
                en: "Each host must use a different display input port".to_owned(),
            });
        }
        if let Some(supported) = &selected.supported_inputs {
            if settings
                .peers
                .iter()
                .filter_map(|peer| peer.input_for(&selected.fingerprint))
                .any(|assigned| !supported.contains(&assigned))
            {
                return Err(DisplayMuxError::Rejected {
                    zh: "輸入值不在這台螢幕的 MCCS capabilities 清單中".to_owned(),
                    en: "The input is not listed in this display's MCCS capabilities".to_owned(),
                });
            }
        }
    }
    for peer in &settings.peers {
        route_endpoint(peer)?;
        if !peer.mac_address.trim().is_empty() {
            MacAddress::from_str(&peer.mac_address)?;
        }
    }
    wake_broadcast_address(settings)?;
    if !settings.shared_key.is_empty() && !has_valid_shared_key(&settings.shared_key) {
        return Err(DisplayMuxError::Rejected {
            zh: format!("配對密碼至少需要 {MIN_SHARED_KEY_LENGTH} 個字元"),
            en: format!(
                "The pairing password must contain at least {MIN_SHARED_KEY_LENGTH} characters"
            ),
        });
    }
    Ok(())
}

fn validate_host_switcher_shortcut(value: &str) -> Result<Shortcut, DisplayMuxError> {
    let shortcut = Shortcut::from_str(value).map_err(|_| DisplayMuxError::Rejected {
        zh: "無法辨識快捷鍵，請同時按下修飾鍵與一個一般按鍵".to_owned(),
        en: "The shortcut was not recognized. Press a modifier and one regular key.".to_owned(),
    })?;
    let required_modifier = platform_primary_shortcut_modifier();
    if !shortcut.mods.intersects(required_modifier) {
        return Err(DisplayMuxError::Rejected {
            zh: "Windows 快捷鍵必須包含 Ctrl；macOS 快捷鍵必須包含 Command".to_owned(),
            en: "The shortcut must include Ctrl on Windows or Command on macOS.".to_owned(),
        });
    }
    if shortcut.mods.bits().count_ones() > 2 {
        return Err(DisplayMuxError::Rejected {
            zh: "Ctrl 或 Command 之外最多只能再搭配一個修飾鍵".to_owned(),
            en: "Use at most one additional modifier with Ctrl or Command.".to_owned(),
        });
    }
    if is_system_shortcut(&shortcut) {
        return Err(DisplayMuxError::Rejected {
                zh: "這是作業系統保留的快捷鍵，按下時不會傳到這個程式，請改用其他組合".to_owned(),
                en: "The operating system keeps this shortcut for itself, so pressing it never reaches this app. Choose another combination.".to_owned(),
            });
    }
    if is_common_application_shortcut(&shortcut) {
        return Err(DisplayMuxError::Rejected {
                zh: "這是瀏覽器或常用應用程式的快捷鍵，請改用其他組合".to_owned(),
                en: "This shortcut is commonly used by browsers or other applications. Choose another combination.".to_owned(),
            });
    }
    Ok(shortcut)
}

fn platform_primary_shortcut_modifier() -> Modifiers {
    #[cfg(target_os = "macos")]
    {
        Modifiers::SUPER
    }
    #[cfg(not(target_os = "macos"))]
    {
        Modifiers::CONTROL
    }
}

/// Shortcuts the operating system claims before any application sees them.
///
/// Registering one of these succeeds — the system simply wins the key
/// afterwards — so there is nothing to detect at runtime and no error to
/// report. The app believes it holds a shortcut that can never reach it, which
/// is what a list like this exists to prevent.
#[cfg(target_os = "macos")]
fn is_system_shortcut(shortcut: &Shortcut) -> bool {
    let cmd = Modifiers::SUPER;
    let taken = [
        (cmd, Code::Space),                      // Spotlight
        (cmd | Modifiers::ALT, Code::Space),     // Finder search window
        (cmd | Modifiers::CONTROL, Code::Space), // Emoji and symbols
        (cmd | Modifiers::ALT, Code::KeyD),      // hide or show the Dock
        (cmd | Modifiers::ALT, Code::Escape),    // Force Quit
        (cmd | Modifiers::CONTROL, Code::KeyF),  // enter full screen
        (cmd | Modifiers::CONTROL, Code::KeyQ),  // lock screen
        (cmd | Modifiers::CONTROL, Code::KeyD),  // look up a word
        (cmd | Modifiers::SHIFT, Code::Digit3),  // screenshot
        (cmd | Modifiers::SHIFT, Code::Digit4),
        (cmd | Modifiers::SHIFT, Code::Digit5),
    ];
    taken
        .iter()
        .any(|(mods, key)| shortcut.mods == *mods && shortcut.key == *key)
}

#[cfg(not(target_os = "macos"))]
fn is_system_shortcut(shortcut: &Shortcut) -> bool {
    let ctrl = Modifiers::CONTROL;
    let taken = [
        (ctrl | Modifiers::SHIFT, Code::Escape), // Task Manager
        (ctrl | Modifiers::ALT, Code::Delete),   // secure attention sequence
    ];
    taken
        .iter()
        .any(|(mods, key)| shortcut.mods == *mods && shortcut.key == *key)
}

fn is_common_application_shortcut(shortcut: &Shortcut) -> bool {
    let primary = platform_primary_shortcut_modifier();
    let primary_only = shortcut.mods == primary;
    let primary_with_shift = shortcut.mods == (primary | Modifiers::SHIFT);
    (primary_only
        && matches!(
            shortcut.key,
            Code::KeyA
                | Code::KeyC
                | Code::KeyF
                | Code::KeyH
                | Code::KeyL
                | Code::KeyM
                | Code::KeyN
                | Code::KeyO
                | Code::KeyP
                | Code::KeyQ
                | Code::KeyR
                | Code::KeyS
                | Code::KeyT
                | Code::KeyV
                | Code::KeyW
                | Code::KeyX
                | Code::KeyY
                | Code::KeyZ
                | Code::Tab
                | Code::F4
        ))
        || (primary_with_shift
            && matches!(
                shortcut.key,
                Code::KeyN | Code::KeyP | Code::KeyR | Code::KeyS | Code::KeyT | Code::KeyW
            ))
}

fn update_host_switcher_shortcut(
    app: &AppHandle,
    previous: &AppSettings,
    next: &AppSettings,
) -> Result<(), String> {
    if previous.host_switcher_enabled == next.host_switcher_enabled
        && previous.host_switcher_shortcut == next.host_switcher_shortcut
    {
        return Ok(());
    }
    let next_shortcut = next
        .host_switcher_enabled
        .then(|| validate_host_switcher_shortcut(&next.host_switcher_shortcut))
        .transpose()
        .map_err(core_user_error)?;
    let previous_shortcut = previous
        .host_switcher_enabled
        .then(|| Shortcut::from_str(&previous.host_switcher_shortcut).ok())
        .flatten();
    let previous_was_registered =
        previous_shortcut.is_some_and(|shortcut| app.global_shortcut().is_registered(shortcut));
    if let Some(shortcut) = previous_shortcut.filter(|_| previous_was_registered) {
        app.global_shortcut()
            .unregister(shortcut)
            .map_err(user_error)?;
    }
    if let Some(shortcut) = next_shortcut {
        if let Err(error) = app.global_shortcut().register(shortcut) {
            if let Some(previous_shortcut) = previous_shortcut.filter(|_| previous_was_registered) {
                if let Err(restore_error) = app.global_shortcut().register(previous_shortcut) {
                    tracing::warn!(error = %restore_error, "unable to restore the previous global shortcut");
                }
            }
            return Err(match UiLocale::current() {
                UiLocale::TraditionalChinese => {
                    format!("無法註冊快捷鍵，可能已被其他程式使用：{error}")
                }
                UiLocale::English => {
                    format!("Unable to register the shortcut. Another app may be using it: {error}")
                }
            });
        }
    }
    Ok(())
}

fn show_host_switcher(app: &AppHandle) {
    let Some(window) = app.get_webview_window("host-switcher") else {
        tracing::warn!("host switcher window is unavailable");
        return;
    };
    if let Err(error) = window.center() {
        tracing::warn!(error = %error, "unable to center the host switcher window");
    }
    if let Err(error) = window.show() {
        tracing::warn!(error = %error, "unable to show the host switcher window");
        return;
    }
    if let Err(error) = window.set_focus() {
        tracing::warn!(error = %error, "unable to focus the host switcher window");
    }
}

fn upsert_discovered_peer(settings: &mut AppSettings, peer: &DiscoveredPeer) {
    if let Some(existing) = settings.peers.iter_mut().find(|item| item.id == peer.id) {
        existing.name.clone_from(&peer.name);
        existing.platform = peer.platform;
        existing.address = peer.address.to_string();
        existing.port = peer.port;
        return;
    }
    settings.peers.push(HostRoute {
        id: peer.id.clone(),
        name: peer.name.clone(),
        platform: peer.platform,
        address: peer.address.to_string(),
        port: peer.port,
        // Learned from the host's signed reply, never from discovery.
        mac_address: String::new(),
        inputs: Vec::new(),
    });
}

/// Largest reply this host sends, before its signature. A paired host reads at
/// most 8 KB of a reply and cannot parse one cut short, so this leaves room for
/// the signature and whatever else a reply grows.
const AGENT_REPLY_BUDGET_BYTES: usize = 7 * 1024;

/// `reply` within `AGENT_REPLY_BUDGET_BYTES`. A host whose Ping cannot be read
/// can no longer be switched to, so the state a paired host only catches up on
/// is dropped first, least needed first, and routing data is always kept. What
/// is dropped still reaches paired hosts in their own notices.
fn fit_agent_reply(mut reply: AgentResponse) -> AgentResponse {
    let fits = |reply: &AgentResponse| {
        serde_json::to_vec(reply).is_ok_and(|bytes| bytes.len() <= AGENT_REPLY_BUDGET_BYTES)
    };
    let shed: [fn(&mut AgentResponse); 6] = [
        |reply| reply.monitor_identity_links.clear(),
        |reply| reply.input_labels.clear(),
        |reply| reply.host_appearances.clear(),
        |reply| reply.host_aliases.clear(),
        |reply| reply.host_order.clear(),
        |reply| reply.host_inputs.clear(),
    ];
    for drop_section in shed {
        if fits(&reply) {
            break;
        }
        drop_section(&mut reply);
    }
    if !fits(&reply) {
        tracing::warn!("agent reply is over budget even without catch-up data");
    }
    reply
}

/// Takes a paired host's wake-on-LAN address from its signed reply, the only
/// place it is sent. Returns whether the saved address changed.
fn adopt_peer_mac_address(
    settings: &mut AppSettings,
    peer_id: &str,
    response: &AgentResponse,
) -> bool {
    let Some(address) = response
        .mac_address
        .as_deref()
        .and_then(|value| MacAddress::from_str(value).ok())
        .map(|value| value.to_string())
    else {
        return false;
    };
    let Some(peer) = settings.peers.iter_mut().find(|peer| peer.id == peer_id) else {
        return false;
    };
    if peer.mac_address == address {
        return false;
    }
    peer.mac_address = address;
    true
}

/// A peer's `Ping` response may report one route (pre-v2) or several
/// (v2+); this always yields the full set to apply.
fn agent_display_routes(response: &AgentResponse) -> Vec<AgentDisplayRoute> {
    if !response.display_routes.is_empty() {
        response.display_routes.clone()
    } else {
        response.display_route.clone().into_iter().collect()
    }
}

/// Which shared display a paired host means by `remote`. Hosts read serial
/// numbers differently, so after an exact match fails the display is matched
/// by model, but only when a single shared display has that model.
fn shared_monitor_index_for_peer(
    monitors: &[SelectedMonitor],
    links: &[MonitorIdentityLink],
    remote: &MonitorFingerprint,
) -> Option<usize> {
    // An identity the user merged into a shared display names that display, so
    // a paired host reading the same panel in another display mode still lands
    // on it rather than looking like a display we do not share.
    //
    // The fall back to the model alone below is not laziness. Two hosts read
    // two different EDID serial fields, so one shared display can reach us
    // under a serial we will never read from it — an Acer VG252Q is
    // `TH6TT0028525` here and `576726074` on the Mac — and `same_identity`
    // reads that as two displays. Matching on the model recovers it without
    // ever guessing: with a second display of that model shared, this returns
    // `None` and the switch is refused rather than written to the wrong panel.
    if let Some(exact) = monitors.iter().position(|selected| {
        monitor_identity::is_same_display(links, &selected.fingerprint, remote)
    }) {
        return Some(exact);
    }
    let mut same_model = monitors.iter().enumerate().filter(|(_, selected)| {
        monitor_identity::identities_for(links, &selected.fingerprint)
            .iter()
            .any(|identity| identity.is_same_model(remote))
    });
    match (same_model.next(), same_model.next()) {
        (Some((index, _)), None) => Some(index),
        _ => None,
    }
}

/// Whether this host believes `selected` is currently showing it. An unset
/// `active_route` renders as this host everywhere else, so it counts as this
/// host here too — which is how a host that was never switched away sees
/// itself.
fn shows_this_host(selected: &SelectedMonitor) -> bool {
    selected.active_route.as_deref().unwrap_or("local") == "local"
}

/// What `apply_verified_peer_route` did with a route a paired host reported.
/// Every rejection carries its reason so the caller can say why a port stayed
/// empty instead of leaving the user with a blank field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerRouteOutcome {
    /// The reported input became the peer's assignment for that display.
    Applied,
    /// The peer was already assigned that input.
    Unchanged,
    /// The peer kept the input it already had, because it did not claim to be
    /// on screen and so cannot vouch for what it reported.
    Kept,
    /// The peer named a display this host does not share.
    UnknownMonitor,
    /// The display does not offer that input.
    Unsupported,
    /// This host or another paired host already uses that input.
    Taken,
    /// The peer is no longer in the saved list.
    UnknownPeer,
}

/// Adopts the input a paired host reported for one shared display.
///
/// A reading the peer cannot vouch for never overwrites an input that is
/// already set: DDC reports the input a display is showing, not the port the
/// reader occupies, so a host that is off screen reads whoever is on screen.
fn apply_verified_peer_route(
    settings: &mut AppSettings,
    peer_id: &str,
    route: AgentDisplayRoute,
) -> PeerRouteOutcome {
    let Some((fingerprint, local_input, supported_inputs, guessed_inputs)) =
        shared_monitor_index_for_peer(
            &settings.shared_monitors,
            &settings.monitor_identity_links,
            &route.monitor,
        )
        .map(|index| &settings.shared_monitors[index])
        .map(|selected| {
            (
                selected.fingerprint.clone(),
                selected.local_input,
                selected.supported_inputs.clone(),
                selected.vendor_indexed_inputs,
            )
        })
    else {
        return PeerRouteOutcome::UnknownMonitor;
    };
    let supported = supported_inputs.as_ref().map_or_else(
        || common_input_sources().contains(&route.input),
        |inputs| inputs.contains(&route.input),
    );
    // What this host believes the display accepts is often its own guess. A
    // display that would not give up its capabilities — because it is busy
    // showing the other computer, or sits behind a hub — leaves this host with
    // the standard MCCS codes, which describe no particular display; one whose
    // capabilities omitted the input it was showing leaves it with the private
    // `1..=max` range, which is a range rather than a list of inputs. Neither
    // knows a vendor-specific port such as the Type-C input this display calls
    // 8, and both discard it.
    //
    // A host that was on screen read its own port from the display itself,
    // which is what `confirmed` records. Against a guess, that wins.
    let inputs_are_a_guess = guessed_inputs || supported_inputs.is_none();
    if !supported && !(route.confirmed && inputs_are_a_guess) {
        tracing::info!(
            peer_id,
            input = route.input.value(),
            confirmed = route.confirmed,
            "paired host reported an input this display is not known to accept"
        );
        return PeerRouteOutcome::Unsupported;
    }
    let Some(existing) = settings
        .peers
        .iter()
        .find(|peer| peer.id == peer_id)
        .map(|peer| peer.input_for(&fingerprint))
    else {
        return PeerRouteOutcome::UnknownPeer;
    };
    if existing == Some(route.input) {
        return PeerRouteOutcome::Unchanged;
    }
    if existing.is_some() && !route.confirmed {
        return PeerRouteOutcome::Kept;
    }
    let taken = local_input == Some(route.input)
        || settings
            .peers
            .iter()
            .any(|peer| peer.id != peer_id && peer.input_for(&fingerprint) == Some(route.input));
    if taken {
        return PeerRouteOutcome::Taken;
    }
    let Some(peer) = settings.peers.iter_mut().find(|peer| peer.id == peer_id) else {
        return PeerRouteOutcome::UnknownPeer;
    };
    peer.set_input_for(&fingerprint, Some(route.input));
    PeerRouteOutcome::Applied
}

/// The address discovery reports for `peer`, when it differs from the stored
/// one. Only a candidate: discovery answers from a cache that can name an
/// address the host has already left, or one of several interfaces where only
/// some accept connections, so following it unchecked replaces a working
/// address with a dead one.
///
/// Matched by host id alone. That id is fixed for the life of an install (see
/// `identifies_the_machine`), so it names the same computer wherever it turns
/// up; an address does not, which is the whole problem. Whatever now answers
/// where the host used to be therefore inherits nothing.
fn moved_peer_endpoint(peer: &HostRoute, discovered: &[DiscoveredPeer]) -> Option<(String, u16)> {
    let found = discovered
        .iter()
        .find(|candidate| candidate.id == peer.id)?;
    let address = found.address.to_string();
    (peer.address != address || peer.port != found.port).then_some((address, found.port))
}

/// Whether the host answering at `peer`'s address proves it holds the pairing
/// password, by signing its reply against the nonce of the Ping it is
/// answering.
///
/// Discovery is unauthenticated and a host id is public, so an address learned
/// there is only a claim. The current client accepts the reply only after its
/// full payload, nonce and exact protocol version pass authentication.
async fn peer_proves_pairing(settings: &AppSettings, peer: &HostRoute) -> bool {
    let Ok(endpoint) = route_endpoint(peer) else {
        return false;
    };
    if !has_valid_shared_key(&settings.shared_key) {
        return false;
    }
    let nonce = next_nonce();
    let key = stretched_pairing_key(&settings.shared_key);
    match AgentClient::new(endpoint, Arc::clone(&key))
        .request(AgentAction::Ping, nonce.clone())
        .await
    {
        Ok(response) => response.proves_pairing(&nonce, &key),
        Err(_) => false,
    }
}

/// `moved_peer_endpoint`, confirmed by the host answering there proving it is
/// the host this computer is paired with.
///
/// Following an address on discovery alone hands the pairing to whoever
/// advertises the right id, which anyone on the network can read off the air
/// and repeat. A response without the current full-payload signature and exact
/// protocol version proves nothing, so the saved address remains unchanged.
async fn confirmed_move(
    settings: &AppSettings,
    peer: &HostRoute,
    discovered: &[DiscoveredPeer],
) -> Option<(String, u16)> {
    let (address, port) = moved_peer_endpoint(peer, discovered)?;
    let candidate = HostRoute {
        address: address.clone(),
        port,
        ..peer.clone()
    };
    if !peer_proves_pairing(settings, &candidate).await {
        tracing::warn!(
            peer = peer.name.as_str(),
            to = address.as_str(),
            "a host answering at a new address did not prove the pairing; staying put"
        );
        return None;
    }
    tracing::info!(
        peer = peer.name.as_str(),
        from = peer.address.as_str(),
        to = address.as_str(),
        "paired host answered at a new address; following it"
    );
    Some((address, port))
}

async fn refresh_paired_endpoints(
    state: &AppRuntime,
    peers: &[DiscoveredPeer],
) -> Result<(), String> {
    let current = read_settings_inner(state)?;
    let mut updated = current.clone();
    for peer in peers {
        let Some(existing) = updated.peers.iter().find(|item| item.id == peer.id) else {
            continue;
        };
        // Everything but where to reach it can be taken as reported; a wrong
        // name costs nothing, a wrong address costs the pairing.
        let moved = confirmed_move(&current, existing, peers).await;
        let Some(existing) = updated.peers.iter_mut().find(|item| item.id == peer.id) else {
            continue;
        };
        existing.name.clone_from(&peer.name);
        existing.platform = peer.platform;
        if let Some((address, port)) = moved {
            existing.address = address;
            existing.port = port;
        }
    }
    if updated != current {
        store_settings(state, updated)?;
    }
    Ok(())
}

async fn restart_agent(state: &AppRuntime, app: &AppHandle) -> Result<(), String> {
    let settings = read_settings_inner(state)?;
    let mut current_task = state.agent_task.lock().await;
    if let Some(task) = current_task.take() {
        task.abort();
    }
    if !has_valid_shared_key(&settings.shared_key) {
        return Ok(());
    }
    let server = AgentServer::new(
        SocketAddr::from(([0, 0, 0, 0], DEFAULT_AGENT_PORT)),
        stretched_pairing_key(&settings.shared_key),
    );
    let live_settings = Arc::clone(&state.settings);
    let local_mac_address = state.local_mac_address.clone();
    let app = app.clone();
    *current_task = Some(tauri::async_runtime::spawn(async move {
        let result = server
            .run(move |action| {
                let live_settings = Arc::clone(&live_settings);
                let local_mac_address = local_mac_address.clone();
                let app = app.clone();
                async move {
                    match action {
                        AgentAction::ActiveInputChanged { monitor, input } => {
                            receive_active_input_notice(app, monitor, input).await
                        }
                        AgentAction::HostOrderChanged {
                            order,
                            updated_at_ms,
                        } => receive_host_order_notice(app, order, updated_at_ms).await,
                        AgentAction::HostAliasesChanged { aliases } => {
                            receive_host_aliases_notice(app, aliases).await
                        }
                        AgentAction::HostAppearancesChanged { appearances } => {
                            receive_host_appearances_notice(app, appearances).await
                        }
                        AgentAction::InputLabelsChanged { labels } => {
                            receive_input_labels_notice(app, labels).await
                        }
                        AgentAction::MonitorIdentitiesChanged { links } => {
                            receive_monitor_identities_notice(app, links).await
                        }
                        AgentAction::DisplayInputsDiscovered {
                            monitor,
                            inputs,
                            vendor_indexed,
                        } => receive_display_inputs(app, monitor, inputs, vendor_indexed).await,
                        AgentAction::LocalInputConfirmed {
                            host_id,
                            monitor,
                            input,
                        } => receive_local_input_confirmed(app, host_id, monitor, input).await,
                        AgentAction::HostInputsChanged { assignments } => {
                            receive_host_inputs_notice(app, assignments).await
                        }
                        AgentAction::DiagnosticsRequested => {
                            let settings = live_settings.read().ok().map(|settings| settings.clone());
                            answer_diagnostics_request(&app, settings)
                        }
                        AgentAction::Ping => {
                            let snapshot = live_settings.read().ok().map(|settings| settings.clone());
                            let display_routes = snapshot
                                .as_ref()
                                .map(|settings| {
                                    settings
                                        .shared_monitors
                                        .iter()
                                        .filter_map(|selected| {
                                            Some(AgentDisplayRoute {
                                                monitor: selected.fingerprint.clone(),
                                                input: selected.local_input?,
                                                confirmed: shows_this_host(selected),
                                            })
                                        })
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default();
                            // What this computer can see of its own displays,
                            // so the asking host can tell "asleep" from "up,
                            // but nothing plugged into that display".
                            let attached_monitors = snapshot.as_ref().and_then(|settings| {
                                let state = app.try_state::<AppRuntime>()?;
                                attached_shared_monitors(&state, settings, unix_time_ms())
                            });
                            if attached_monitors.is_none() {
                                refresh_attached_monitors_soon(&app);
                            }
                            let (
                                host_order,
                                host_order_updated_at_ms,
                                host_aliases,
                                host_appearances,
                                input_labels,
                                monitor_identity_links,
                                host_inputs,
                            ) = snapshot
                                .map(|settings| {
                                    (
                                        settings.host_order,
                                        settings.host_order_updated_at_ms,
                                        host_alias::shareable_aliases(&settings.host_aliases),
                                        host_appearance::shareable_appearances(
                                            &settings.host_appearances,
                                        ),
                                        input_label::shareable_labels(&settings.input_labels),
                                        settings.monitor_identity_links,
                                        settings.host_inputs,
                                    )
                                })
                                .unwrap_or_default();
                            fit_agent_reply(AgentResponse {
                                ready: true,
                                message: ui_text(
                                    "MuxSU Agent 已就緒",
                                    "MuxSU Agent is ready",
                                )
                                .to_owned(),
                                display_route: display_routes.first().cloned(),
                                display_routes,
                                attached_monitors,
                                protocol_version: AGENT_PROTOCOL_VERSION,
                                host_order,
                                host_order_updated_at_ms,
                                host_aliases,
                                host_appearances,
                                input_labels,
                                monitor_identity_links,
                                host_inputs,
                                mac_address: local_mac_address,
                                diagnostics: None,
                                // Filled in by the listener, which holds the
                                // nonce this reply has to be bound to.
                                signature: None,
                            })
                        }
                        AgentAction::SwitchInput { monitor, input } => {
                            let resolved = live_settings.read().ok().map(|settings| {
                                match &monitor {
                                    Some(requested) => shared_monitor_index_for_peer(
                                        &settings.shared_monitors,
                                        &settings.monitor_identity_links,
                                        requested,
                                    )
                                        .map(|index| &settings.shared_monitors[index])
                                        .map(|selected| {
                                            (
                                                monitor_identity::identities_for(
                                                    &settings.monitor_identity_links,
                                                    &selected.fingerprint,
                                                ),
                                                selected.vendor_indexed_inputs,
                                            )
                                        })
                                        .ok_or_else(|| ui_text(
                                            "找不到指定的共用螢幕，請確認雙方設定一致",
                                            "The requested shared display was not found; confirm both hosts' selections match",
                                        ).to_owned()),
                                    None => match settings.shared_monitors.as_slice() {
                                        [] => Err(ui_text(
                                            "這台主機尚未選擇共用螢幕",
                                            "No shared display is selected on this host",
                                        )
                                        .to_owned()),
                                        [only] => Ok((
                                            monitor_identity::identities_for(
                                                &settings.monitor_identity_links,
                                                &only.fingerprint,
                                            ),
                                            only.vendor_indexed_inputs,
                                        )),
                                        _ => Err(ui_text(
                                            "配對主機切換到了多台共用螢幕，請將這台電腦更新到最新版本",
                                            "The paired host is now managing multiple shared displays; update this computer to the latest version.",
                                        )
                                        .to_owned()),
                                    },
                                }
                            });
                            let (identities, vendor_indexed) = match resolved {
                                Some(Ok(resolved)) => resolved,
                                Some(Err(message)) => {
                                    return AgentResponse {
                                        ready: false,
                                        message,
                                        display_route: None,
                                        display_routes: Vec::new(),
                                        protocol_version: AGENT_PROTOCOL_VERSION,
                                        ..AgentResponse::default()
                                    };
                                }
                                None => {
                                    return AgentResponse {
                                        ready: false,
                                        message: ui_text(
                                            "無法讀取這台主機的設定",
                                            "Unable to read this host's settings",
                                        )
                                        .to_owned(),
                                        display_route: None,
                                        display_routes: Vec::new(),
                                        protocol_version: AGENT_PROTOCOL_VERSION,
                                        ..AgentResponse::default()
                                    };
                                }
                            };
                            match tauri::async_runtime::spawn_blocking(move || {
                                run_switch(identities, input)
                            })
                            .await
                            {
                                Ok(Ok(_)) => AgentResponse {
                                    ready: true,
                                    message: match UiLocale::current() {
                                        UiLocale::TraditionalChinese => format!(
                                            "遠端主機已切換至 {}",
                                            input_label(vendor_indexed, input)
                                        ),
                                        UiLocale::English => format!(
                                            "The remote host switched to {}",
                                            input_label(vendor_indexed, input)
                                        ),
                                    },
                                    display_route: None,
                                    display_routes: Vec::new(),
                                    protocol_version: AGENT_PROTOCOL_VERSION,
                                    ..AgentResponse::default()
                                },
                                Ok(Err(error)) => AgentResponse {
                                    ready: false,
                                    message: core_user_error(error),
                                    display_route: None,
                                    display_routes: Vec::new(),
                                    protocol_version: AGENT_PROTOCOL_VERSION,
                                    ..AgentResponse::default()
                                },
                                Err(error) => AgentResponse {
                                    ready: false,
                                    message: match UiLocale::current() {
                                        UiLocale::TraditionalChinese => {
                                            format!("切換工作無法執行：{error}")
                                        }
                                        UiLocale::English => {
                                            format!("The switching task could not run: {error}")
                                        }
                                    },
                                    display_route: None,
                                    display_routes: Vec::new(),
                                    protocol_version: AGENT_PROTOCOL_VERSION,
                                    ..AgentResponse::default()
                                },
                            }
                        }
                    }
                }
            })
            .await;
        if let Err(error) = result {
            tracing::error!(error = %error, "MuxSU agent stopped");
        }
    }));
    Ok(())
}

/// Marks which route ("local" or a peer id) a shared monitor's input was
/// just confirmed switched to, so the dashboard can show the true active
/// host instead of always assuming local. Returns `false` (no-op) if the
/// monitor was since unselected, e.g. removed while the switch was in flight.
fn set_active_route(
    settings: &mut AppSettings,
    fingerprint: &MonitorFingerprint,
    route_id: &str,
    now_ms: u64,
) -> bool {
    let Some(selected) = settings
        .shared_monitors
        .iter_mut()
        .find(|selected| selected.fingerprint.matches_exactly(fingerprint))
    else {
        return false;
    };
    selected.active_route = Some(route_id.to_owned());
    selected.active_route_confirmed_at_ms = now_ms;
    true
}

fn record_active_route(
    state: &AppRuntime,
    fingerprint: &MonitorFingerprint,
    route_id: &str,
) -> Result<(), String> {
    let mut settings = read_settings(state)?;
    if set_active_route(&mut settings, fingerprint, route_id, unix_time_ms()) {
        store_settings(state, settings)?;
    }
    Ok(())
}

/// Frontend event telling the dashboard to re-read which host is active.
const ACTIVE_ROUTE_CHANGED_EVENT: &str = "active-route-changed";
/// Carries a `tray::SwitchNotice` to the window that shows it.
const SWITCH_NOTICE_EVENT: &str = "switch-notice";

/// How long a confirmed switch outranks live reads of the display's input.
/// Displays can keep reporting the previous input for several seconds while
/// they change over, and Windows returns from a switch without waiting.
const ACTIVE_ROUTE_SETTLE_MS: u64 = 15_000;

/// Tells every paired host that `fingerprint` now shows `input`, so their
/// dashboards update without a manual refresh. The durable outbox retains a
/// copy for each offline host until its signed ACK arrives.
fn announce_active_input(
    state: &AppRuntime,
    fingerprint: &MonitorFingerprint,
    input: DisplayInput,
) {
    broadcast_to_peers(
        state,
        &AgentAction::ActiveInputChanged {
            monitor: fingerprint.clone(),
            input,
        },
    );
}

/// Mirrors route changes learned from a live display read to every UI window
/// and paired host. App-initiated switches already take this path directly;
/// this closes the gap for the monitor's own buttons, another DDC utility, or
/// a cable change discovered by a refresh.
fn publish_active_input_updates(
    state: &AppRuntime,
    app: &AppHandle,
    updates: &[ActiveInputUpdate],
) {
    if updates.is_empty() {
        return;
    }
    if let Err(error) = app.emit(ACTIVE_ROUTE_CHANGED_EVENT, ()) {
        tracing::warn!(error = %error, "unable to notify windows of a detected active host change");
    }
    for update in updates {
        announce_active_input(state, &update.monitor, update.input);
    }
}

#[derive(Clone)]
struct NoticeDeliveryRuntime {
    settings: Arc<RwLock<AppSettings>>,
    settings_path: PathBuf,
    in_flight: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

fn notice_delivery_runtime(state: &AppRuntime) -> NoticeDeliveryRuntime {
    NoticeDeliveryRuntime {
        settings: Arc::clone(&state.settings),
        settings_path: state.settings_path.clone(),
        in_flight: Arc::clone(&state.notices_in_flight),
    }
}

/// Whether a newly queued action completely replaces an older pending one.
/// This is intentionally directional: a full host-input snapshot supersedes
/// an older one-off confirmation, while a later one-off confirmation cannot
/// discard unrelated assignments in the full snapshot.
fn notice_supersedes(new: &AgentAction, pending: &AgentAction) -> bool {
    match (new, pending) {
        (AgentAction::HostOrderChanged { .. }, AgentAction::HostOrderChanged { .. })
        | (AgentAction::HostAliasesChanged { .. }, AgentAction::HostAliasesChanged { .. })
        | (
            AgentAction::HostAppearancesChanged { .. },
            AgentAction::HostAppearancesChanged { .. },
        )
        | (AgentAction::InputLabelsChanged { .. }, AgentAction::InputLabelsChanged { .. })
        | (
            AgentAction::MonitorIdentitiesChanged { .. },
            AgentAction::MonitorIdentitiesChanged { .. },
        )
        | (AgentAction::HostInputsChanged { .. }, AgentAction::HostInputsChanged { .. })
        | (AgentAction::HostInputsChanged { .. }, AgentAction::LocalInputConfirmed { .. }) => true,
        (
            AgentAction::ActiveInputChanged { monitor: left, .. },
            AgentAction::ActiveInputChanged { monitor: right, .. },
        )
        | (
            AgentAction::DisplayInputsDiscovered { monitor: left, .. },
            AgentAction::DisplayInputsDiscovered { monitor: right, .. },
        )
        | (
            AgentAction::LocalInputConfirmed { monitor: left, .. },
            AgentAction::LocalInputConfirmed { monitor: right, .. },
        ) => left.matches_exactly(right),
        _ => false,
    }
}

fn update_delivery_settings(
    runtime: &NoticeDeliveryRuntime,
    update: impl FnOnce(&mut AppSettings),
) -> Result<(), String> {
    let mut settings = runtime.settings.write().map_err(|_| {
        ui_text(
            "無法更新同步佇列，請重新啟動 MuxSU",
            "Unable to update the sync queue. Restart MuxSU.",
        )
        .to_owned()
    })?;
    let previous = settings.clone();
    update(&mut settings);
    if let Err(error) = persist_settings(&runtime.settings_path, &settings) {
        *settings = previous;
        return Err(core_user_error(error));
    }
    Ok(())
}

/// Attempts before a notice is given up on. At the five-minute retry ceiling
/// this is about a week of MuxSU running. A peer that is still unreachable
/// catches up anyway: every response it gives carries the shared state.
const MAX_NOTICE_ATTEMPTS: u32 = 2_000;

fn retry_delay_ms(attempts: u32) -> u64 {
    let exponent = attempts.min(9);
    1_000_u64.saturating_mul(1_u64 << exponent).min(300_000)
}

async fn deliver_pending_notice(runtime: NoticeDeliveryRuntime, notice_id: String) {
    let pending = runtime.settings.read().ok().and_then(|settings| {
        let notice = settings
            .pending_peer_notices
            .iter()
            .find(|notice| notice.id == notice_id)?
            .clone();
        let peer = settings
            .peers
            .iter()
            .find(|peer| peer.id == notice.peer_id)?
            .clone();
        Some((settings.clone(), peer, notice))
    });

    let Some((settings, peer, notice)) = pending else {
        let _ = update_delivery_settings(&runtime, |settings| {
            settings
                .pending_peer_notices
                .retain(|queued| queued.id != notice_id);
        });
        return;
    };
    {
        let Ok(mut in_flight) = runtime.in_flight.lock() else {
            return;
        };
        // One request at a time per peer preserves mutation order. In
        // particular, an older active-screen notice cannot arrive after its
        // replacement merely because its first attempt was slow.
        if !in_flight.insert(peer.id.clone()) {
            return;
        }
    }

    {
        match request_peer(&settings, &peer, notice.action.clone()).await {
            Ok(_) => {
                // `request_peer` accepts only a ready response whose complete
                // payload is signed for this request's nonce. That is the ACK.
                if let Err(error) = update_delivery_settings(&runtime, |settings| {
                    settings
                        .pending_peer_notices
                        .retain(|queued| queued.id != notice.id);
                }) {
                    tracing::warn!(peer = peer.name.as_str(), error = %error, "unable to persist a notice ACK");
                }
            }
            Err(error) => {
                let now = unix_time_ms();
                let mut dropped = false;
                if let Err(persist_error) = update_delivery_settings(&runtime, |settings| {
                    dropped = record_failed_notice_attempt(settings, &notice.id, now);
                }) {
                    tracing::warn!(peer = peer.name.as_str(), error = %persist_error, "unable to save a notice retry");
                }
                if dropped {
                    tracing::warn!(peer = peer.name.as_str(), error = %error, "paired host never ACKed the notice; giving up");
                } else {
                    tracing::info!(peer = peer.name.as_str(), error = %error, "paired host did not ACK the notice; retry scheduled");
                }
            }
        }
    }

    if let Ok(mut in_flight) = runtime.in_flight.lock() {
        in_flight.remove(&peer.id);
    }
}

/// Schedules the next attempt after a failed delivery, or drops the notice
/// once it has used every attempt. Returns whether it was dropped.
fn record_failed_notice_attempt(settings: &mut AppSettings, notice_id: &str, now_ms: u64) -> bool {
    let Some(index) = settings
        .pending_peer_notices
        .iter()
        .position(|queued| queued.id == notice_id)
    else {
        return false;
    };
    let queued = &mut settings.pending_peer_notices[index];
    queued.attempts = queued.attempts.saturating_add(1);
    if queued.attempts >= MAX_NOTICE_ATTEMPTS {
        settings.pending_peer_notices.remove(index);
        return true;
    }
    queued.next_attempt_at_ms = now_ms.saturating_add(retry_delay_ms(queued.attempts));
    false
}

/// The notices to send now: for each peer with no request in flight, the
/// oldest one that is due. One backing off after a failure does not hold back
/// the rest: a newer notice of the same kind has already replaced an older
/// one, and every entry is merged by its own timestamp, so notices of
/// different kinds need no particular order.
fn due_notice_ids(
    notices: &[PendingPeerNotice],
    in_flight: &std::collections::HashSet<String>,
    now_ms: u64,
) -> Vec<String> {
    let mut seen_peers = std::collections::HashSet::new();
    notices
        .iter()
        .filter(|notice| {
            notice.next_attempt_at_ms <= now_ms && !in_flight.contains(&notice.peer_id)
        })
        .filter(|notice| seen_peers.insert(notice.peer_id.as_str()))
        .map(|notice| notice.id.clone())
        .collect()
}

fn retry_pending_notices(state: &AppRuntime) {
    let runtime = notice_delivery_runtime(state);
    let in_flight = runtime
        .in_flight
        .lock()
        .map(|in_flight| in_flight.clone())
        .unwrap_or_default();
    let ids = read_settings(state)
        .map(|settings| due_notice_ids(&settings.pending_peer_notices, &in_flight, unix_time_ms()))
        .unwrap_or_default();
    for id in ids {
        tauri::async_runtime::spawn(deliver_pending_notice(runtime.clone(), id));
    }
}

/// Durably queues `action` for every paired host, sends immediately, and only
/// removes each copy after that peer returns a valid signed ACK. Newer full
/// state replaces an older unsent notice in the same stream; this bounds the
/// queue and prevents an old retry from undoing a later edit.
fn broadcast_to_peers(state: &AppRuntime, action: &AgentAction) -> bool {
    let Ok(settings) = read_settings(state) else {
        return false;
    };
    if !has_valid_shared_key(&settings.shared_key) || settings.peers.is_empty() {
        return false;
    }
    let peer_ids = settings
        .peers
        .iter()
        .map(|peer| peer.id.clone())
        .collect::<Vec<_>>();
    let runtime = notice_delivery_runtime(state);
    let mut ids = Vec::with_capacity(peer_ids.len());
    let action = action.clone();
    let queued = update_delivery_settings(&runtime, |settings| {
        for peer_id in &peer_ids {
            settings.pending_peer_notices.retain(|pending| {
                pending.peer_id != *peer_id || !notice_supersedes(&action, &pending.action)
            });
            let id = next_nonce();
            settings.pending_peer_notices.push(PendingPeerNotice {
                id: id.clone(),
                peer_id: peer_id.clone(),
                action: action.clone(),
                attempts: 0,
                next_attempt_at_ms: 0,
            });
            ids.push(id);
        }
    });
    if let Err(error) = queued {
        tracing::warn!(error = %error, "unable to queue a paired-host notice");
        return false;
    }
    for id in ids {
        tauri::async_runtime::spawn(deliver_pending_notice(runtime.clone(), id));
    }
    true
}

/// Frontend event telling the dashboard and host switcher to re-read the order.
const HOST_ORDER_CHANGED_EVENT: &str = "host-order-changed";

/// Adopts a host order from a paired host when it is well formed and newer
/// than the saved one. Returns whether the saved order changed.
fn apply_host_order_notice(
    settings: &mut AppSettings,
    order: Vec<String>,
    updated_at_ms: u64,
    now_ms: u64,
) -> bool {
    if updated_at_ms <= settings.host_order_updated_at_ms
        || !is_plausible_revision(updated_at_ms, now_ms)
        || !host_order::is_valid_shared_host_order(&order)
    {
        return false;
    }
    settings.host_order = order;
    settings.host_order_updated_at_ms = updated_at_ms;
    true
}

/// Whether a paired host whose order changed at `theirs_updated_at_ms` is
/// behind ours.
fn host_order_is_newer_than(settings: &AppSettings, theirs_updated_at_ms: u64) -> bool {
    !settings.host_order.is_empty() && settings.host_order_updated_at_ms > theirs_updated_at_ms
}

/// Minimum gap between exchanges of host names and order with paired hosts.
const HOST_LAYOUT_EXCHANGE_INTERVAL_MS: u64 = 30_000;
static LAST_HOST_LAYOUT_EXCHANGE_MS: AtomicU64 = AtomicU64::new(0);

/// Catches up with changes a host missed while it was offline. Called when the
/// app starts and when the dashboard refreshes, at most every 30 seconds.
#[tauri::command]
fn exchange_host_layout(state: State<'_, AppRuntime>, app: AppHandle) {
    retry_pending_notices(&state);
    exchange_host_layout_with_peers(&state, &app);
}

/// Asks every paired host for its host names and order, adopts whatever is
/// newer, and sends back whatever that host is missing. Runs in the background.
fn exchange_host_layout_with_peers(state: &AppRuntime, app: &AppHandle) {
    let now_ms = unix_time_ms();
    let last_ms = LAST_HOST_LAYOUT_EXCHANGE_MS.load(Ordering::Relaxed);
    if now_ms.saturating_sub(last_ms) < HOST_LAYOUT_EXCHANGE_INTERVAL_MS
        || LAST_HOST_LAYOUT_EXCHANGE_MS
            .compare_exchange(last_ms, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    let Ok(settings) = read_settings(state) else {
        return;
    };
    if !has_valid_shared_key(&settings.shared_key) {
        return;
    }
    let settings = Arc::new(settings);
    for index in 0..settings.peers.len() {
        let settings = Arc::clone(&settings);
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let peer = &settings.peers[index];
            let theirs = match request_peer(&settings, peer, AgentAction::Ping).await {
                Ok(response) => response,
                Err(error) => {
                    tracing::debug!(peer = peer.name.as_str(), error = %error, "paired host unavailable for host layout exchange");
                    return;
                }
            };
            let their_order_updated_at_ms = theirs.host_order_updated_at_ms;
            let their_aliases = theirs.host_aliases.clone();
            let their_appearances = theirs.host_appearances.clone();
            let their_labels = theirs.input_labels.clone();
            let their_identity_links = theirs.monitor_identity_links.clone();
            let their_host_inputs = theirs.host_inputs.clone();
            let their_id = peer.id.clone();
            let app_for_adopt = app.clone();
            let adopted = run_display_task(app_for_adopt.clone(), move |state| {
                let mut latest = read_settings(state)?;
                let now_ms = unix_time_ms();
                let mac_address_changed = adopt_peer_mac_address(&mut latest, &their_id, &theirs);
                let order_changed = apply_host_order_notice(
                    &mut latest,
                    theirs.host_order,
                    theirs.host_order_updated_at_ms,
                    now_ms,
                );
                let names_changed = adopt_host_aliases(&mut latest, &theirs.host_aliases, now_ms);
                let appearances_changed =
                    adopt_host_appearances(&mut latest, &theirs.host_appearances, now_ms);
                let labels_changed =
                    adopt_input_labels(&mut latest, &theirs.input_labels, now_ms);
                let identities_changed =
                    adopt_monitor_identities(&mut latest, &theirs.monitor_identity_links, now_ms);
                let host_inputs_update =
                    apply_host_input_updates(&mut latest, &theirs.host_inputs, now_ms);
                let host_inputs_accepted = host_inputs_update.is_some();
                let host_inputs_changed = host_inputs_update.unwrap_or(false);
                let latest = if order_changed
                    || names_changed
                    || appearances_changed
                    || labels_changed
                    || identities_changed
                    || host_inputs_accepted
                    || mac_address_changed
                {
                    store_settings(state, latest)?
                } else {
                    latest
                };
                for (changed, event) in [
                    (order_changed, HOST_ORDER_CHANGED_EVENT),
                    (names_changed, HOST_NAMES_CHANGED_EVENT),
                    (appearances_changed, HOST_APPEARANCES_CHANGED_EVENT),
                    (labels_changed, INPUT_LABELS_CHANGED_EVENT),
                    (identities_changed, MONITOR_IDENTITIES_CHANGED_EVENT),
                    (host_inputs_changed, PEER_INPUTS_CHANGED_EVENT),
                ] {
                    if changed {
                        if let Err(error) = app_for_adopt.emit(event, ()) {
                            tracing::warn!(error = %error, event, "unable to notify windows of a host layout change");
                        }
                    }
                }
                Ok(latest)
            })
            .await;
            let latest = match adopted {
                Ok(latest) => latest,
                Err(error) => {
                    tracing::warn!(peer = peer.name.as_str(), error = %error, "unable to adopt a paired host's host layout");
                    return;
                }
            };
            if host_order_is_newer_than(&latest, their_order_updated_at_ms) {
                let action = AgentAction::HostOrderChanged {
                    order: latest.host_order.clone(),
                    updated_at_ms: latest.host_order_updated_at_ms,
                };
                if let Err(error) = request_peer(&latest, peer, action).await {
                    tracing::info!(peer = peer.name.as_str(), error = %error, "paired host did not accept the host order");
                }
            }
            if host_alias::has_newer_entries(&latest.host_aliases, &their_aliases) {
                let action = AgentAction::HostAliasesChanged {
                    aliases: host_alias::shareable_aliases(&latest.host_aliases),
                };
                if let Err(error) = request_peer(&latest, peer, action).await {
                    tracing::info!(peer = peer.name.as_str(), error = %error, "paired host did not accept the host names");
                }
            }
            if host_appearance::has_newer_entries(&latest.host_appearances, &their_appearances) {
                let action = AgentAction::HostAppearancesChanged {
                    appearances: host_appearance::shareable_appearances(&latest.host_appearances),
                };
                if let Err(error) = request_peer(&latest, peer, action).await {
                    tracing::info!(peer = peer.name.as_str(), error = %error, "paired host did not accept the host icons and colours");
                }
            }
            if monitor_identity::needs_push(&latest.monitor_identity_links, &their_identity_links) {
                let action = AgentAction::MonitorIdentitiesChanged {
                    links: latest.monitor_identity_links.clone(),
                };
                if let Err(error) = request_peer(&latest, peer, action).await {
                    tracing::info!(peer = peer.name.as_str(), error = %error, "paired host did not accept the display identities");
                }
            }
            let their_labels = labels_on_local_monitors(&latest, &their_labels);
            if input_label::has_newer_entries(&latest.input_labels, &their_labels) {
                let action = AgentAction::InputLabelsChanged {
                    labels: input_label::shareable_labels(&latest.input_labels),
                };
                if let Err(error) = request_peer(&latest, peer, action).await {
                    tracing::info!(peer = peer.name.as_str(), error = %error, "paired host did not accept the input notes");
                }
            }
            if host_inputs_have_newer_entries(&latest, &their_host_inputs) {
                let action = AgentAction::HostInputsChanged {
                    assignments: latest.host_inputs.clone(),
                };
                if let Err(error) = request_peer(&latest, peer, action).await {
                    tracing::info!(peer = peer.name.as_str(), error = %error, "paired host did not accept the host input snapshot");
                }
            }
        });
    }
}

fn host_inputs_have_newer_entries(settings: &AppSettings, theirs: &[AgentHostInput]) -> bool {
    settings.host_inputs.iter().any(|ours| {
        theirs
            .iter()
            .find(|theirs| {
                theirs.host_id == ours.host_id
                    && (theirs.monitor.matches_exactly(&ours.monitor)
                        || shared_monitor_index_for_peer(
                            &settings.shared_monitors,
                            &settings.monitor_identity_links,
                            &theirs.monitor,
                        )
                        .is_some_and(|index| {
                            settings.shared_monitors[index]
                                .fingerprint
                                .matches_exactly(&ours.monitor)
                        }))
            })
            .map(|theirs| theirs.updated_at_ms < ours.updated_at_ms)
            .unwrap_or(true)
    })
}

fn peer_ids(settings: &AppSettings) -> Vec<&str> {
    settings.peers.iter().map(|peer| peer.id.as_str()).collect()
}

fn ordered_routes(state: &AppRuntime, settings: &AppSettings) -> Vec<String> {
    host_order::ordered_route_ids(
        &settings.host_order,
        &state.local_host_id,
        &peer_ids(settings),
    )
}

/// Route ids ("local" and peer ids) in the saved host card order.
#[tauri::command]
fn get_host_order(state: State<'_, AppRuntime>) -> Result<Vec<String>, String> {
    let settings = read_settings(&state)?;
    Ok(ordered_routes(&state, &settings))
}

/// Saves a new host card order from dashboard route ids and shares it with
/// every paired host. Returns the order as saved.
#[tauri::command]
fn set_host_order(
    route_ids: Vec<String>,
    state: State<'_, AppRuntime>,
    app: AppHandle,
) -> Result<Vec<String>, String> {
    let mut settings = read_settings(&state)?;
    let order = host_order::host_order_from_route_ids(
        &route_ids,
        &state.local_host_id,
        &peer_ids(&settings),
        &settings.host_order,
    )
    .ok_or_else(|| {
        ui_text(
            "主機清單已變更，請重新整理後再調整順序",
            "The host list changed. Refresh and reorder again.",
        )
        .to_owned()
    })?;
    // Never move backwards in time, or peers holding a newer order ignore this one.
    let updated_at_ms = unix_time_ms().max(settings.host_order_updated_at_ms + 1);
    settings.host_order = order.clone();
    settings.host_order_updated_at_ms = updated_at_ms;
    let settings = store_settings(&state, settings)?;
    if let Err(error) = app.emit(HOST_ORDER_CHANGED_EVENT, ()) {
        tracing::warn!(error = %error, "unable to notify the host switcher of a host order change");
    }
    broadcast_to_peers(
        &state,
        &AgentAction::HostOrderChanged {
            order,
            updated_at_ms,
        },
    );
    Ok(ordered_routes(&state, &settings))
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

async fn receive_host_order_notice(
    app: AppHandle,
    order: Vec<String>,
    updated_at_ms: u64,
) -> AgentResponse {
    let applied = tauri::async_runtime::spawn_blocking(move || {
        // Serialize with dashboard scans, which write back a settings snapshot.
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = app.state::<AppRuntime>();
        let mut settings = read_settings(&state)?;
        if apply_host_order_notice(&mut settings, order, updated_at_ms, unix_time_ms()) {
            store_settings(&state, settings)?;
            if let Err(error) = app.emit(HOST_ORDER_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify windows of a host order change");
            }
        }
        Ok::<(), String>(())
    })
    .await;
    agent_notice_response(applied)
}

/// Frontend event telling windows to re-read custom host names.
const HOST_NAMES_CHANGED_EVENT: &str = "host-names-changed";

/// Custom names by route id ("local" and peer ids); hosts without one are omitted.
fn route_host_names(state: &AppRuntime, settings: &AppSettings) -> HashMap<String, String> {
    std::iter::once((host_order::LOCAL_ROUTE_ID, state.local_host_id.as_str()))
        .chain(
            settings
                .peers
                .iter()
                .map(|peer| (peer.id.as_str(), peer.id.as_str())),
        )
        .filter_map(|(route, host_id)| {
            host_alias::alias_for(&settings.host_aliases, host_id)
                .map(|name| (route.to_owned(), name.to_owned()))
        })
        .collect()
}

#[tauri::command]
fn get_host_names(state: State<'_, AppRuntime>) -> Result<HashMap<String, String>, String> {
    let settings = read_settings(&state)?;
    Ok(route_host_names(&state, &settings))
}

/// Gives the host behind `route_id` a custom name (empty restores the default)
/// and shares every custom name with paired hosts. Returns names by route id.
#[tauri::command]
fn set_host_name(
    route_id: String,
    name: String,
    state: State<'_, AppRuntime>,
    app: AppHandle,
) -> Result<HashMap<String, String>, String> {
    let mut settings = read_settings(&state)?;
    let host_id = if route_id == host_order::LOCAL_ROUTE_ID {
        state.local_host_id.clone()
    } else {
        settings
            .peers
            .iter()
            .find(|peer| peer.id == route_id)
            .map(|peer| peer.id.clone())
            .ok_or_else(|| {
                ui_text(
                    "找不到這台主機，請重新整理後再試一次",
                    "This host was not found. Refresh and try again.",
                )
                .to_owned()
            })?
    };
    let name = host_alias::normalize_alias(&name).map_err(alias_error_text)?;
    settings.host_aliases =
        host_alias::with_alias(&settings.host_aliases, &host_id, name, unix_time_ms());
    let settings = store_settings(&state, settings)?;
    if host_id == state.local_host_id {
        // Machines that have not paired with this one list it by what it
        // advertises, so a rename that stops here is invisible to them.
        let advertised = host_alias::alias_for(&settings.host_aliases, &host_id)
            .unwrap_or(state.local_host_name.as_str());
        if let Some(discovery) = state.discovery.get() {
            if let Err(error) = discovery.advertise_name(advertised) {
                tracing::warn!(error = %error, "unable to advertise this computer's new name");
            }
        }
    }
    if let Err(error) = app.emit(HOST_NAMES_CHANGED_EVENT, ()) {
        tracing::warn!(error = %error, "unable to notify the host switcher of a host name change");
    }
    broadcast_to_peers(
        &state,
        &AgentAction::HostAliasesChanged {
            aliases: host_alias::shareable_aliases(&settings.host_aliases),
        },
    );
    Ok(route_host_names(&state, &settings))
}

fn alias_error_text(error: host_alias::AliasError) -> String {
    match error {
        host_alias::AliasError::TooLong => match UiLocale::current() {
            UiLocale::TraditionalChinese => {
                format!("主機名稱最多 {} 個字", host_alias::MAX_ALIAS_CHARS)
            }
            UiLocale::English => format!(
                "Host names can be at most {} characters",
                host_alias::MAX_ALIAS_CHARS
            ),
        },
        host_alias::AliasError::ControlCharacter => ui_text(
            "主機名稱不能包含換行或控制字元",
            "Host names cannot contain line breaks or control characters",
        )
        .to_owned(),
    }
}

/// Frontend event telling windows to re-read custom host icons and colours.
const HOST_APPEARANCES_CHANGED_EVENT: &str = "host-appearances-changed";

/// A host's custom icon and colour as the frontend reads them; `None` keeps
/// the default.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RouteAppearance {
    icon: Option<String>,
    color: Option<String>,
}

/// The discovery id behind a route id ("local" or a peer id), if it is a host
/// this computer knows.
fn host_id_for_route(state: &AppRuntime, settings: &AppSettings, route_id: &str) -> Option<String> {
    if route_id == host_order::LOCAL_ROUTE_ID {
        return Some(state.local_host_id.clone());
    }
    settings
        .peers
        .iter()
        .find(|peer| peer.id == route_id)
        .map(|peer| peer.id.clone())
}

/// Custom looks by route id; hosts left at their default are omitted.
fn route_host_appearances(
    state: &AppRuntime,
    settings: &AppSettings,
) -> HashMap<String, RouteAppearance> {
    std::iter::once((host_order::LOCAL_ROUTE_ID, state.local_host_id.as_str()))
        .chain(
            settings
                .peers
                .iter()
                .map(|peer| (peer.id.as_str(), peer.id.as_str())),
        )
        .filter_map(|(route, host_id)| {
            match host_appearance::appearance_for(&settings.host_appearances, host_id) {
                (None, None) => None,
                (icon, color) => Some((
                    route.to_owned(),
                    RouteAppearance {
                        icon: icon.map(str::to_owned),
                        color: color.map(str::to_owned),
                    },
                )),
            }
        })
        .collect()
}

#[tauri::command]
fn get_host_appearances(
    state: State<'_, AppRuntime>,
) -> Result<HashMap<String, RouteAppearance>, String> {
    let settings = read_settings(&state)?;
    Ok(route_host_appearances(&state, &settings))
}

/// Gives the host behind `route_id` an icon and colour (empty restores the
/// default) and shares every custom look with paired hosts. Returns looks by
/// route id.
#[tauri::command]
fn set_host_appearance(
    route_id: String,
    icon: String,
    color: String,
    state: State<'_, AppRuntime>,
    app: AppHandle,
) -> Result<HashMap<String, RouteAppearance>, String> {
    let mut settings = read_settings(&state)?;
    let host_id = host_id_for_route(&state, &settings, &route_id).ok_or_else(|| {
        ui_text(
            "找不到這台主機，請重新整理後再試一次",
            "This host was not found. Refresh and try again.",
        )
        .to_owned()
    })?;
    host_appearance::validate(&icon, &color).map_err(|_| {
        ui_text(
            "這個圖示或顏色無法使用",
            "This icon or colour is not available",
        )
        .to_owned()
    })?;
    settings.host_appearances = host_appearance::with_appearance(
        &settings.host_appearances,
        &host_id,
        icon,
        color,
        unix_time_ms(),
    );
    let settings = store_settings(&state, settings)?;
    if let Err(error) = app.emit(HOST_APPEARANCES_CHANGED_EVENT, ()) {
        tracing::warn!(error = %error, "unable to notify windows of a host icon change");
    }
    broadcast_to_peers(
        &state,
        &AgentAction::HostAppearancesChanged {
            appearances: host_appearance::shareable_appearances(&settings.host_appearances),
        },
    );
    Ok(route_host_appearances(&state, &settings))
}

async fn receive_host_appearances_notice(
    app: AppHandle,
    appearances: Vec<HostAppearance>,
) -> AgentResponse {
    let applied = tauri::async_runtime::spawn_blocking(move || {
        // Serialize with dashboard scans, which write back a settings snapshot.
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = app.state::<AppRuntime>();
        let mut settings = read_settings(&state)?;
        if adopt_host_appearances(&mut settings, &appearances, unix_time_ms()) {
            store_settings(&state, settings)?;
            if let Err(error) = app.emit(HOST_APPEARANCES_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify windows of a host icon change");
            }
        }
        Ok::<(), String>(())
    })
    .await;
    agent_notice_response(applied)
}

/// Merges a paired host's custom host icons and colours into `settings`.
/// Returns whether any changed.
fn adopt_host_appearances(
    settings: &mut AppSettings,
    incoming: &[HostAppearance],
    now_ms: u64,
) -> bool {
    // Looks only for hosts this one knows, like `adopt_host_aliases`.
    let is_known = |host_id: &str| {
        host_id == settings.local_host_id || settings.peers.iter().any(|peer| peer.id == host_id)
    };
    let incoming: Vec<HostAppearance> = incoming
        .iter()
        .filter(|entry| is_known(&entry.host_id))
        .filter(|entry| is_plausible_revision(entry.updated_at_ms, now_ms))
        .cloned()
        .collect();
    match host_appearance::merged_appearances(&settings.host_appearances, &incoming) {
        Some(merged) => {
            settings.host_appearances = merged;
            true
        }
        None => false,
    }
}

async fn receive_host_aliases_notice(app: AppHandle, aliases: Vec<HostAlias>) -> AgentResponse {
    let applied = tauri::async_runtime::spawn_blocking(move || {
        // Serialize with dashboard scans, which write back a settings snapshot.
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = app.state::<AppRuntime>();
        let mut settings = read_settings(&state)?;
        if adopt_host_aliases(&mut settings, &aliases, unix_time_ms()) {
            store_settings(&state, settings)?;
            if let Err(error) = app.emit(HOST_NAMES_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify windows of a host name change");
            }
        }
        Ok::<(), String>(())
    })
    .await;
    agent_notice_response(applied)
}

/// Frontend event telling windows to re-read input names and notes.
const INPUT_LABELS_CHANGED_EVENT: &str = "input-labels-changed";
const MONITOR_IDENTITIES_CHANGED_EVENT: &str = "monitor-identities-changed";

/// Merges a paired host's display-identity claims into ours, keeping the newer
/// entry for each alias. Returns whether anything changed.
fn adopt_monitor_identities(
    settings: &mut AppSettings,
    incoming: &[MonitorIdentityLink],
    now_ms: u64,
) -> bool {
    let incoming: Vec<MonitorIdentityLink> = incoming
        .iter()
        .filter(|link| is_plausible_revision(link.updated_at_ms, now_ms))
        .cloned()
        .collect();
    match monitor_identity::merged_links(&settings.monitor_identity_links, &incoming) {
        Some(merged) => {
            settings.monitor_identity_links = merged;
            true
        }
        None => false,
    }
}

/// `labels` from a paired host with each display mapped to this host's shared
/// display, since hosts can read the same display's serial number differently.
fn labels_on_local_monitors(settings: &AppSettings, labels: &[InputLabel]) -> Vec<InputLabel> {
    input_label::with_local_monitors(labels, |monitor| {
        shared_monitor_index_for_peer(
            &settings.shared_monitors,
            &settings.monitor_identity_links,
            monitor,
        )
        .map(|index| settings.shared_monitors[index].fingerprint.clone())
    })
}

/// Merges a paired host's custom host names into `settings`. Returns whether
/// any changed.
fn adopt_host_aliases(settings: &mut AppSettings, incoming: &[HostAlias], now_ms: u64) -> bool {
    // Names only hosts this one knows: each is stored and shared on, so a name
    // for any other id would only grow the settings.
    let is_known = |host_id: &str| {
        host_id == settings.local_host_id || settings.peers.iter().any(|peer| peer.id == host_id)
    };
    let incoming: Vec<HostAlias> = incoming
        .iter()
        .filter(|alias| is_known(&alias.host_id))
        .filter(|alias| is_plausible_revision(alias.updated_at_ms, now_ms))
        .cloned()
        .collect();
    match host_alias::merged_aliases(&settings.host_aliases, &incoming) {
        Some(merged) => {
            settings.host_aliases = merged;
            true
        }
        None => false,
    }
}

/// Merges a paired host's input notes into `settings`. Returns whether any changed.
fn adopt_input_labels(settings: &mut AppSettings, incoming: &[InputLabel], now_ms: u64) -> bool {
    let incoming: Vec<InputLabel> = labels_on_local_monitors(settings, incoming)
        .into_iter()
        .filter(|label| is_plausible_revision(label.updated_at_ms, now_ms))
        .collect();
    match input_label::merged_labels(&settings.input_labels, &incoming) {
        Some(merged) => {
            settings.input_labels = merged;
            true
        }
        None => false,
    }
}

/// Moves everything keyed by `alias` onto `primary`, so a merge keeps the
/// inputs and notes the user already set against the identity being absorbed.
/// Entries already held for `primary` win; nothing is overwritten.
fn adopt_alias_settings(
    settings: &mut AppSettings,
    alias: &MonitorFingerprint,
    primary: &MonitorFingerprint,
) {
    for peer in &mut settings.peers {
        if let Some(input) = peer.input_for(alias) {
            if peer.input_for(primary).is_none() {
                peer.set_input_for(primary, Some(input));
            }
            peer.set_input_for(alias, None);
        }
    }
    let moved_inputs = settings
        .host_inputs
        .iter()
        .filter(|entry| entry.monitor.matches_exactly(alias))
        .cloned()
        .collect::<Vec<_>>();
    settings
        .host_inputs
        .retain(|entry| !entry.monitor.matches_exactly(alias));
    for mut moved in moved_inputs {
        let primary_is_newer = settings.host_inputs.iter().any(|entry| {
            entry.host_id == moved.host_id
                && entry.monitor.matches_exactly(primary)
                && entry.updated_at_ms >= moved.updated_at_ms
        });
        if !primary_is_newer {
            settings.host_inputs.retain(|entry| {
                entry.host_id != moved.host_id || !entry.monitor.matches_exactly(primary)
            });
            moved.monitor = primary.clone();
            settings.host_inputs.push(moved);
        }
    }
    let now = unix_time_ms();
    let moved = settings
        .input_labels
        .iter()
        .filter(|entry| entry.monitor.matches_exactly(alias))
        .cloned()
        .collect::<Vec<_>>();
    for entry in moved {
        if input_label::label_for(&settings.input_labels, primary, entry.input).is_none() {
            settings.input_labels = input_label::with_label(
                &settings.input_labels,
                primary,
                entry.input,
                entry.label.clone(),
                now,
            );
        }
        settings.input_labels = input_label::with_label(
            &settings.input_labels,
            alias,
            entry.input,
            String::new(),
            now,
        );
    }
}

/// The fingerprint behind an id the UI holds. A display the user wants to merge
/// is usually not a shared display at all — it is the unfamiliar one that
/// appeared when the display mode changed — so a display present right now is
/// accepted by its platform id as well.
fn fingerprint_for_ui_id(
    settings: &AppSettings,
    monitor_id: &str,
) -> Result<MonitorFingerprint, String> {
    if let Ok(selected) = find_shared_monitor(settings, monitor_id) {
        return Ok(selected.fingerprint.clone());
    }
    // Withdrawing a claim must work when neither identity is shared or present
    // any more, which is the state a stale claim leaves behind.
    if let Some(link) = settings
        .monitor_identity_links
        .iter()
        .find(|link| monitor_key(&link.alias) == monitor_id)
    {
        return Ok(link.alias.clone());
    }
    platform_controller()
        .and_then(|controller| controller.enumerate())
        .map_err(core_user_error)?
        .into_iter()
        .find(|monitor| monitor.id.as_str() == monitor_id)
        .map(|monitor| monitor.fingerprint)
        .ok_or_else(display_not_found)
}

/// Declares that the display `alias_id` is the same physical display as the
/// shared display `primary_id`, or withdraws that claim when `primary_id` is
/// `None`.
///
/// Some displays publish a different EDID product code per display mode, which
/// reads as a different display on every host at once. Only the user can say
/// the two are one panel, so this records that claim, folds the absorbed
/// display's own settings into the one it joins, and shares the claim with
/// paired hosts. Switching still demands an exact fingerprint match against a
/// display present right now, so a claim never widens what may be written to.
#[tauri::command]
fn set_monitor_identity_link(
    alias_id: String,
    primary_id: Option<String>,
    state: State<'_, AppRuntime>,
    app: AppHandle,
) -> Result<AppSettings, String> {
    let mut settings = read_settings(&state)?;
    let alias = fingerprint_for_ui_id(&settings, &alias_id)?;
    let primary = match primary_id {
        Some(primary_id) => Some(
            find_shared_monitor(&settings, &primary_id)?
                .fingerprint
                .clone(),
        ),
        None => None,
    };
    if primary
        .as_ref()
        .is_some_and(|primary| primary.matches_exactly(&alias))
    {
        return Err(ui_text(
            "無法把螢幕合併到自己",
            "A display cannot be merged into itself",
        )
        .to_owned());
    }

    settings.monitor_identity_links = monitor_identity::with_link(
        &settings.monitor_identity_links,
        &alias,
        primary.as_ref(),
        unix_time_ms(),
    );
    if let Some(primary) = primary.as_ref() {
        adopt_alias_settings(&mut settings, &alias, primary);
        // The absorbed identity is no longer its own shared display; it is
        // reached through the display it joined.
        settings
            .shared_monitors
            .retain(|selected| !selected.fingerprint.matches_exactly(&alias));
    }
    let settings = store_settings(&state, settings)?;
    if let Err(error) = app.emit(MONITOR_IDENTITIES_CHANGED_EVENT, ()) {
        tracing::warn!(error = %error, "unable to notify windows of a display identity change");
    }
    broadcast_to_peers(
        &state,
        &AgentAction::MonitorIdentitiesChanged {
            links: settings.monitor_identity_links.clone(),
        },
    );
    Ok(settings)
}

/// Sets which input this computer occupies on a shared display, or clears it.
///
/// Reading it can only get so far: VCP 0x60 says what a display is showing,
/// never which port the reader is plugged into, so a host that has never been
/// on screen cannot learn its own port and a display with a vendor-specific
/// input may never be read confidently. The user can always see which cable
/// they used.
///
/// A port set here is announced to paired hosts exactly like a detected one,
/// so correcting it on one computer corrects what every other computer will
/// switch to.
#[tauri::command]
async fn set_local_input(
    monitor_id: String,
    input: Option<u32>,
    app: AppHandle,
) -> Result<AppSettings, String> {
    let settings = run_display_task(app.clone(), move |state| {
        let mut settings = read_settings(state)?;
        let fingerprint = find_shared_monitor(&settings, &monitor_id)?
            .fingerprint
            .clone();
        let input = match input {
            Some(value) => Some(DisplayInput::new(value).map_err(core_user_error)?),
            None => None,
        };
        let Some(selected) = settings
            .shared_monitors
            .iter_mut()
            .find(|selected| selected.fingerprint.matches_exactly(&fingerprint))
        else {
            return Err(display_not_found());
        };
        selected.local_input = input;
        let host_id = state.local_host_id.clone();
        record_host_input_update(&mut settings, &host_id, &fingerprint, input, unix_time_ms());
        store_settings(state, settings)
    })
    .await?;
    if let Err(error) = app.emit(PEER_INPUTS_CHANGED_EVENT, ()) {
        tracing::warn!(error = %error, "unable to notify windows of an input change");
    }
    if let Some(state) = app.try_state::<AppRuntime>() {
        broadcast_to_peers(
            &state,
            &AgentAction::HostInputsChanged {
                assignments: settings.host_inputs.clone(),
            },
        );
        announce_confirmed_local_inputs(&state, &settings);
    }
    Ok(settings)
}

/// Sets the note for one input of a shared display (empty clears it) and
/// shares every note with paired hosts. Returns that display's input options.
#[tauri::command]
fn set_input_label(
    monitor_id: String,
    input: u32,
    label: String,
    state: State<'_, AppRuntime>,
    app: AppHandle,
) -> Result<Vec<InputOption>, String> {
    let mut settings = read_settings(&state)?;
    let fingerprint = find_shared_monitor(&settings, &monitor_id)?
        .fingerprint
        .clone();
    let input = DisplayInput::new(input).map_err(core_user_error)?;
    let label = input_label::normalize_label(&label).map_err(label_error_text)?;
    settings.input_labels = input_label::with_label(
        &settings.input_labels,
        &fingerprint,
        input,
        label,
        unix_time_ms(),
    );
    let settings = store_settings(&state, settings)?;
    if let Err(error) = app.emit(INPUT_LABELS_CHANGED_EVENT, ()) {
        tracing::warn!(error = %error, "unable to notify windows of an input note change");
    }
    broadcast_to_peers(
        &state,
        &AgentAction::InputLabelsChanged {
            labels: input_label::shareable_labels(&settings.input_labels),
        },
    );
    input_options(&settings, &monitor_id)
}

fn label_error_text(error: input_label::LabelError) -> String {
    match error {
        input_label::LabelError::TooLong => match UiLocale::current() {
            UiLocale::TraditionalChinese => {
                format!("輸入備註最多 {} 個字", input_label::MAX_LABEL_CHARS)
            }
            UiLocale::English => format!(
                "Input notes can be at most {} characters",
                input_label::MAX_LABEL_CHARS
            ),
        },
        input_label::LabelError::ControlCharacter => ui_text(
            "輸入備註不能包含換行或控制字元",
            "Input notes cannot contain line breaks or control characters",
        )
        .to_owned(),
    }
}

/// Sends one shared display out through a paired host's input and straight
/// back to this computer's, so it re-establishes the link — and a built-in
/// USB hub or KVM, which follows the active input rather than the panel,
/// re-binds along with it.
///
/// The way back is the whole problem. A display answers DDC/CI on the input
/// it is showing and no other, so the moment this computer leaves the screen
/// it cannot write to that display at all: the return has to be made by the
/// host that is on screen by then. That is why the only input worth going out
/// through belongs to a paired host, why that host's agent has to answer
/// immediately before the display is sent anywhere, and why this refuses
/// rather than strand a display where nothing can recall it from.
#[tauri::command]
async fn resync_display_input(
    monitor_id: String,
    app: AppHandle,
    state: State<'_, AppRuntime>,
) -> Result<OperationResult, String> {
    let settings = read_settings(&state)?;
    let selected = find_shared_monitor(&settings, &monitor_id)?.clone();
    let expected = selected.local_input.ok_or_else(|| {
        ui_text(
            "尚未設定這台電腦使用的螢幕輸入",
            "The display input for this computer is not configured",
        )
        .to_owned()
    })?;
    let partners = resync_partners(&settings, &selected);
    if partners.is_empty() {
        return Err(match UiLocale::current() {
            UiLocale::TraditionalChinese => format!(
                "{} 沒有可以把畫面送回來的主機。畫面離開這台電腦之後，只有當下在畫面上的那台主機能把它切回來，所以這需要另一台已配對、而且在這台螢幕上設定了輸入的主機。",
                selected.name
            ),
            UiLocale::English => format!(
                "Nothing could bring {} back. Once the display leaves this computer, only the host it is then showing can switch it back, so this needs a paired host with an input set on this display.",
                selected.name
            ),
        });
    }

    // Asked right now rather than read from the last scan: the display is
    // about to be sent somewhere only that host can recall it from.
    let mut silent = Vec::new();
    let mut ready = None;
    for (peer, input) in &partners {
        match request_peer(&settings, peer, AgentAction::Ping).await {
            Ok(_) => {
                ready = Some((peer.clone(), *input));
                break;
            }
            Err(detail) => silent.push(format!("{}（{detail}）", peer.name)),
        }
    }
    let Some((peer, via)) = ready else {
        let silent = silent.join("；");
        return Err(match UiLocale::current() {
            UiLocale::TraditionalChinese => format!(
                "沒有主機可以把畫面切回來，所以沒有動 {}。{silent}",
                selected.name
            ),
            UiLocale::English => format!(
                "No host could switch the display back, so {} was left alone. {silent}",
                selected.name
            ),
        });
    };

    // Out. This computer is the one on screen, so this is the one write it
    // can still make.
    run_display_task(app.clone(), {
        let settings = settings.clone();
        let selected = selected.clone();
        move |_state| leave_for(&settings, &selected, expected, via)
    })
    .await?;
    // The display is that host's until it comes back, and every window says
    // which host a display is on. A return that fails must not leave them all
    // claiming this computer.
    record_active_route(&state, &selected.fingerprint, &peer.id)?;
    sleep(RESYNC_DWELL).await;

    // Back. The host now on screen is the one that can still write to the
    // display, so it is asked first rather than as a fallback. This
    // computer's own write proves nothing here: the platform controller
    // trusts a write whose read-back it cannot take, and it cannot take one
    // while the display is showing somebody else — which is exactly when
    // this runs.
    let asked = request_return(&settings, &peer, &selected, expected).await;
    sleep(RETURN_SETTLE).await;
    let mut back = confirmed_back(&app, &settings, &selected, expected).await;
    if !back {
        // Last resort: the displays that do answer an input they are not
        // showing, and the host that went away in the last two seconds.
        back = nudged_back(&app, &settings, &selected, expected).await;
    }
    if !back {
        return match asked {
            Err(detail) => Err(stranded_text(&selected, &peer, via, expected, &detail)),
            Ok(()) => Ok(OperationResult {
                title: match UiLocale::current() {
                    UiLocale::TraditionalChinese => format!("已請 {} 把畫面切回來", peer.name),
                    UiLocale::English => format!("{} was asked to switch it back", peer.name),
                },
                detail: stranded_text(
                    &selected,
                    &peer,
                    via,
                    expected,
                    ui_text(
                        "這台電腦還讀不到畫面已經回來",
                        "this computer still cannot read the display as back",
                    ),
                ),
                peer_woken: false,
                warning: true,
            }),
        };
    }
    record_active_route(&state, &selected.fingerprint, "local")?;
    announce_active_input(&state, &selected.fingerprint, expected);
    if let Err(error) = app.emit(ACTIVE_ROUTE_CHANGED_EVENT, ()) {
        tracing::warn!(error = %error, "unable to notify windows of a re-seated display");
    }

    let port = noted_input_label(&settings, &selected, expected);
    Ok(OperationResult {
        title: ui_text("已重新送出訊號", "Signal re-seated").to_owned(),
        detail: match UiLocale::current() {
            UiLocale::TraditionalChinese => format!(
                "{} 已切到 {}（{}）再切回 {port}。",
                selected.name,
                peer.name,
                noted_input_label(&settings, &selected, via)
            ),
            UiLocale::English => format!(
                "{} went to {} ({}) and back to {port}.",
                selected.name,
                peer.name,
                noted_input_label(&settings, &selected, via)
            ),
        },
        peer_woken: false,
        warning: false,
    })
}

/// How long the display is left on the other host's input. It has to lock
/// onto that signal for leaving it again to mean anything to its own USB —
/// and its DDC/CI bus has to settle before that host is asked to move it
/// again, because a display that has just changed input answers badly.
const RESYNC_DWELL: Duration = Duration::from_millis(2_500);

/// The paired hosts that could bring this display back, with the input each
/// one sits on. A host with no input here cannot be switched to at all, and
/// one on the same input as this computer is no way out of it.
fn resync_partners(
    settings: &AppSettings,
    selected: &SelectedMonitor,
) -> Vec<(HostRoute, DisplayInput)> {
    settings
        .peers
        .iter()
        .filter_map(|peer| {
            let input = peer.input_for(&selected.fingerprint)?;
            (Some(input) != selected.local_input).then(|| (peer.clone(), input))
        })
        .collect()
}

/// Sends the display to `via`, refusing unless this computer is what it is
/// showing: a display showing somebody else is somebody else's picture to
/// move, and this computer would not be the one that could move it back.
fn leave_for(
    settings: &AppSettings,
    selected: &SelectedMonitor,
    expected: DisplayInput,
    via: DisplayInput,
) -> Result<(), String> {
    let (controller, target) = live_display(settings, selected)?;
    let current = controller.read_input(&target).map_err(core_user_error)?;
    if current != expected {
        return Err(match UiLocale::current() {
            UiLocale::TraditionalChinese => format!(
                "{} 目前顯示的是 {}，不是這台電腦的輸入。請先切換回這台電腦再重新送出訊號。",
                selected.name,
                noted_input_label(settings, selected, current)
            ),
            UiLocale::English => format!(
                "{} is showing {}, not this computer's input. Switch it back to this computer before re-seating the signal.",
                selected.name,
                noted_input_label(settings, selected, current)
            ),
        });
    }
    controller
        .write_input(&target, via)
        .map_err(core_user_error)
}

/// How long the display is given to settle before it is read back.
const RETURN_SETTLE: Duration = Duration::from_millis(900);

/// How many times the host on screen is asked before the display is called
/// stranded. A display that has just changed input answers DDC/CI badly for
/// a few seconds — Windows reports `ERROR_GRAPHICS_DDCCI_INVALID_MESSAGE_COMMAND`
/// and the host's read fails before it ever writes, observed on an MSI MPG
/// 274U — and this ask is the only thing that brings the picture back, so one
/// bad moment must not be the end of it.
const RETURN_ATTEMPTS: u32 = 3;
const RETURN_RETRY_DELAY: Duration = Duration::from_millis(1_500);

/// Asks the host now on screen to put the display back on this computer's
/// input. It is the only host that can: a display answers DDC/CI on the input
/// it is showing and no other.
async fn request_return(
    settings: &AppSettings,
    peer: &HostRoute,
    selected: &SelectedMonitor,
    expected: DisplayInput,
) -> Result<(), String> {
    let mut refusal = ui_text("沒有送出要求", "the request was never sent").to_owned();
    for attempt in 0..RETURN_ATTEMPTS {
        match ask_to_switch(settings, peer, selected, expected).await {
            Ok(()) => return Ok(()),
            Err(detail) => {
                tracing::info!(
                    peer = peer.name.as_str(),
                    attempt = attempt + 1,
                    error = %detail,
                    "the host on screen could not switch the display back yet"
                );
                refusal = detail;
                if attempt + 1 < RETURN_ATTEMPTS {
                    sleep(RETURN_RETRY_DELAY).await;
                }
            }
        }
    }
    Err(refusal)
}

async fn ask_to_switch(
    settings: &AppSettings,
    peer: &HostRoute,
    selected: &SelectedMonitor,
    expected: DisplayInput,
) -> Result<(), String> {
    let monitor = resolve_switch_monitor_field(settings, peer, &selected.fingerprint).await?;
    request_peer(
        settings,
        peer,
        AgentAction::SwitchInput {
            monitor,
            input: expected,
        },
    )
    .await?;
    Ok(())
}

/// Whether the display can be read from here *and* reads as this computer's
/// input. This is the one place where "could not tell" must not be taken for
/// "it came back": a display this computer cannot read is, as a rule, a
/// display that is showing somebody else.
async fn confirmed_back(
    app: &AppHandle,
    settings: &AppSettings,
    selected: &SelectedMonitor,
    expected: DisplayInput,
) -> bool {
    let settings = settings.clone();
    let selected = selected.clone();
    run_display_task(app.clone(), move |_state| {
        let (controller, target) = live_display(&settings, &selected)?;
        Ok(matches!(controller.read_input(&target), Ok(current) if current == expected))
    })
    .await
    .unwrap_or(false)
}

/// One write from this computer, then the same clean-reading test. Only the
/// displays that answer an input they are not showing can be moved this way,
/// which is why it runs after the host on screen has already been asked.
async fn nudged_back(
    app: &AppHandle,
    settings: &AppSettings,
    selected: &SelectedMonitor,
    expected: DisplayInput,
) -> bool {
    let settings = settings.clone();
    let selected = selected.clone();
    run_display_task(app.clone(), move |_state| {
        let (controller, target) = live_display(&settings, &selected)?;
        let _ = controller.write_input(&target, expected);
        thread::sleep(RETURN_SETTLE);
        Ok(matches!(controller.read_input(&target), Ok(current) if current == expected))
    })
    .await
    .unwrap_or(false)
}

/// The display is on the other host's input and neither host could move it.
/// Whoever reads this is looking at the wrong computer, so it names the two
/// ways out rather than describing the fault.
fn stranded_text(
    selected: &SelectedMonitor,
    peer: &HostRoute,
    via: DisplayInput,
    expected: DisplayInput,
    detail: &str,
) -> String {
    let label = |input| input_label(selected.vendor_indexed_inputs, input);
    match UiLocale::current() {
        UiLocale::TraditionalChinese => format!(
            "{} 已切到 {}（{}），而且切不回 {}。請用螢幕上的按鍵切回來，或在 {} 上用 MuxSU 切回這台電腦。（{detail}）",
            selected.name,
            peer.name,
            label(via),
            label(expected),
            peer.name
        ),
        UiLocale::English => format!(
            "{} went to {} ({}) and would not come back to {}. Use the display's own buttons, or switch it back to this computer from MuxSU on {}. ({detail})",
            selected.name,
            peer.name,
            label(via),
            label(expected),
            peer.name
        ),
    }
}

/// Reads one shared display's inputs again from the display itself, replacing
/// what this computer learned before.
///
/// The stored list is only let go of once a fresh one has been read. A display
/// answers the host it is showing and no other, so a reading taken while it is
/// elsewhere says nothing — and what is stored may have come from the paired
/// host that could read the display when this one could not.
#[tauri::command]
async fn redetect_display(monitor_id: String, app: AppHandle) -> Result<OperationResult, String> {
    let result = run_display_task(app.clone(), move |state| {
        let mut settings = read_settings(state)?;
        let selected = find_shared_monitor(&settings, &monitor_id)?.clone();
        let controller = platform_controller().map_err(core_user_error)?;
        let present = controller.enumerate().map_err(core_user_error)?;
        let monitor = present_display(&settings, &selected, &present)?.clone();

        let mut probe = selected.clone();
        probe.supported_inputs = None;
        probe.vendor_indexed_inputs = false;
        refresh_selected_input_data(&controller, &monitor, &mut probe).map_err(core_user_error)?;
        let read_a_list = probe.supported_inputs.is_some();
        if !read_a_list {
            probe.supported_inputs = selected.supported_inputs.clone();
            probe.vendor_indexed_inputs = selected.vendor_indexed_inputs;
        }
        let local_input = probe.local_input;
        let count = probe.supported_inputs.as_ref().map_or(0, Vec::len);

        let Some(stored) = settings
            .shared_monitors
            .iter_mut()
            .find(|stored| stored.fingerprint.matches_exactly(&selected.fingerprint))
        else {
            return Err(display_not_found());
        };
        let inputs_changed = stored.supported_inputs != probe.supported_inputs;
        *stored = probe;
        let settings = store_settings(state, settings)?;
        // Paired hosts are told a display's input list once. A re-detection
        // that read a different list makes what they were told wrong, so let
        // the next scan tell them again.
        if inputs_changed {
            if let Ok(mut announced) = state.announced_input_lists.lock() {
                announced.remove(&monitor_key(&selected.fingerprint));
            }
        }

        let port = local_input.map_or_else(
            || ui_text("尚未確認", "not confirmed yet").to_owned(),
            |input| noted_input_label(&settings, &selected, input),
        );
        Ok(if read_a_list {
            OperationResult {
                title: ui_text("已重新偵測", "Re-detected").to_owned(),
                detail: match UiLocale::current() {
                    UiLocale::TraditionalChinese => {
                        format!("{} 回報 {count} 個輸入，這台電腦的輸入為 {port}。", selected.name)
                    }
                    UiLocale::English => format!(
                        "{} reported {count} inputs; this computer's input is {port}.",
                        selected.name
                    ),
                },
                peer_woken: false,
                warning: false,
            }
        } else {
            OperationResult {
                title: ui_text("讀不到輸入清單", "No input list was read").to_owned(),
                detail: match UiLocale::current() {
                    UiLocale::TraditionalChinese => format!(
                        "{} 這次沒有回報它接受的輸入，先前的設定已保留。螢幕顯示其他電腦時通常讀不到。",
                        selected.name
                    ),
                    UiLocale::English => format!(
                        "{} did not report the inputs it accepts this time, so what was stored is kept. A display showing another computer usually cannot be read.",
                        selected.name
                    ),
                },
                peer_woken: false,
                warning: true,
            }
        })
    })
    .await?;
    if let Err(error) = app.emit(INPUT_LABELS_CHANGED_EVENT, ()) {
        tracing::warn!(error = %error, "unable to notify windows of a re-detected display");
    }
    Ok(result)
}

/// The display present right now that a maintenance action on this shared
/// display may act on, with the controller that reaches it. Picked by exactly
/// the rule a switch uses, so maintenance can never reach a display the user
/// did not name.
fn live_display(
    settings: &AppSettings,
    selected: &SelectedMonitor,
) -> Result<(impl MonitorControl, MonitorId), String> {
    let controller = platform_controller().map_err(core_user_error)?;
    let present = controller.enumerate().map_err(core_user_error)?;
    let id = present_display(settings, selected, &present)?.id.clone();
    Ok((controller, id))
}

fn present_display<'a>(
    settings: &AppSettings,
    selected: &SelectedMonitor,
    present: &'a [MonitorDescriptor],
) -> Result<&'a MonitorDescriptor, String> {
    let identities =
        monitor_identity::identities_for(&settings.monitor_identity_links, &selected.fingerprint);
    let fingerprint = switch_target(&identities, present).map_err(core_user_error)?;
    present
        .iter()
        .find(|monitor| fingerprint.matches_exactly(&monitor.fingerprint))
        .ok_or_else(display_not_found)
}

/// Every input of this display that a host here is known to use.
async fn receive_input_labels_notice(app: AppHandle, labels: Vec<InputLabel>) -> AgentResponse {
    let applied = tauri::async_runtime::spawn_blocking(move || {
        // Serialize with dashboard scans, which write back a settings snapshot.
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = app.state::<AppRuntime>();
        let mut settings = read_settings(&state)?;
        if adopt_input_labels(&mut settings, &labels, unix_time_ms()) {
            store_settings(&state, settings)?;
            if let Err(error) = app.emit(INPUT_LABELS_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify windows of an input note change");
            }
        }
        Ok::<(), String>(())
    })
    .await;
    agent_notice_response(applied)
}

async fn receive_monitor_identities_notice(
    app: AppHandle,
    links: Vec<MonitorIdentityLink>,
) -> AgentResponse {
    let applied = tauri::async_runtime::spawn_blocking(move || {
        // Serialize with dashboard scans, which write back a settings snapshot.
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = app.state::<AppRuntime>();
        let mut settings = read_settings(&state)?;
        if adopt_monitor_identities(&mut settings, &links, unix_time_ms()) {
            store_settings(&state, settings)?;
            if let Err(error) = app.emit(MONITOR_IDENTITIES_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify windows of a display identity change");
            }
        }
        Ok::<(), String>(())
    })
    .await;
    agent_notice_response(applied)
}

/// Takes a paired host's reading of what a display accepts, but only when this
/// host has no reading of its own. A display answers the host it is showing and
/// no other, so the host that is off screen has nothing better than the
/// standard MCCS codes — which name no particular display, and cannot express a
/// vendor-specific input at all. A reading beats that. It never replaces a
/// reading taken here.
async fn receive_display_inputs(
    app: AppHandle,
    monitor: MonitorFingerprint,
    inputs: Vec<DisplayInput>,
    vendor_indexed: bool,
) -> AgentResponse {
    let applied = tauri::async_runtime::spawn_blocking(move || {
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = app.state::<AppRuntime>();
        let mut settings = read_settings(&state)?;
        if inputs.is_empty() {
            return Ok(());
        }
        let Some(index) = shared_monitor_index_for_peer(
            &settings.shared_monitors,
            &settings.monitor_identity_links,
            &monitor,
        ) else {
            return Ok(());
        };
        let selected = &mut settings.shared_monitors[index];
        if selected.supported_inputs.is_some() {
            return Ok(());
        }
        tracing::info!(
            monitor_id = monitor_key(&selected.fingerprint).as_str(),
            count = inputs.len(),
            vendor_indexed,
            "adopted a paired host's reading of a display's inputs"
        );
        selected.supported_inputs = Some(inputs);
        selected.vendor_indexed_inputs = vendor_indexed;
        store_settings(&state, settings)?;
        if let Err(error) = app.emit(INPUT_LABELS_CHANGED_EVENT, ()) {
            tracing::warn!(error = %error, "unable to notify windows of a display's inputs");
        }
        Ok::<(), String>(())
    })
    .await;
    agent_notice_response(applied)
}

fn agent_notice_response(applied: Result<Result<(), String>, tauri::Error>) -> AgentResponse {
    let (ready, message) = match applied {
        Ok(Ok(())) => (true, ui_text("已同步設定", "Settings synced").to_owned()),
        Ok(Err(message)) => (false, message),
        Err(error) => (false, user_error(error)),
    };
    AgentResponse {
        ready,
        message,
        display_route: None,
        display_routes: Vec::new(),
        protocol_version: AGENT_PROTOCOL_VERSION,
        ..AgentResponse::default()
    }
}

async fn receive_active_input_notice(
    app: AppHandle,
    monitor: MonitorFingerprint,
    input: DisplayInput,
) -> AgentResponse {
    let applied = tauri::async_runtime::spawn_blocking(move || {
        // Serialize with dashboard scans, which write back a settings snapshot.
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = app.state::<AppRuntime>();
        let mut settings = read_settings(&state)?;
        if let Some(changed) =
            apply_active_input_notice(&mut settings, &monitor, input, unix_time_ms())
        {
            // Store even when the route already agrees: the in-memory settle
            // timestamp is what stops an immediate scan from restoring the
            // display's stale pre-switch input.
            store_settings(&state, settings)?;
            if changed {
                tracing::info!(
                    input = input.value(),
                    "paired host notice moved the active host"
                );
                if let Err(error) = app.emit(ACTIVE_ROUTE_CHANGED_EVENT, ()) {
                    tracing::warn!(error = %error, "unable to notify the dashboard of an active host change");
                }
            }
        }
        Ok::<(), String>(())
    })
    .await;
    agent_notice_response(applied)
}

/// Frontend event telling the settings page to re-read the saved peer inputs.
const PEER_INPUTS_CHANGED_EVENT: &str = "peer-inputs-changed";

/// Adopts a paired host's confirmed port. The sender vouches for the value
/// because the display was showing *it* when the input was read, which is the
/// only moment a DDC read identifies the reader's own port rather than
/// whichever host happens to be on screen.
async fn receive_local_input_confirmed(
    app: AppHandle,
    host_id: String,
    monitor: MonitorFingerprint,
    input: DisplayInput,
) -> AgentResponse {
    let applied = tauri::async_runtime::spawn_blocking(move || {
        // Serialize with dashboard scans, which write back a settings snapshot.
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = app.state::<AppRuntime>();
        let mut settings = read_settings(&state)?;
        let (route_outcome, active_changed) = apply_local_input_confirmation(
            &mut settings,
            &host_id,
            &monitor,
            input,
            unix_time_ms(),
        );
        let input_changed = route_outcome == PeerRouteOutcome::Applied;
        let input_recorded = matches!(
            route_outcome,
            PeerRouteOutcome::Applied | PeerRouteOutcome::Unchanged
        );
        if input_recorded {
            let local_monitor = shared_monitor_index_for_peer(
                &settings.shared_monitors,
                &settings.monitor_identity_links,
                &monitor,
            )
            .map(|index| settings.shared_monitors[index].fingerprint.clone());
            if let Some(local_monitor) = local_monitor {
                record_host_input_update(
                    &mut settings,
                    &host_id,
                    &local_monitor,
                    Some(input),
                    unix_time_ms(),
                );
            }
        }
        if input_recorded || active_changed.is_some() {
            store_settings(&state, settings)?;
        }
        if input_changed {
            tracing::info!(
                host_id = host_id.as_str(),
                input = input.value(),
                "adopted a paired host's confirmed display input"
            );
            if let Err(error) = app.emit(PEER_INPUTS_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify windows of a paired host's input");
            }
        }
        if active_changed == Some(true) {
            if let Err(error) = app.emit(ACTIVE_ROUTE_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify the dashboard of an active host change");
            }
        }
        Ok::<(), String>(())
    })
    .await;
    agent_notice_response(applied)
}

/// Applies a versioned host/display assignment. `Some(false)` means the
/// version/tombstone was accepted but the visible value already agreed;
/// `None` means the display/host is unknown or the entry is stale.
/// Applies a paired host's batch of assignments. A batch larger than any real
/// group could send is refused whole. `None` means nothing was accepted;
/// otherwise whether any assignment changed a port.
fn apply_host_input_updates(
    settings: &mut AppSettings,
    updates: &[AgentHostInput],
    now_ms: u64,
) -> Option<bool> {
    if updates.len() > MAX_SHARED_HOST_INPUTS {
        return None;
    }
    let mut accepted = false;
    let mut changed = false;
    for update in updates {
        if let Some(value_changed) = apply_host_input_update(settings, update, now_ms) {
            accepted = true;
            changed |= value_changed;
        }
    }
    accepted.then_some(changed)
}

fn apply_host_input_update(
    settings: &mut AppSettings,
    update: &AgentHostInput,
    now_ms: u64,
) -> Option<bool> {
    // Only paired hosts may enter the ledger: it is persisted and returned in
    // every Ping, so an unknown id could grow it without bound. This host's
    // own port is set only here, from its display or its user.
    let is_known = settings.peers.iter().any(|peer| peer.id == update.host_id);
    if !is_known
        || !host_order::is_valid_host_id(&update.host_id)
        || !is_plausible_revision(update.updated_at_ms, now_ms)
    {
        return None;
    }
    let monitor_index = shared_monitor_index_for_peer(
        &settings.shared_monitors,
        &settings.monitor_identity_links,
        &update.monitor,
    )?;
    let fingerprint = settings.shared_monitors[monitor_index].fingerprint.clone();
    let current_version = settings
        .host_inputs
        .iter()
        .find(|entry| {
            entry.host_id == update.host_id && entry.monitor.matches_exactly(&fingerprint)
        })
        .map(|entry| entry.updated_at_ms)
        .unwrap_or_default();
    if update.updated_at_ms <= current_version {
        return None;
    }

    let mut displaced_conflict = false;
    if let Some(input) = update.input {
        // A paired host may not take the port this host says it is on.
        if settings.shared_monitors[monitor_index].local_input == Some(input) {
            return None;
        }
        let conflicts: Vec<String> = settings
            .peers
            .iter()
            .filter(|peer| peer.id != update.host_id && peer.input_for(&fingerprint) == Some(input))
            .map(|peer| peer.id.clone())
            .collect();
        let newer_conflict = conflicts.iter().any(|host_id| {
            settings.host_inputs.iter().any(|entry| {
                entry.host_id == *host_id
                    && entry.monitor.matches_exactly(&fingerprint)
                    && entry.updated_at_ms > update.updated_at_ms
            })
        });
        if newer_conflict {
            return None;
        }
        for host_id in conflicts {
            displaced_conflict = true;
            if let Some(peer) = settings.peers.iter_mut().find(|peer| peer.id == host_id) {
                peer.set_input_for(&fingerprint, None);
            }
            settings.host_inputs.retain(|entry| {
                entry.host_id != host_id || !entry.monitor.matches_exactly(&fingerprint)
            });
            settings.host_inputs.push(AgentHostInput {
                host_id,
                monitor: fingerprint.clone(),
                input: None,
                updated_at_ms: update.updated_at_ms,
            });
        }
    }

    let peer = settings
        .peers
        .iter_mut()
        .find(|peer| peer.id == update.host_id)?;
    let target_changed = peer.input_for(&fingerprint) != update.input;
    peer.set_input_for(&fingerprint, update.input);
    let changed = displaced_conflict || target_changed;

    settings.host_inputs.retain(|entry| {
        entry.host_id != update.host_id || !entry.monitor.matches_exactly(&fingerprint)
    });
    settings.host_inputs.push(AgentHostInput {
        host_id: update.host_id.clone(),
        monitor: fingerprint,
        input: update.input,
        updated_at_ms: update.updated_at_ms,
    });
    Some(changed)
}

fn record_host_input_update(
    settings: &mut AppSettings,
    host_id: &str,
    monitor: &MonitorFingerprint,
    input: Option<DisplayInput>,
    now_ms: u64,
) -> AgentHostInput {
    let previous = settings
        .host_inputs
        .iter()
        .find(|entry| entry.host_id == host_id && entry.monitor.matches_exactly(monitor))
        .map(|entry| entry.updated_at_ms)
        .unwrap_or_default();
    let update = AgentHostInput {
        host_id: host_id.to_owned(),
        monitor: monitor.clone(),
        input,
        updated_at_ms: now_ms.max(previous.saturating_add(1)),
    };
    settings
        .host_inputs
        .retain(|entry| entry.host_id != host_id || !entry.monitor.matches_exactly(monitor));
    settings.host_inputs.push(update.clone());
    update
}

async fn receive_host_inputs_notice(
    app: AppHandle,
    assignments: Vec<AgentHostInput>,
) -> AgentResponse {
    let applied = tauri::async_runtime::spawn_blocking(move || {
        let _scan = DASHBOARD_SCAN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = app.state::<AppRuntime>();
        let mut settings = read_settings(&state)?;
        let update = apply_host_input_updates(&mut settings, &assignments, unix_time_ms());
        let changed = update.unwrap_or(false);
        if update.is_some() {
            store_settings(&state, settings)?;
        }
        if changed {
            if let Err(error) = app.emit(PEER_INPUTS_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify windows of synchronized host inputs");
            }
        }
        Ok::<(), String>(())
    })
    .await;
    agent_notice_response(applied)
}

/// Applies everything a signed `LocalInputConfirmed` notice proves in one
/// settings snapshot. Besides learning the sender's port, the receiver learns
/// that the sender is currently on screen. Doing both atomically makes this
/// independent of whether the separate active-input notice arrives first.
fn apply_local_input_confirmation(
    settings: &mut AppSettings,
    host_id: &str,
    monitor: &MonitorFingerprint,
    input: DisplayInput,
    now_ms: u64,
) -> (PeerRouteOutcome, Option<bool>) {
    let route_outcome = apply_verified_peer_route(
        settings,
        host_id,
        AgentDisplayRoute {
            monitor: monitor.clone(),
            input,
            confirmed: true,
        },
    );
    // Only an accepted assignment may drive the screen state. If the input is
    // already assigned to somebody else, resolving by input would incorrectly
    // mark that other owner active even though this signed notice names the
    // sender.
    let active_changed = matches!(
        route_outcome,
        PeerRouteOutcome::Applied | PeerRouteOutcome::Unchanged
    )
    .then(|| apply_active_input_notice(settings, monitor, input, now_ms))
    .flatten();
    (route_outcome, active_changed)
}

/// Asks every paired host once, at startup, which port it occupies. A host
/// announces its port when the value changes, so a computer that was off at
/// that moment would otherwise never hear it.
fn adopt_peer_routes_at_startup(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(runtime) = app.try_state::<AppRuntime>() else {
            return;
        };
        let Ok(settings) = read_settings_inner(&runtime) else {
            return;
        };
        if !has_valid_shared_key(&settings.shared_key) {
            return;
        }
        // A host that did not answer may simply have moved, so the addresses
        // discovery knows are worth a second try before giving up on it.
        let discovered = runtime
            .discovery
            .get()
            .and_then(|discovery| discovery.peers().ok())
            .unwrap_or_default();
        let settings = Arc::new(settings);
        let mut adopted = false;
        for index in 0..settings.peers.len() {
            let peer = &settings.peers[index];
            let response = match request_peer(&settings, peer, AgentAction::Ping).await {
                Ok(response) => response,
                Err(error) => {
                    let moved = confirmed_move(&settings, peer, &discovered).await;
                    let Some((address, port)) = moved else {
                        tracing::info!(
                            peer = peer.name.as_str(),
                            error = %error,
                            "paired host did not answer the startup input query"
                        );
                        continue;
                    };
                    if let Ok(mut saved) = read_settings_inner(&runtime) {
                        if let Some(stored) = saved.peers.iter_mut().find(|item| item.id == peer.id)
                        {
                            stored.address = address;
                            stored.port = port;
                            let _ = store_settings(&runtime, saved);
                        }
                    }
                    continue;
                }
            };
            let mut saved = match read_settings_inner(&runtime) {
                Ok(saved) => saved,
                Err(_) => return,
            };
            let host_inputs_update =
                apply_host_input_updates(&mut saved, &response.host_inputs, unix_time_ms());
            let accepted = host_inputs_update.is_some();
            let mut changed = host_inputs_update.unwrap_or(false);
            adopted |= changed;
            changed |= adopt_peer_mac_address(&mut saved, &peer.id, &response);
            for route in agent_display_routes(&response) {
                if apply_verified_peer_route(&mut saved, &peer.id, route)
                    == PeerRouteOutcome::Applied
                {
                    changed = true;
                    adopted = true;
                }
            }
            if changed || accepted {
                if let Err(error) = store_settings(&runtime, saved) {
                    tracing::warn!(error = %error, "unable to save a paired host's reported input");
                    return;
                }
            }
        }
        if adopted {
            if let Err(error) = app.emit(PEER_INPUTS_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify windows of a paired host's input");
            }
        }
    });
}

/// Tells every paired host which port this computer occupies on the displays
/// it is currently showing on, so their settings fill themselves in. Only
/// displays this host is on screen for are announced, and only when the value
/// changed since the last announcement, so an idle scan loop stays quiet.
/// Tells paired hosts what a display said it accepts, for displays this host
/// has read. A display answers only the host it is showing, so the other host
/// has nothing but the standard MCCS codes to offer — and cannot express a
/// vendor-specific input with them at all. Sending costs nothing and the
/// receiver keeps its own reading if it has one.
fn announce_discovered_inputs(state: &AppRuntime, settings: &AppSettings) {
    let discovered: Vec<(MonitorFingerprint, Vec<DisplayInput>, bool)> = settings
        .shared_monitors
        .iter()
        .filter_map(|selected| {
            let inputs = selected.supported_inputs.clone()?;
            (!inputs.is_empty()).then(|| {
                (
                    selected.fingerprint.clone(),
                    inputs,
                    selected.vendor_indexed_inputs,
                )
            })
        })
        .collect();
    let Ok(mut announced) = state.announced_input_lists.lock() else {
        return;
    };
    for (fingerprint, inputs, vendor_indexed) in discovered {
        let key = monitor_key(&fingerprint);
        if announced.contains(&key) {
            continue;
        }
        if broadcast_to_peers(
            state,
            &AgentAction::DisplayInputsDiscovered {
                monitor: fingerprint,
                inputs,
                vendor_indexed,
            },
        ) {
            announced.insert(key);
        }
    }
}

fn announce_confirmed_local_inputs(state: &AppRuntime, settings: &AppSettings) {
    let host_id = state.local_host_id.clone();
    if host_id.is_empty() {
        return;
    }
    let confirmed: Vec<(MonitorFingerprint, DisplayInput)> = settings
        .shared_monitors
        .iter()
        .filter(|selected| shows_this_host(selected))
        .filter_map(|selected| Some((selected.fingerprint.clone(), selected.local_input?)))
        .collect();
    let Ok(mut announced) = state.announced_inputs.lock() else {
        return;
    };
    for (fingerprint, input) in confirmed {
        let key = monitor_key(&fingerprint);
        if announced.get(&key) == Some(&input) {
            continue;
        }
        if broadcast_to_peers(
            state,
            &AgentAction::LocalInputConfirmed {
                host_id: host_id.clone(),
                monitor: fingerprint,
                input,
            },
        ) {
            announced.insert(key, input);
        }
    }
}

fn store_settings(state: &AppRuntime, mut settings: AppSettings) -> Result<AppSettings, String> {
    let mut current = state.settings.write().map_err(|_| {
        ui_text(
            "無法更新設定，請重新啟動 MuxSU",
            "Unable to update settings. Restart MuxSU.",
        )
        .to_owned()
    })?;
    // Delivery ACKs and retry scheduling run independently of display scans.
    // A settings snapshot taken just before one of those updates must not
    // overwrite the durable outbox. Removing a peer (or a full reset) still
    // drops only that peer's queued work.
    settings.pending_peer_notices = current
        .pending_peer_notices
        .iter()
        .filter(|notice| settings.peers.iter().any(|peer| peer.id == notice.peer_id))
        .cloned()
        .collect();
    persist_settings(&state.settings_path, &settings).map_err(core_user_error)?;
    *current = settings.clone();
    Ok(settings)
}

fn read_settings(state: &AppRuntime) -> Result<AppSettings, String> {
    read_settings_inner(state)
}

fn read_settings_inner(state: &AppRuntime) -> Result<AppSettings, String> {
    state
        .settings
        .read()
        .map(|settings| settings.clone())
        .map_err(|_| {
            ui_text(
                "無法讀取設定，請重新啟動 MuxSU",
                "Unable to read settings. Restart MuxSU.",
            )
            .to_owned()
        })
}

fn load_settings(path: &Path) -> AppSettings {
    let Ok(contents) = fs::read_to_string(path) else {
        return AppSettings::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return AppSettings::default();
    };
    let mut settings = settings_from_value(value);
    // Claims used to be matched on an exact fingerprint, so the same
    // equivalence arriving from a host that reads serial numbers differently
    // was stored again rather than recognised. Anyone who merged before this
    // has a settings file holding both, listed on screen as one display merged
    // into itself, twice.
    if let Some(links) = monitor_identity::deduplicated(&settings.monitor_identity_links) {
        tracing::info!(
            removed = settings.monitor_identity_links.len() - links.len(),
            "collapsed display identity claims that name the same display"
        );
        settings.monitor_identity_links = links;
    }
    ensure_host_input_history(&mut settings);
    settings
}

/// Gives assignments written by versions before the sync ledger a baseline
/// revision. Real edits use Unix milliseconds and therefore always supersede
/// this migration value; retained tombstones continue to win over it.
fn ensure_host_input_history(settings: &mut AppSettings) {
    let mut missing = Vec::new();
    for selected in &settings.shared_monitors {
        if let Some(input) = selected
            .local_input
            .filter(|_| !settings.local_host_id.is_empty())
        {
            missing.push(AgentHostInput {
                host_id: settings.local_host_id.clone(),
                monitor: selected.fingerprint.clone(),
                input: Some(input),
                updated_at_ms: 1,
            });
        }
        for peer in &settings.peers {
            if let Some(input) = peer.input_for(&selected.fingerprint) {
                missing.push(AgentHostInput {
                    host_id: peer.id.clone(),
                    monitor: selected.fingerprint.clone(),
                    input: Some(input),
                    updated_at_ms: 1,
                });
            }
        }
    }
    for entry in missing {
        let exists = settings.host_inputs.iter().any(|saved| {
            saved.host_id == entry.host_id && saved.monitor.matches_exactly(&entry.monitor)
        });
        if !exists {
            settings.host_inputs.push(entry);
        }
    }
}

/// The `AppSettings`/`HostRoute` shape shipped before multi-monitor support:
/// a single optional `sharedMonitor` plus top-level `localInput`/
/// `supportedInputs`, and one `input` value per peer. Frozen here purely to
/// migrate existing users' `settings.json` without data loss — the live
/// `AppSettings`/`HostRoute` types have since moved to `sharedMonitors`/
/// `inputs`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SingleMonitorHostRoute {
    id: String,
    name: String,
    platform: DestinationHost,
    address: String,
    port: u16,
    mac_address: String,
    input: Option<DisplayInput>,
}

impl Default for SingleMonitorHostRoute {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            platform: local_host(),
            address: String::new(),
            port: DEFAULT_AGENT_PORT,
            mac_address: String::new(),
            input: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct SingleMonitorSettings {
    local_host: DestinationHost,
    shared_monitor: Option<SelectedMonitor>,
    local_input: Option<DisplayInput>,
    supported_inputs: Option<Vec<DisplayInput>>,
    peers: Vec<SingleMonitorHostRoute>,
    broadcast_ip: String,
    wake_port: u16,
    shared_key: String,
    wait_seconds: u64,
    autostart: bool,
    check_updates: bool,
    onboarding_completed: bool,
    host_switcher_enabled: bool,
    host_switcher_shortcut: String,
}

impl Default for SingleMonitorSettings {
    fn default() -> Self {
        Self {
            local_host: local_host(),
            shared_monitor: None,
            local_input: None,
            supported_inputs: None,
            peers: Vec::new(),
            broadcast_ip: "255.255.255.255".to_owned(),
            wake_port: 9,
            shared_key: String::new(),
            wait_seconds: 45,
            autostart: true,
            check_updates: true,
            onboarding_completed: false,
            host_switcher_enabled: false,
            host_switcher_shortcut: DEFAULT_HOST_SWITCHER_SHORTCUT.to_owned(),
        }
    }
}

fn settings_from_value(value: serde_json::Value) -> AppSettings {
    if value.get("sharedMonitors").is_some() {
        let is_existing_install = value.get("onboardingCompleted").is_none();
        let mut settings = serde_json::from_value::<AppSettings>(value).unwrap_or_default();
        if is_existing_install {
            settings.onboarding_completed = true;
        }
        settings
    } else if value.get("sharedMonitor").is_some() || value.get("peers").is_some() {
        migrate_single_monitor_settings(value)
    } else {
        serde_json::from_value::<LegacySettings>(value)
            .map(migrate_legacy_settings)
            .unwrap_or_default()
    }
}

fn migrate_single_monitor_settings(value: serde_json::Value) -> AppSettings {
    let is_existing_install = value.get("onboardingCompleted").is_none();
    let old = serde_json::from_value::<SingleMonitorSettings>(value).unwrap_or_default();
    let fingerprint = old
        .shared_monitor
        .as_ref()
        .map(|monitor| monitor.fingerprint.clone());
    let shared_monitors = old
        .shared_monitor
        .into_iter()
        .map(|mut monitor| {
            monitor.local_input = old.local_input;
            monitor.supported_inputs = old.supported_inputs.clone();
            monitor
        })
        .collect::<Vec<_>>();
    let peers = old
        .peers
        .into_iter()
        .map(|peer| {
            let inputs = match (&fingerprint, peer.input) {
                (Some(monitor), Some(input)) => vec![MonitorInputAssignment {
                    monitor: monitor.clone(),
                    input,
                }],
                _ => Vec::new(),
            };
            HostRoute {
                id: peer.id,
                name: peer.name,
                platform: peer.platform,
                address: peer.address,
                port: peer.port,
                mac_address: peer.mac_address,
                inputs,
            }
        })
        .collect();
    AppSettings {
        local_host: old.local_host,
        shared_monitors,
        peers,
        broadcast_ip: old.broadcast_ip,
        wake_port: old.wake_port,
        shared_key: old.shared_key,
        wait_seconds: old.wait_seconds,
        autostart: old.autostart,
        check_updates: old.check_updates,
        onboarding_completed: old.onboarding_completed || is_existing_install,
        host_switcher_enabled: old.host_switcher_enabled,
        host_switcher_shortcut: old.host_switcher_shortcut,
        host_order: Vec::new(),
        host_order_updated_at_ms: 0,
        host_aliases: Vec::new(),
        host_appearances: Vec::new(),
        input_labels: Vec::new(),
        monitor_identity_links: Vec::new(),
        host_inputs: Vec::new(),
        pending_peer_notices: Vec::new(),
        shared_monitors_chosen: false,
        local_host_id: String::new(),
        diagnostics_enabled: false,
        diagnostics_asked: false,
    }
}

fn migrate_legacy_settings(legacy: LegacySettings) -> AppSettings {
    let peers = if legacy.peer_id.is_empty() || legacy.peer_ip.is_empty() {
        Vec::new()
    } else {
        vec![HostRoute {
            id: legacy.peer_id,
            name: legacy.peer_name,
            platform: match legacy.local_host {
                DestinationHost::Windows => DestinationHost::Mac,
                DestinationHost::Mac => DestinationHost::Windows,
            },
            address: legacy.peer_ip,
            port: legacy.peer_port,
            mac_address: legacy.peer_mac,
            inputs: Vec::new(),
        }]
    };
    AppSettings {
        local_host: legacy.local_host,
        // 舊版沒有保存使用者選擇，也沒有螢幕身分可供輸入值附掛；升級後要求
        // 重新選取，避免沿用硬體假設。
        shared_monitors: Vec::new(),
        peers,
        broadcast_ip: legacy.broadcast_ip,
        wake_port: legacy.wake_port,
        shared_key: legacy.shared_key,
        wait_seconds: legacy.wait_seconds,
        autostart: legacy.autostart,
        check_updates: legacy.check_updates,
        onboarding_completed: true,
        host_switcher_enabled: false,
        host_switcher_shortcut: DEFAULT_HOST_SWITCHER_SHORTCUT.to_owned(),
        host_order: Vec::new(),
        host_order_updated_at_ms: 0,
        host_aliases: Vec::new(),
        host_appearances: Vec::new(),
        input_labels: Vec::new(),
        monitor_identity_links: Vec::new(),
        host_inputs: Vec::new(),
        pending_peer_notices: Vec::new(),
        shared_monitors_chosen: false,
        local_host_id: String::new(),
        diagnostics_enabled: false,
        diagnostics_asked: false,
    }
}

fn persist_settings(path: &Path, settings: &AppSettings) -> Result<(), DisplayMuxError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
    }
    let serialized = serde_json::to_vec_pretty(settings)
        .map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
    // Written beside the file and renamed over it: a write cut short then
    // leaves the previous settings whole, where rewriting in place left a
    // truncated file that loads as defaults and loses every pairing.
    let temporary = path.with_extension("json.tmp");
    write_owner_only(&temporary, &serialized)
        .and_then(|()| fs::rename(&temporary, path))
        .map_err(|error| {
            fs::remove_file(&temporary).ok();
            DisplayMuxError::Backend(error.to_string())
        })
}

/// Writes `bytes` to `path` durably, restricted to this account before any of
/// the pairing password reaches it.
fn write_owner_only(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    // A leftover from an earlier attempt keeps the mode it was created with.
    restrict_to_owner(path);
    std::io::Write::write_all(&mut file, bytes)?;
    file.sync_all()
}

/// Keeps the settings file readable only by the account that owns it.
///
/// It holds the pairing password in the clear, and that password is the whole
/// of the agent's authentication — anyone who reads it can direct this
/// computer's displays from anywhere on the network. Written under the default
/// mask it lands world-readable, so on a machine with more than one account
/// the others could simply open it.
///
/// Best effort on purpose: a file that cannot be tightened is still a file the
/// user needs, so this reports rather than refuses.
#[cfg(unix)]
fn restrict_to_owner(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        tracing::warn!(error = %error, "unable to restrict the settings file to this user");
    }
}

/// Windows puts the file under the user's own AppData, which is already denied
/// to other accounts by its inherited ACL, and rewriting that ACL by hand is
/// more likely to lock the user out than to help.
#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) {}

fn next_nonce() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{now}-{counter}", std::process::id())
}

/// Switches the display named by `identities` — its own identity and any the
/// user merged into it. Exactly one of them is handed to the service, so a
/// merge decides *which* display is meant and never relaxes the exact
/// fingerprint match the write itself still demands.
fn run_switch(
    identities: Vec<MonitorFingerprint>,
    input: DisplayInput,
) -> Result<SwitchOutcome, DisplayMuxError> {
    let controller = platform_controller()?;
    let shared_monitor = switch_target(&identities, &controller.enumerate()?)?;
    let service = DisplayMuxService::new(controller, DisplayMuxProfile { shared_monitor });
    service.switch_to_input(input, SwitchMode::Apply)
}

/// The fingerprint of the one display present right now that a switch of the
/// shared display `identities` names should write to.
///
/// An exact match wins. Failing that, a display counts when it differs from
/// one of `identities` only by a serial number one side could not read — the
/// rule `monitor_identity::same_identity` uses for merges — because a merge
/// made on the host that reads the serial carries it, and the host that
/// cannot read it would otherwise find nothing to switch while the display
/// sits right there. Tolerating that never picks between displays: when more
/// than one present display could be the target, nothing is written.
fn switch_target(
    identities: &[MonitorFingerprint],
    present: &[MonitorDescriptor],
) -> Result<MonitorFingerprint, DisplayMuxError> {
    if let Some(exact) = identities.iter().find(|identity| {
        present
            .iter()
            .any(|monitor| identity.matches_exactly(&monitor.fingerprint))
    }) {
        return Ok(exact.clone());
    }
    let mut candidates = Vec::<&MonitorFingerprint>::new();
    for monitor in present {
        let named = identities
            .iter()
            .any(|identity| monitor_identity::same_identity(identity, &monitor.fingerprint));
        if named && !candidates.contains(&&monitor.fingerprint) {
            candidates.push(&monitor.fingerprint);
        }
    }
    match candidates.as_slice() {
        [] => Err(DisplayMuxError::TargetNotFound),
        [only] => Ok((*only).clone()),
        many => Err(DisplayMuxError::AmbiguousTarget { count: many.len() }),
    }
}

fn enumerate_monitor_inventory() -> Result<MonitorInventory, DisplayMuxError> {
    let controller = platform_controller()?;
    monitor_inventory(&controller)
}

fn monitor_inventory<C: MonitorControl>(
    controller: &C,
) -> Result<MonitorInventory, DisplayMuxError> {
    let detected = controller.enumerate()?;
    let current_inputs: HashMap<_, _> = detected
        .iter()
        .filter_map(|monitor| match controller.read_input(&monitor.id) {
            Ok(input) => Some((monitor.id.clone(), input)),
            Err(error) => {
                tracing::debug!(
                    monitor_id = monitor.id.as_str(),
                    error = %error,
                    "display does not expose a controllable DDC/CI input"
                );
                None
            }
        })
        .collect();
    let controllable = detected
        .iter()
        .filter(|monitor| current_inputs.contains_key(&monitor.id))
        .cloned()
        .collect();
    Ok(MonitorInventory {
        detected,
        controllable,
        current_inputs,
    })
}

/// Records which displays a scan just saw, so a paired host's `Ping` can be
/// answered without scanning displays inside the reply. Enumeration is the
/// only thing that knows whether a cable is there, so every scan feeds this.
fn remember_attached_monitors(state: &AppRuntime, inventory: &MonitorInventory) {
    let snapshot = AttachedSnapshot {
        fingerprints: inventory
            .detected
            .iter()
            .map(|monitor| monitor.fingerprint.clone())
            .collect(),
        taken_at_ms: unix_time_ms(),
    };
    if let Ok(mut stored) = state.attached_monitors.lock() {
        *stored = snapshot;
    }
}

/// The shared displays this computer can see right now, for the reply to a
/// paired host's `Ping`. `None` when the last scan is too old to answer for:
/// the reply then says nothing about displays, which the asking host shows as
/// unknown.
///
/// Only displays shared here are reported. They are the ones already named in
/// everything paired hosts exchange, and the only ones the asking host has
/// anywhere to show.
fn attached_shared_monitors(
    state: &AppRuntime,
    settings: &AppSettings,
    now_ms: u64,
) -> Option<Vec<MonitorFingerprint>> {
    let snapshot = state.attached_monitors.lock().ok()?.clone();
    shared_monitors_seen_in(&snapshot, settings, now_ms)
}

/// The shared displays `snapshot` saw, or `None` when it is too old to answer
/// for. Split out from `attached_shared_monitors` so the rule that decides
/// between "not attached" and "unknown" can be tested on its own.
fn shared_monitors_seen_in(
    snapshot: &AttachedSnapshot,
    settings: &AppSettings,
    now_ms: u64,
) -> Option<Vec<MonitorFingerprint>> {
    if snapshot.taken_at_ms == 0
        || now_ms.saturating_sub(snapshot.taken_at_ms) > ATTACHED_SNAPSHOT_TTL_MS
    {
        return None;
    }
    Some(
        settings
            .shared_monitors
            .iter()
            .filter(|selected| {
                snapshot.fingerprints.iter().any(|seen| {
                    monitor_identity::is_same_display(
                        &settings.monitor_identity_links,
                        &selected.fingerprint,
                        seen,
                    )
                })
            })
            .map(|selected| selected.fingerprint.clone())
            .collect(),
    )
}

/// Starts a scan to refresh what `attached_shared_monitors` reports, unless
/// one is already running.
///
/// Answering a `Ping` never waits for it. A host waking up is pinged once a
/// second, and holding each reply for a DDC/CI scan would make the host look
/// slower to answer than it is; the reply says "unknown" and the next one,
/// moments later, carries the fresh reading.
fn refresh_attached_monitors_soon(app: &AppHandle) {
    let Some(runtime) = app.try_state::<AppRuntime>() else {
        return;
    };
    let running = Arc::clone(&runtime.attached_scan_running);
    if running.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let scanned = run_display_task(app, |state| {
            let inventory = enumerate_monitor_inventory().map_err(core_user_error)?;
            remember_attached_monitors(state, &inventory);
            Ok(())
        })
        .await;
        if let Err(error) = scanned {
            tracing::debug!(error = %error, "unable to refresh which displays are attached");
        }
        running.store(false, Ordering::SeqCst);
    });
}

/// Re-derives each shared display's active route from the input it just
/// reported, so a switch made outside this app (from another host, the
/// display's own buttons, or a cable swap) shows up on the next refresh.
/// Only an input owned by exactly one route is trusted; an unreadable display
/// or an unrecognized or ambiguous input keeps the last confirmed route.
/// A display confirmed switched within `ACTIVE_ROUTE_SETTLE_MS` is skipped: it
/// can still report its previous input while it changes over.
/// Returns whether any route changed.
#[cfg(test)]
fn sync_active_routes_with_live_inputs(
    settings: &mut AppSettings,
    inventory: &MonitorInventory,
    now_ms: u64,
) -> bool {
    !active_input_updates_from_live_inputs(settings, inventory, now_ms).is_empty()
}

/// The changed routes plus the exact live inputs that proved each change, so
/// callers can immediately mirror the result to local windows and paired
/// hosts. Kept separate from the bool wrapper because most unit tests only
/// care whether settings moved.
fn active_input_updates_from_live_inputs(
    settings: &mut AppSettings,
    inventory: &MonitorInventory,
    now_ms: u64,
) -> Vec<ActiveInputUpdate> {
    let peers = settings.peers.clone();
    let links = settings.monitor_identity_links.clone();
    let mut updates = Vec::new();
    for selected in &mut settings.shared_monitors {
        if now_ms.saturating_sub(selected.active_route_confirmed_at_ms) < ACTIVE_ROUTE_SETTLE_MS {
            continue;
        }
        let Some(current) = inventory
            .controllable
            .iter()
            .find(|monitor| is_selected_display(&links, selected, monitor))
            .and_then(|monitor| inventory.current_inputs.get(&monitor.id))
        else {
            continue;
        };
        let previous = selected.active_route.clone();
        if adopt_route_for_input(selected, &peers, *current) == Some(true) {
            tracing::info!(
                monitor = selected.name.as_str(),
                input = current.value(),
                previous = previous.as_deref().unwrap_or(host_order::LOCAL_ROUTE_ID),
                active = selected.active_route.as_deref().unwrap_or_default(),
                "live input moved the active host"
            );
            updates.push(ActiveInputUpdate {
                monitor: selected.fingerprint.clone(),
                input: *current,
            });
        }
    }
    updates
}

/// Applies a paired host's notice that it switched `fingerprint` to `input`.
/// The receiver resolves the owning route from its own settings rather than
/// trusting a route id from the sender. A recognized notice starts the settle
/// period (see `sync_active_routes_with_live_inputs`) even when the route
/// already matches. `Some(false)` means the notice was recognized and only
/// refreshed that settle period; `None` means no single configured route owns
/// the reported input.
fn apply_active_input_notice(
    settings: &mut AppSettings,
    fingerprint: &MonitorFingerprint,
    input: DisplayInput,
    now_ms: u64,
) -> Option<bool> {
    let index = shared_monitor_index_for_peer(
        &settings.shared_monitors,
        &settings.monitor_identity_links,
        fingerprint,
    )?;
    let peers = &settings.peers;
    let selected = &mut settings.shared_monitors[index];
    let changed = adopt_route_for_input(selected, peers, input)?;
    selected.active_route_confirmed_at_ms = now_ms;
    Some(changed)
}

/// Marks the single route ("local" or a peer id) configured for `input` as the
/// display's active route. An input owned by no route or by several keeps the
/// last confirmed route. Returns whether the route changed, or `None` when no
/// single route owns `input`.
fn adopt_route_for_input(
    selected: &mut SelectedMonitor,
    peers: &[HostRoute],
    input: DisplayInput,
) -> Option<bool> {
    let local = (selected.local_input == Some(input)).then_some("local");
    let owners: Vec<&str> = local
        .into_iter()
        .chain(
            peers
                .iter()
                .filter(|peer| peer.input_for(&selected.fingerprint) == Some(input))
                .map(|peer| peer.id.as_str()),
        )
        .collect();
    let [owner] = owners.as_slice() else {
        return None;
    };
    // An unset route is rendered as this host, so it already agrees.
    if selected.active_route.as_deref().unwrap_or("local") == *owner {
        return Some(false);
    }
    selected.active_route = Some((*owner).to_owned());
    Some(true)
}

/// External monitors the OS reports but whose DDC/CI input cannot be read.
fn uncontrollable_monitors(inventory: &MonitorInventory) -> Vec<MonitorDescriptor> {
    inventory
        .detected
        .iter()
        .filter(|monitor| {
            !monitor.built_in
                && !inventory
                    .controllable
                    .iter()
                    .any(|controllable| controllable.id == monitor.id)
        })
        .cloned()
        .collect()
}

/// True when the input recorded for this host is of a different kind than
/// the physical connection (e.g. DP recorded on an HDMI link), which usually
/// means the monitor was showing another host when the input was read.
fn connection_input_conflict(
    selected: &SelectedMonitor,
    connection: Option<&muxsu_core::MonitorConnection>,
) -> bool {
    if selected.vendor_indexed_inputs {
        return false;
    }
    let (Some(input), Some(sink)) = (
        selected.local_input,
        connection.and_then(|connection| connection.sink_interface),
    ) else {
        return false;
    };
    muxsu_core::input_matches_sink(sink, input) == Some(false)
}

fn common_input_sources() -> Vec<DisplayInput> {
    [0x01, 0x03, 0x0f, 0x11, 0x12, 0x1b]
        .into_iter()
        .filter_map(|value| DisplayInput::new(value).ok())
        .collect()
}

fn refresh_selected_input_data<C: MonitorControl>(
    controller: &C,
    monitor: &MonitorDescriptor,
    selected: &mut SelectedMonitor,
) -> Result<(), DisplayMuxError> {
    // Reading VCP 0x60 is non-disruptive. Never write or cycle ports for discovery.
    //
    // Nothing already stored is cleared on the way in. A display answers the
    // host it is showing and no other, so a read failing here is ordinary —
    // and what is stored may be the port the user typed in precisely because
    // this read cannot reach them. Wiping first meant one unreadable moment
    // took the port with it, then put it back on whichever later scan
    // succeeded: a value that came and went for no reason the user could see.
    let current = controller.read_input(&monitor.id)?;
    if reading_is_this_host_port(monitor, current) {
        selected.local_input = Some(current);
    }
    let advertised = match controller.supported_inputs(&monitor.id) {
        Ok(inputs) if !inputs.is_empty() => Some(inputs),
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(
                monitor_id = monitor.id.as_str(),
                error = %error,
                "monitor capabilities unavailable; using common MCCS input list"
            );
            None
        }
    };
    // Likewise for the input list: unreadable capabilities are not a reason to
    // discard a list, which may have come from the paired host that could read
    // this display when this one could not.
    let Some(advertised) = advertised else {
        return Ok(());
    };
    if advertised.contains(&current) {
        selected.supported_inputs = Some(advertised);
        selected.vendor_indexed_inputs = false;
        return Ok(());
    }

    // The display is showing an input its own capabilities string does not
    // list, so that list cannot be trusted for writes either. Fall back to
    // the private index range the display reports for VCP 0x60, if any.
    match controller.input_value_maximum(&monitor.id) {
        Ok(Some(maximum)) => {
            let inputs = vendor_index_inputs(maximum, current);
            tracing::warn!(
                monitor_id = monitor.id.as_str(),
                current = current.value(),
                maximum,
                "capabilities omit the active input; using the display's private 1..=max index list"
            );
            selected.supported_inputs = Some(inputs);
            selected.vendor_indexed_inputs = true;
        }
        Ok(None) => {
            tracing::warn!(
                monitor_id = monitor.id.as_str(),
                current = current.value(),
                "capabilities omit the active input and no value range is available; using common MCCS input list"
            );
        }
        Err(error) => {
            tracing::warn!(
                monitor_id = monitor.id.as_str(),
                error = %error,
                "capabilities omit the active input and the value range could not be read; using common MCCS input list"
            );
        }
    }
    Ok(())
}

/// `1..=maximum`, always including `current` even if the display under-reports
/// its range, capped at the one-byte VCP value space.
fn vendor_index_inputs(maximum: u32, current: DisplayInput) -> Vec<DisplayInput> {
    let upper = maximum.max(current.value()).min(u32::from(u8::MAX));
    (1..=upper)
        .filter_map(|value| DisplayInput::new(value).ok())
        .collect()
}

/// Reconciles every currently selected monitor against fresh enumeration
/// results. A selection is only ever refreshed from a monitor whose full EDID
/// fingerprint matches it exactly, so it is never reassigned to a different
/// physical monitor — the exact-fingerprint safety guarantee in
/// product-facts.md, which governs which display may be *switched*.
///
/// Failing to match is not evidence the display is gone. It is asleep, showing
/// a paired host that its other inputs cannot see past, or re-enumerated after
/// a mode switch reporting a serial this host can no longer read the same way
/// (an unreadable EDID falls back to CoreGraphics identity on macOS, and an
/// empty WMI `SerialNumberID` reads as no serial on Windows). Discarding the
/// selection on any of those also discarded every paired host's input for it,
/// so an unmatched selection is kept as it stands and reported as unavailable.
/// Only the user removes a shared display.
///
/// Auto-select only fires from an empty selection; once at least one monitor is
/// selected, a newly appeared monitor is never added automatically.
fn reconcile_monitor_selection(
    settings: &mut AppSettings,
    controllable: &[MonitorDescriptor],
) -> Vec<MonitorSelectionChange> {
    // Auto-select is a one-time onboarding step. An empty list stops meaning
    // "nothing chosen yet" the moment the user chooses, so emptying the list
    // on purpose must not be undone on the next refresh.
    let never_chosen = !settings.shared_monitors_chosen && settings.shared_monitors.is_empty();
    // Taken by value so the selections below can be borrowed mutably.
    let links = settings.monitor_identity_links.clone();
    let mut changes = Vec::new();
    for selected in &mut settings.shared_monitors {
        let Some(current) = controllable.iter().find(|monitor| {
            monitor_identity::is_same_display(&links, &selected.fingerprint, &monitor.fingerprint)
        }) else {
            // Unidentified this time round; keep the stored identity rather
            // than adopting an unproven reading of it.
            continue;
        };
        let mut refreshed = SelectedMonitor::from(current);
        refreshed.fingerprint = selected.fingerprint.clone();
        refreshed.local_input = selected.local_input;
        refreshed.supported_inputs = selected.supported_inputs.clone();
        refreshed.vendor_indexed_inputs = selected.vendor_indexed_inputs;
        refreshed.active_route = selected.active_route.clone();
        refreshed.active_route_confirmed_at_ms = selected.active_route_confirmed_at_ms;
        let metadata_changed = selected.name != refreshed.name
            || selected.max_resolution != refreshed.max_resolution
            || selected.resolution_source != refreshed.resolution_source;
        if metadata_changed {
            let name = refreshed.name.clone();
            *selected = refreshed;
            changes.push(MonitorSelectionChange::RefreshedMetadata { name });
        }
    }

    if never_chosen {
        let mut auto_candidates = controllable.iter().filter(|monitor| !monitor.built_in);
        if let Some(only) = auto_candidates.next() {
            if auto_candidates.next().is_none() {
                let name = only.name.clone();
                settings.shared_monitors.push(SelectedMonitor::from(only));
                settings.shared_monitors_chosen = true;
                changes.push(MonitorSelectionChange::SelectedOnlyMonitor { name });
            }
        }
    }

    changes
}

#[cfg(target_os = "windows")]
fn platform_controller() -> Result<impl MonitorControl, DisplayMuxError> {
    muxsu_core::windows::WindowsMonitorController::new()
}

#[cfg(target_os = "macos")]
fn platform_controller() -> Result<impl MonitorControl, DisplayMuxError> {
    Ok(muxsu_core::macos::MacOsMonitorController::new())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn platform_controller() -> Result<UnsupportedController, DisplayMuxError> {
    Err(DisplayMuxError::UnsupportedPlatform)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
struct UnsupportedController;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
impl MonitorControl for UnsupportedController {
    fn enumerate(&self) -> Result<Vec<MonitorDescriptor>, DisplayMuxError> {
        Err(DisplayMuxError::UnsupportedPlatform)
    }
    fn read_input(
        &self,
        _monitor: &muxsu_core::MonitorId,
    ) -> Result<DisplayInput, DisplayMuxError> {
        Err(DisplayMuxError::UnsupportedPlatform)
    }
    fn supported_inputs(
        &self,
        _monitor: &muxsu_core::MonitorId,
    ) -> Result<Vec<DisplayInput>, DisplayMuxError> {
        Err(DisplayMuxError::UnsupportedPlatform)
    }
    fn write_input(
        &self,
        _monitor: &muxsu_core::MonitorId,
        _input: DisplayInput,
    ) -> Result<(), DisplayMuxError> {
        Err(DisplayMuxError::UnsupportedPlatform)
    }
}

#[cfg(target_os = "windows")]
const fn local_host() -> DestinationHost {
    DestinationHost::Windows
}
#[cfg(target_os = "macos")]
const fn local_host() -> DestinationHost {
    DestinationHost::Mac
}
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const fn local_host() -> DestinationHost {
    DestinationHost::Windows
}

fn user_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn core_user_error(error: DisplayMuxError) -> String {
    error.localized_message(UiLocale::current() == UiLocale::TraditionalChinese)
}

fn update_error(error: impl std::fmt::Display) -> String {
    tracing::warn!(error = %error, "application update check failed");
    ui_text(
        "無法檢查更新；請確認網路可連線至 GitHub Releases，稍後再試一次",
        "Unable to check for updates. Confirm that GitHub Releases is reachable and try again later.",
    ).to_owned()
}

/// Names the step that failed and keeps the updater's own detail (an HTTP
/// status such as `403 Forbidden`), so a failed download is not mistaken for
/// a bad signature.
fn update_install_error(error: tauri_plugin_updater::Error) -> String {
    use tauri_plugin_updater::Error;

    tracing::error!(error = %error, "signed application update installation failed");
    let (template, detail) = match &error {
        Error::Network(message) => (
            ui_text(
                "更新檔下載失敗（{detail}）；目前版本未變更。請稍後再試，或從 GitHub Releases 手動下載安裝檔",
                "The update could not be downloaded ({detail}). The current version was not changed; try again later, or download the installer from GitHub Releases.",
            ),
            message
                .strip_prefix("Download request failed with status: ")
                .unwrap_or(message)
                .to_owned(),
        ),
        Error::Reqwest(_) | Error::Io(_) => (
            ui_text(
                "更新檔下載失敗（{detail}）；目前版本未變更。請稍後再試，或從 GitHub Releases 手動下載安裝檔",
                "The update could not be downloaded ({detail}). The current version was not changed; try again later, or download the installer from GitHub Releases.",
            ),
            error.to_string(),
        ),
        Error::Minisign(_) | Error::Base64(_) | Error::SignatureUtf8(_) => (
            ui_text(
                "更新檔簽章驗證失敗（{detail}），已拒絕安裝；目前版本未變更",
                "The update's signature did not verify ({detail}), so it was not installed. The current version was not changed.",
            ),
            error.to_string(),
        ),
        _ => (
            ui_text(
                "更新安裝失敗（{detail}）；目前版本未變更，請稍後再試一次",
                "The update could not be installed ({detail}). The current version was not changed; try again later.",
            ),
            error.to_string(),
        ),
    };
    template.replace("{detail}", &detail)
}
fn has_valid_shared_key(shared_key: &str) -> bool {
    shared_key.chars().count() >= MIN_SHARED_KEY_LENGTH
}

fn stretched_pairing_key(shared_key: &str) -> Arc<[u8]> {
    let cache = PAIRING_KEY_CACHE.get_or_init(|| StdMutex::new(None));
    if let Some((_, key)) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .filter(|(password, _)| password == shared_key)
    {
        return Arc::clone(key);
    }

    // Derive outside the lock. A second first caller may do the same work,
    // but no request is blocked behind an expensive password operation.
    let key = Arc::<[u8]>::from(derive_pairing_key(shared_key).to_vec());
    *cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Some((shared_key.to_owned(), Arc::clone(&key)));
    key
}

fn settings_for_current_build(settings: AppSettings) -> AppSettings {
    // A development executable may point at a dev server and, on Windows, may
    // be a console process. Never persist it as a login item.
    #[cfg(debug_assertions)]
    let settings = AppSettings {
        autostart: false,
        ..settings
    };
    settings
}

fn autostart_args() -> Option<Vec<&'static str>> {
    #[cfg(target_os = "windows")]
    {
        Some(vec!["--autostart"])
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

fn launched_from_autostart(args: impl IntoIterator<Item = String>) -> bool {
    args.into_iter().any(|arg| arg == "--autostart")
}

fn hide_main_window(window: &tauri::Window) {
    #[cfg(target_os = "windows")]
    if let Err(error) = window.set_skip_taskbar(true) {
        tracing::warn!(error = %error, "unable to remove MuxSU from the taskbar");
    }
    if let Err(error) = window.hide() {
        tracing::warn!(error = %error, "unable to hide MuxSU in the system tray");
    }
    #[cfg(target_os = "macos")]
    show_in_dock(window.app_handle(), false);
}

/// Puts MuxSU in the Dock and Command-Tab while its window is open, and takes
/// it back out when the window closes.
///
/// With no window open the app is a menu bar item and nothing more; that is
/// what the Dock icon was taken away for. But an open window with no Dock icon
/// cannot be reached with Command-Tab and disappears behind other windows with
/// no way to bring it forward except the menu bar.
#[cfg(target_os = "macos")]
fn show_in_dock(app: &AppHandle, window_open: bool) {
    let policy = if window_open {
        tauri::ActivationPolicy::Regular
    } else {
        tauri::ActivationPolicy::Accessory
    };
    if let Err(error) = app.set_activation_policy(policy) {
        tracing::warn!(error = %error, window_open, "unable to change MuxSU's place in the Dock");
    }
}

#[cfg(target_os = "windows")]
fn hide_windows_main_webview(window: &tauri::WebviewWindow) {
    if let Err(error) = window.set_skip_taskbar(true) {
        tracing::warn!(error = %error, "unable to remove MuxSU from the taskbar");
    }
    if let Err(error) = window.hide() {
        tracing::warn!(error = %error, "unable to hide MuxSU in the system tray");
    }
}

fn show_main_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        tracing::warn!("unable to find the MuxSU main window");
        return;
    };
    #[cfg(target_os = "windows")]
    if let Err(error) = window.set_skip_taskbar(false) {
        tracing::warn!(error = %error, "unable to restore MuxSU to the taskbar");
    }
    // Before showing, so the window opens in an app that can take focus.
    #[cfg(target_os = "macos")]
    show_in_dock(app, true);
    if let Err(error) = window.show() {
        tracing::warn!(error = %error, "unable to show MuxSU from the system tray");
    }
    if let Err(error) = window.unminimize() {
        tracing::warn!(error = %error, "unable to unminimize MuxSU");
    }
    if let Err(error) = window.set_focus() {
        tracing::warn!(error = %error, "unable to focus MuxSU");
    }
}

/// The status item this app lives in on macOS.
///
/// It leaves the Dock when its window closes, so this is the only way back to
/// the window once it is closed. Before there was one, closing the window left the app running
/// with nothing on screen pointing at it and no way to reopen it — the Dock
/// icon was there but nothing answered a click.
#[cfg(target_os = "macos")]
fn setup_macos_status_item(app: &tauri::App) -> tauri::Result<()> {
    use tauri::{image::Image, tray::TrayIconBuilder};

    let menu = tray::build_menu(app.handle())?;
    // The menu bar draws a template image in whatever colour it is using, so
    // the icon carries a shape in its alpha channel and no colour of its own.
    // The app icon would come out as a filled rounded square.
    //
    // Raw pixels rather than a PNG: decoding one would mean compiling an image
    // decoder into the app, which is a large dependency and a parser to keep
    // patched, for a 44-pixel square that never changes.
    const MENU_BAR_ICON_SIDE: u32 = 44;
    let icon = Image::new(
        include_bytes!("../icons/menubar.rgba"),
        MENU_BAR_ICON_SIDE,
        MENU_BAR_ICON_SIDE,
    );
    TrayIconBuilder::with_id(tray::TRAY_ID)
        .menu(&menu)
        .icon(icon)
        .icon_as_template(true)
        // Opening the menu on a left click is what every other status item
        // does; this one has nowhere else to put "quit".
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| tray::handle_menu_event(app, event.id().as_ref()))
        .build(app)?;
    tray::follow_changes(app.handle());
    Ok(())
}

#[cfg(target_os = "windows")]
fn setup_windows_tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let menu = tray::build_menu(app.handle())?;
    let mut tray_icon = TrayIconBuilder::with_id(tray::TRAY_ID)
        .menu(&menu)
        .tooltip("MuxSU")
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| tray::handle_menu_event(app, event.id().as_ref()))
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            ) {
                show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon().cloned() {
        tray_icon = tray_icon.icon(icon);
    }
    tray_icon.build(app)?;
    tray::follow_changes(app.handle());
    Ok(())
}

/// Answers a paired host's request for this computer's diagnostic snapshot,
/// redacted here, and only when this computer's user has allowed diagnostics:
/// the other computer's consent does not speak for this one.
fn answer_diagnostics_request(app: &AppHandle, settings: Option<AppSettings>) -> AgentResponse {
    let refused = |message: &str| AgentResponse {
        ready: false,
        message: message.to_owned(),
        protocol_version: AGENT_PROTOCOL_VERSION,
        ..AgentResponse::default()
    };
    let Some(settings) = settings else {
        return refused(ui_text(
            "無法讀取這台主機的設定",
            "Unable to read this host's settings",
        ));
    };
    if !settings.diagnostics_enabled {
        return refused(ui_text(
            "這台主機未允許分享診斷資訊",
            "This host has not allowed diagnostics to be shared",
        ));
    }
    let local_host_name = app.state::<AppRuntime>().local_host_name.clone();
    let redactor = diagnostics::Redactor::new(&settings, &local_host_name);
    match diagnostics::snapshot_for_agent_reply(diagnostics::host_snapshot(&settings)) {
        Some(snapshot) => AgentResponse {
            ready: true,
            message: ui_text("已附上診斷資訊", "Diagnostics attached").to_owned(),
            protocol_version: AGENT_PROTOCOL_VERSION,
            diagnostics: Some(redactor.redact(&snapshot)),
            ..AgentResponse::default()
        },
        None => refused(ui_text(
            "診斷資訊太大，無法傳送",
            "The diagnostics are too large to send",
        )),
    }
}

/// Each paired host's snapshot, or why there is none. Asked one at a time:
/// there is rarely more than one, and a report is not in a hurry.
async fn collect_peer_diagnostics(settings: &AppSettings) -> Vec<diagnostics::PairedHostReport> {
    let mut reports = Vec::with_capacity(settings.peers.len());
    for (index, peer) in settings.peers.iter().enumerate() {
        let reference = format!("peer-{}", index + 1);
        let answer = request_peer(settings, peer, AgentAction::DiagnosticsRequested)
            .await
            .and_then(|response| {
                response
                    .diagnostics
                    .ok_or_else(|| "no snapshot in the reply".to_owned())
            })
            .and_then(|snapshot| {
                serde_json::from_str::<diagnostics::HostSnapshot>(&snapshot)
                    .map_err(|error| format!("unreadable snapshot: {error}"))
            });
        reports.push(match answer {
            Ok(snapshot) => diagnostics::PairedHostReport {
                reference,
                snapshot: Some(snapshot),
                unavailable: None,
            },
            Err(reason) => diagnostics::PairedHostReport {
                reference,
                snapshot: None,
                unavailable: Some(reason),
            },
        });
    }
    reports
}

async fn build_diagnostic_report(
    app: &AppHandle,
    trigger: diagnostics::ReportTrigger,
) -> Result<diagnostics::PreparedReport, String> {
    let settings = read_settings(&app.state::<AppRuntime>())?;
    let paired_hosts = collect_peer_diagnostics(&settings).await;
    let local_host_name = app.state::<AppRuntime>().local_host_name.clone();
    let redactor = diagnostics::Redactor::new(&settings, &local_host_name);
    let report = diagnostics::DiagnosticReport::new(
        &settings,
        &redactor,
        trigger,
        paired_hosts,
        unix_time_ms(),
    );
    diagnostics::PreparedReport::from_report(&report)
}

/// Sends a report of a failed switch in the background, when the user allowed
/// it, this build has somewhere to send it, and none went recently.
fn report_failure_if_allowed(app: &AppHandle, message: &str) {
    let Some(target) = diagnostics_upload::configured_target() else {
        return;
    };
    let allowed = read_settings(&app.state::<AppRuntime>())
        .is_ok_and(|settings| settings.diagnostics_enabled);
    if !allowed || !diagnostics::claim_automatic_report_slot() {
        return;
    }
    let app = app.clone();
    let trigger = diagnostics::ReportTrigger::SwitchFailed {
        message: message.to_owned(),
    };
    tauri::async_runtime::spawn(async move {
        let sent = match build_diagnostic_report(&app, trigger).await {
            Ok(report) => diagnostics_upload::upload(&target, &report, unix_time_ms())
                .await
                .map(|()| report.report_id),
            Err(error) => Err(error),
        };
        match sent {
            Ok(report_id) => {
                tracing::info!(report_id, "sent a diagnostic report of a failed switch")
            }
            Err(error) => {
                tracing::warn!(error = %error, "unable to send a diagnostic report of a failed switch")
            }
        }
    });
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticsStatus {
    /// Whether this build knows where to send a report.
    upload_available: bool,
}

#[tauri::command]
fn diagnostics_status() -> DiagnosticsStatus {
    DiagnosticsStatus {
        upload_available: diagnostics_upload::configured_target().is_some(),
    }
}

/// Records the user's answer to "may MuxSU send diagnostics", which also marks
/// the question as asked.
#[tauri::command]
fn set_diagnostics_consent(
    enabled: bool,
    state: State<'_, AppRuntime>,
) -> Result<AppSettings, String> {
    let settings = AppSettings {
        diagnostics_enabled: enabled,
        diagnostics_asked: true,
        ..read_settings(&state)?
    };
    store_settings(&state, settings)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticPreview {
    report_id: String,
    preview: String,
}

/// Builds a report for the user to read before anything leaves the computer.
/// It is held by id, so sending or saving it uses exactly the text shown.
#[tauri::command]
async fn prepare_diagnostic_report(app: AppHandle) -> Result<DiagnosticPreview, String> {
    // A fresh scan, so the report shows the displays as they are now.
    let event_app = app.clone();
    if let Err(error) = run_display_task(app.clone(), move |state| {
        build_dashboard_state(state, &event_app)
    })
    .await
    {
        tracing::warn!(error = %error, "unable to scan displays for a diagnostic report");
    }
    let report = build_diagnostic_report(&app, diagnostics::ReportTrigger::Manual).await?;
    let preview = DiagnosticPreview {
        report_id: report.report_id.clone(),
        preview: report.json.clone(),
    };
    diagnostics::keep_pending(report);
    Ok(preview)
}

fn pending_report(report_id: &str) -> Result<diagnostics::PreparedReport, String> {
    diagnostics::pending(report_id).ok_or_else(|| {
        ui_text(
            "這份診斷報告已不是最新的，請重新產生",
            "This diagnostic report is out of date; prepare it again",
        )
        .to_owned()
    })
}

#[tauri::command]
async fn send_diagnostic_report(report_id: String) -> Result<OperationResult, String> {
    let target = diagnostics_upload::configured_target().ok_or_else(|| {
        ui_text(
            "這個版本沒有設定診斷報告的傳送位置，請改用「存成檔案」",
            "This build has nowhere to send diagnostic reports; save it to a file instead",
        )
        .to_owned()
    })?;
    let report = pending_report(&report_id)?;
    diagnostics_upload::upload(&target, &report, unix_time_ms())
        .await
        .map_err(|error| {
            format!(
                "{}: {error}",
                ui_text("無法傳送診斷報告", "Unable to send the diagnostic report")
            )
        })?;
    tracing::info!(report_id, "sent a diagnostic report");
    Ok(OperationResult {
        title: ui_text("診斷報告已傳送", "Diagnostic report sent").to_owned(),
        detail: match UiLocale::current() {
            UiLocale::TraditionalChinese => format!("回報問題時請附上報告編號 {report_id}"),
            UiLocale::English => format!("Quote report id {report_id} when describing the problem"),
        },
        peer_woken: false,
        warning: false,
    })
}

/// Saves a prepared report next to the log files and shows it in the file
/// manager. Returns where it went.
#[tauri::command]
fn save_diagnostic_report(report_id: String, app: AppHandle) -> Result<String, String> {
    use tauri_plugin_opener::OpenerExt;

    let path = diagnostics::save(&pending_report(&report_id)?).map_err(|error| {
        format!(
            "{}: {error}",
            ui_text("無法儲存診斷報告", "Unable to save the diagnostic report")
        )
    })?;
    if let Err(error) = app.opener().reveal_item_in_dir(&path) {
        tracing::warn!(error = %error, "unable to show the saved diagnostic report");
    }
    Ok(path.display().to_string())
}

pub fn run() -> anyhow::Result<()> {
    // Ahead of the builder, because every plugin below initialises before
    // `setup` starts the log file, and a panic in that window left nothing
    // behind at all.
    diagnostics::install_panic_logger();
    let builder = tauri::Builder::default()
        // This must remain the first plugin so a second launch exits before any
        // other plugin or application setup can create duplicate resources.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tracing::info!("second MuxSU launch redirected to the existing instance");
            show_main_window(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            autostart_args(),
        ))
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state == ShortcutState::Pressed {
                        show_host_switcher(app);
                    }
                })
                .build(),
        )
        .plugin(tauri_plugin_updater::Builder::new().build());
    let builder = builder.on_window_event(|window, event| match (window.label(), event) {
        ("host-switcher", tauri::WindowEvent::CloseRequested { api, .. }) => {
            api.prevent_close();
            if let Err(error) = window.hide() {
                tracing::warn!(error = %error, "unable to hide the host switcher window");
            }
        }
        ("main", tauri::WindowEvent::CloseRequested { api, .. }) => {
            api.prevent_close();
            hide_main_window(window);
        }
        #[cfg(target_os = "windows")]
        ("main", tauri::WindowEvent::Resized(_)) if window.is_minimized().unwrap_or(false) => {
            hide_main_window(window);
        }
        _ => {}
    });
    builder
        .setup(|app| {
            // Started here rather than first thing in `run`, because where the
            // log file goes is only known once the app is. Nothing before this
            // point logs.
            let log_dir = app.path().app_log_dir().ok();
            diagnostics::init_logging(log_dir.as_deref());
            let config_dir = app
                .path()
                .app_config_dir()
                .map_err(|error| anyhow::anyhow!(error))?;
            let settings_path = config_dir.join("settings.json");
            let settings = settings_for_current_build(load_settings(&settings_path));
            let detected = LocalHostIdentity::detect(local_host()).unwrap_or_else(|error| {
                tracing::warn!(error = %error, "unable to read this computer's host name");
                LocalHostIdentity::from_parts("MuxSU".to_owned(), local_host(), None)
            });
            // Fixed once and then kept: every paired host stores this id, so
            // re-deriving it would silently strand this computer's pairings,
            // its place in the shared host order and its custom name.
            let mut settings = settings;
            let first_run = settings.local_host_id.is_empty();
            let identity = if first_run {
                settings.local_host_id = detected.id.clone();
                ensure_host_input_history(&mut settings);
                detected
            } else {
                LocalHostIdentity::with_id(
                    settings.local_host_id.clone(),
                    detected.name,
                    detected.platform,
                    detected.mac_address,
                )
            };
            // Saved after `manage` below, along with everything else that
            // touches the disk, the registry or the network.
            let new_identity_to_save = first_run.then(|| settings.clone());
            #[cfg(all(target_os = "windows", not(debug_assertions)))]
            let autostart_wanted = settings.autostart;
            let discovery_identity = identity.clone();
            app.manage(AppRuntime {
                settings: Arc::new(RwLock::new(settings)),
                settings_path: settings_path.clone(),
                agent_task: Mutex::new(None),
                discovery: OnceLock::new(),
                local_host_id: identity.id,
                local_host_name: identity.name,
                local_mac_address: identity.mac_address,
                announced_inputs: Arc::new(std::sync::Mutex::new(HashMap::new())),
                announced_input_lists: Arc::new(std::sync::Mutex::new(
                    std::collections::HashSet::new(),
                )),
                attached_monitors: Arc::new(std::sync::Mutex::new(AttachedSnapshot::default())),
                attached_scan_running: Arc::new(AtomicBool::new(false)),
                host_presence: Arc::new(std::sync::Mutex::new(HashMap::new())),
                presence_check_running: Arc::new(AtomicBool::new(false)),
                notices_in_flight: Arc::new(std::sync::Mutex::new(
                    std::collections::HashSet::new(),
                )),
            });
            // Tauri builds every window in its config before it calls this
            // hook, and a webview that is up starts calling commands at once.
            // So the first command used to arrive before this hook had run at
            // all — before the log file, before `manage` — and reading
            // `State<AppRuntime>` panicked. With `panic = "abort"` that ended
            // the process: on Windows, a window that opened and vanished a few
            // seconds later, every time, leaving nothing in any log to say
            // why. Each window carries `create: false` so Tauri leaves it
            // alone, and they are built here instead, after `manage` above.
            for window in app.config().app.windows.clone() {
                tauri::WebviewWindowBuilder::from_config(app.handle(), &window)?.build()?;
            }
            // Past this point the state a command needs exists and the windows
            // that call them are up, so the work below is free to be slow.
            if let Some(settings) = new_identity_to_save {
                if let Err(error) = persist_settings(&settings_path, &settings) {
                    tracing::warn!(error = %error, "unable to save this computer's host id");
                }
            }
            #[cfg(debug_assertions)]
            if let Err(error) = app.autolaunch().disable() {
                tracing::warn!(error = %error, "unable to remove development autostart entry");
            }
            #[cfg(all(target_os = "windows", not(debug_assertions)))]
            if autostart_wanted {
                if let Err(error) = app.autolaunch().enable() {
                    tracing::warn!(error = %error, "unable to refresh the login autostart entry");
                }
            }
            match MdnsPeerDiscovery::start(&discovery_identity, DEFAULT_AGENT_PORT) {
                Ok(discovery) => {
                    if app.state::<AppRuntime>().discovery.set(discovery).is_err() {
                        tracing::warn!("MuxSU mDNS discovery had already been started");
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "unable to start MuxSU mDNS discovery");
                }
            }
            if let Some(runtime) = app.try_state::<AppRuntime>() {
                let mut settings = read_settings_inner(&runtime).map_err(anyhow::Error::msg)?;
                if settings.host_switcher_enabled {
                    // A saved shortcut the system turns out to own can never
                    // fire, and a warning in a log is not something the user
                    // reads. Fall back to the default and save that, so the
                    // switcher works and the settings page shows the
                    // combination that is actually registered.
                    let saved = validate_host_switcher_shortcut(&settings.host_switcher_shortcut);
                    if let Err(error) = &saved {
                        tracing::warn!(
                            error = %error,
                            shortcut = settings.host_switcher_shortcut,
                            "saved host switcher shortcut cannot be used; falling back to the default"
                        );
                    }
                    let shortcut = match saved {
                        Ok(shortcut) => Some(shortcut),
                        Err(_) => match validate_host_switcher_shortcut(
                            DEFAULT_HOST_SWITCHER_SHORTCUT,
                        ) {
                            Ok(shortcut) => {
                                settings.host_switcher_shortcut =
                                    DEFAULT_HOST_SWITCHER_SHORTCUT.to_owned();
                                if let Err(error) = store_settings(&runtime, settings.clone()) {
                                    tracing::warn!(error = %error, "unable to save the replacement host switcher shortcut");
                                }
                                Some(shortcut)
                            }
                            Err(error) => {
                                tracing::warn!(error = %error, "the default host switcher shortcut is invalid");
                                None
                            }
                        },
                    };
                    if let Some(shortcut) = shortcut {
                        if let Err(error) = app.global_shortcut().register(shortcut) {
                            tracing::warn!(error = %error, "unable to register the host switcher shortcut");
                        }
                    }
                }
            }
            #[cfg(target_os = "macos")]
            {
                setup_macos_status_item(app)?;
                // In the Dock while the window is open, out of it otherwise
                // (see `show_in_dock`). The bundle's LSUIElement keeps the icon
                // away from launch, so an app started with no window never
                // flashes one; but the windowing layer resets the policy to
                // Regular once launching finishes, so it is set here either way.
                let window_open = app
                    .get_webview_window("main")
                    .and_then(|window| window.is_visible().ok())
                    .unwrap_or(false);
                show_in_dock(app.handle(), window_open);
            }
            #[cfg(target_os = "windows")]
            {
                setup_windows_tray(app)?;
                if launched_from_autostart(std::env::args()) {
                    if let Some(window) = app.get_webview_window("main") {
                        hide_windows_main_webview(&window);
                    }
                }
            }
            let handle: AppHandle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Some(runtime) = handle.try_state::<AppRuntime>() {
                    if let Err(error) = restart_agent(&runtime, &handle).await {
                        tracing::warn!(error = %error, "unable to start MuxSU agent");
                    }
                    retry_pending_notices(&runtime);
                    exchange_host_layout_with_peers(&runtime, &handle);
                    adopt_peer_routes_at_startup(&handle);
                }
            });
            let retry_handle: AppHandle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    sleep(Duration::from_secs(5)).await;
                    let Some(runtime) = retry_handle.try_state::<AppRuntime>() else {
                        return;
                    };
                    retry_pending_notices(&runtime);
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            set_locale,
            discover_peers,
            select_peer,
            remove_peer,
            add_shared_monitor,
            remove_shared_monitor,
            get_settings,
            get_host_switcher_state,
            get_switch_notice,
            hide_switch_notice,
            get_host_order,
            set_host_order,
            get_host_names,
            set_host_name,
            get_host_appearances,
            set_host_appearance,
            set_input_label,
            set_monitor_identity_link,
            set_local_input,
            resync_display_input,
            redetect_display,
            reset_settings,
            exchange_host_layout,
            hide_host_switcher,
            check_host_switcher_shortcut,
            complete_onboarding,
            get_input_options,
            save_settings,
            check_for_update,
            install_update,
            get_dashboard_state,
            probe_peer,
            get_host_presence,
            refresh_host_presence,
            wake_peer,
            switch_host,
            diagnostics_status,
            set_diagnostics_consent,
            prepare_diagnostic_report,
            send_diagnostic_report,
            save_diagnostic_report,
            refresh_tray
        ])
        .run(tauri::generate_context!())
        .map_err(anyhow::Error::from)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn update_install_error_keeps_the_download_status() {
        let message = update_install_error(tauri_plugin_updater::Error::Network(
            "Download request failed with status: 403 Forbidden".to_owned(),
        ));

        assert!(message.contains("(403 Forbidden)") || message.contains("（403 Forbidden）"));
        assert!(!message.contains("Download request failed"));
    }

    struct SelectionController {
        monitors: Vec<MonitorDescriptor>,
        controllable: HashSet<String>,
    }

    impl MonitorControl for SelectionController {
        fn enumerate(&self) -> Result<Vec<MonitorDescriptor>, DisplayMuxError> {
            Ok(self.monitors.clone())
        }

        fn read_input(
            &self,
            monitor: &muxsu_core::MonitorId,
        ) -> Result<DisplayInput, DisplayMuxError> {
            if self.controllable.contains(monitor.as_str()) {
                DisplayInput::new(0x0f)
            } else {
                Err(DisplayMuxError::Backend("DDC/CI unavailable".to_owned()))
            }
        }

        fn supported_inputs(
            &self,
            monitor: &muxsu_core::MonitorId,
        ) -> Result<Vec<DisplayInput>, DisplayMuxError> {
            if self.controllable.contains(monitor.as_str()) {
                Ok(vec![
                    DisplayInput::new(0x0f).unwrap(),
                    DisplayInput::new(0x11).unwrap(),
                    DisplayInput::new(0x1b).unwrap(),
                ])
            } else {
                Err(DisplayMuxError::Backend(
                    "capabilities unavailable".to_owned(),
                ))
            }
        }

        fn write_input(
            &self,
            _monitor: &muxsu_core::MonitorId,
            _input: DisplayInput,
        ) -> Result<(), DisplayMuxError> {
            unreachable!("selection tests never write an input")
        }
    }

    fn monitor(id: &str) -> MonitorDescriptor {
        MonitorDescriptor {
            id: muxsu_core::MonitorId::new(id),
            name: id.to_owned(),
            fingerprint: MonitorFingerprint::new("ACM", id, Some(format!("serial-{id}"))),
            active: true,
            built_in: false,
            max_resolution: Some(muxsu_core::MonitorResolution::new(2560, 1440)),
            resolution_source: Some(ResolutionSource::WindowsDisplayMode),
            connection: None,
        }
    }

    #[test]
    fn uncontrollable_monitors_list_detected_externals_that_ddc_cannot_reach() {
        let external = monitor("external");
        let mut internal = monitor("internal");
        internal.built_in = true;
        let unreachable = monitor("unreachable");
        let controller = SelectionController {
            monitors: vec![internal, unreachable.clone(), external.clone()],
            controllable: HashSet::from(["internal".to_owned(), external.id.as_str().to_owned()]),
        };
        let inventory = monitor_inventory(&controller).unwrap();

        assert_eq!(uncontrollable_monitors(&inventory), vec![unreachable]);
    }

    fn hdmi_connection() -> muxsu_core::MonitorConnection {
        muxsu_core::MonitorConnection::classify(
            Some(muxsu_core::HostOutput::UsbC),
            None,
            Some(muxsu_core::SinkInterface::Hdmi),
            false,
        )
    }

    fn selected_with_input(value: u32, vendor_indexed: bool) -> SelectedMonitor {
        let mut selected = SelectedMonitor::from(&monitor("shared"));
        selected.local_input = DisplayInput::new(value).ok();
        selected.vendor_indexed_inputs = vendor_indexed;
        selected
    }

    #[test]
    fn recorded_displayport_input_on_an_hdmi_connection_is_flagged() {
        let connection = hdmi_connection();
        assert!(connection_input_conflict(
            &selected_with_input(0x0f, false),
            Some(&connection)
        ));
        assert!(!connection_input_conflict(
            &selected_with_input(0x11, false),
            Some(&connection)
        ));
    }

    #[test]
    fn input_conflict_is_never_guessed_from_vague_data() {
        let connection = hdmi_connection();
        // Private index values, missing connection data, or no recorded input.
        assert!(!connection_input_conflict(
            &selected_with_input(0x0f, true),
            Some(&connection)
        ));
        assert!(!connection_input_conflict(
            &selected_with_input(0x0f, false),
            None
        ));
        let unset = SelectedMonitor::from(&monitor("shared"));
        assert!(!connection_input_conflict(&unset, Some(&connection)));
    }

    #[test]
    fn shared_key_requires_at_least_fifteen_characters() {
        assert!(!has_valid_shared_key("12345678901234"));
        assert!(has_valid_shared_key("123456789012345"));
        assert!(has_valid_shared_key("這是一組至少十五字元的配對密碼"));
    }

    #[test]
    fn frontend_receives_backend_resolved_monitor_identities() {
        let primary = monitor("primary");
        let alias = monitor("alias");
        let settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&primary)],
            monitor_identity_links: monitor_identity::with_link(
                &[],
                &alias.fingerprint,
                Some(&primary.fingerprint),
                1,
            ),
            ..AppSettings::default()
        };

        let resolved = resolved_monitor_identities(&settings, &[alias.clone()], &[]);
        let primary_json = serde_json::to_string(&primary.fingerprint).unwrap();
        let alias_json = serde_json::to_string(&alias.fingerprint).unwrap();

        assert_eq!(resolved.get(&primary_json), resolved.get(&alias_json));
        assert_eq!(
            resolved.get(&alias_json),
            Some(&monitor_key(&primary.fingerprint))
        );
    }

    /// A host that is unreachable rather than asleep leaves every poll hanging
    /// until the agent client's own timeouts expire. Counting polls instead of
    /// seconds kept the waiting dialog up for several times the promised wait.
    #[tokio::test(start_paused = true)]
    async fn waiting_for_a_peer_never_outlasts_the_configured_seconds() {
        // Accepted by the backlog but never answered, so each poll hangs.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let settings = AppSettings {
            shared_key: "pairing-password".to_owned(),
            wait_seconds: 45,
            ..AppSettings::default()
        };
        let peer = HostRoute {
            address: address.ip().to_string(),
            port: address.port(),
            ..peer_route("peer")
        };

        let started = Instant::now();
        assert!(wait_until_peer_ready(&settings, &peer).await.is_err());

        assert!(started.elapsed() <= Duration::from_secs(settings.wait_seconds));
    }

    fn seen(fingerprints: &[&MonitorFingerprint], taken_at_ms: u64) -> AttachedSnapshot {
        AttachedSnapshot {
            fingerprints: fingerprints.iter().map(|value| (*value).clone()).collect(),
            taken_at_ms,
        }
    }

    /// "That host sees no display" and "that host has not said" look the same
    /// on the wire but mean opposite things to somebody about to switch, so a
    /// scan too old to answer for reports nothing rather than an empty list.
    #[test]
    fn a_scan_too_old_to_answer_for_reports_nothing_rather_than_no_display() {
        let shared = monitor("shared");
        let settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&shared)],
            ..AppSettings::default()
        };
        let now = ATTACHED_SNAPSHOT_TTL_MS * 3;

        let fresh = seen(&[&shared.fingerprint], now - ATTACHED_SNAPSHOT_TTL_MS);
        let stale = seen(&[&shared.fingerprint], now - ATTACHED_SNAPSHOT_TTL_MS - 1);

        assert_eq!(
            shared_monitors_seen_in(&fresh, &settings, now),
            Some(vec![shared.fingerprint.clone()])
        );
        assert_eq!(shared_monitors_seen_in(&stale, &settings, now), None);
        assert_eq!(
            shared_monitors_seen_in(&AttachedSnapshot::default(), &settings, now),
            None
        );
    }

    /// A shared display the scan did not see is the case this exists for: the
    /// host is up and that display is not on it.
    #[test]
    fn only_the_shared_displays_a_scan_saw_are_reported() {
        let attached = monitor("attached");
        let unplugged = monitor("unplugged");
        let not_shared = monitor("not-shared");
        let settings = AppSettings {
            shared_monitors: vec![
                SelectedMonitor::from(&attached),
                SelectedMonitor::from(&unplugged),
            ],
            ..AppSettings::default()
        };

        let reported = shared_monitors_seen_in(
            &seen(&[&attached.fingerprint, &not_shared.fingerprint], 1_000),
            &settings,
            1_000,
        );

        assert_eq!(reported, Some(vec![attached.fingerprint]));
    }

    /// A display with two identities reaches a scan under either of them, and
    /// the user has already said they are one panel.
    #[test]
    fn a_display_seen_under_a_merged_identity_counts_as_attached() {
        let uhd = msi_monitor("3CF0");
        let fhd = msi_monitor("7CF0");
        let settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&uhd)],
            monitor_identity_links: monitor_identity::with_link(
                &[],
                &fhd.fingerprint,
                Some(&uhd.fingerprint),
                1,
            ),
            ..AppSettings::default()
        };

        let reported = shared_monitors_seen_in(&seen(&[&fhd.fingerprint], 1_000), &settings, 1_000);

        assert_eq!(reported, Some(vec![uhd.fingerprint]));
    }

    #[test]
    fn a_peer_that_says_nothing_about_displays_leaves_them_unknown() {
        let shared = monitor("shared");
        let settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&shared)],
            ..AppSettings::default()
        };

        let silent = AgentResponse::default();
        let sees_none = AgentResponse {
            attached_monitors: Some(Vec::new()),
            ..AgentResponse::default()
        };

        assert_eq!(peer_attached_monitor_keys(&settings, &silent), None);
        assert_eq!(
            peer_attached_monitor_keys(&settings, &sees_none),
            Some(Vec::new())
        );
    }

    /// The reply names displays by the fingerprints that host reads, which are
    /// not always the ones this computer reads for the same panel.
    #[test]
    fn a_peers_displays_are_reported_under_this_computers_own_keys() {
        let uhd = msi_monitor("3CF0");
        let fhd = msi_monitor("7CF0");
        let elsewhere = monitor("elsewhere");
        let settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&uhd)],
            monitor_identity_links: monitor_identity::with_link(
                &[],
                &fhd.fingerprint,
                Some(&uhd.fingerprint),
                1,
            ),
            ..AppSettings::default()
        };
        let response = AgentResponse {
            attached_monitors: Some(vec![fhd.fingerprint, elsewhere.fingerprint]),
            ..AgentResponse::default()
        };

        assert_eq!(
            peer_attached_monitor_keys(&settings, &response),
            Some(vec![monitor_key(&uhd.fingerprint)])
        );
    }

    fn msi_monitor(product_code: &str) -> MonitorDescriptor {
        MonitorDescriptor {
            id: muxsu_core::MonitorId::new(format!("macos:MSI:{product_code}:NO-SERIAL")),
            fingerprint: MonitorFingerprint::new("MSI", product_code, None::<String>),
            name: "MPG 274U E16M".to_owned(),
            ..monitor("msi")
        }
    }

    fn shared_status(monitor: &MonitorDescriptor) -> SharedMonitorStatus {
        SharedMonitorStatus {
            monitor_key: monitor_key(&monitor.fingerprint),
            fingerprint: monitor.fingerprint.clone(),
            name: monitor.name.clone(),
            ddc_available: false,
            display_state: SharedDisplayState::Unavailable,
            status_text: String::new(),
            connection: None,
            connection_input_conflict: false,
        }
    }

    /// The MSI MPG 274U publishes `MSI:7CF0` once it is set to 1920x1080, which
    /// reads as a display nobody has shared while the shared `MSI:3CF0` goes
    /// unreadable. The settings page offers the merge either way; the curated
    /// table is what lets it point at the right one instead of a bare list.
    #[test]
    fn a_known_second_identity_is_matched_to_the_shared_display_it_belongs_to() {
        let uhd = msi_monitor("3CF0");
        let fhd = msi_monitor("7CF0");
        let settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&uhd)],
            ..AppSettings::default()
        };

        let suggestions = merge_suggestions(&settings, &[&fhd], &[shared_status(&uhd)]);

        assert_eq!(
            suggestions,
            vec![MergeSuggestion {
                monitor_id: fhd.id.as_str().to_owned(),
                primary_key: monitor_key(&uhd.fingerprint),
                primary_label: "MSI / 3CF0".to_owned(),
            }]
        );
    }

    #[test]
    fn a_display_that_is_already_the_shared_one_is_not_offered_a_merge() {
        let uhd = msi_monitor("3CF0");
        let settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&uhd)],
            ..AppSettings::default()
        };

        assert!(merge_suggestions(&settings, &[&uhd], &[shared_status(&uhd)]).is_empty());
    }

    /// A display the table knows nothing about leaves the page exactly as it was
    /// before: the merge is still offered, with nothing preselected.
    #[test]
    fn an_unknown_display_gets_no_suggestion() {
        let shared = msi_monitor("3CF0");
        let stranger = monitor("some-other-display");
        let settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&shared)],
            ..AppSettings::default()
        };

        assert!(merge_suggestions(&settings, &[&stranger], &[shared_status(&shared)]).is_empty());
    }

    /// A merge the user already declared removes the suggestion, so the page
    /// stops offering what has been settled.
    #[test]
    fn a_declared_merge_removes_the_suggestion() {
        let uhd = msi_monitor("3CF0");
        let fhd = msi_monitor("7CF0");
        let settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&uhd)],
            monitor_identity_links: monitor_identity::with_link(
                &[],
                &fhd.fingerprint,
                Some(&uhd.fingerprint),
                1,
            ),
            ..AppSettings::default()
        };

        assert!(merge_suggestions(&settings, &[&fhd], &[shared_status(&uhd)]).is_empty());
    }

    /// Windows reads the MSI MPG 274U's serial number and macOS reads none, so
    /// a merge made on one host arrives on the other naming the display with a
    /// serial it cannot read. The backend already counts that as one display;
    /// the key the screen compares has to as well, or the merged display is
    /// offered for merging again after every switch between hosts.
    #[test]
    fn a_merge_from_a_host_that_reads_the_serial_resolves_on_one_that_does_not() {
        let serialled = |product: &str| {
            MonitorFingerprint::new("MSI", product, Some("CF0H246200009".to_owned()))
        };
        let bare = |product: &str| MonitorFingerprint::new("MSI", product, None::<String>);
        let present = |fingerprint: MonitorFingerprint| MonitorDescriptor {
            fingerprint,
            ..monitor("mpg")
        };

        for (shared, claim_alias, claim_primary, seen) in [
            // On the Mac, holding the claim the PC sent.
            (
                bare("3CF0"),
                serialled("7CF0"),
                serialled("3CF0"),
                bare("7CF0"),
            ),
            // On the PC, holding the claim the Mac sent.
            (
                serialled("3CF0"),
                bare("7CF0"),
                bare("3CF0"),
                serialled("7CF0"),
            ),
        ] {
            let settings = AppSettings {
                shared_monitors: vec![SelectedMonitor::from(&present(shared.clone()))],
                monitor_identity_links: monitor_identity::with_link(
                    &[],
                    &claim_alias,
                    Some(&claim_primary),
                    1,
                ),
                ..AppSettings::default()
            };

            let resolved = resolved_monitor_identities(&settings, &[present(seen.clone())], &[]);
            let key = |fingerprint: &MonitorFingerprint| {
                resolved
                    .get(&serde_json::to_string(fingerprint).unwrap())
                    .cloned()
            };

            assert!(key(&shared).is_some());
            assert_eq!(key(&seen), key(&shared));
        }
    }

    #[test]
    fn two_displays_of_one_model_with_different_serials_keep_different_keys() {
        let first = MonitorFingerprint::new("DEL", "A1B2", Some("first".to_owned()));
        let second = MonitorFingerprint::new("DEL", "A1B2", Some("second".to_owned()));
        let settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&MonitorDescriptor {
                fingerprint: first.clone(),
                ..monitor("first")
            })],
            ..AppSettings::default()
        };

        let resolved = resolved_monitor_identities(
            &settings,
            &[MonitorDescriptor {
                fingerprint: second.clone(),
                ..monitor("second")
            }],
            &[],
        );

        assert_ne!(
            resolved.get(&serde_json::to_string(&first).unwrap()),
            resolved.get(&serde_json::to_string(&second).unwrap())
        );
    }

    fn present(product: &str, serial: Option<&str>) -> MonitorDescriptor {
        MonitorDescriptor {
            fingerprint: MonitorFingerprint::new("MSI", product, serial.map(str::to_owned)),
            ..monitor(product)
        }
    }

    /// The merge was made on the PC, which reads the MSI's serial number, so
    /// the claim names the 1080p identity with it. The Mac reads no serial and
    /// used to find nothing to switch, though the display was right there.
    #[test]
    fn a_merge_carrying_another_hosts_serial_still_finds_the_display_here() {
        let identities = vec![
            MonitorFingerprint::new("MSI", "3CF0", None::<String>),
            MonitorFingerprint::new("MSI", "7CF0", Some("CF0H246200009".to_owned())),
        ];

        let target = switch_target(&identities, &[present("7CF0", None)]);

        assert_eq!(
            target,
            Ok(MonitorFingerprint::new("MSI", "7CF0", None::<String>))
        );
    }

    #[test]
    fn an_exact_match_is_preferred_over_a_tolerant_one() {
        let exact = MonitorFingerprint::new("MSI", "7CF0", Some("CF0H246200009".to_owned()));

        let target = switch_target(
            std::slice::from_ref(&exact),
            &[
                present("7CF0", None),
                present("7CF0", Some("CF0H246200009")),
            ],
        );

        assert_eq!(target, Ok(exact));
    }

    /// Tolerating a missing serial must never pick between two displays: with
    /// two that could each be the one, nothing is written.
    #[test]
    fn two_displays_that_could_each_be_the_target_are_refused() {
        // Two of this model, one in each mode, and this host reads a serial
        // from neither: either could be the merged display.
        let identities = vec![
            MonitorFingerprint::new("MSI", "3CF0", Some("CF0H246200009".to_owned())),
            MonitorFingerprint::new("MSI", "7CF0", Some("CF0H246200009".to_owned())),
        ];

        let target = switch_target(&identities, &[present("3CF0", None), present("7CF0", None)]);

        assert_eq!(target, Err(DisplayMuxError::AmbiguousTarget { count: 2 }));
    }

    #[test]
    fn a_display_with_a_different_serial_is_not_a_target() {
        let identities = vec![MonitorFingerprint::new(
            "MSI",
            "7CF0",
            Some("CF0H246200009".to_owned()),
        )];

        let target = switch_target(&identities, &[present("7CF0", Some("other-panel"))]);

        assert_eq!(target, Err(DisplayMuxError::TargetNotFound));
    }

    #[test]
    fn new_install_does_not_assume_a_monitor_or_input() {
        let settings = AppSettings::default();
        assert!(settings.shared_monitors.is_empty());
        assert!(settings.peers.is_empty());
        assert!(settings.check_updates);
        assert!(!settings.onboarding_completed);
        assert!(!settings.host_switcher_enabled);
        assert_eq!(
            settings.host_switcher_shortcut,
            DEFAULT_HOST_SWITCHER_SHORTCUT
        );
    }

    /// The shipped default was Command+Option+Space, which macOS binds to its
    /// Finder search window. Registering it succeeds and the key never
    /// arrives, so the switcher looked broken rather than taken.
    /// The file holds the pairing password in the clear, and that password is
    /// the whole of the agent's authentication.
    #[cfg(unix)]
    #[test]
    fn the_settings_file_is_not_left_readable_by_other_accounts() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!("muxsu-perms-{}", std::process::id()));
        let path = directory.join("settings.json");
        persist_settings(&path, &AppSettings::default()).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;

        fs::remove_dir_all(&directory).ok();
        assert_eq!(mode, 0o600, "settings were written as {mode:o}");
    }

    /// Rewriting the file in place left it truncated when the write was cut
    /// short, and a file that does not parse loads as defaults: the pairing
    /// password and every paired host gone. A replacement is written beside
    /// it and renamed over it, so the old file stays whole until then.
    #[cfg(unix)]
    #[test]
    fn saved_settings_replace_the_file_whole_rather_than_rewrite_it() {
        use std::os::unix::fs::MetadataExt;

        let directory = std::env::temp_dir().join(format!("muxsu-atomic-{}", std::process::id()));
        let path = directory.join("settings.json");
        persist_settings(&path, &AppSettings::default()).unwrap();
        let first = fs::metadata(&path).unwrap().ino();
        let changed = AppSettings {
            wait_seconds: 99,
            ..AppSettings::default()
        };
        persist_settings(&path, &changed).unwrap();

        let second = fs::metadata(&path).unwrap().ino();
        let saved: AppSettings = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let leftovers = fs::read_dir(&directory).unwrap().count();
        fs::remove_dir_all(&directory).ok();

        assert_ne!(first, second, "the settings file was rewritten in place");
        assert_eq!(saved.wait_seconds, 99);
        assert_eq!(leftovers, 1, "a temporary file was left behind");
    }

    #[test]
    fn host_switcher_shortcut_refuses_what_the_system_already_owns() {
        #[cfg(target_os = "macos")]
        {
            assert!(validate_host_switcher_shortcut("CommandOrControl+Alt+Space").is_err());
            assert!(validate_host_switcher_shortcut("CommandOrControl+Space").is_err());
            assert!(validate_host_switcher_shortcut("CommandOrControl+Control+Space").is_err());
            assert!(validate_host_switcher_shortcut("CommandOrControl+Alt+KeyD").is_err());
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(validate_host_switcher_shortcut("CommandOrControl+Shift+Escape").is_err());
        }
        assert!(validate_host_switcher_shortcut(DEFAULT_HOST_SWITCHER_SHORTCUT).is_ok());
    }

    #[test]
    fn host_switcher_shortcut_requires_a_non_shift_modifier() {
        assert!(validate_host_switcher_shortcut(DEFAULT_HOST_SWITCHER_SHORTCUT).is_ok());
        assert!(validate_host_switcher_shortcut("CommandOrControl+KeyK").is_ok());
        assert!(validate_host_switcher_shortcut("CommandOrControl+Shift+KeyA").is_ok());
        assert!(validate_host_switcher_shortcut("Control+Super+KeyC").is_ok());
        assert!(validate_host_switcher_shortcut("CommandOrControl+KeyW").is_err());
        assert!(validate_host_switcher_shortcut("CommandOrControl+KeyS").is_err());
        assert!(validate_host_switcher_shortcut("CommandOrControl+Shift+KeyW").is_err());
        assert!(validate_host_switcher_shortcut("Shift+KeyK").is_err());
        assert!(validate_host_switcher_shortcut("KeyK").is_err());
        assert!(validate_host_switcher_shortcut("CommandOrControl+Alt+Shift+KeyA").is_err());
        assert!(validate_host_switcher_shortcut("not-a-shortcut").is_err());
    }

    #[test]
    fn existing_settings_receive_disabled_host_switcher_defaults() {
        let mut value = serde_json::to_value(AppSettings::default()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("hostSwitcherEnabled");
        object.remove("hostSwitcherShortcut");

        let settings = settings_from_value(value);

        assert!(!settings.host_switcher_enabled);
        assert_eq!(
            settings.host_switcher_shortcut,
            DEFAULT_HOST_SWITCHER_SHORTCUT
        );
    }

    #[test]
    fn existing_install_without_onboarding_marker_does_not_show_first_run_flow() {
        let mut value = serde_json::to_value(AppSettings::default()).unwrap();
        value.as_object_mut().unwrap().remove("onboardingCompleted");

        let settings = settings_from_value(value);

        assert!(settings.onboarding_completed);
    }

    #[test]
    fn development_builds_do_not_register_autostart() {
        let settings = settings_for_current_build(AppSettings::default());
        assert_eq!(settings.autostart, !cfg!(debug_assertions));
    }

    #[test]
    fn only_the_explicit_login_argument_starts_windows_hidden() {
        assert!(launched_from_autostart([
            "MuxSU.exe".to_owned(),
            "--autostart".to_owned(),
        ]));
        assert!(!launched_from_autostart(["MuxSU.exe".to_owned()]));
    }

    #[test]
    fn automatic_switch_tracks_wake_and_network_fallback_state() {
        assert!(!NetworkPreparation::NotRequired.peer_woken());
        assert!(!NetworkPreparation::Ready { wake_sent: true }.warning());
        assert!(NetworkPreparation::Ready { wake_sent: true }.peer_woken());
        assert!(NetworkPreparation::Unavailable {
            wake_sent: false,
            reason: "offline".to_owned(),
        }
        .warning());
    }

    #[test]
    fn v011_selected_monitor_without_resolution_source_still_loads() {
        let value = serde_json::json!({
            "name": "Existing monitor",
            "fingerprint": {
                "manufacturer_id": "ACM",
                "product_code": "1234",
                "serial_number": "serial"
            },
            "maxResolution": { "width": 3440, "height": 1440 }
        });
        let selected: SelectedMonitor = serde_json::from_value(value).unwrap();
        assert_eq!(selected.resolution_source, None);
        assert_eq!(selected.max_resolution.unwrap().width, 3440);
        assert!(selected.local_input.is_none());
        assert!(selected.supported_inputs.is_none());
    }

    #[test]
    fn migration_preserves_the_previous_two_host_configuration() {
        let cases = [
            (DestinationHost::Windows, DestinationHost::Mac),
            (DestinationHost::Mac, DestinationHost::Windows),
        ];

        for (local_host, peer_platform) in cases {
            let legacy = LegacySettings {
                local_host,
                peer_id: "peer".to_owned(),
                peer_name: "Peer computer".to_owned(),
                peer_ip: "192.168.1.20".to_owned(),
                ..LegacySettings::default()
            };
            let migrated = migrate_legacy_settings(legacy);
            // The legacy shape has no monitor identity to attach an input
            // guess to; upgrading requires an explicit re-selection.
            assert!(migrated.shared_monitors.is_empty());
            assert!(migrated.onboarding_completed);
            assert_eq!(migrated.peers[0].platform, peer_platform);
            assert!(migrated.peers[0].inputs.is_empty());
        }
    }

    #[test]
    fn migrate_single_monitor_settings_carries_forward_selection_and_peer_input() {
        let selected = monitor("shared");
        let value = serde_json::json!({
            "localHost": "mac",
            "sharedMonitor": {
                "name": selected.name,
                "fingerprint": {
                    "manufacturer_id": selected.fingerprint.manufacturer_id,
                    "product_code": selected.fingerprint.product_code,
                    "serial_number": selected.fingerprint.serial_number,
                },
            },
            "localInput": 15,
            "supportedInputs": [15, 17],
            "peers": [{
                "id": "peer",
                "name": "Peer computer",
                "platform": "windows",
                "address": "192.168.1.20",
                "port": DEFAULT_AGENT_PORT,
                "macAddress": "",
                "input": 17,
            }],
        });

        let migrated = migrate_single_monitor_settings(value);

        assert_eq!(migrated.shared_monitors.len(), 1);
        let migrated_selection = &migrated.shared_monitors[0];
        assert!(migrated_selection
            .fingerprint
            .matches_exactly(&selected.fingerprint));
        assert_eq!(migrated_selection.local_input.unwrap().value(), 15);
        assert_eq!(
            migrated_selection
                .supported_inputs
                .as_ref()
                .unwrap()
                .iter()
                .map(|input| input.value())
                .collect::<Vec<_>>(),
            vec![15, 17]
        );
        assert_eq!(
            migrated.peers[0]
                .input_for(&selected.fingerprint)
                .unwrap()
                .value(),
            17
        );
    }

    #[test]
    fn selects_and_persists_the_only_controllable_monitor() {
        let external = monitor("external");
        let mut internal = monitor("internal");
        internal.built_in = true;
        let uncontrollable = monitor("uncontrollable");
        let controller = SelectionController {
            monitors: vec![internal, uncontrollable, external.clone()],
            controllable: HashSet::from(["internal".to_owned(), external.id.as_str().to_owned()]),
        };
        let inventory = monitor_inventory(&controller).unwrap();
        assert_eq!(inventory.detected.len(), 3);
        assert_eq!(inventory.controllable.len(), 2);
        let mut settings = AppSettings::default();

        let changes = reconcile_monitor_selection(&mut settings, &inventory.controllable);

        assert_eq!(
            changes,
            vec![MonitorSelectionChange::SelectedOnlyMonitor {
                name: "external".to_owned()
            }]
        );
        assert_eq!(
            settings.shared_monitors,
            vec![SelectedMonitor::from(&external)]
        );
    }

    #[test]
    fn selected_monitor_records_current_input_and_capability_values_without_writes() {
        let external = monitor("external");
        let controller = SelectionController {
            monitors: vec![external.clone()],
            controllable: HashSet::from([external.id.as_str().to_owned()]),
        };
        let mut selected = SelectedMonitor::from(&external);

        refresh_selected_input_data(&controller, &external, &mut selected).unwrap();

        assert_eq!(selected.local_input.unwrap().value(), 0x0f);
        assert_eq!(
            selected
                .supported_inputs
                .unwrap()
                .iter()
                .map(|input| input.value())
                .collect::<Vec<_>>(),
            vec![0x0f, 0x11, 0x1b]
        );
        assert!(!selected.vendor_indexed_inputs);
    }

    /// Mimics an MStar-style display: capabilities advertise MCCS codes it
    /// never honours, while the live value and range use a private index.
    struct VendorIndexedController {
        maximum: Option<u32>,
    }

    impl MonitorControl for VendorIndexedController {
        fn enumerate(&self) -> Result<Vec<MonitorDescriptor>, DisplayMuxError> {
            Ok(Vec::new())
        }

        fn read_input(
            &self,
            _monitor: &muxsu_core::MonitorId,
        ) -> Result<DisplayInput, DisplayMuxError> {
            DisplayInput::new(0x08)
        }

        fn supported_inputs(
            &self,
            _monitor: &muxsu_core::MonitorId,
        ) -> Result<Vec<DisplayInput>, DisplayMuxError> {
            Ok(vec![
                DisplayInput::new(0x0f).unwrap(),
                DisplayInput::new(0x11).unwrap(),
            ])
        }

        fn input_value_maximum(
            &self,
            _monitor: &muxsu_core::MonitorId,
        ) -> Result<Option<u32>, DisplayMuxError> {
            Ok(self.maximum)
        }

        fn write_input(
            &self,
            _monitor: &muxsu_core::MonitorId,
            _input: DisplayInput,
        ) -> Result<(), DisplayMuxError> {
            unreachable!("selection tests never write an input")
        }
    }

    /// A display answers the host it is showing and no other, so this read
    /// failing is ordinary — and what it would discard may be the port the
    /// user set by hand because the read cannot reach them.
    struct UnreadableController;

    impl MonitorControl for UnreadableController {
        fn enumerate(&self) -> Result<Vec<MonitorDescriptor>, DisplayMuxError> {
            Ok(Vec::new())
        }

        fn read_input(
            &self,
            _monitor: &muxsu_core::MonitorId,
        ) -> Result<DisplayInput, DisplayMuxError> {
            Err(DisplayMuxError::Backend("DDC/CI unavailable".to_owned()))
        }

        fn supported_inputs(
            &self,
            _monitor: &muxsu_core::MonitorId,
        ) -> Result<Vec<DisplayInput>, DisplayMuxError> {
            Err(DisplayMuxError::Backend("DDC/CI unavailable".to_owned()))
        }

        fn input_value_maximum(
            &self,
            _monitor: &muxsu_core::MonitorId,
        ) -> Result<Option<u32>, DisplayMuxError> {
            Err(DisplayMuxError::Backend("DDC/CI unavailable".to_owned()))
        }

        fn write_input(
            &self,
            _monitor: &muxsu_core::MonitorId,
            _input: DisplayInput,
        ) -> Result<(), DisplayMuxError> {
            unreachable!("selection tests never write an input")
        }
    }

    #[test]
    fn a_display_that_cannot_be_read_keeps_the_port_already_stored() {
        let external = monitor("external");
        let mut selected = SelectedMonitor::from(&external);
        selected.local_input = Some(DisplayInput::new(0x11).unwrap());
        selected.supported_inputs = Some(vec![DisplayInput::new(0x11).unwrap()]);

        let outcome = refresh_selected_input_data(&UnreadableController, &external, &mut selected);

        assert!(outcome.is_err(), "an unreadable display should report so");
        assert_eq!(
            selected.local_input.map(|input| input.value()),
            Some(0x11),
            "a stored port was discarded because one read failed"
        );
        assert!(
            selected.supported_inputs.is_some(),
            "a stored input list was discarded because one read failed"
        );
    }

    #[test]
    fn capabilities_that_omit_the_active_input_fall_back_to_the_private_index_range() {
        let external = monitor("external");
        let mut selected = SelectedMonitor::from(&external);

        refresh_selected_input_data(
            &VendorIndexedController {
                maximum: Some(0x0e),
            },
            &external,
            &mut selected,
        )
        .unwrap();

        assert_eq!(selected.local_input.unwrap().value(), 0x08);
        assert!(selected.vendor_indexed_inputs);
        assert_eq!(
            selected
                .supported_inputs
                .unwrap()
                .iter()
                .map(|input| input.value())
                .collect::<Vec<_>>(),
            (1..=0x0e).collect::<Vec<_>>()
        );
    }

    #[test]
    fn capabilities_that_omit_the_active_input_without_a_range_use_the_common_list() {
        let external = monitor("external");
        let mut selected = SelectedMonitor::from(&external);

        refresh_selected_input_data(
            &VendorIndexedController { maximum: None },
            &external,
            &mut selected,
        )
        .unwrap();

        assert_eq!(selected.supported_inputs, None);
        assert!(!selected.vendor_indexed_inputs);
    }

    #[test]
    fn vendor_index_inputs_always_include_the_active_value() {
        let current = DisplayInput::new(0x08).unwrap();
        let values = vendor_index_inputs(0x05, current)
            .iter()
            .map(|input| input.value())
            .collect::<Vec<_>>();
        assert_eq!(values, (1..=0x08).collect::<Vec<_>>());
    }

    #[test]
    fn vendor_indexed_inputs_are_labelled_by_index_not_mccs_name() {
        let seven = DisplayInput::new(0x07).unwrap();
        assert!(input_label(true, seven).ends_with(" 7"));
        assert!(!input_label(false, seven).ends_with(" 7"));
    }

    #[test]
    fn common_input_names_include_vga_dvi_dp_hdmi_and_type_c_without_codes() {
        let inputs = common_input_sources();
        for value in [0x01, 0x03, 0x0f, 0x11, 0x1b] {
            assert!(inputs.iter().any(|input| input.value() == value));
        }
        assert_eq!(
            localized_input_name(DisplayInput::new(0x01).unwrap()),
            "VGA"
        );
        assert_eq!(
            localized_input_name(DisplayInput::new(0x03).unwrap()),
            "DVI"
        );
        assert_eq!(localized_input_name(DisplayInput::new(0x0f).unwrap()), "DP");
        assert_eq!(
            localized_input_name(DisplayInput::new(0x1b).unwrap()),
            "Type-C"
        );
        assert!(!localized_input_name(DisplayInput::new(0x11).unwrap()).contains("0x"));
    }

    #[test]
    fn verified_peer_route_is_applied_only_for_the_same_monitor_and_free_port() {
        let selected = monitor("shared");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor {
                local_input: DisplayInput::new(0x0f).ok(),
                supported_inputs: Some(vec![
                    DisplayInput::new(0x0f).unwrap(),
                    DisplayInput::new(0x11).unwrap(),
                    DisplayInput::new(0x12).unwrap(),
                ]),
                ..SelectedMonitor::from(&selected)
            }],
            ..AppSettings::default()
        };
        settings.peers.push(HostRoute {
            id: "peer".to_owned(),
            name: "Peer".to_owned(),
            platform: DestinationHost::Mac,
            address: "192.168.1.20".to_owned(),
            port: DEFAULT_AGENT_PORT,
            mac_address: String::new(),
            inputs: Vec::new(),
        });

        assert_eq!(
            apply_verified_peer_route(
                &mut settings,
                "peer",
                AgentDisplayRoute {
                    monitor: selected.fingerprint.clone(),
                    input: DisplayInput::new(0x11).unwrap(),
                    confirmed: false,
                },
            ),
            PeerRouteOutcome::Applied
        );
        assert_eq!(
            settings.peers[0]
                .input_for(&selected.fingerprint)
                .unwrap()
                .value(),
            0x11
        );

        settings.peers[0].set_input_for(&selected.fingerprint, None);
        assert_eq!(
            apply_verified_peer_route(
                &mut settings,
                "peer",
                AgentDisplayRoute {
                    monitor: monitor("different").fingerprint,
                    input: DisplayInput::new(0x12).unwrap(),
                    confirmed: false,
                },
            ),
            PeerRouteOutcome::UnknownMonitor
        );
        assert!(settings.peers[0].input_for(&selected.fingerprint).is_none());

        assert_eq!(
            apply_verified_peer_route(
                &mut settings,
                "peer",
                AgentDisplayRoute {
                    monitor: selected.fingerprint.clone(),
                    input: DisplayInput::new(0x0f).unwrap(),
                    confirmed: false,
                },
            ),
            PeerRouteOutcome::Taken
        );
        assert!(settings.peers[0].input_for(&selected.fingerprint).is_none());
    }

    /// A host that is off screen reads whichever host *is* on screen, so its
    /// report must not overwrite an input the user already has set.
    #[test]
    fn an_unconfirmed_report_keeps_the_input_a_peer_already_has() {
        let display = monitor("display");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor {
                supported_inputs: Some(vec![
                    DisplayInput::new(0x0f).unwrap(),
                    DisplayInput::new(0x11).unwrap(),
                ]),
                ..SelectedMonitor::from(&display)
            }],
            ..AppSettings::default()
        };
        settings.peers.push(peer_route("peer"));
        settings.peers[0].set_input_for(&display.fingerprint, DisplayInput::new(0x11).ok());

        assert_eq!(
            apply_verified_peer_route(
                &mut settings,
                "peer",
                AgentDisplayRoute {
                    monitor: display.fingerprint.clone(),
                    input: DisplayInput::new(0x0f).unwrap(),
                    confirmed: false,
                },
            ),
            PeerRouteOutcome::Kept
        );
        assert_eq!(
            settings.peers[0]
                .input_for(&display.fingerprint)
                .unwrap()
                .value(),
            0x11
        );
    }

    /// A host that is on screen when it reads the input can only be reading
    /// its own port, so it corrects a wrong value the user picked earlier.
    #[test]
    fn a_confirmed_report_corrects_the_input_a_peer_already_has() {
        let display = monitor("display");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor {
                supported_inputs: Some(vec![
                    DisplayInput::new(0x0f).unwrap(),
                    DisplayInput::new(0x11).unwrap(),
                ]),
                ..SelectedMonitor::from(&display)
            }],
            ..AppSettings::default()
        };
        settings.peers.push(peer_route("peer"));
        settings.peers[0].set_input_for(&display.fingerprint, DisplayInput::new(0x11).ok());

        assert_eq!(
            apply_verified_peer_route(
                &mut settings,
                "peer",
                AgentDisplayRoute {
                    monitor: display.fingerprint.clone(),
                    input: DisplayInput::new(0x0f).unwrap(),
                    confirmed: true,
                },
            ),
            PeerRouteOutcome::Applied
        );
        assert_eq!(
            settings.peers[0]
                .input_for(&display.fingerprint)
                .unwrap()
                .value(),
            0x0f
        );
    }

    /// Re-reporting the value a peer already has is not a change to save.
    #[test]
    fn re_reporting_the_same_input_changes_nothing() {
        let display = monitor("display");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor {
                supported_inputs: Some(vec![DisplayInput::new(0x0f).unwrap()]),
                ..SelectedMonitor::from(&display)
            }],
            ..AppSettings::default()
        };
        settings.peers.push(peer_route("peer"));
        settings.peers[0].set_input_for(&display.fingerprint, DisplayInput::new(0x0f).ok());

        assert_eq!(
            apply_verified_peer_route(
                &mut settings,
                "peer",
                AgentDisplayRoute {
                    monitor: display.fingerprint.clone(),
                    input: DisplayInput::new(0x0f).unwrap(),
                    confirmed: true,
                },
            ),
            PeerRouteOutcome::Unchanged
        );
    }

    /// A display this host does not offer cannot be assigned, however sure
    /// the reporting host is.
    #[test]
    fn a_confirmed_report_of_an_unsupported_input_is_rejected() {
        let display = monitor("display");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor {
                supported_inputs: Some(vec![DisplayInput::new(0x0f).unwrap()]),
                ..SelectedMonitor::from(&display)
            }],
            ..AppSettings::default()
        };
        settings.peers.push(peer_route("peer"));

        assert_eq!(
            apply_verified_peer_route(
                &mut settings,
                "peer",
                AgentDisplayRoute {
                    monitor: display.fingerprint.clone(),
                    input: DisplayInput::new(0x11).unwrap(),
                    confirmed: true,
                },
            ),
            PeerRouteOutcome::Unsupported
        );
        assert!(settings.peers[0].input_for(&display.fingerprint).is_none());
    }

    #[test]
    fn a_display_never_switched_away_still_counts_as_showing_this_host() {
        let display = monitor("display");
        let mut selected = SelectedMonitor::from(&display);

        assert!(shows_this_host(&selected));

        selected.active_route = Some("local".to_owned());
        assert!(shows_this_host(&selected));

        selected.active_route = Some("peer".to_owned());
        assert!(!shows_this_host(&selected));
    }

    fn peer_route(id: &str) -> HostRoute {
        HostRoute {
            id: id.to_owned(),
            name: "Peer".to_owned(),
            platform: DestinationHost::Mac,
            address: "192.168.1.20".to_owned(),
            port: DEFAULT_AGENT_PORT,
            mac_address: String::new(),
            inputs: Vec::new(),
        }
    }

    #[test]
    fn verified_peer_route_for_one_monitor_never_leaks_into_another() {
        let monitor_a = monitor("monitor-a");
        let monitor_b = monitor("monitor-b");
        let mut settings = AppSettings {
            shared_monitors: vec![
                SelectedMonitor {
                    supported_inputs: Some(vec![DisplayInput::new(0x0f).unwrap()]),
                    ..SelectedMonitor::from(&monitor_a)
                },
                SelectedMonitor {
                    supported_inputs: Some(vec![DisplayInput::new(0x0f).unwrap()]),
                    ..SelectedMonitor::from(&monitor_b)
                },
            ],
            ..AppSettings::default()
        };
        settings.peers.push(HostRoute {
            id: "peer".to_owned(),
            name: "Peer".to_owned(),
            platform: DestinationHost::Mac,
            address: "192.168.1.20".to_owned(),
            port: DEFAULT_AGENT_PORT,
            mac_address: String::new(),
            inputs: Vec::new(),
        });

        assert_eq!(
            apply_verified_peer_route(
                &mut settings,
                "peer",
                AgentDisplayRoute {
                    monitor: monitor_b.fingerprint.clone(),
                    input: DisplayInput::new(0x0f).unwrap(),
                    confirmed: false,
                },
            ),
            PeerRouteOutcome::Applied
        );

        assert!(settings.peers[0]
            .input_for(&monitor_a.fingerprint)
            .is_none());
        assert_eq!(
            settings.peers[0]
                .input_for(&monitor_b.fingerprint)
                .unwrap()
                .value(),
            0x0f
        );
    }

    /// Only a paired host that has an input on *this* display can bring it
    /// back, and a host on the same input as this computer is no way out of
    /// that input.
    #[test]
    fn the_hosts_that_could_bring_a_display_back_are_the_ones_with_a_port_on_it() {
        let shared = monitor("shared");
        let elsewhere = monitor("elsewhere");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor {
                local_input: Some(DisplayInput::new(0x11).unwrap()),
                ..SelectedMonitor::from(&shared)
            }],
            ..AppSettings::default()
        };
        let mut partner = peer_route("partner");
        partner.set_input_for(&shared.fingerprint, DisplayInput::new(0x0f).ok());
        let mut on_the_same_input = peer_route("same-input");
        on_the_same_input.set_input_for(&shared.fingerprint, DisplayInput::new(0x11).ok());
        let mut only_elsewhere = peer_route("elsewhere-only");
        only_elsewhere.set_input_for(&elsewhere.fingerprint, DisplayInput::new(0x12).ok());
        settings.peers = vec![partner, on_the_same_input, only_elsewhere];

        let partners = resync_partners(&settings, &settings.shared_monitors[0]);

        assert_eq!(
            partners
                .iter()
                .map(|(peer, input)| (peer.id.as_str(), input.value()))
                .collect::<Vec<_>>(),
            vec![("partner", 0x0f)]
        );
    }

    /// A display left on the other host's input is the one failure the user
    /// cannot see this app to fix, so the message has to name where it went,
    /// where it should be, and the two ways to move it.
    #[test]
    fn a_stranded_display_is_reported_with_both_inputs_and_a_way_back() {
        let shared = monitor("shared");
        let settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&shared)],
            ..AppSettings::default()
        };

        let message = stranded_text(
            &settings.shared_monitors[0],
            &peer_route("partner"),
            DisplayInput::new(0x0f).unwrap(),
            DisplayInput::new(0x11).unwrap(),
            "the agent did not answer",
        );

        assert!(message.contains("HDMI 1"), "{message}");
        assert!(message.contains("DP"), "{message}");
        assert!(message.contains("own buttons"), "{message}");
        assert!(message.contains("Peer"), "{message}");
    }

    #[test]
    fn set_active_route_updates_the_matching_monitor_only() {
        let monitor_a = monitor("monitor-a");
        let monitor_b = monitor("monitor-b");
        let mut settings = AppSettings {
            shared_monitors: vec![
                SelectedMonitor::from(&monitor_a),
                SelectedMonitor::from(&monitor_b),
            ],
            ..AppSettings::default()
        };

        assert!(set_active_route(
            &mut settings,
            &monitor_b.fingerprint,
            "peer",
            SETTLED_MS
        ));

        assert_eq!(settings.shared_monitors[0].active_route, None);
        assert_eq!(
            settings.shared_monitors[1].active_route,
            Some("peer".to_owned())
        );
    }

    #[test]
    fn set_active_route_is_a_noop_for_an_unselected_monitor() {
        let selected = monitor("shared");
        let missing = monitor("missing");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&selected)],
            ..AppSettings::default()
        };

        assert!(!set_active_route(
            &mut settings,
            &missing.fingerprint,
            "peer",
            SETTLED_MS
        ));
        assert_eq!(settings.shared_monitors[0].active_route, None);
    }

    fn peer_using_input(id: &str, monitor: &MonitorDescriptor, input: u32) -> HostRoute {
        HostRoute {
            id: id.to_owned(),
            name: id.to_owned(),
            platform: DestinationHost::Windows,
            address: "192.168.1.30".to_owned(),
            port: DEFAULT_AGENT_PORT,
            mac_address: String::new(),
            inputs: vec![MonitorInputAssignment {
                monitor: monitor.fingerprint.clone(),
                input: DisplayInput::new(input).unwrap(),
            }],
        }
    }

    fn routed_settings(
        shared: &MonitorDescriptor,
        local_input: u32,
        peer_input: u32,
        active_route: Option<&str>,
    ) -> AppSettings {
        AppSettings {
            shared_monitors: vec![SelectedMonitor {
                local_input: DisplayInput::new(local_input).ok(),
                active_route: active_route.map(str::to_owned),
                ..SelectedMonitor::from(shared)
            }],
            peers: vec![peer_using_input("peer", shared, peer_input)],
            ..AppSettings::default()
        }
    }

    fn inventory_reading(monitor: &MonitorDescriptor, input: u32) -> MonitorInventory {
        MonitorInventory {
            detected: vec![monitor.clone()],
            controllable: vec![monitor.clone()],
            current_inputs: HashMap::from([(
                monitor.id.clone(),
                DisplayInput::new(input).unwrap(),
            )]),
        }
    }

    #[test]
    fn monitor_inventory_keeps_the_input_each_controllable_display_reported() {
        let external = monitor("external");
        let unreachable = monitor("unreachable");
        let controller = SelectionController {
            monitors: vec![unreachable.clone(), external.clone()],
            controllable: HashSet::from([external.id.as_str().to_owned()]),
        };

        let inventory = monitor_inventory(&controller).unwrap();

        assert_eq!(
            inventory.current_inputs.get(&external.id),
            Some(&DisplayInput::new(0x0f).unwrap())
        );
        assert!(!inventory.current_inputs.contains_key(&unreachable.id));
    }

    #[test]
    fn live_input_moves_active_route_to_the_peer_that_owns_it() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("local"));

        let changed = sync_active_routes_with_live_inputs(
            &mut settings,
            &inventory_reading(&shared, 0x07),
            SETTLED_MS,
        );

        assert!(changed);
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("peer")
        );
    }

    #[test]
    fn live_route_change_keeps_the_monitor_and_input_needed_for_remote_sync() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("local"));

        let updates = active_input_updates_from_live_inputs(
            &mut settings,
            &inventory_reading(&shared, 0x07),
            SETTLED_MS,
        );

        assert_eq!(
            updates,
            vec![ActiveInputUpdate {
                monitor: shared.fingerprint,
                input: DisplayInput::new(0x07).unwrap(),
            }]
        );
    }

    #[test]
    fn live_input_moves_active_route_back_to_this_host_after_an_external_switch() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("peer"));

        let changed = sync_active_routes_with_live_inputs(
            &mut settings,
            &inventory_reading(&shared, 0x08),
            SETTLED_MS,
        );

        assert!(changed);
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("local")
        );
    }

    #[test]
    fn live_input_matching_no_route_keeps_the_last_confirmed_route() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("peer"));

        let changed = sync_active_routes_with_live_inputs(
            &mut settings,
            &inventory_reading(&shared, 0x03),
            SETTLED_MS,
        );

        assert!(!changed);
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("peer")
        );
    }

    #[test]
    fn live_input_shared_by_several_routes_keeps_the_last_confirmed_route() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x07, 0x07, Some("peer"));

        let changed = sync_active_routes_with_live_inputs(
            &mut settings,
            &inventory_reading(&shared, 0x07),
            SETTLED_MS,
        );

        assert!(!changed);
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("peer")
        );
    }

    #[test]
    fn unreadable_display_keeps_the_last_confirmed_route() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("peer"));
        let inventory = MonitorInventory {
            detected: vec![shared.clone()],
            controllable: Vec::new(),
            current_inputs: HashMap::new(),
        };

        assert!(!sync_active_routes_with_live_inputs(
            &mut settings,
            &inventory,
            SETTLED_MS
        ));
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("peer")
        );
    }

    #[test]
    fn peer_notice_marks_this_host_active_when_it_names_the_local_input() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("peer"));

        let changed = apply_active_input_notice(
            &mut settings,
            &shared.fingerprint,
            DisplayInput::new(0x08).unwrap(),
            SETTLED_MS,
        );

        assert_eq!(changed, Some(true));
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("local")
        );
    }

    #[test]
    fn peer_notice_marks_the_peer_that_owns_the_input_active() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("local"));

        let changed = apply_active_input_notice(
            &mut settings,
            &shared.fingerprint,
            DisplayInput::new(0x07).unwrap(),
            SETTLED_MS,
        );

        assert_eq!(changed, Some(true));
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("peer")
        );
    }

    #[test]
    fn confirmed_peer_port_also_marks_that_peer_on_screen() {
        // The active-input notice can arrive before the port notice. One
        // LocalInputConfirmed action must therefore be enough to learn both
        // the route assignment and which host is currently displayed.
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("local"));
        settings.peers[0].inputs.clear();

        let (route_outcome, active_changed) = apply_local_input_confirmation(
            &mut settings,
            "peer",
            &shared.fingerprint,
            DisplayInput::new(0x07).unwrap(),
            SETTLED_MS,
        );

        assert_eq!(route_outcome, PeerRouteOutcome::Applied);
        assert_eq!(active_changed, Some(true));
        assert_eq!(
            settings.peers[0].input_for(&shared.fingerprint),
            DisplayInput::new(0x07).ok()
        );
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("peer")
        );
    }

    #[test]
    fn rejected_peer_port_cannot_move_the_screen_to_the_inputs_other_owner() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("peer"));
        settings.peers[0].inputs.clear();

        let (route_outcome, active_changed) = apply_local_input_confirmation(
            &mut settings,
            "peer",
            &shared.fingerprint,
            DisplayInput::new(0x08).unwrap(),
            SETTLED_MS,
        );

        assert_eq!(route_outcome, PeerRouteOutcome::Taken);
        assert_eq!(active_changed, None);
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("peer")
        );
    }

    #[test]
    fn peer_notice_for_an_unknown_display_or_input_changes_nothing() {
        let shared = monitor("shared");
        let other = monitor("other");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("peer"));

        assert_eq!(
            apply_active_input_notice(
                &mut settings,
                &other.fingerprint,
                DisplayInput::new(0x08).unwrap(),
                SETTLED_MS
            ),
            None
        );
        assert_eq!(
            apply_active_input_notice(
                &mut settings,
                &shared.fingerprint,
                DisplayInput::new(0x03).unwrap(),
                SETTLED_MS
            ),
            None
        );
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("peer")
        );
    }

    /// How a paired host can describe `display`: same model, but it read a
    /// different serial number (EDID text on Windows, a number or none on macOS).
    fn as_seen_by_peer(display: &MonitorDescriptor) -> MonitorFingerprint {
        MonitorFingerprint {
            serial_number: Some("EDID-TEXT-SERIAL".to_owned()),
            ..display.fingerprint.clone()
        }
    }

    #[test]
    fn peer_notice_matches_the_display_when_hosts_read_different_serials() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("peer"));

        let changed = apply_active_input_notice(
            &mut settings,
            &as_seen_by_peer(&shared),
            DisplayInput::new(0x08).unwrap(),
            SETTLED_MS,
        );

        assert_eq!(changed, Some(true));
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("local")
        );
    }

    #[test]
    fn peer_display_is_not_guessed_between_two_shared_displays_of_the_same_model() {
        let left = monitor("twin");
        let right = MonitorDescriptor {
            id: muxsu_core::MonitorId::new("twin-right"),
            fingerprint: MonitorFingerprint {
                serial_number: Some("serial-twin-right".to_owned()),
                ..left.fingerprint.clone()
            },
            ..left.clone()
        };
        let settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&left), SelectedMonitor::from(&right)],
            ..AppSettings::default()
        };

        assert_eq!(
            shared_monitor_index_for_peer(
                &settings.shared_monitors,
                &settings.monitor_identity_links,
                &as_seen_by_peer(&left)
            ),
            None
        );
        assert_eq!(
            shared_monitor_index_for_peer(
                &settings.shared_monitors,
                &settings.monitor_identity_links,
                &right.fingerprint
            ),
            Some(1)
        );
    }

    #[test]
    fn display_state_explains_an_unreadable_display_that_another_host_is_showing() {
        let peer = Some("2cf05de0c029-windows");

        assert_eq!(shared_display_state(true, peer), SharedDisplayState::Ready);
        assert_eq!(
            shared_display_state(false, peer),
            SharedDisplayState::OnOtherHost
        );
        assert_eq!(
            shared_display_state(false, Some("local")),
            SharedDisplayState::Unavailable
        );
        assert_eq!(
            shared_display_state(false, None),
            SharedDisplayState::Unavailable
        );
    }

    #[test]
    fn newer_host_order_from_a_peer_replaces_the_saved_order() {
        let mut settings = AppSettings {
            host_order: vec!["mac-a".to_owned(), "pc-b".to_owned()],
            host_order_updated_at_ms: 100,
            ..AppSettings::default()
        };

        let changed = apply_host_order_notice(
            &mut settings,
            vec!["pc-b".to_owned(), "mac-a".to_owned()],
            200,
            LEDGER_NOW_MS,
        );

        assert!(changed);
        assert_eq!(settings.host_order, vec!["pc-b", "mac-a"]);
        assert_eq!(settings.host_order_updated_at_ms, 200);
    }

    #[test]
    fn a_peer_needs_our_host_order_only_when_ours_is_newer() {
        let settings = AppSettings {
            host_order: vec!["mac-a".to_owned(), "pc-b".to_owned()],
            host_order_updated_at_ms: 200,
            ..AppSettings::default()
        };

        assert!(host_order_is_newer_than(&settings, 0));
        assert!(host_order_is_newer_than(&settings, 199));
        assert!(!host_order_is_newer_than(&settings, 200));
        assert!(!host_order_is_newer_than(&AppSettings::default(), 0));
    }

    #[test]
    fn stale_or_malformed_host_order_from_a_peer_is_ignored() {
        let saved = vec!["mac-a".to_owned(), "pc-b".to_owned()];
        let mut settings = AppSettings {
            host_order: saved.clone(),
            host_order_updated_at_ms: 100,
            ..AppSettings::default()
        };

        assert!(!apply_host_order_notice(
            &mut settings,
            vec!["pc-b".to_owned()],
            100,
            LEDGER_NOW_MS
        ));
        assert!(!apply_host_order_notice(
            &mut settings,
            vec!["pc-b".to_owned(), "pc-b".to_owned()],
            300,
            LEDGER_NOW_MS
        ));
        assert_eq!(settings.host_order, saved);
        assert_eq!(settings.host_order_updated_at_ms, 100);
    }

    #[test]
    fn shared_state_dated_past_the_clock_tolerance_is_ignored() {
        let now = LEDGER_NOW_MS;
        let poisoned = now + MAX_REVISION_AHEAD_MS + 1;
        let shared = monitor("shared");
        let mut settings = AppSettings {
            local_host_id: "this-host".to_owned(),
            shared_monitors: vec![SelectedMonitor::from(&shared)],
            host_order: vec!["mac-a".to_owned(), "pc-b".to_owned()],
            host_order_updated_at_ms: 100,
            ..AppSettings::default()
        };

        assert!(!apply_host_order_notice(
            &mut settings,
            vec!["pc-b".to_owned(), "mac-a".to_owned()],
            poisoned,
            now
        ));
        assert!(!adopt_host_aliases(
            &mut settings,
            &[HostAlias {
                host_id: "pc-b".to_owned(),
                name: "Poisoned".to_owned(),
                updated_at_ms: poisoned,
            }],
            now
        ));
        assert!(!adopt_input_labels(
            &mut settings,
            &[InputLabel {
                monitor: shared.fingerprint.clone(),
                input: DisplayInput::new(8).unwrap(),
                label: "Poisoned".to_owned(),
                updated_at_ms: poisoned,
            }],
            now
        ));
        assert!(!adopt_monitor_identities(
            &mut settings,
            &[MonitorIdentityLink {
                alias: monitor("alias").fingerprint,
                primary: Some(shared.fingerprint.clone()),
                updated_at_ms: poisoned,
            }],
            now
        ));
        assert_eq!(settings.host_order_updated_at_ms, 100);
        assert!(settings.host_aliases.is_empty());
        assert!(settings.input_labels.is_empty());
        assert!(settings.monitor_identity_links.is_empty());
    }

    /// Names are stored and shared on; one for a host nobody here knows would
    /// only grow the settings file.
    #[test]
    fn a_name_for_a_host_this_one_does_not_know_is_ignored() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, None);
        settings.local_host_id = "this-host".to_owned();
        let named = |host_id: &str| HostAlias {
            host_id: host_id.to_owned(),
            name: "Desk".to_owned(),
            updated_at_ms: 10,
        };

        assert!(!adopt_host_aliases(
            &mut settings,
            &[named("stranger")],
            LEDGER_NOW_MS
        ));
        assert!(adopt_host_aliases(
            &mut settings,
            &[named("peer"), named("this-host")],
            LEDGER_NOW_MS
        ));
        assert_eq!(settings.host_aliases.len(), 2);
    }

    /// A time well past any settle period, for tests about other behaviour.
    const SETTLED_MS: u64 = ACTIVE_ROUTE_SETTLE_MS * 100;

    #[test]
    fn live_input_right_after_a_confirmed_switch_does_not_revert_it() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, None);
        let switched_at = SETTLED_MS;
        assert!(set_active_route(
            &mut settings,
            &shared.fingerprint,
            "peer",
            switched_at
        ));
        // The display still reports the old input while it changes over.
        let stale = inventory_reading(&shared, 0x08);

        let changed_while_settling = sync_active_routes_with_live_inputs(
            &mut settings,
            &stale,
            switched_at + ACTIVE_ROUTE_SETTLE_MS - 1,
        );
        assert!(!changed_while_settling);
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("peer")
        );

        let changed_after_settling = sync_active_routes_with_live_inputs(
            &mut settings,
            &stale,
            switched_at + ACTIVE_ROUTE_SETTLE_MS,
        );
        assert!(changed_after_settling);
        assert_eq!(
            settings.shared_monitors[0].active_route.as_deref(),
            Some("local")
        );
    }

    #[test]
    fn peer_notice_starts_the_settle_period_even_when_the_route_already_matches() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, Some("peer"));

        let changed = apply_active_input_notice(
            &mut settings,
            &shared.fingerprint,
            DisplayInput::new(0x07).unwrap(),
            SETTLED_MS,
        );

        assert_eq!(changed, Some(false));
        assert_eq!(
            settings.shared_monitors[0].active_route_confirmed_at_ms,
            SETTLED_MS
        );
        assert!(!sync_active_routes_with_live_inputs(
            &mut settings,
            &inventory_reading(&shared, 0x08),
            SETTLED_MS + 1
        ));
    }

    #[test]
    fn unset_active_route_already_means_this_host() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, None);

        assert!(!sync_active_routes_with_live_inputs(
            &mut settings,
            &inventory_reading(&shared, 0x08),
            SETTLED_MS
        ));
        assert_eq!(settings.shared_monitors[0].active_route, None);
    }

    #[test]
    fn settings_reject_duplicate_and_unadvertised_input_assignments() {
        let selected = monitor("shared");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor {
                local_input: DisplayInput::new(0x0f).ok(),
                supported_inputs: Some(vec![
                    DisplayInput::new(0x0f).unwrap(),
                    DisplayInput::new(0x11).unwrap(),
                ]),
                ..SelectedMonitor::from(&selected)
            }],
            ..AppSettings::default()
        };
        settings.peers.push(HostRoute {
            id: "peer".to_owned(),
            name: "Peer".to_owned(),
            platform: DestinationHost::Mac,
            address: "192.168.1.20".to_owned(),
            port: DEFAULT_AGENT_PORT,
            mac_address: String::new(),
            inputs: Vec::new(),
        });
        settings.peers[0].set_input_for(&selected.fingerprint, DisplayInput::new(0x0f).ok());
        assert!(validate_settings(&settings).is_err());

        settings.peers[0].set_input_for(&selected.fingerprint, DisplayInput::new(0x1b).ok());
        assert!(validate_settings(&settings).is_err());
    }

    #[test]
    fn the_same_input_value_may_be_assigned_to_different_monitors() {
        let monitor_a = monitor("monitor-a");
        let monitor_b = monitor("monitor-b");
        let mut settings = AppSettings {
            shared_monitors: vec![
                SelectedMonitor::from(&monitor_a),
                SelectedMonitor::from(&monitor_b),
            ],
            ..AppSettings::default()
        };
        settings.peers.push(HostRoute {
            id: "peer".to_owned(),
            name: "Peer".to_owned(),
            platform: DestinationHost::Mac,
            address: "192.168.1.20".to_owned(),
            port: DEFAULT_AGENT_PORT,
            mac_address: String::new(),
            inputs: Vec::new(),
        });
        settings.peers[0].set_input_for(&monitor_a.fingerprint, DisplayInput::new(0x0f).ok());
        settings.peers[0].set_input_for(&monitor_b.fingerprint, DisplayInput::new(0x0f).ok());

        assert!(validate_settings(&settings).is_ok());
    }

    #[test]
    fn emptying_the_shared_list_on_purpose_is_not_undone_by_auto_select() {
        // With one controllable display, removing it emptied the list and the
        // next refresh auto-selected it straight back, so it could never be
        // removed at all.
        let only = monitor("only");
        let mut settings = AppSettings {
            shared_monitors: Vec::new(),
            shared_monitors_chosen: true,
            ..AppSettings::default()
        };

        let changes = reconcile_monitor_selection(&mut settings, std::slice::from_ref(&only));

        assert!(changes.is_empty());
        assert!(settings.shared_monitors.is_empty());
    }

    #[test]
    fn auto_select_still_runs_for_a_computer_that_has_never_chosen() {
        let only = monitor("only");
        let mut settings = AppSettings::default();

        let changes = reconcile_monitor_selection(&mut settings, std::slice::from_ref(&only));

        assert_eq!(
            changes,
            vec![MonitorSelectionChange::SelectedOnlyMonitor {
                name: "only".to_owned()
            }]
        );
        assert_eq!(settings.shared_monitors.len(), 1);
        assert!(
            settings.shared_monitors_chosen,
            "auto-select decides the list, so it must not run twice"
        );
    }

    fn configured_settings() -> AppSettings {
        let shared = monitor("shared");
        AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&shared)],
            shared_monitors_chosen: true,
            monitor_identity_links: monitor_identity::with_link(
                &[],
                &MonitorFingerprint::new("MSI", "7CF0", None::<String>),
                Some(&MonitorFingerprint::new("MSI", "3CF0", None::<String>)),
                10,
            ),
            input_labels: vec![InputLabel {
                monitor: shared.fingerprint.clone(),
                input: DisplayInput::new(8).unwrap(),
                label: "USB-C".to_owned(),
                updated_at_ms: 10,
            }],
            peers: vec![peer_using_input("ITX-PC", &shared, 7)],
            shared_key: "pairing-password".to_owned(),
            host_switcher_shortcut: "Alt+Q".to_owned(),
            local_host_id: "kept-id".to_owned(),
            ..AppSettings::default()
        }
    }

    fn inventory_showing(monitor: &MonitorDescriptor, input: u32) -> MonitorInventory {
        MonitorInventory {
            detected: vec![monitor.clone()],
            controllable: vec![monitor.clone()],
            current_inputs: HashMap::from([(
                monitor.id.clone(),
                DisplayInput::new(input).unwrap(),
            )]),
        }
    }

    fn discovered(id: &str, address: &str, port: u16) -> DiscoveredPeer {
        DiscoveredPeer {
            id: id.to_owned(),
            name: "whatever discovery calls it".to_owned(),
            platform: DestinationHost::Windows,
            address: address.parse().unwrap(),
            port,
        }
    }

    #[test]
    fn a_host_reported_somewhere_else_is_a_candidate_to_try() {
        let mut peer = peer_using_input("ITX-PC", &monitor("shared"), 7);
        peer.address = "192.168.50.93".to_owned();
        let moved = moved_peer_endpoint(&peer, &[discovered(&peer.id, "192.168.50.97", 47653)]);

        assert_eq!(moved, Some(("192.168.50.97".to_owned(), 47653)));
    }

    #[test]
    fn a_host_reported_where_it_already_is_is_not_a_candidate() {
        let mut peer = peer_using_input("ITX-PC", &monitor("shared"), 7);
        peer.address = "192.168.50.97".to_owned();
        let port = peer.port;

        assert_eq!(
            moved_peer_endpoint(&peer, &[discovered(&peer.id, "192.168.50.97", port)]),
            None
        );
    }

    #[test]
    fn another_computer_at_the_same_address_is_never_a_candidate() {
        // Only the host id decides. Something else answering where the host
        // used to be must not inherit the pairing, however reachable it is.
        let mut peer = peer_using_input("ITX-PC", &monitor("shared"), 7);
        peer.address = "192.168.50.93".to_owned();

        assert_eq!(
            moved_peer_endpoint(
                &peer,
                &[discovered("somebody-else", "192.168.50.97", 47653)]
            ),
            None
        );
    }

    #[test]
    fn a_blank_port_is_filled_from_the_reading_the_scan_already_took() {
        // The port was only read when the display was selected, so a display
        // added while it was showing another host stayed blank no matter how
        // often the user refreshed after switching it here.
        let mut display = monitor("shared");
        display.connection = Some(muxsu_core::MonitorConnection::classify(
            Some(muxsu_core::HostOutput::DisplayPort),
            None,
            Some(muxsu_core::SinkInterface::DisplayPort),
            false,
        ));
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&display)],
            ..AppSettings::default()
        };
        settings.shared_monitors[0].local_input = None;

        assert!(fill_unset_local_inputs(
            &mut settings,
            &inventory_showing(&display, 0x0f)
        ));
        assert_eq!(
            settings.shared_monitors[0].local_input,
            Some(DisplayInput::new(0x0f).unwrap())
        );
    }

    #[test]
    fn a_reading_the_wiring_contradicts_does_not_fill_a_blank_port() {
        // HDMI 1 cannot be this host's port on a DisplayPort link: the display
        // is showing somebody else, and storing it would aim a switch wrong.
        let mut display = monitor("shared");
        display.connection = Some(muxsu_core::MonitorConnection::classify(
            Some(muxsu_core::HostOutput::DisplayPort),
            None,
            Some(muxsu_core::SinkInterface::DisplayPort),
            false,
        ));
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&display)],
            ..AppSettings::default()
        };
        settings.shared_monitors[0].local_input = None;

        assert!(!fill_unset_local_inputs(
            &mut settings,
            &inventory_showing(&display, 0x11)
        ));
        assert_eq!(settings.shared_monitors[0].local_input, None);
    }

    #[test]
    fn a_port_already_set_is_never_overwritten_by_a_scan() {
        let display = monitor("shared");
        let chosen = DisplayInput::new(8).unwrap();
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&display)],
            ..AppSettings::default()
        };
        settings.shared_monitors[0].local_input = Some(chosen);

        assert!(!fill_unset_local_inputs(
            &mut settings,
            &inventory_showing(&display, 0x0f)
        ));
        assert_eq!(settings.shared_monitors[0].local_input, Some(chosen));
    }

    #[test]
    fn a_reset_list_is_not_refilled_by_auto_select() {
        // Auto-select runs for a computer that has never chosen. A reset is a
        // choice, made by someone who is present, so the list stays empty
        // instead of the display reappearing on the next refresh.
        for scope in [ResetScope::Displays, ResetScope::Everything] {
            let mut settings = settings_after_reset(&configured_settings(), scope);
            assert!(settings.shared_monitors.is_empty());

            let only = monitor("only");
            let changes = reconcile_monitor_selection(&mut settings, std::slice::from_ref(&only));

            assert!(changes.is_empty(), "{scope:?} refilled the list");
            assert!(
                settings.shared_monitors.is_empty(),
                "{scope:?} refilled the list"
            );
        }
    }

    #[test]
    fn resetting_displays_keeps_the_pairing() {
        let settings = settings_after_reset(&configured_settings(), ResetScope::Displays);

        assert!(settings.shared_monitors.is_empty());
        assert!(
            settings.shared_monitors_chosen,
            "emptied on purpose, so auto-select must not refill it"
        );
        assert!(settings.monitor_identity_links.is_empty());
        assert!(settings.input_labels.is_empty());
        assert_eq!(settings.peers.len(), 1, "the paired host survives");
        assert!(
            settings.peers[0].inputs.is_empty(),
            "its inputs named displays that are gone"
        );
        assert_eq!(settings.shared_key, "pairing-password");
        assert_eq!(settings.host_switcher_shortcut, "Alt+Q");
    }

    #[test]
    fn resetting_everything_keeps_only_this_computer_identity() {
        let settings = settings_after_reset(&configured_settings(), ResetScope::Everything);

        assert_eq!(
            settings.local_host_id, "kept-id",
            "paired hosts name this computer by its id, so a local reset must not change it"
        );
        assert!(settings.shared_monitors.is_empty());
        assert!(settings.peers.is_empty());
        assert!(settings.shared_key.is_empty());
        assert!(settings.monitor_identity_links.is_empty());
        assert_eq!(
            settings.host_switcher_shortcut,
            AppSettings::default().host_switcher_shortcut
        );
    }

    #[test]
    fn a_withdrawn_claim_is_not_shown_as_one() {
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let links = monitor_identity::with_link(
            &monitor_identity::with_link(&[], &at_1080, Some(&at_4k), 10),
            &at_1080,
            None,
            20,
        );
        let settings = AppSettings {
            monitor_identity_links: links,
            ..AppSettings::default()
        };

        assert!(monitor_identity_claims(&settings).is_empty());
    }

    #[test]
    fn a_claim_is_shown_with_keys_that_withdraw_it() {
        // Neither identity is shared or present here, which is exactly the
        // state a claim left behind by earlier testing sits in.
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let settings = AppSettings {
            monitor_identity_links: monitor_identity::with_link(&[], &at_1080, Some(&at_4k), 10),
            ..AppSettings::default()
        };

        let claims = monitor_identity_claims(&settings);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].alias_key, monitor_key(&at_1080));
        assert_eq!(claims[0].primary_label, "MSI / 3CF0");
        assert_eq!(
            fingerprint_for_ui_id(&settings, &claims[0].alias_key),
            Ok(at_1080)
        );
    }

    #[test]
    fn a_shared_display_is_named_by_its_own_key_so_it_can_be_removed_while_absent() {
        // Removal used to enumerate and fail when the display was not present,
        // which left exactly the displays a user wants to drop — asleep, on
        // another host, or reporting an identity this host no longer knows —
        // as the only ones that could not be dropped.
        let absent = monitor("disconnected");
        let settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&absent)],
            ..AppSettings::default()
        };

        let key = monitor_key(&absent.fingerprint);
        assert_eq!(
            find_shared_monitor(&settings, &key).map(|selected| selected.fingerprint.clone()),
            Ok(absent.fingerprint)
        );
    }

    #[test]
    fn a_selection_stored_under_an_alias_is_not_added_a_second_time() {
        // Guards the shape that produced three byte-identical entries: the
        // stored selection was the alias, so it failed to recognise itself.
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let mut selected = SelectedMonitor::from(&monitor("shared"));
        selected.fingerprint = at_4k.clone();
        let mut present = monitor("shared");
        present.fingerprint = at_4k.clone();
        let links = monitor_identity::with_link(&[], &at_4k, Some(&at_1080), 10);

        assert!(
            is_selected_display(&links, &selected, &present),
            "a display stored under an alias must recognise itself"
        );
    }

    #[test]
    fn a_merged_identity_is_recognised_as_the_shared_display_everywhere() {
        // Reconciling kept the selection but the dashboard still reported it as
        // missing, so the display read as "not found" right after a merge.
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let mut selected = SelectedMonitor::from(&monitor("shared"));
        selected.fingerprint = at_1080.clone();
        let mut present = monitor("shared");
        present.fingerprint = at_4k.clone();
        let links = monitor_identity::with_link(&[], &at_4k, Some(&at_1080), 10);

        assert!(is_selected_display(&links, &selected, &present));
        assert!(!is_selected_display(
            &links,
            &selected,
            &monitor("somebody-else")
        ));
    }

    #[test]
    fn a_merged_identity_keeps_the_shared_display_available() {
        // The MSI MPG 274U publishes MSI:3CF0 at 4K and MSI:7CF0 at 1080. Once
        // the user says they are one panel, the display stays usable in both.
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let mut selected = SelectedMonitor::from(&monitor("shared"));
        selected.fingerprint = at_4k.clone();
        let mut present = monitor("shared");
        present.fingerprint = at_1080.clone();
        let mut settings = AppSettings {
            shared_monitors: vec![selected],
            monitor_identity_links: monitor_identity::with_link(&[], &at_1080, Some(&at_4k), 10),
            ..AppSettings::default()
        };

        let changes = reconcile_monitor_selection(&mut settings, std::slice::from_ref(&present));

        assert!(changes.is_empty());
        assert_eq!(
            settings.shared_monitors[0].fingerprint, at_4k,
            "the stored identity stays put so paired hosts keep naming the same display"
        );
    }

    #[test]
    fn an_unmerged_identity_of_the_same_model_is_still_a_different_display() {
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let mut selected = SelectedMonitor::from(&monitor("shared"));
        selected.fingerprint = at_4k;
        let mut present = monitor("shared");
        present.fingerprint = at_1080;
        let mut settings = AppSettings {
            shared_monitors: vec![selected.clone()],
            ..AppSettings::default()
        };

        let changes = reconcile_monitor_selection(&mut settings, std::slice::from_ref(&present));

        assert!(changes.is_empty());
        assert_eq!(settings.shared_monitors, vec![selected]);
    }

    #[test]
    fn a_merged_identity_names_the_same_display_to_a_paired_host() {
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let mut selected = SelectedMonitor::from(&monitor("shared"));
        selected.fingerprint = at_4k.clone();
        let monitors = [selected];
        let links = monitor_identity::with_link(&[], &at_1080, Some(&at_4k), 10);

        assert_eq!(
            shared_monitor_index_for_peer(&monitors, &links, &at_1080),
            Some(0)
        );
        assert_eq!(
            shared_monitor_index_for_peer(
                &monitors,
                &links,
                &MonitorFingerprint::new("ACR", "0725", Some("576726074".to_owned()))
            ),
            None
        );
    }

    /// Windows reads the EDID's serial-text descriptor and macOS its 32-bit
    /// number, so one Acer VG252Q shared between these computers arrives from
    /// the Mac under a serial this host will never read from it. Matching on
    /// the model alone is what gets the switch onto the right panel.
    fn acer(serial: &str) -> MonitorFingerprint {
        MonitorFingerprint::new("ACR", "0725", Some(serial.to_owned()))
    }

    #[test]
    fn one_display_two_hosts_read_different_serials_from_still_resolves() {
        let mut selected = SelectedMonitor::from(&monitor("shared"));
        selected.fingerprint = acer("TH6TT0028525");
        let monitors = [selected];

        assert!(!monitor_identity::same_identity(
            &monitors[0].fingerprint,
            &acer("576726074")
        ));
        assert_eq!(
            shared_monitor_index_for_peer(&monitors, &[], &acer("576726074")),
            Some(0)
        );
    }

    #[test]
    fn two_of_one_model_shared_refuse_a_peer_rather_than_guess() {
        let mut first = SelectedMonitor::from(&monitor("shared"));
        first.fingerprint = acer("TH6TT0028525");
        let mut second = SelectedMonitor::from(&monitor("other"));
        second.fingerprint = acer("SECOND-UNIT");

        assert_eq!(
            shared_monitor_index_for_peer(&[first, second], &[], &acer("576726074")),
            None
        );
    }

    #[test]
    fn merging_moves_a_paired_host_input_onto_the_display_it_joins() {
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let input = DisplayInput::new(7).unwrap();
        let mut peer = peer_using_input("ITX-PC", &monitor("shared"), 1);
        peer.inputs.clear();
        peer.set_input_for(&at_1080, Some(input));
        let mut settings = AppSettings {
            peers: vec![peer],
            ..AppSettings::default()
        };

        adopt_alias_settings(&mut settings, &at_1080, &at_4k);

        assert_eq!(settings.peers[0].input_for(&at_4k), Some(input));
        assert_eq!(settings.peers[0].input_for(&at_1080), None);
    }

    #[test]
    fn merging_never_overwrites_an_input_already_set_for_the_display_it_joins() {
        let at_4k = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let at_1080 = MonitorFingerprint::new("MSI", "7CF0", None::<String>);
        let kept = DisplayInput::new(7).unwrap();
        let mut peer = peer_using_input("ITX-PC", &monitor("shared"), 1);
        peer.inputs.clear();
        peer.set_input_for(&at_4k, Some(kept));
        peer.set_input_for(&at_1080, Some(DisplayInput::new(8).unwrap()));
        let mut settings = AppSettings {
            peers: vec![peer],
            ..AppSettings::default()
        };

        adopt_alias_settings(&mut settings, &at_1080, &at_4k);

        assert_eq!(settings.peers[0].input_for(&at_4k), Some(kept));
    }

    #[test]
    fn a_selection_whose_serial_stops_being_readable_is_kept() {
        // A display re-enumerated after a mode switch can report the same model
        // with no serial at all (an unreadable EDID on macOS, an empty WMI
        // SerialNumberID on Windows). That is a failure to identify it, not
        // proof it is gone, so the user's selection has to survive it.
        let selected_monitor = monitor("shared");
        let mut reappeared = selected_monitor.clone();
        reappeared.fingerprint = MonitorFingerprint::new("ACM", "shared", None::<String>);
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&selected_monitor)],
            ..AppSettings::default()
        };

        let changes = reconcile_monitor_selection(&mut settings, std::slice::from_ref(&reappeared));

        assert!(changes.is_empty());
        assert_eq!(
            settings.shared_monitors,
            vec![SelectedMonitor::from(&selected_monitor)],
            "the stored identity must not be rewritten from an unproven reading"
        );
    }

    #[test]
    fn a_selection_that_cannot_be_identified_is_kept_rather_than_reassigned() {
        let previous = monitor("disconnected");
        let replacement = monitor("replacement");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&previous)],
            ..AppSettings::default()
        };

        let changes =
            reconcile_monitor_selection(&mut settings, std::slice::from_ref(&replacement));

        assert!(changes.is_empty());
        assert_eq!(
            settings.shared_monitors,
            vec![SelectedMonitor::from(&previous)],
            "an unidentified selection is never reassigned to a different monitor"
        );
    }

    fn shared_with_guessed_inputs(display: &MonitorDescriptor, upper: u32) -> SelectedMonitor {
        let mut selected = SelectedMonitor::from(display);
        selected.vendor_indexed_inputs = true;
        selected.supported_inputs = Some(
            (1..=upper)
                .filter_map(|value| DisplayInput::new(value).ok())
                .collect(),
        );
        selected
    }

    #[test]
    fn a_confirmed_port_outside_a_guessed_list_is_still_adopted() {
        // This host never saw the input its display accepts, so it was given
        // the private 1..=max range instead — a range, not a list of inputs.
        // The other host read its own port from the display while on screen,
        // and dropping that leaves its port blank with nothing said.
        let display = monitor("shared");
        let mut settings = AppSettings {
            shared_monitors: vec![shared_with_guessed_inputs(&display, 14)],
            peers: vec![peer_using_input("ITX-PC", &display, 8)],
            ..AppSettings::default()
        };
        settings.peers[0].inputs.clear();
        let peer_id = settings.peers[0].id.clone();
        let route = AgentDisplayRoute {
            monitor: display.fingerprint.clone(),
            input: DisplayInput::new(15).unwrap(),
            confirmed: true,
        };

        assert_eq!(
            apply_verified_peer_route(&mut settings, &peer_id, route),
            PeerRouteOutcome::Applied
        );
        assert_eq!(
            settings.peers[0].input_for(&display.fingerprint),
            Some(DisplayInput::new(15).unwrap())
        );
    }

    #[test]
    fn a_confirmed_vendor_port_is_adopted_when_this_host_never_read_the_display() {
        // A display busy showing the other computer will not give up its
        // capabilities, so this host has only the standard MCCS codes — which
        // describe no particular display and do not include the Type-C input
        // this one calls 8. That is how a port read on the other computer was
        // thrown away here.
        let display = monitor("shared");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&display)],
            peers: vec![peer_using_input("ITX-PC", &display, 7)],
            ..AppSettings::default()
        };
        settings.shared_monitors[0].supported_inputs = None;
        settings.shared_monitors[0].local_input = None;
        settings.peers[0].inputs.clear();
        let peer_id = settings.peers[0].id.clone();
        let vendor_port = DisplayInput::new(8).unwrap();
        assert!(!common_input_sources().contains(&vendor_port));

        assert_eq!(
            apply_verified_peer_route(
                &mut settings,
                &peer_id,
                AgentDisplayRoute {
                    monitor: display.fingerprint.clone(),
                    input: vendor_port,
                    confirmed: true,
                }
            ),
            PeerRouteOutcome::Applied
        );
        assert_eq!(
            settings.peers[0].input_for(&display.fingerprint),
            Some(vendor_port)
        );
    }

    #[test]
    fn an_unconfirmed_port_outside_a_guessed_list_is_still_refused() {
        // Unconfirmed means the host was not on screen, so its reading is of
        // whoever was: it has no more standing than the guess.
        let display = monitor("shared");
        let mut settings = AppSettings {
            shared_monitors: vec![shared_with_guessed_inputs(&display, 14)],
            peers: vec![peer_using_input("ITX-PC", &display, 8)],
            ..AppSettings::default()
        };
        settings.peers[0].inputs.clear();
        let peer_id = settings.peers[0].id.clone();
        let route = AgentDisplayRoute {
            monitor: display.fingerprint.clone(),
            input: DisplayInput::new(15).unwrap(),
            confirmed: false,
        };

        assert_eq!(
            apply_verified_peer_route(&mut settings, &peer_id, route),
            PeerRouteOutcome::Unsupported
        );
    }

    #[test]
    fn a_paired_host_input_note_names_this_host_shared_display_input() {
        let ours = MonitorFingerprint::new("MSI", "3CF0", None::<String>);
        let theirs = MonitorFingerprint::new("MSI", "3CF0", Some("PC-SERIAL".to_owned()));
        let mut selected = SelectedMonitor::from(&monitor("mpg"));
        selected.fingerprint = ours.clone();
        selected.vendor_indexed_inputs = true;
        let mut settings = AppSettings {
            shared_monitors: vec![selected.clone()],
            ..AppSettings::default()
        };
        let input = DisplayInput::new(8).unwrap();
        let notice = [InputLabel {
            monitor: theirs,
            input,
            label: "USB-C".to_owned(),
            updated_at_ms: 10,
        }];

        assert!(adopt_input_labels(&mut settings, &notice, LEDGER_NOW_MS));
        assert!(!adopt_input_labels(&mut settings, &notice, LEDGER_NOW_MS));

        assert_eq!(settings.input_labels[0].monitor, ours);
        let name = noted_input_label(&settings, &selected, input);
        assert!(name.starts_with("USB-C"));
        assert!(name.contains(&input_label(true, input)));
        assert_eq!(
            noted_input_label(&settings, &selected, DisplayInput::new(7).unwrap()),
            input_label(true, DisplayInput::new(7).unwrap())
        );
    }

    #[test]
    fn keeps_a_missing_selection_that_a_paired_host_is_showing() {
        let switched_away = monitor("switched-away");
        let stays = monitor("stays");
        let mut selected = SelectedMonitor::from(&switched_away);
        selected.active_route = Some("peer-a".to_owned());
        let mut settings = AppSettings {
            shared_monitors: vec![selected.clone(), SelectedMonitor::from(&stays)],
            peers: vec![peer_using_input("peer-a", &switched_away, 0x0f)],
            ..AppSettings::default()
        };
        let monitors = [stays.clone()];

        let changes = reconcile_monitor_selection(&mut settings, &monitors);

        assert!(changes.is_empty());
        assert_eq!(
            settings.shared_monitors,
            vec![selected, SelectedMonitor::from(&stays)]
        );
    }

    #[test]
    fn a_missing_selection_is_kept_even_when_its_active_host_is_no_longer_paired() {
        let disconnected = monitor("disconnected");
        let mut selected = SelectedMonitor::from(&disconnected);
        selected.active_route = Some("removed-peer".to_owned());
        let mut settings = AppSettings {
            shared_monitors: vec![selected.clone()],
            ..AppSettings::default()
        };

        let changes = reconcile_monitor_selection(&mut settings, &[]);

        assert!(changes.is_empty());
        assert_eq!(settings.shared_monitors, vec![selected]);
    }

    #[test]
    fn never_guesses_between_multiple_controllable_monitors() {
        let mut settings = AppSettings::default();
        let monitors = [monitor("first"), monitor("second")];

        assert!(reconcile_monitor_selection(&mut settings, &monitors).is_empty());
        assert!(settings.shared_monitors.is_empty());
    }

    #[test]
    fn missing_selection_is_kept_and_not_replaced_when_multiple_candidates_remain() {
        let disconnected = monitor("disconnected");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&disconnected)],
            ..AppSettings::default()
        };
        let monitors = [monitor("first"), monitor("second")];

        let changes = reconcile_monitor_selection(&mut settings, &monitors);

        assert!(changes.is_empty());
        assert_eq!(
            settings.shared_monitors,
            vec![SelectedMonitor::from(&disconnected)]
        );
    }

    #[test]
    fn one_monitor_disappearing_does_not_affect_another_independently_selected_monitor() {
        let stays = monitor("stays");
        let disconnected = monitor("disconnected");
        let mut settings = AppSettings {
            shared_monitors: vec![
                SelectedMonitor::from(&stays),
                SelectedMonitor::from(&disconnected),
            ],
            ..AppSettings::default()
        };
        let monitors = [stays.clone()];

        let changes = reconcile_monitor_selection(&mut settings, &monitors);

        assert!(changes.is_empty());
        assert_eq!(
            settings.shared_monitors,
            vec![
                SelectedMonitor::from(&stays),
                SelectedMonitor::from(&disconnected),
            ]
        );
    }

    #[test]
    fn auto_select_never_adds_a_new_monitor_once_one_is_already_selected() {
        let already_selected = monitor("already-selected");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&already_selected)],
            ..AppSettings::default()
        };
        let monitors = [already_selected.clone(), monitor("new-arrival")];

        let changes = reconcile_monitor_selection(&mut settings, &monitors);

        assert!(changes.is_empty());
        assert_eq!(
            settings.shared_monitors,
            vec![SelectedMonitor::from(&already_selected)]
        );
    }

    #[test]
    fn metadata_refresh_for_one_selected_monitor_does_not_touch_another() {
        let mut refreshed = monitor("refreshed");
        let unaffected = monitor("unaffected");
        let mut settings = AppSettings {
            shared_monitors: vec![
                SelectedMonitor::from(&refreshed),
                SelectedMonitor::from(&unaffected),
            ],
            ..AppSettings::default()
        };
        refreshed.name = "renamed".to_owned();
        let monitors = [refreshed.clone(), unaffected.clone()];

        let changes = reconcile_monitor_selection(&mut settings, &monitors);

        assert_eq!(
            changes,
            vec![MonitorSelectionChange::RefreshedMetadata {
                name: "renamed".to_owned()
            }]
        );
        assert_eq!(settings.shared_monitors[0].name, "renamed");
        assert_eq!(
            settings.shared_monitors[1],
            SelectedMonitor::from(&unaffected)
        );
    }

    #[test]
    fn automatic_offline_fallback_explains_the_black_screen_risk() {
        let target = monitor("external");
        let input = DisplayInput::new(0x11).unwrap();
        let preparation = NetworkPreparation::Unavailable {
            wake_sent: true,
            reason: "Agent 沒有回應".to_owned(),
        };
        let result = outcome_result(
            SwitchOutcome::AlreadySelected { target, input },
            &preparation,
            |input| input_label(false, input),
        );

        assert!(result.warning);
        assert!(result.peer_woken);
        assert!(result
            .detail
            .contains("local DDC/CI was selected automatically"));
        assert!(result.detail.contains("temporarily blank"));
    }

    #[test]
    fn locale_detection_uses_traditional_chinese_and_falls_back_to_english() {
        assert_eq!(locale_from_tag("zh-TW"), UiLocale::TraditionalChinese);
        assert_eq!(locale_from_tag("zh-Hant-HK"), UiLocale::TraditionalChinese);
        assert_eq!(locale_from_tag("en-US"), UiLocale::English);
        assert_eq!(locale_from_tag("ja-JP"), UiLocale::English);
        assert_eq!(locale_from_tag("zh-CN"), UiLocale::English);
    }

    #[test]
    fn transient_ddc_failure_does_not_replace_a_still_detected_selection() {
        let selected = monitor("selected");
        let replacement = monitor("replacement");
        let mut settings = AppSettings {
            shared_monitors: vec![SelectedMonitor::from(&selected)],
            ..AppSettings::default()
        };

        // The selection is detected but unreadable, so it is absent from the
        // controllable list while another monitor is present in it.
        assert!(
            reconcile_monitor_selection(&mut settings, std::slice::from_ref(&replacement))
                .is_empty()
        );
        assert_eq!(
            settings.shared_monitors,
            vec![SelectedMonitor::from(&selected)]
        );
    }

    #[test]
    fn a_newer_host_input_tombstone_clears_and_blocks_stale_resurrection() {
        let shared = monitor("shared");
        let old_input = DisplayInput::new(0x11).unwrap();
        let mut settings = routed_settings(&shared, 0x08, 0x11, None);
        settings.local_host_id = "this-host".to_owned();
        settings.host_inputs = vec![host_input("peer", &shared, 0x11, 10)];

        let cleared = AgentHostInput {
            input: None,
            ..host_input("peer", &shared, 0x11, 20)
        };
        assert_eq!(
            apply_host_input_update(&mut settings, &cleared, LEDGER_NOW_MS),
            Some(true)
        );
        assert_eq!(settings.peers[0].input_for(&shared.fingerprint), None);

        let stale = host_input("peer", &shared, 0x11, 15);
        assert_eq!(
            apply_host_input_update(&mut settings, &stale, LEDGER_NOW_MS),
            None
        );
        assert_eq!(settings.peers[0].input_for(&shared.fingerprint), None);
        assert_eq!(settings.host_inputs[0].input, None);
        assert_ne!(settings.host_inputs[0].input, Some(old_input));
    }

    /// Later than every revision the host input tests write.
    const LEDGER_NOW_MS: u64 = 1_000_000;

    fn host_input(
        host_id: &str,
        shared: &MonitorDescriptor,
        input: u32,
        at: u64,
    ) -> AgentHostInput {
        AgentHostInput {
            host_id: host_id.to_owned(),
            monitor: shared.fingerprint.clone(),
            input: DisplayInput::new(input).ok(),
            updated_at_ms: at,
        }
    }

    #[test]
    fn a_host_input_for_an_unknown_host_is_rejected_even_when_it_takes_a_port() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, None);
        settings.local_host_id = "this-host".to_owned();

        // 0x07 belongs to "peer", so accepting this would displace it.
        let stranger = host_input("stranger", &shared, 0x07, 500);

        assert_eq!(
            apply_host_input_update(&mut settings, &stranger, LEDGER_NOW_MS),
            None
        );
        assert_eq!(
            settings.peers[0].input_for(&shared.fingerprint),
            DisplayInput::new(0x07).ok()
        );
        assert!(settings.host_inputs.is_empty());
    }

    #[test]
    fn a_host_input_that_displaces_another_peer_still_assigns_the_port() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, None);
        settings.local_host_id = "this-host".to_owned();
        settings
            .peers
            .push(peer_using_input("other-peer", &shared, 0x0f));

        let moved = host_input("other-peer", &shared, 0x07, 500);

        assert_eq!(
            apply_host_input_update(&mut settings, &moved, LEDGER_NOW_MS),
            Some(true)
        );
        assert_eq!(settings.peers[0].input_for(&shared.fingerprint), None);
        assert_eq!(
            settings.peers[1].input_for(&shared.fingerprint),
            DisplayInput::new(0x07).ok()
        );
    }

    /// Only this host says which port it is on, from its own display reading
    /// or its own user's choice. A paired host may neither set it nor take it.
    #[test]
    fn a_paired_host_cannot_set_or_take_this_hosts_port() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, None);
        settings.local_host_id = "this-host".to_owned();

        let set_ours = host_input("this-host", &shared, 0x0f, 500);
        let take_ours = host_input("peer", &shared, 0x08, 500);

        assert_eq!(
            apply_host_input_update(&mut settings, &set_ours, LEDGER_NOW_MS),
            None
        );
        assert_eq!(
            apply_host_input_update(&mut settings, &take_ours, LEDGER_NOW_MS),
            None
        );
        assert_eq!(
            settings.shared_monitors[0].local_input,
            DisplayInput::new(0x08).ok()
        );
        assert_eq!(
            settings.peers[0].input_for(&shared.fingerprint),
            DisplayInput::new(0x07).ok()
        );
        assert!(settings.host_inputs.is_empty());
    }

    #[test]
    fn a_display_reset_clears_only_this_hosts_port() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, None);
        settings.local_host_id = "this-host".to_owned();
        settings.host_inputs = vec![host_input("peer", &shared, 0x07, 10)];

        // This host's port predates the ledger, so it has no entry yet.
        let ledger = host_inputs_after_display_reset(&settings, LEDGER_NOW_MS);

        assert_eq!(
            ledger,
            vec![AgentHostInput {
                input: None,
                ..host_input("this-host", &shared, 0x08, LEDGER_NOW_MS)
            }]
        );
    }

    #[test]
    fn a_paired_host_saved_with_a_public_address_is_not_contacted() {
        let shared = monitor("shared");
        let mut peer = peer_using_input("peer", &shared, 0x07);
        assert!(route_endpoint(&peer).is_ok());

        peer.address = "8.8.8.8".to_owned();
        assert!(route_endpoint(&peer).is_err());
    }

    #[test]
    fn a_paired_hosts_wake_address_comes_only_from_its_signed_reply() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, None);
        settings.peers[0].mac_address = "11:22:33:44:55:66".to_owned();

        // Discovery is unauthenticated, so it no longer touches the address.
        upsert_discovered_peer(&mut settings, &discovered("peer", "192.168.1.30", 47_653));
        assert_eq!(settings.peers[0].mac_address, "11:22:33:44:55:66");

        let malformed = AgentResponse {
            mac_address: Some("not a mac".to_owned()),
            ..AgentResponse::default()
        };
        assert!(!adopt_peer_mac_address(&mut settings, "peer", &malformed));
        assert_eq!(settings.peers[0].mac_address, "11:22:33:44:55:66");

        let signed = AgentResponse {
            mac_address: Some("aa-bb-cc-dd-ee-ff".to_owned()),
            ..AgentResponse::default()
        };
        assert!(adopt_peer_mac_address(&mut settings, "peer", &signed));
        assert_eq!(settings.peers[0].mac_address, "AA:BB:CC:DD:EE:FF");
    }

    /// A reply over the agent's 8 KB read limit is cut short and fails to
    /// parse, and a host whose Ping fails can no longer be switched to. The
    /// catch-up data goes first; what switching needs stays.
    #[test]
    fn an_oversized_agent_reply_sheds_catch_up_data_and_keeps_its_routes() {
        let shared = monitor("shared");
        let route = AgentDisplayRoute {
            monitor: shared.fingerprint.clone(),
            input: DisplayInput::new(0x0f).unwrap(),
            confirmed: true,
        };
        let links = (0..200)
            .map(|index| MonitorIdentityLink {
                alias: monitor(&format!("alias-{index}")).fingerprint,
                primary: Some(shared.fingerprint.clone()),
                updated_at_ms: 10,
            })
            .collect();
        let reply = AgentResponse {
            ready: true,
            display_routes: vec![route.clone()],
            host_order: vec!["peer".to_owned()],
            monitor_identity_links: links,
            ..AgentResponse::default()
        };

        let fitted = fit_agent_reply(reply);

        assert!(serde_json::to_vec(&fitted).unwrap().len() <= AGENT_REPLY_BUDGET_BYTES);
        assert_eq!(fitted.display_routes, vec![route]);
        assert!(fitted.monitor_identity_links.is_empty());
        assert_eq!(fitted.host_order, vec!["peer"], "shed more than needed");
    }

    #[test]
    fn unsharing_a_display_forgets_every_port_on_it() {
        let shared = monitor("shared");
        let other = monitor("other");
        let mut settings = routed_settings(&shared, 0x08, 0x07, None);
        settings.local_host_id = "this-host".to_owned();
        settings.host_inputs = vec![
            host_input("this-host", &shared, 0x08, 10),
            host_input("peer", &shared, 0x07, 10),
            host_input("peer", &other, 0x07, 10),
        ];

        unshare_monitor(&mut settings, &shared.fingerprint);

        assert!(settings.shared_monitors.is_empty());
        assert_eq!(settings.peers[0].input_for(&shared.fingerprint), None);
        assert_eq!(
            settings.host_inputs,
            vec![host_input("peer", &other, 0x07, 10)]
        );
    }

    #[test]
    fn wake_packets_are_broadcast_only_on_a_local_network() {
        let with_broadcast = |address: &str| AppSettings {
            broadcast_ip: address.to_owned(),
            ..AppSettings::default()
        };

        assert!(wake_broadcast_address(&with_broadcast("255.255.255.255")).is_ok());
        assert!(wake_broadcast_address(&with_broadcast("192.168.1.255")).is_ok());
        assert!(wake_broadcast_address(&with_broadcast("8.8.8.8")).is_err());
        assert!(validate_settings(&with_broadcast("8.8.8.8")).is_err());
    }

    #[test]
    fn the_settings_form_cannot_change_paired_hosts() {
        let shared = monitor("shared");
        let saved = routed_settings(&shared, 0x08, 0x07, None);
        let mut submitted = saved.clone();
        submitted.peers[0].set_input_for(&shared.fingerprint, DisplayInput::new(0x0f).ok());
        submitted.peers[0].address = "10.0.0.99".to_owned();
        submitted.wait_seconds = saved.wait_seconds + 5;

        let accepted = settings_from_form(submitted, &saved);

        assert_eq!(accepted.peers, saved.peers);
        assert_eq!(accepted.wait_seconds, saved.wait_seconds + 5);
    }

    #[test]
    fn a_host_input_dated_past_the_clock_tolerance_is_rejected() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, None);
        settings.local_host_id = "this-host".to_owned();

        let poisoned = host_input(
            "peer",
            &shared,
            0x0f,
            LEDGER_NOW_MS + MAX_REVISION_AHEAD_MS + 1,
        );
        assert_eq!(
            apply_host_input_update(&mut settings, &poisoned, LEDGER_NOW_MS),
            None
        );
        assert!(settings.host_inputs.is_empty());

        let drifted = host_input("peer", &shared, 0x0f, LEDGER_NOW_MS + MAX_REVISION_AHEAD_MS);
        assert_eq!(
            apply_host_input_update(&mut settings, &drifted, LEDGER_NOW_MS),
            Some(true)
        );
    }

    #[test]
    fn an_oversized_host_input_batch_is_rejected_whole() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, None);
        settings.local_host_id = "this-host".to_owned();
        let flood: Vec<AgentHostInput> = (0..=MAX_SHARED_HOST_INPUTS as u64)
            .map(|index| host_input("peer", &shared, 0x0f, 100 + index))
            .collect();

        assert_eq!(
            apply_host_input_updates(&mut settings, &flood, LEDGER_NOW_MS),
            None
        );
        assert!(settings.host_inputs.is_empty());

        assert_eq!(
            apply_host_input_updates(&mut settings, &flood[..1], LEDGER_NOW_MS),
            Some(true)
        );
    }

    #[test]
    fn forgetting_a_peer_drops_its_host_input_history() {
        let shared = monitor("shared");
        let mut settings = routed_settings(&shared, 0x08, 0x07, None);
        settings.local_host_id = "this-host".to_owned();
        settings.host_inputs = vec![
            host_input("this-host", &shared, 0x08, 10),
            host_input("peer", &shared, 0x07, 10),
        ];

        forget_peer(&mut settings, "peer");

        assert!(settings.peers.is_empty());
        assert_eq!(settings.host_inputs.len(), 1);
        assert_eq!(settings.host_inputs[0].host_id, "this-host");
    }

    #[test]
    fn durable_notice_streams_coalesce_only_matching_state() {
        let monitor_a = monitor("a").fingerprint;
        let monitor_b = monitor("b").fingerprint;
        let active_a = AgentAction::ActiveInputChanged {
            monitor: monitor_a.clone(),
            input: DisplayInput::new(0x11).unwrap(),
        };
        let active_a_newer = AgentAction::ActiveInputChanged {
            monitor: monitor_a,
            input: DisplayInput::new(0x12).unwrap(),
        };
        let active_b = AgentAction::ActiveInputChanged {
            monitor: monitor_b,
            input: DisplayInput::new(0x11).unwrap(),
        };

        assert!(notice_supersedes(&active_a, &active_a_newer));
        assert!(!notice_supersedes(&active_a, &active_b));
        assert!(notice_supersedes(
            &AgentAction::HostInputsChanged {
                assignments: Vec::new()
            },
            &AgentAction::HostInputsChanged {
                assignments: Vec::new()
            }
        ));
        let one_off_confirmation = AgentAction::LocalInputConfirmed {
            host_id: "host".to_owned(),
            monitor: monitor("shared").fingerprint,
            input: DisplayInput::new(0x11).unwrap(),
        };
        let full_snapshot = AgentAction::HostInputsChanged {
            assignments: Vec::new(),
        };
        assert!(notice_supersedes(&full_snapshot, &one_off_confirmation));
        assert!(!notice_supersedes(&one_off_confirmation, &full_snapshot));
    }

    fn queued(id: &str, peer_id: &str, next_attempt_at_ms: u64) -> PendingPeerNotice {
        PendingPeerNotice {
            id: id.to_owned(),
            peer_id: peer_id.to_owned(),
            action: AgentAction::Ping,
            attempts: 0,
            next_attempt_at_ms,
        }
    }

    #[test]
    fn each_peer_is_sent_its_oldest_due_notice_one_at_a_time() {
        let notices = [
            queued("a1", "peer-a", 10),
            queued("a2", "peer-a", 0),
            queued("b1", "peer-b", 500),
            queued("b2", "peer-b", 0),
            queued("c1", "peer-c", 0),
            queued("d1", "peer-d", 0),
        ];
        let in_flight = std::collections::HashSet::from(["peer-d".to_owned()]);

        // peer-b's oldest notice is backing off after failing, and must not
        // hold back its newer one; peer-d already has a request in flight.
        assert_eq!(
            due_notice_ids(&notices, &in_flight, 100),
            vec!["a1", "b2", "c1"]
        );
    }

    #[test]
    fn a_notice_is_dropped_once_it_has_used_every_attempt() {
        let mut settings = AppSettings {
            pending_peer_notices: vec![PendingPeerNotice {
                attempts: MAX_NOTICE_ATTEMPTS - 2,
                ..queued("n", "peer", 0)
            }],
            ..AppSettings::default()
        };

        assert!(!record_failed_notice_attempt(&mut settings, "n", 1_000));
        assert_eq!(
            settings.pending_peer_notices[0].attempts,
            MAX_NOTICE_ATTEMPTS - 1
        );
        assert_eq!(
            settings.pending_peer_notices[0].next_attempt_at_ms,
            1_000 + retry_delay_ms(MAX_NOTICE_ATTEMPTS - 1)
        );

        assert!(record_failed_notice_attempt(&mut settings, "n", 2_000));
        assert!(settings.pending_peer_notices.is_empty());
    }

    #[test]
    fn notice_retry_backoff_is_bounded() {
        assert_eq!(retry_delay_ms(1), 2_000);
        assert_eq!(retry_delay_ms(2), 4_000);
        assert_eq!(retry_delay_ms(30), 300_000);
    }
}
