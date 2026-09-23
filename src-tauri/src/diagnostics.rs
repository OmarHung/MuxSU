//! Opt-in diagnostics: a log kept on disk, a snapshot of what this host sees,
//! and a report built from both with everything that identifies a person or a
//! network taken out.
//!
//! Most faults this app has had were two hosts disagreeing about one display —
//! one reads a serial number the other cannot, one holds a merge the other
//! resolves differently. Neither host's view alone shows that, so a report
//! carries the paired hosts' snapshots too, each built and redacted on the host
//! it describes and only when that host's user has allowed diagnostics.
//!
//! Serial numbers are replaced by a short hash rather than removed: "both hosts
//! read the same serial", "one host reads none" and "the serials differ" are
//! exactly what a report has to show. The hash is keyed by the stretched
//! pairing key, which every host in a group shares, so their values compare
//! while anyone reading a report cannot match them against guessed serials.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use muxsu_core::{DisplayInput, MonitorConnection, MonitorDescriptor, MonitorFingerprint};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::{monitor_identity, AppSettings};

const LOG_FILE_PREFIX: &str = "muxsu";
const LOG_FILE_SUFFIX: &str = "log";
/// Days of log kept on disk.
const KEPT_LOG_FILES: usize = 7;
/// What the file log records unless `RUST_LOG` says otherwise. The console
/// log keeps its old default so development output does not change.
const FILE_LOG_FILTER: &str = "warn,muxsu_app_lib=info,muxsu_core=info";
/// Most log lines one report carries, newest last.
const REPORT_LOG_LINES: usize = 400;
/// Longest single log line kept in a report.
const REPORT_LOG_LINE_CHARS: usize = 600;
/// Largest snapshot a paired host sends back. The agent drops any reply over
/// 8 KiB, and the snapshot travels as an escaped string inside one.
pub const PEER_SNAPSHOT_MAX_BYTES: usize = 5 * 1024;
/// Least time between two reports sent without the user asking, so a display
/// that fails every switch does not send one per click.
const AUTOMATIC_REPORT_INTERVAL: Duration = Duration::from_secs(30 * 60);

static LOG_DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
static LAST_INVENTORY: Mutex<Option<ObservedInventory>> = Mutex::new(None);
static PENDING_REPORT: Mutex<Option<PreparedReport>> = Mutex::new(None);
static LAST_AUTOMATIC_REPORT: Mutex<Option<Instant>> = Mutex::new(None);

/// Starts logging to the console, as before, and to a daily file in
/// `log_dir` when one is available. Without a file the app runs as it always
/// has; it only has less to put in a report.
pub fn init_logging(log_dir: Option<&Path>) {
    let console = tracing_subscriber::fmt::layer()
        .with_target(false)
        .compact()
        .with_filter(EnvFilter::from_default_env());
    let file = log_dir.and_then(|directory| {
        // The appender prunes old files as it starts and complains on stderr
        // when the directory is not there yet, which it never is the first time.
        fs::create_dir_all(directory).ok()?;
        let appender = tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix(LOG_FILE_PREFIX)
            .filename_suffix(LOG_FILE_SUFFIX)
            .max_log_files(KEPT_LOG_FILES)
            .build(directory)
            .ok()?;
        let _ = LOG_DIRECTORY.set(directory.to_path_buf());
        let filter =
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(FILE_LOG_FILTER));
        Some(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_target(false)
                .with_writer(appender)
                .with_filter(filter),
        )
    });
    let _ = tracing_subscriber::registry()
        .with(console)
        .with(file)
        .try_init();
}

pub fn log_directory() -> Option<&'static Path> {
    LOG_DIRECTORY.get().map(PathBuf::as_path)
}

/// Writes a panic down before the process ends.
///
/// Installed first thing in `run`, ahead of every plugin, because a panic
/// raised while they initialise happens long before `setup` reaches
/// [`init_logging`] — and with `panic = "abort"` in the release profile it
/// takes the process with it. On Windows that surfaced as a bare `0xc0000409`
/// in the event log, an app that vanished a few seconds after opening, and not
/// one line anywhere saying why.
///
/// The rolling log is used once it exists, so a panic sits with the lines
/// around it and travels with a diagnostic report. Earlier than that there is
/// no app yet to say where its log directory is, so the panic goes to a fixed
/// file in the temp directory, which is writable this early.
pub fn install_panic_logger() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        // A no-op before `init_logging`, and the usual path afterwards.
        tracing::error!(panic = %panic, "MuxSU is stopping because of a panic");
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default();
        let record = format!("---- panic at unix {seconds} ----\n{panic}\n{backtrace}\n");
        // Nothing here may fail loudly: a panic inside a panic hook aborts
        // with even less to show than the panic being reported.
        let _ = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(panic_log_path(log_directory()))
            .and_then(|mut file| std::io::Write::write_all(&mut file, record.as_bytes()));
        previous(panic);
    }));
}

/// Where [`install_panic_logger`] writes, given the log directory if logging
/// has started. Split out from the hook so both halves can be tested without
/// panicking a test process.
fn panic_log_path(directory: Option<&Path>) -> PathBuf {
    match directory {
        Some(directory) => directory.join(format!("{LOG_FILE_PREFIX}-panic.{LOG_FILE_SUFFIX}")),
        None => std::env::temp_dir().join(format!("{LOG_FILE_PREFIX}-panic.{LOG_FILE_SUFFIX}")),
    }
}

/// The last lines of the newest log files, oldest first.
fn recent_log_lines(directory: &Path, limit: usize) -> Vec<String> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut files = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with(LOG_FILE_PREFIX) && name.ends_with(LOG_FILE_SUFFIX)
                })
        })
        .collect::<Vec<_>>();
    // Daily files are named by date, so name order is age order.
    files.sort();
    let mut lines = Vec::new();
    for file in files.iter().rev() {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        let mut older = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.chars().take(REPORT_LOG_LINE_CHARS).collect::<String>())
            .collect::<Vec<_>>();
        older.append(&mut lines);
        lines = older;
        if lines.len() >= limit {
            break;
        }
    }
    let excess = lines.len().saturating_sub(limit);
    lines.split_off(excess)
}

/// What the last display scan saw, kept so a report — and a paired host asking
/// for one — never has to wait on DDC/CI.
struct ObservedInventory {
    at: Instant,
    monitors: Vec<(MonitorDescriptor, Option<DisplayInput>)>,
}

/// Records the displays a scan found and the input each controllable one
/// reported. Called from every dashboard scan.
pub fn remember_inventory(
    detected: &[MonitorDescriptor],
    current_inputs: &HashMap<muxsu_core::MonitorId, DisplayInput>,
) {
    let monitors = detected
        .iter()
        .map(|monitor| (monitor.clone(), current_inputs.get(&monitor.id).copied()))
        .collect();
    *LAST_INVENTORY
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(ObservedInventory {
        at: Instant::now(),
        monitors,
    });
}

/// A display identity with its serial number replaced by a comparable hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentitySnapshot {
    pub manufacturer: String,
    pub product: String,
    pub serial: Option<String>,
}

impl IdentitySnapshot {
    fn new(fingerprint: &MonitorFingerprint, serial_key: &[u8]) -> Self {
        Self {
            manufacturer: fingerprint.manufacturer_id.clone(),
            product: fingerprint.product_code.clone(),
            serial: fingerprint
                .serial_number
                .as_deref()
                .map(|serial| serial_hash(serial, serial_key)),
        }
    }
}

/// The key serial hashes are made with: the stretched pairing key when this
/// host is paired, so it is as costly to guess as the password itself. An
/// unpaired host has no one to compare with, so a key drawn once per run does.
fn serial_key(settings: &AppSettings) -> Vec<u8> {
    if crate::has_valid_shared_key(&settings.shared_key) {
        crate::stretched_pairing_key(&settings.shared_key).to_vec()
    } else {
        unpaired_serial_key().to_vec()
    }
}

fn unpaired_serial_key() -> &'static [u8; 32] {
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut key = [0; 32];
        if ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut key).is_err() {
            tracing::warn!("unable to draw a key for diagnostic serial hashes");
        }
        key
    })
}

/// `serial:` and the first 8 hex digits of the serial's HMAC-SHA256 under
/// `key`: enough to tell whether two hosts read the same serial, too little to
/// read it back, and not reproducible without the pairing key.
fn serial_hash(serial: &str, key: &[u8]) -> String {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(b"muxsu-serial:");
    mac.update(serial.as_bytes());
    let digest = mac.finalize().into_bytes();
    let hex = digest
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("serial:{hex}")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorSnapshot {
    pub name: String,
    pub identity: IdentitySnapshot,
    /// The shared identity this one resolves to through the user's merges.
    pub resolves_to: IdentitySnapshot,
    pub built_in: bool,
    pub controllable: bool,
    pub current_input: Option<u32>,
    pub resolution: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<MonitorConnection>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedMonitorSnapshot {
    pub name: String,
    pub identity: IdentitySnapshot,
    pub local_input: Option<u32>,
    pub supported_inputs: Option<Vec<u32>>,
    pub vendor_indexed_inputs: bool,
    /// `local`, or the paired host as `peer-N`.
    pub active_route: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityLinkSnapshot {
    pub alias: IdentitySnapshot,
    pub primary: Option<IdentitySnapshot>,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerInputSnapshot {
    pub identity: IdentitySnapshot,
    pub input: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerSnapshot {
    pub reference: String,
    pub platform: String,
    pub has_mac_address: bool,
    pub inputs: Vec<PeerInputSnapshot>,
}

/// One host's view of its displays and of the settings that decide which of
/// them is shared.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSnapshot {
    pub platform: String,
    pub app_version: String,
    /// Seconds since the displays below were scanned; `None` when this run
    /// has not scanned yet, and then `monitors` is empty.
    pub scanned_seconds_ago: Option<u64>,
    pub monitors: Vec<MonitorSnapshot>,
    pub shared_monitors: Vec<SharedMonitorSnapshot>,
    pub identity_links: Vec<IdentityLinkSnapshot>,
    pub peers: Vec<PeerSnapshot>,
    /// Set when the display list was left out to fit an agent reply.
    #[serde(default)]
    pub monitors_omitted: bool,
}

fn peer_reference(settings: &AppSettings, route: &str) -> String {
    if route == crate::host_order::LOCAL_ROUTE_ID {
        return route.to_owned();
    }
    settings
        .peers
        .iter()
        .position(|peer| peer.id == route)
        .map_or_else(
            || "peer-unknown".to_owned(),
            |index| format!("peer-{}", index + 1),
        )
}

/// This host's snapshot from `settings` and the last display scan.
pub fn host_snapshot(settings: &AppSettings) -> HostSnapshot {
    let links = &settings.monitor_identity_links;
    let key = serial_key(settings);
    let identity = |fingerprint: &MonitorFingerprint| IdentitySnapshot::new(fingerprint, &key);
    let inventory = LAST_INVENTORY
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let monitors = inventory
        .as_ref()
        .map(|inventory| {
            inventory
                .monitors
                .iter()
                .map(|(monitor, input)| MonitorSnapshot {
                    name: monitor.name.clone(),
                    identity: identity(&monitor.fingerprint),
                    resolves_to: identity(monitor_identity::primary_for(
                        links,
                        &monitor.fingerprint,
                    )),
                    built_in: monitor.built_in,
                    controllable: input.is_some(),
                    current_input: input.map(DisplayInput::value),
                    resolution: monitor
                        .max_resolution
                        .map(|resolution| format!("{}x{}", resolution.width, resolution.height)),
                    connection: monitor.connection.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    HostSnapshot {
        platform: std::env::consts::OS.to_owned(),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        scanned_seconds_ago: inventory
            .as_ref()
            .map(|inventory| inventory.at.elapsed().as_secs()),
        monitors,
        shared_monitors: settings
            .shared_monitors
            .iter()
            .map(|selected| SharedMonitorSnapshot {
                name: selected.name.clone(),
                identity: identity(&selected.fingerprint),
                local_input: selected.local_input.map(DisplayInput::value),
                supported_inputs: selected
                    .supported_inputs
                    .as_ref()
                    .map(|inputs| inputs.iter().map(|input| input.value()).collect()),
                vendor_indexed_inputs: selected.vendor_indexed_inputs,
                active_route: selected
                    .active_route
                    .as_deref()
                    .map(|route| peer_reference(settings, route)),
            })
            .collect(),
        identity_links: links
            .iter()
            .map(|link| IdentityLinkSnapshot {
                alias: identity(&link.alias),
                primary: link.primary.as_ref().map(identity),
                updated_at_ms: link.updated_at_ms,
            })
            .collect(),
        peers: settings
            .peers
            .iter()
            .enumerate()
            .map(|(index, peer)| PeerSnapshot {
                reference: format!("peer-{}", index + 1),
                platform: format!("{:?}", peer.platform).to_lowercase(),
                has_mac_address: !peer.mac_address.trim().is_empty(),
                inputs: peer
                    .inputs
                    .iter()
                    .map(|assignment| PeerInputSnapshot {
                        identity: identity(&assignment.monitor),
                        input: assignment.input.value(),
                    })
                    .collect(),
            })
            .collect(),
        monitors_omitted: false,
    }
}

/// `snapshot` as JSON no longer than `PEER_SNAPSHOT_MAX_BYTES`, dropping the
/// display list first and giving up only if the settings alone do not fit.
pub fn snapshot_for_agent_reply(snapshot: HostSnapshot) -> Option<String> {
    let full = serde_json::to_string(&snapshot).ok()?;
    if full.len() <= PEER_SNAPSHOT_MAX_BYTES {
        return Some(full);
    }
    let reduced = serde_json::to_string(&HostSnapshot {
        monitors: Vec::new(),
        monitors_omitted: true,
        ..snapshot
    })
    .ok()?;
    (reduced.len() <= PEER_SNAPSHOT_MAX_BYTES).then_some(reduced)
}

/// Replaces what identifies a person, a computer or a network in free text:
/// the pairing password, host names and ids, addresses, raw serial numbers and
/// the home directory, then any IPv4 or MAC address left over.
pub struct Redactor {
    replacements: Vec<(String, String)>,
}

fn ipv4_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN
        .get_or_init(|| Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b").expect("IPv4 pattern is valid"))
}

fn mac_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"\b(?:[0-9A-Fa-f]{2}[:-]){5}[0-9A-Fa-f]{2}\b").expect("MAC pattern is valid")
    })
}

impl Redactor {
    pub fn new(settings: &AppSettings, local_host_name: &str) -> Self {
        let mut replacements = Vec::<(String, String)>::new();
        let mut add = |value: &str, replacement: String| {
            let value = value.trim();
            // Short values would match inside unrelated words.
            if value.chars().count() >= 3 {
                replacements.push((value.to_owned(), replacement));
            }
        };
        add(&settings.shared_key, "[pairing-key]".to_owned());
        add(&settings.local_host_id, "[this-host]".to_owned());
        add(local_host_name, "[this-host]".to_owned());
        for (index, peer) in settings.peers.iter().enumerate() {
            let reference = format!("[peer-{}]", index + 1);
            add(&peer.id, reference.clone());
            add(&peer.name, reference.clone());
            add(&peer.address, "[ip]".to_owned());
            add(&peer.mac_address, "[mac]".to_owned());
        }
        for alias in &settings.host_aliases {
            add(&alias.name, "[host-name]".to_owned());
        }
        let serials = settings
            .shared_monitors
            .iter()
            .map(|selected| &selected.fingerprint)
            .chain(
                settings
                    .peers
                    .iter()
                    .flat_map(|peer| peer.inputs.iter().map(|assignment| &assignment.monitor)),
            )
            .chain(
                settings
                    .monitor_identity_links
                    .iter()
                    .flat_map(|link| std::iter::once(&link.alias).chain(link.primary.as_ref())),
            )
            .filter_map(|fingerprint| fingerprint.serial_number.clone())
            .collect::<Vec<_>>();
        let observed = LAST_INVENTORY
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|inventory| {
                inventory
                    .monitors
                    .iter()
                    .filter_map(|(monitor, _)| monitor.fingerprint.serial_number.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let key = serial_key(settings);
        for serial in serials.iter().chain(&observed) {
            add(serial, serial_hash(serial, &key));
        }
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            add(&home.to_string_lossy(), "~".to_owned());
        }
        // Longest first, so a value never loses its middle to a shorter one.
        replacements.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
        replacements.dedup_by(|later, earlier| later.0 == earlier.0);
        Self { replacements }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut redacted = text.to_owned();
        for (value, replacement) in &self.replacements {
            if redacted.contains(value.as_str()) {
                redacted = redacted.replace(value.as_str(), replacement);
            }
        }
        let redacted = mac_pattern().replace_all(&redacted, "[mac]");
        ipv4_pattern().replace_all(&redacted, "[ip]").into_owned()
    }
}

/// Why a report was made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ReportTrigger {
    /// The user asked for it from the settings page.
    Manual,
    /// A switch failed and the user had allowed diagnostics to be sent.
    SwitchFailed { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedHostReport {
    pub reference: String,
    pub snapshot: Option<HostSnapshot>,
    /// Why there is no snapshot: offline, older version, or not allowed.
    pub unavailable: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticReport {
    pub report_id: String,
    pub created_at_ms: u64,
    pub trigger: ReportTrigger,
    pub this_host: HostSnapshot,
    pub paired_hosts: Vec<PairedHostReport>,
    pub recent_log: Vec<String>,
}

impl DiagnosticReport {
    /// A report of this host, with the paired hosts' answers already
    /// collected. Everything free-form goes through `redactor`.
    pub fn new(
        settings: &AppSettings,
        redactor: &Redactor,
        trigger: ReportTrigger,
        paired_hosts: Vec<PairedHostReport>,
        now_ms: u64,
    ) -> Self {
        let trigger = match trigger {
            ReportTrigger::SwitchFailed { message } => ReportTrigger::SwitchFailed {
                message: redactor.redact(&message),
            },
            manual => manual,
        };
        let recent_log = log_directory()
            .map(|directory| recent_log_lines(directory, REPORT_LOG_LINES))
            .unwrap_or_default()
            .iter()
            .map(|line| redactor.redact(line))
            .collect();
        Self {
            report_id: report_id(now_ms),
            created_at_ms: now_ms,
            trigger,
            this_host: host_snapshot(settings),
            paired_hosts: paired_hosts
                .into_iter()
                .map(|host| PairedHostReport {
                    unavailable: host.unavailable.map(|reason| redactor.redact(&reason)),
                    ..host
                })
                .collect(),
            recent_log,
        }
    }
}

/// 32 lowercase hex digits, the shape Sentry expects of an event id.
fn report_id(now_ms: u64) -> String {
    static COUNTER: Mutex<u64> = Mutex::new(0);
    let count = {
        let mut counter = COUNTER.lock().unwrap_or_else(PoisonError::into_inner);
        *counter += 1;
        *counter
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let digest =
        Sha256::digest(format!("{now_ms}:{nanos}:{}:{count}", std::process::id()).as_bytes());
    digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A report shown to the user and not yet sent or saved. Sending and saving
/// use the stored text by id, so what leaves is exactly what was shown.
#[derive(Clone, Debug)]
pub struct PreparedReport {
    pub report_id: String,
    pub trigger: ReportTrigger,
    pub json: String,
}

impl PreparedReport {
    pub fn from_report(report: &DiagnosticReport) -> Result<Self, String> {
        Ok(Self {
            report_id: report.report_id.clone(),
            trigger: report.trigger.clone(),
            json: serde_json::to_string_pretty(report).map_err(|error| error.to_string())?,
        })
    }
}

pub fn keep_pending(report: PreparedReport) {
    *PENDING_REPORT
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(report);
}

pub fn pending(report_id: &str) -> Option<PreparedReport> {
    PENDING_REPORT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .filter(|report| report.report_id == report_id)
        .cloned()
}

/// Writes a prepared report next to the log files and returns its path.
pub fn save(report: &PreparedReport) -> Result<PathBuf, String> {
    let directory = log_directory().ok_or_else(|| "no log directory".to_owned())?;
    let path = directory.join(format!("muxsu-report-{}.json", report.report_id));
    fs::write(&path, &report.json).map_err(|error| error.to_string())?;
    Ok(path)
}

/// Whether an automatic report may go now, and if so records that one did.
pub fn claim_automatic_report_slot() -> bool {
    claim_slot(
        &LAST_AUTOMATIC_REPORT,
        Instant::now(),
        AUTOMATIC_REPORT_INTERVAL,
    )
}

fn claim_slot(last: &Mutex<Option<Instant>>, now: Instant, interval: Duration) -> bool {
    let mut last = last.lock().unwrap_or_else(PoisonError::into_inner);
    if last.is_some_and(|previous| now.duration_since(previous) < interval) {
        return false;
    }
    *last = Some(now);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostRoute, MonitorInputAssignment, SelectedMonitor};
    use muxsu_core::DestinationHost;

    fn settings() -> AppSettings {
        let mpg = MonitorFingerprint::new("MSI", "3CF0", Some("CF0H246200009".to_owned()));
        AppSettings {
            shared_key: "correct horse battery staple".to_owned(),
            local_host_id: "omarsmacbook-mac".to_owned(),
            shared_monitors: vec![SelectedMonitor {
                name: "MPG 274U".to_owned(),
                fingerprint: mpg.clone(),
                max_resolution: None,
                resolution_source: None,
                local_input: DisplayInput::new(8).ok(),
                supported_inputs: None,
                vendor_indexed_inputs: true,
                active_route: Some("2cf05de0c029-windows".to_owned()),
                active_route_confirmed_at_ms: 0,
            }],
            peers: vec![HostRoute {
                id: "2cf05de0c029-windows".to_owned(),
                name: "ITX-PC".to_owned(),
                platform: DestinationHost::Windows,
                address: "192.168.50.93".to_owned(),
                port: 47653,
                mac_address: "2C:F0:5D:E0:C0:29".to_owned(),
                inputs: vec![MonitorInputAssignment {
                    monitor: mpg,
                    input: DisplayInput::new(7).expect("valid input"),
                }],
            }],
            ..AppSettings::default()
        }
    }

    #[test]
    fn a_panic_before_logging_starts_still_has_somewhere_to_go() {
        let path = panic_log_path(None);

        assert_eq!(path.parent(), Some(std::env::temp_dir().as_path()));
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("muxsu-panic.log")
        );
    }

    #[test]
    fn a_panic_after_logging_starts_joins_the_other_logs() {
        let directory = Path::new("/tmp/muxsu-logs");

        let path = panic_log_path(Some(directory));

        assert_eq!(path, directory.join("muxsu-panic.log"));
    }

    #[test]
    fn free_text_loses_every_identifying_value() {
        let redactor = Redactor::new(&settings(), "Omar's MacBook Pro");

        let redacted = redactor.redact(
            "switch to ITX-PC (2cf05de0c029-windows) at 192.168.50.93 / 10.0.0.7, \
             mac 2C:F0:5D:E0:C0:29 aa-bb-cc-dd-ee-ff, key correct horse battery staple, \
             serial CF0H246200009 from Omar's MacBook Pro",
        );

        for secret in [
            "ITX-PC",
            "2cf05de0c029",
            "192.168.50.93",
            "10.0.0.7",
            "2C:F0:5D:E0:C0:29",
            "aa-bb-cc-dd-ee-ff",
            "correct horse",
            "CF0H246200009",
            "Omar",
        ] {
            assert!(!redacted.contains(secret), "{secret} survived: {redacted}");
        }
        assert!(redacted.contains("[peer-1]"));
        assert!(redacted.contains(&serial_hash("CF0H246200009", &serial_key(&settings()))));
    }

    #[test]
    fn a_snapshot_names_hosts_by_reference_and_hashes_serials() {
        let snapshot = host_snapshot(&settings());
        let json = serde_json::to_string(&snapshot).unwrap();

        assert!(!json.contains("CF0H246200009"));
        assert!(!json.contains("ITX-PC"));
        assert!(!json.contains("192.168"));
        assert_eq!(
            snapshot.shared_monitors[0].active_route.as_deref(),
            Some("peer-1")
        );
        assert_eq!(snapshot.peers[0].inputs[0].input, 7);
    }

    #[test]
    fn a_whole_report_carries_nothing_identifying() {
        let settings = settings();
        let redactor = Redactor::new(&settings, "Omar's MacBook Pro");
        let report = DiagnosticReport::new(
            &settings,
            &redactor,
            ReportTrigger::SwitchFailed {
                message: "Neither this computer nor ITX-PC could switch: 192.168.50.93 refused"
                    .to_owned(),
            },
            vec![PairedHostReport {
                reference: "peer-1".to_owned(),
                snapshot: None,
                unavailable: Some("ITX-PC did not answer".to_owned()),
            }],
            1,
        );

        let json = PreparedReport::from_report(&report).unwrap().json;

        for secret in [
            "ITX-PC",
            "192.168.50.93",
            "2C:F0:5D:E0:C0:29",
            "2cf05de0c029",
            "correct horse",
            "CF0H246200009",
            "omarsmacbook",
        ] {
            assert!(!json.contains(secret), "{secret} survived");
        }
        assert!(json.contains("[peer-1] could switch"));
    }

    /// Hosts sharing a pairing password hash a serial alike, so a report can
    /// still compare them; without the password the hash cannot be matched
    /// against guessed serials.
    #[test]
    fn two_hosts_hash_one_serial_alike_and_only_with_their_pairing_key() {
        let group = b"one pairing group's key";
        assert_eq!(
            serial_hash("CF0H246200009", group),
            serial_hash("CF0H246200009", group)
        );
        assert_ne!(serial_hash("first", group), serial_hash("second", group));
        assert_ne!(
            serial_hash("CF0H246200009", group),
            serial_hash("CF0H246200009", b"another group's key")
        );
    }

    /// An unpaired host has nobody to compare with, but an empty key would
    /// make its hashes as easy to match against guessed serials as no key.
    #[test]
    fn an_unpaired_hosts_serial_hash_is_not_keyed_by_a_known_value() {
        let unpaired = AppSettings::default();
        let key = serial_key(&unpaired);

        assert_eq!(key, serial_key(&unpaired), "one report must hash alike");
        assert_ne!(
            serial_hash("CF0H246200009", &key),
            serial_hash("CF0H246200009", b"")
        );
    }

    #[test]
    fn an_agent_reply_drops_the_display_list_before_it_overflows() {
        let mut snapshot = host_snapshot(&settings());
        let monitor = MonitorSnapshot {
            name: "x".repeat(200),
            identity: IdentitySnapshot::new(
                &MonitorFingerprint::new("MSI", "3CF0", None::<String>),
                b"",
            ),
            resolves_to: IdentitySnapshot::new(
                &MonitorFingerprint::new("MSI", "3CF0", None::<String>),
                b"",
            ),
            built_in: false,
            controllable: true,
            current_input: Some(8),
            resolution: None,
            connection: None,
        };
        snapshot.monitors = vec![monitor; 40];

        let reply = snapshot_for_agent_reply(snapshot).expect("settings alone fit");
        let parsed: HostSnapshot = serde_json::from_str(&reply).unwrap();

        assert!(reply.len() <= PEER_SNAPSHOT_MAX_BYTES);
        assert!(parsed.monitors_omitted);
        assert!(parsed.monitors.is_empty());
    }

    #[test]
    fn automatic_reports_are_spaced_out() {
        let last = Mutex::new(None);
        let start = Instant::now();
        let interval = Duration::from_secs(60);

        assert!(claim_slot(&last, start, interval));
        assert!(!claim_slot(
            &last,
            start + Duration::from_secs(30),
            interval
        ));
        assert!(claim_slot(&last, start + Duration::from_secs(61), interval));
    }

    #[test]
    fn the_newest_log_lines_are_kept_in_order() {
        let directory = std::env::temp_dir().join(format!("muxsu-log-test-{}", report_id(1)));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("muxsu.2026-09-17.log"), "a\nb\nc\n").unwrap();
        fs::write(directory.join("muxsu.2026-09-18.log"), "d\ne\n").unwrap();
        fs::write(directory.join("other.txt"), "ignored\n").unwrap();

        let lines = recent_log_lines(&directory, 3);
        fs::remove_dir_all(&directory).unwrap();

        assert_eq!(lines, vec!["c", "d", "e"]);
    }

    #[test]
    fn report_ids_are_sentry_event_ids() {
        let id = report_id(1);

        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|char| char.is_ascii_hexdigit()));
        assert_ne!(id, report_id(1));
    }
}
