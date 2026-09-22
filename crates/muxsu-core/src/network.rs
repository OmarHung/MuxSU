use std::{
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroU32,
    str::FromStr,
    sync::{Arc, Mutex as StdMutex, PoisonError, RwLock as StdRwLock},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, Mac};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use ring::pbkdf2;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::Mutex,
    time::timeout,
};

use crate::{
    DestinationHost, DiscoveredPeer, DisplayInput, DisplayMuxError, MonitorFingerprint,
    PeerDiscovery,
};

pub const DEFAULT_AGENT_PORT: u16 = 47_653;
pub const MUXSU_SERVICE_TYPE: &str = "_muxsu._tcp.local.";
/// Bumped whenever authentication or request interpretation changes in a way
/// that cannot safely interoperate with an older agent.
pub const AGENT_PROTOCOL_VERSION: u32 = 5;
const PAIRING_KEY_ITERATIONS: u32 = 600_000;
const PAIRING_KEY_SALT: &[u8] = b"MuxSU pairing key v1";
const PAIRING_KEY_BYTES: usize = 32;
const MAX_CLOCK_SKEW: Duration = Duration::from_secs(30);
const MAX_PACKET_BYTES: usize = 8 * 1024;
/// Most connections the agent handles at once, and from any one address. Each
/// may be held for the request timeout before anything is authenticated, so
/// without a cap anyone who can reach the port could exhaust this host's
/// sockets and keep paired hosts out.
const MAX_OPEN_CONNECTIONS: usize = 64;
const MAX_OPEN_CONNECTIONS_PER_ADDRESS: usize = 8;
/// Pause after a failed accept, such as running out of file descriptors, so
/// the agent neither spins nor stops serving.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);
/// How long a connection may take to deliver its request line. Anyone on the
/// network can open a socket here; only what arrives over it is authenticated,
/// so a caller that sends nothing has to be given up on rather than waited for.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5);

type HmacSha256 = Hmac<Sha256>;

/// Turns the user-entered pairing password into the key used by the wire
/// protocol. The fixed, application-specific salt is intentional: two hosts
/// that have never communicated must derive the same key from the same text.
/// PBKDF2 still makes every captured request substantially more expensive to
/// attack offline than using the password bytes directly as an HMAC key.
pub fn derive_pairing_key(password: &str) -> [u8; PAIRING_KEY_BYTES] {
    let mut key = [0_u8; PAIRING_KEY_BYTES];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(PAIRING_KEY_ITERATIONS).expect("the PBKDF2 work factor is non-zero"),
        PAIRING_KEY_SALT,
        password.as_bytes(),
        &mut key,
    );
    key
}

pub struct MdnsPeerDiscovery {
    daemon: ServiceDaemon,
    local_id: String,
    /// What this host is advertised as. Kept so the advertisement can be
    /// replaced when the user renames this computer: other machines only ever
    /// see the advertised name until they pair, so a rename that stops at the
    /// settings file leaves them calling this host by the name it had at
    /// startup.
    advertised: StdRwLock<AdvertisedService>,
    peers: Arc<StdRwLock<HashMap<String, DiscoveredPeer>>>,
}

#[derive(Clone, Debug)]
struct AdvertisedService {
    name: String,
    port: u16,
    properties: HashMap<String, String>,
    /// As the library registered it, not as we would spell it. A host name may
    /// contain dots — `Omars-MacBook-Pro-M4-Pro-5.local` does — and those are
    /// escaped inside the instance label, so a reconstructed name does not
    /// match and withdrawing the record silently does nothing, leaving the host
    /// listed twice.
    fullname: String,
}

impl AdvertisedService {
    fn info(&self) -> Result<ServiceInfo, DisplayMuxError> {
        let dns_host_name = format!("{}.local.", dns_label(&self.name));
        ServiceInfo::new(
            MUXSU_SERVICE_TYPE,
            &self.name,
            &dns_host_name,
            "",
            self.port,
            self.properties.clone(),
        )
        .map(ServiceInfo::enable_addr_auto)
        .map_err(|error| DisplayMuxError::Backend(error.to_string()))
    }
}

/// How this computer identifies itself to paired hosts. `id` is the same value
/// a peer stores for this host after discovering it, so it can name this host
/// in data shared between hosts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalHostIdentity {
    pub id: String,
    pub name: String,
    pub platform: DestinationHost,
    pub mac_address: Option<String>,
}

impl LocalHostIdentity {
    pub fn detect(platform: DestinationHost) -> Result<Self, DisplayMuxError> {
        let host_name = hostname::get()
            .map_err(|error| DisplayMuxError::Backend(error.to_string()))?
            .to_string_lossy()
            .trim()
            .to_owned();
        let name = if host_name.is_empty() {
            "MuxSU".to_owned()
        } else {
            host_name
        };
        let detected = match mac_address::get_mac_address() {
            Ok(address) => address,
            Err(error) => {
                tracing::warn!(error = %error, "unable to advertise wake-on-lan address");
                None
            }
        };
        // The address is still worth advertising for wake-on-LAN, but only a
        // universally administered one may name the host: see
        // `identifies_the_machine`.
        let id = peer_id(
            &name,
            platform_name(platform),
            detected
                .filter(identifies_the_machine)
                .map(|address| address.to_string())
                .as_deref(),
        );
        Ok(Self::with_id(
            id,
            name,
            platform,
            detected.map(|address| address.to_string()),
        ))
    }

    /// An identity that keeps `id` whatever this machine's interfaces report
    /// now. Which interface is enumerated first is not stable, but peers store
    /// the id, so it has to outlive any of them: a host that renames itself
    /// loses its pairings, its place in the shared host order and its custom
    /// name on every other host.
    pub fn with_id(
        id: String,
        name: String,
        platform: DestinationHost,
        mac_address: Option<String>,
    ) -> Self {
        Self {
            id,
            name,
            platform,
            mac_address,
        }
    }

    pub fn from_parts(
        name: String,
        platform: DestinationHost,
        mac_address: Option<String>,
    ) -> Self {
        let id = peer_id(&name, platform_name(platform), mac_address.as_deref());
        Self {
            id,
            name,
            platform,
            mac_address,
        }
    }
}

/// What this host tells everyone on the network about itself. Not its MAC
/// address: an advertisement is readable by, and forgeable by, any device
/// there, so paired hosts learn it from this host's signed replies instead.
fn advertised_properties(identity: &LocalHostIdentity) -> HashMap<String, String> {
    HashMap::from([
        ("id".to_owned(), identity.id.clone()),
        ("name".to_owned(), identity.name.clone()),
        (
            "platform".to_owned(),
            platform_name(identity.platform).to_owned(),
        ),
    ])
}

impl MdnsPeerDiscovery {
    pub fn start(identity: &LocalHostIdentity, port: u16) -> Result<Self, DisplayMuxError> {
        let friendly_name = identity.name.clone();
        let local_id = identity.id.clone();
        let properties = advertised_properties(identity);

        let mut advertised = AdvertisedService {
            name: friendly_name.clone(),
            port,
            properties,
            fullname: String::new(),
        };
        let info = advertised.info()?;
        advertised.fullname = info.get_fullname().to_owned();
        let daemon =
            ServiceDaemon::new().map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
        daemon
            .register(info)
            .map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
        let receiver = daemon
            .browse(MUXSU_SERVICE_TYPE)
            .map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
        let peers = Arc::new(StdRwLock::new(HashMap::new()));
        let observed_peers = Arc::clone(&peers);
        let observed_local_id = local_id.clone();

        thread::Builder::new()
            .name("muxsu-mdns".to_owned())
            .spawn(move || {
                while let Ok(event) = receiver.recv() {
                    match event {
                        ServiceEvent::ServiceResolved(service) => {
                            let Some(peer) = discovered_peer(&service) else {
                                continue;
                            };
                            if peer.id == observed_local_id {
                                continue;
                            }
                            if let Ok(mut current) = observed_peers.write() {
                                current.insert(service.get_fullname().to_owned(), peer);
                            }
                        }
                        ServiceEvent::ServiceRemoved(_, fullname) => {
                            if let Ok(mut current) = observed_peers.write() {
                                current.remove(&fullname);
                            }
                        }
                        _ => {}
                    }
                }
            })
            .map_err(|error| DisplayMuxError::Backend(error.to_string()))?;

        tracing::info!(host = %friendly_name, "MuxSU mDNS discovery started");
        Ok(Self {
            daemon,
            local_id,
            advertised: StdRwLock::new(advertised),
            peers,
        })
    }

    /// Re-advertises this host under `name`. Other machines list a host by the
    /// name it advertises until they pair with it, so a rename has to reach the
    /// advertisement or it is invisible to everyone who has not paired yet —
    /// which is exactly the list a user renames a host to recognise it in.
    pub fn advertise_name(&self, name: &str) -> Result<(), DisplayMuxError> {
        let mut advertised = self
            .advertised
            .write()
            .map_err(|_| DisplayMuxError::Backend("無法更新區域網路廣告名稱".to_owned()))?;
        if advertised.name == name {
            return Ok(());
        }
        let previous = advertised.fullname.clone();
        let mut updated = advertised.clone();
        updated.name = name.to_owned();
        updated
            .properties
            .insert("name".to_owned(), name.to_owned());
        let info = updated.info()?;
        updated.fullname = info.get_fullname().to_owned();
        // Withdraw the old record first: leaving it would have this host listed
        // twice, once under a name it no longer answers to.
        let _ = self.daemon.unregister(&previous);
        self.daemon
            .register(info)
            .map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
        *advertised = updated;
        Ok(())
    }

    pub fn local_id(&self) -> &str {
        &self.local_id
    }
}

impl PeerDiscovery for MdnsPeerDiscovery {
    fn peers(&self) -> Result<Vec<DiscoveredPeer>, DisplayMuxError> {
        let current = self
            .peers
            .read()
            .map_err(|_| DisplayMuxError::Backend("無法讀取區域網路搜尋結果".to_owned()))?;
        // Keyed by service record, and one host can hold more than one — a
        // renamed host until its old record expires, or a host answering on
        // several interfaces. The host id is what identifies it, so the list
        // offers each host once.
        let mut seen = HashSet::new();
        let mut peers = current
            .values()
            .filter(|peer| seen.insert(peer.id.clone()))
            .cloned()
            .collect::<Vec<_>>();
        peers.sort_by(|left, right| left.name.to_lowercase().cmp(&right.name.to_lowercase()));
        Ok(peers)
    }
}

fn discovered_peer(service: &mdns_sd::ResolvedService) -> Option<DiscoveredPeer> {
    let platform = match service.get_property_val_str("platform")? {
        "windows" => DestinationHost::Windows,
        "mac" => DestinationHost::Mac,
        _ => return None,
    };
    let address = preferred_address(
        service
            .get_addresses()
            .iter()
            .map(mdns_sd::ScopedIp::to_ip_addr),
    )?;
    let name = service
        .get_property_val_str("name")
        .map(str::to_owned)
        .unwrap_or_else(|| service.get_hostname().trim_end_matches('.').to_owned());
    let id = service
        .get_property_val_str("id")
        .map(str::to_owned)
        .unwrap_or_else(|| service.get_fullname().to_owned());
    Some(DiscoveredPeer {
        id,
        name,
        platform,
        address,
        port: service.get_port(),
    })
}

/// Whether `address` is on a network MuxSU is meant for: this computer, a
/// private LAN (RFC 1918), link-local, the shared range VPNs such as
/// Tailscale use (RFC 6598), or an IPv6 unique-local or link-local network.
/// The agent answers nothing else, and connects to nothing else, so a host
/// with a public address is never exposed to, or led toward, the internet.
pub fn is_local_network_address(address: IpAddr) -> bool {
    match address.to_canonical() {
        IpAddr::V4(address) => {
            let [first, second, ..] = address.octets();
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || (first == 100 && (64..128).contains(&second))
        }
        IpAddr::V6(address) => {
            address.is_loopback() || address.is_unique_local() || address.is_unicast_link_local()
        }
    }
}

fn preferred_address(addresses: impl Iterator<Item = IpAddr>) -> Option<IpAddr> {
    addresses
        .filter(|address| !address.is_loopback() && is_local_network_address(*address))
        .min_by_key(|address| match address {
            IpAddr::V4(address) if address.is_private() => 0,
            IpAddr::V4(_) => 1,
            IpAddr::V6(_) => 2,
        })
}

fn platform_name(platform: DestinationHost) -> &'static str {
    match platform {
        DestinationHost::Windows => "windows",
        DestinationHost::Mac => "mac",
    }
}

/// Whether a MAC address names the machine rather than one of its virtual or
/// privacy interfaces. macOS hands out locally administered addresses for
/// Wi-Fi privacy, AWDL, bridges and the Apple Silicon `anpi` devices, and
/// which of them is enumerated first is not stable — one Mac was seen
/// identifying itself as three different hosts. An address with the
/// locally-administered bit set, or an all-zero one, therefore never becomes a
/// host id; the host name is used instead, which at least does not change on
/// its own.
fn identifies_the_machine(address: &mac_address::MacAddress) -> bool {
    let bytes = address.bytes();
    bytes != [0; 6] && bytes[0] & 0b0000_0010 == 0
}

fn peer_id(host_name: &str, platform: &str, mac_address: Option<&str>) -> String {
    let identity = mac_address.unwrap_or(host_name);
    format!(
        "{}-{platform}",
        identity
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase()
    )
}

fn dns_label(host_name: &str) -> String {
    let label = host_name
        .split('.')
        .next()
        .unwrap_or(host_name)
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned();
    if label.is_empty() {
        "muxsu".to_owned()
    } else {
        label
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacAddress([u8; 6]);

impl MacAddress {
    pub const fn octets(self) -> [u8; 6] {
        self.0
    }
}

impl FromStr for MacAddress {
    type Err = DisplayMuxError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let parts = value
            .split([':', '-'])
            .map(|part| u8::from_str_radix(part, 16))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| DisplayMuxError::InvalidMacAddress(value.to_owned()))?;

        let octets: [u8; 6] = parts
            .try_into()
            .map_err(|_| DisplayMuxError::InvalidMacAddress(value.to_owned()))?;
        Ok(Self(octets))
    }
}

impl fmt::Display for MacAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeTarget {
    pub mac_address: MacAddress,
    pub broadcast_address: Ipv4Addr,
    pub port: u16,
}

impl WakeTarget {
    pub async fn wake(&self) -> Result<(), DisplayMuxError> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .map_err(|error| DisplayMuxError::WakeFailed(error.to_string()))?;
        socket
            .set_broadcast(true)
            .map_err(|error| DisplayMuxError::WakeFailed(error.to_string()))?;

        let mut packet = [0_u8; 102];
        packet[..6].fill(0xff);
        for chunk in packet[6..].chunks_exact_mut(6) {
            chunk.copy_from_slice(&self.mac_address.octets());
        }

        socket
            .send_to(&packet, (self.broadcast_address, self.port))
            .await
            .map_err(|error| DisplayMuxError::WakeFailed(error.to_string()))?;
        tracing::info!(
            broadcast = %self.broadcast_address,
            port = self.port,
            "wake-on-lan packet sent"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerEndpoint {
    pub address: IpAddr,
    pub port: u16,
}

impl PeerEndpoint {
    pub const fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.address, self.port)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentAction {
    Ping,
    SwitchInput {
        /// Which shared monitor to switch. `None` is the pre-v2 shape: the
        /// receiving agent must have exactly one shared monitor selected to
        /// accept it unambiguously (see `AGENT_PROTOCOL_VERSION`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        monitor: Option<MonitorFingerprint>,
        input: DisplayInput,
    },
    /// Notice that a paired host just switched `monitor` to `input`, so the
    /// receiver can update which host it shows as active. Application senders
    /// retain and retry it until the signed response acknowledges delivery.
    ActiveInputChanged {
        monitor: MonitorFingerprint,
        input: DisplayInput,
    },
    /// Notice of the host card order a paired host just saved,
    /// as `LocalHostIdentity::id` values. `updated_at_ms` lets receivers keep
    /// the most recent order when several hosts change it.
    HostOrderChanged {
        order: Vec<String>,
        updated_at_ms: u64,
    },
    /// Notice of every custom host name a paired host knows.
    /// Receivers merge entry by entry, keeping the newer `updated_at_ms`.
    HostAliasesChanged {
        aliases: Vec<HostAlias>,
    },
    /// Notice of every custom host icon and colour a paired host knows.
    /// Receivers merge entry by entry, keeping the newer `updated_at_ms`.
    HostAppearancesChanged {
        appearances: Vec<HostAppearance>,
    },
    /// Notice of every input note a paired host knows. Receivers
    /// merge entry by entry, keeping the newer `updated_at_ms`.
    InputLabelsChanged {
        labels: Vec<InputLabel>,
    },
    /// Notice of every display-identity claim a paired host knows.
    /// Receivers merge entry by entry, keeping the newer `updated_at_ms`.
    MonitorIdentitiesChanged {
        links: Vec<MonitorIdentityLink>,
    },
    /// Notice of the inputs a display told the sender it accepts.
    /// A display only answers the host it is showing, so the other host is
    /// left guessing at standard MCCS codes and cannot name a vendor-specific
    /// input at all. Receivers take these only when they have none of their
    /// own: a reading taken from the display beats a guess, but not another
    /// reading. `vendor_indexed` marks a list that is the display's private
    /// `1..=max` range rather than MCCS codes, so both hosts label it alike.
    DisplayInputsDiscovered {
        monitor: MonitorFingerprint,
        inputs: Vec<DisplayInput>,
        vendor_indexed: bool,
    },
    /// Notice that the sender is on screen on `monitor` and reads
    /// `input` there, so `input` is the port the sender is plugged into.
    /// Receivers adopt it as that host's input without the user picking one.
    /// Only a host that is on screen can vouch for its own port: DDC reports
    /// the input a display shows, never which port the reader occupies.
    /// Agents older than this variant reject the request; senders ignore that.
    LocalInputConfirmed {
        /// The sender's `LocalHostIdentity::id`, so the receiver knows whose
        /// port this is. Signed with the rest of the action, so only a holder
        /// of the shared key can send it.
        host_id: String,
        monitor: MonitorFingerprint,
        input: DisplayInput,
    },
    /// A versioned, authoritative display-port assignment. Unlike
    /// `LocalInputConfirmed`, this also represents a manual correction and an
    /// explicit clear (`input: None`). Sending the complete changed entries
    /// makes retries idempotent and lets peers that were offline converge.
    HostInputsChanged {
        assignments: Vec<AgentHostInput>,
    },
    /// Asks for the receiver's diagnostic snapshot, for a report the sender's
    /// user is putting together. Receivers answer only when their own user
    /// has allowed diagnostics, and redact the snapshot before it leaves.
    /// Agents older than this variant reject the request; senders note that.
    DiagnosticsRequested,
}

/// A user-chosen display name for a host, keyed by `LocalHostIdentity::id`.
/// An empty `name` records that the custom name was cleared, so the clear
/// reaches paired hosts instead of being undone by their older entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostAlias {
    pub host_id: String,
    pub name: String,
    pub updated_at_ms: u64,
}

/// A user-chosen icon and colour for a host, keyed by `LocalHostIdentity::id`.
/// Both are names from a fixed set the app knows how to draw; an empty value
/// means the host's default, so a reset reaches paired hosts like a clear
/// `HostAlias` does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostAppearance {
    pub host_id: String,
    pub icon: String,
    pub color: String,
    pub updated_at_ms: u64,
}

/// A user's claim that `alias` and `primary` are the same physical display.
///
/// Some displays publish a different EDID product code per display mode: the
/// MSI MPG 274U reports `MSI:3CF0` at 3840x2160 and `MSI:7CF0` at 1920x1080,
/// with no serial number in either EDID or the platform's own record. Nothing
/// the display reports stays put across that switch, so the equivalence can
/// only come from the user. Each entry carries its own timestamp, like
/// `InputLabel`, and an empty `primary` records that the claim was withdrawn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorIdentityLink {
    pub alias: MonitorFingerprint,
    pub primary: Option<MonitorFingerprint>,
    pub updated_at_ms: u64,
}

/// A user note for one input of a shared display, such as "USB-C" for a display
/// that only reports input numbers. `monitor` is the sender's fingerprint for
/// the display; receivers map it to their own. An empty `label` records that
/// the note was cleared, like `HostAlias`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InputLabel {
    pub monitor: MonitorFingerprint,
    pub input: DisplayInput,
    pub label: String,
    pub updated_at_ms: u64,
}

/// The port one host occupies on one shared display. `input: None` is a
/// tombstone: it must be retained and exchanged so an offline peer cannot
/// resurrect an assignment that was cleared while it was away.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentHostInput {
    pub host_id: String,
    pub monitor: MonitorFingerprint,
    pub input: Option<DisplayInput>,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRequest {
    pub timestamp_seconds: u64,
    pub nonce: String,
    pub action: AgentAction,
    pub signature: String,
}

impl AgentRequest {
    pub fn signed(
        action: AgentAction,
        nonce: impl Into<String>,
        shared_key: &[u8],
    ) -> Result<Self, DisplayMuxError> {
        let timestamp_seconds = unix_time()?;
        let nonce = nonce.into();
        let signature = sign(timestamp_seconds, &nonce, &action, shared_key)?;
        Ok(Self {
            timestamp_seconds,
            nonce,
            action,
            signature,
        })
    }

    fn verify(&self, shared_key: &[u8]) -> Result<(), DisplayMuxError> {
        let now = unix_time()?;
        if now.abs_diff(self.timestamp_seconds) > MAX_CLOCK_SKEW.as_secs() {
            return Err(DisplayMuxError::StaleRequest);
        }

        let supplied =
            hex::decode(&self.signature).map_err(|_| DisplayMuxError::AuthenticationFailed)?;
        let payload = signing_payload(self.timestamp_seconds, &self.nonce, &self.action)?;
        let mut mac = HmacSha256::new_from_slice(shared_key)
            .map_err(|_| DisplayMuxError::AuthenticationFailed)?;
        mac.update(&payload);
        mac.verify_slice(&supplied)
            .map_err(|_| DisplayMuxError::AuthenticationFailed)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentResponse {
    pub ready: bool,
    pub message: String,
    /// Kept for compatibility with pre-v2 clients that only read this field
    /// for their one implicit shared monitor; populated as
    /// `display_routes.first().cloned()` by v2+ responders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_route: Option<AgentDisplayRoute>,
    /// One entry per shared monitor the responder currently knows an input
    /// for. Absent on a pre-v2 peer's response, which deserializes to an
    /// empty vec via `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub display_routes: Vec<AgentDisplayRoute>,
    /// The shared displays the responder can see on its own side right now,
    /// whether or not DDC/CI reaches them — enough to tell a user that a host
    /// is up but has no cable to the display they are switching.
    ///
    /// `None` is "the responder did not say": an agent that predates this
    /// field, or one whose own view of its displays is too old to answer for.
    /// It must read as unknown, never as "nothing attached", so an empty list
    /// keeps its own meaning: scanned, and none of the shared displays is
    /// there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_monitors: Option<Vec<MonitorFingerprint>>,
    /// The responder's `AGENT_PROTOCOL_VERSION`. It is covered by the response
    /// signature and must match before any response data is accepted.
    #[serde(default)]
    pub protocol_version: u32,
    /// The responder's host card order and when it last changed, so a host
    /// that was offline when the order changed can catch up. Empty and `0` on
    /// agents that predate this field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_order: Vec<String>,
    #[serde(default)]
    pub host_order_updated_at_ms: u64,
    /// The responder's custom host names, for the same catch-up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_aliases: Vec<HostAlias>,
    /// The responder's custom host icons and colours, for the same catch-up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_appearances: Vec<HostAppearance>,
    /// The responder's input notes, for the same catch-up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_labels: Vec<InputLabel>,
    /// The responder's display-identity claims, for the same catch-up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub monitor_identity_links: Vec<MonitorIdentityLink>,
    /// Versioned host/display input assignments for offline catch-up. Entries
    /// with `input: None` are intentional clears and must not be discarded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_inputs: Vec<AgentHostInput>,
    /// The responder's wake-on-LAN address, in reply to `Ping`. Sent here,
    /// signed, rather than in the discovery advertisement anyone could forge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac_address: Option<String>,
    /// The responder's redacted diagnostic snapshot as JSON, only in reply to
    /// `DiagnosticsRequested`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<String>,
    /// Proof that whatever answered holds the pairing password, bound to the
    /// nonce of the request it answers so it cannot be lifted from an earlier
    /// exchange. Current clients reject responses where this is absent or
    /// fails to cover the complete response payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// What a reply's signature covers: its complete payload except for the
/// signature itself, plus the request nonce. In particular, the protocol
/// version and every piece of synchronized state are authenticated rather
/// than trusted merely because they arrived on the paired host's socket.
fn response_signing_payload(
    nonce: &str,
    response: &AgentResponse,
) -> Result<Vec<u8>, DisplayMuxError> {
    let mut unsigned = response.clone();
    unsigned.signature = None;
    serde_json::to_vec(&("muxsu-response-v4", nonce, unsigned))
        .map_err(|error| DisplayMuxError::Backend(error.to_string()))
}

impl AgentResponse {
    /// Signs this reply against the nonce of the request it answers.
    pub fn signed_for(mut self, nonce: &str, shared_key: &[u8]) -> Self {
        self.signature = response_signing_payload(nonce, &self)
            .ok()
            .and_then(|payload| {
                let mut mac = HmacSha256::new_from_slice(shared_key).ok()?;
                mac.update(&payload);
                Some(hex::encode(mac.finalize().into_bytes()))
            });
        self
    }

    /// Whether this reply proves the responder holds the pairing password.
    ///
    /// False for a reply that carries no valid full-payload signature.
    pub fn proves_pairing(&self, nonce: &str, shared_key: &[u8]) -> bool {
        let Some(signature) = &self.signature else {
            return false;
        };
        let Ok(supplied) = hex::decode(signature) else {
            return false;
        };
        let Ok(payload) = response_signing_payload(nonce, self) else {
            return false;
        };
        let Ok(mut mac) = HmacSha256::new_from_slice(shared_key) else {
            return false;
        };
        mac.update(&payload);
        mac.verify_slice(&supplied).is_ok()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDisplayRoute {
    pub monitor: MonitorFingerprint,
    pub input: DisplayInput,
    /// True when the responder believes `monitor` is currently showing it, so
    /// `input` is the port it is plugged into rather than a reading taken
    /// while another host was on screen. DDC reports the input the display
    /// shows, never which port the reader occupies, so only a host that is on
    /// screen can vouch for its own port. Absent on agents that predate this
    /// field, which deserializes to `false` via `#[serde(default)]`.
    #[serde(default)]
    pub confirmed: bool,
}

#[derive(Clone)]
pub struct AgentClient {
    endpoint: PeerEndpoint,
    shared_key: Arc<[u8]>,
    connect_timeout: Duration,
}

impl AgentClient {
    pub fn new(endpoint: PeerEndpoint, shared_key: impl Into<Arc<[u8]>>) -> Self {
        Self {
            endpoint,
            shared_key: shared_key.into(),
            connect_timeout: Duration::from_secs(2),
        }
    }

    pub async fn request(
        &self,
        action: AgentAction,
        nonce: impl Into<String>,
    ) -> Result<AgentResponse, DisplayMuxError> {
        let nonce = nonce.into();
        let request = AgentRequest::signed(action, nonce.clone(), &self.shared_key)?;
        let stream = timeout(
            self.connect_timeout,
            TcpStream::connect(self.endpoint.socket_addr()),
        )
        .await
        .map_err(|_| DisplayMuxError::PeerUnavailable("連線逾時".to_owned()))?
        .map_err(|error| DisplayMuxError::PeerUnavailable(error.to_string()))?;
        let (reader, mut writer) = stream.into_split();
        let mut payload = serde_json::to_vec(&request)
            .map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
        payload.push(b'\n');
        writer
            .write_all(&payload)
            .await
            .map_err(|error| DisplayMuxError::PeerUnavailable(error.to_string()))?;

        let mut response = String::new();
        timeout(
            self.connect_timeout,
            BufReader::new(reader)
                .take(MAX_PACKET_BYTES as u64)
                .read_line(&mut response),
        )
        .await
        .map_err(|_| DisplayMuxError::PeerUnavailable("回應逾時".to_owned()))?
        .map_err(|error| DisplayMuxError::PeerUnavailable(error.to_string()))?;
        let response = parse_agent_response(&response)?;
        verify_agent_response(&response, &nonce, &self.shared_key)?;
        Ok(response)
    }
}

fn verify_agent_response(
    response: &AgentResponse,
    nonce: &str,
    shared_key: &[u8],
) -> Result<(), DisplayMuxError> {
    // A version mismatch is safe to report before authentication because it
    // can only reject the response, never make untrusted data actionable.
    if response.protocol_version != AGENT_PROTOCOL_VERSION {
        return Err(DisplayMuxError::UnreadableRequest);
    }
    if !response.proves_pairing(nonce, shared_key) {
        return Err(DisplayMuxError::AuthenticationFailed);
    }
    Ok(())
}

pub struct AgentServer {
    bind_address: SocketAddr,
    shared_key: Arc<[u8]>,
    seen_nonces: Arc<Mutex<HashMap<String, u64>>>,
    request_timeout: Duration,
    connection_limits: ConnectionLimits,
}

/// Counts the connections open from each address, so the agent can refuse a
/// new one past its caps.
#[derive(Clone)]
struct ConnectionLimits {
    total: usize,
    per_address: usize,
    open: Arc<StdMutex<HashMap<IpAddr, usize>>>,
}

/// One admitted connection. Frees its place when dropped.
struct ConnectionSlot {
    address: IpAddr,
    open: Arc<StdMutex<HashMap<IpAddr, usize>>>,
}

impl ConnectionLimits {
    fn new(total: usize, per_address: usize) -> Self {
        Self {
            total,
            per_address,
            open: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    fn try_admit(&self, address: IpAddr) -> Option<ConnectionSlot> {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        let from_address = open.get(&address).copied().unwrap_or_default();
        if open.values().sum::<usize>() >= self.total || from_address >= self.per_address {
            return None;
        }
        open.insert(address, from_address + 1);
        Some(ConnectionSlot {
            address,
            open: Arc::clone(&self.open),
        })
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = open.get_mut(&self.address) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                open.remove(&self.address);
            }
        }
    }
}

impl AgentServer {
    pub fn new(bind_address: SocketAddr, shared_key: impl Into<Arc<[u8]>>) -> Self {
        Self {
            bind_address,
            shared_key: shared_key.into(),
            seen_nonces: Arc::new(Mutex::new(HashMap::new())),
            request_timeout: REQUEST_READ_TIMEOUT,
            connection_limits: ConnectionLimits::new(
                MAX_OPEN_CONNECTIONS,
                MAX_OPEN_CONNECTIONS_PER_ADDRESS,
            ),
        }
    }

    /// How many connections may be open at once, in total and from one
    /// address. Exposed so a test can reach the caps with a few connections.
    pub fn with_connection_limits(mut self, total: usize, per_address: usize) -> Self {
        self.connection_limits = ConnectionLimits::new(total, per_address);
        self
    }

    /// How long a caller has to deliver its request line. Exposed so a test can
    /// wait a moment rather than the several seconds a real caller is given.
    pub fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    pub async fn run<H, Fut>(self, handler: H) -> Result<(), DisplayMuxError>
    where
        H: Fn(AgentAction) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = AgentResponse> + Send + 'static,
    {
        let listener = TcpListener::bind(self.bind_address)
            .await
            .map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
        let handler = Arc::new(handler);

        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::warn!(error = %error, "agent could not accept a connection");
                    tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                    continue;
                }
            };
            if !is_local_network_address(peer.ip()) {
                tracing::debug!(peer = %peer, "agent connection refused: not a local network");
                continue;
            }
            let Some(slot) = self.connection_limits.try_admit(peer.ip()) else {
                tracing::debug!(peer = %peer, "agent connection refused: too many open");
                continue;
            };
            let shared_key = Arc::clone(&self.shared_key);
            let seen_nonces = Arc::clone(&self.seen_nonces);
            let handler = Arc::clone(&handler);
            let request_timeout = self.request_timeout;
            tokio::spawn(async move {
                let _slot = slot;
                if let Err(error) =
                    handle_connection(stream, shared_key, seen_nonces, handler, request_timeout)
                        .await
                {
                    tracing::warn!(peer = %peer, error = %error, "agent request rejected");
                }
            });
        }
    }
}

async fn handle_connection<H, Fut>(
    stream: TcpStream,
    shared_key: Arc<[u8]>,
    seen_nonces: Arc<Mutex<HashMap<String, u64>>>,
    handler: Arc<H>,
    request_timeout: Duration,
) -> Result<(), DisplayMuxError>
where
    H: Fn(AgentAction) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = AgentResponse> + Send + 'static,
{
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    // Bounded in size and in time. The size cap alone leaves a caller that
    // sends nothing at all holding this task open indefinitely, and nothing
    // here is authenticated until the whole line has arrived — so the cost of
    // that is available to anyone who can reach the port.
    timeout(
        request_timeout,
        BufReader::new(reader)
            .take(MAX_PACKET_BYTES as u64)
            .read_line(&mut line),
    )
    .await
    .map_err(|_| DisplayMuxError::PeerUnavailable("連線逾時".to_owned()))?
    .map_err(|error| DisplayMuxError::PeerUnavailable(error.to_string()))?;
    let request: AgentRequest = match serde_json::from_str(&line) {
        Ok(request) => request,
        Err(parse_error) => {
            // Not an authentication failure: the request never got far enough
            // to be checked. A host that predates an action cannot read it.
            let error = DisplayMuxError::UnreadableRequest;
            tracing::warn!(error = %error, detail = %parse_error, "agent request rejected");
            return write_agent_response(&mut writer, &rejection_response(&error)).await;
        }
    };
    if let Err(error) = request.verify(&shared_key) {
        tracing::warn!(error = %error, "agent request rejected");
        let response = rejection_response(&error).signed_for(&request.nonce, &shared_key);
        return write_agent_response(&mut writer, &response).await;
    }

    let mut nonces = seen_nonces.lock().await;
    // Remembered only for as long as a replay of them could still be accepted.
    // The previous cap emptied the whole set on reaching a count, which let
    // every nonce in it be used a second time; a request older than the skew
    // window is already refused above, so forgetting those costs nothing.
    let now = unix_time().unwrap_or(request.timestamp_seconds);
    nonces.retain(|_, seen| now.saturating_sub(*seen) <= MAX_CLOCK_SKEW.as_secs());
    if nonces
        .insert(request.nonce.clone(), request.timestamp_seconds)
        .is_some()
    {
        let error = DisplayMuxError::StaleRequest;
        drop(nonces);
        tracing::warn!(error = %error, "agent request rejected");
        return write_agent_response(&mut writer, &rejection_response(&error)).await;
    }
    drop(nonces);

    let response = handler(request.action)
        .await
        .signed_for(&request.nonce, &shared_key);
    write_agent_response(&mut writer, &response).await
}

async fn write_agent_response(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    response: &AgentResponse,
) -> Result<(), DisplayMuxError> {
    let mut payload = serde_json::to_vec(&response)
        .map_err(|error| DisplayMuxError::Backend(error.to_string()))?;
    payload.push(b'\n');
    writer
        .write_all(&payload)
        .await
        .map_err(|error| DisplayMuxError::PeerUnavailable(error.to_string()))
}

fn rejection_response(error: &DisplayMuxError) -> AgentResponse {
    let message = match error {
        DisplayMuxError::StaleRequest => "連線驗證失敗，請確認兩台主機的系統時間已同步後再試一次",
        // Saying "wrong password" for everything sent users to change a
        // password that was right while the real cause went unmentioned.
        DisplayMuxError::UnreadableRequest => {
            "另一台主機無法解讀這個要求，請將兩台主機更新到相同版本"
        }
        _ => "配對密碼不一致，請在兩台主機輸入完全相同的配對密碼並重新儲存",
    };
    AgentResponse {
        ready: false,
        message: message.to_owned(),
        display_route: None,
        display_routes: Vec::new(),
        protocol_version: AGENT_PROTOCOL_VERSION,
        ..AgentResponse::default()
    }
}

fn parse_agent_response(response: &str) -> Result<AgentResponse, DisplayMuxError> {
    if response.trim().is_empty() {
        return Err(DisplayMuxError::PeerUnavailable(
            "另一台主機未回傳結果；請確認兩台主機皆已更新至最新版，並重新檢查配對密碼".to_owned(),
        ));
    }
    serde_json::from_str(response)
        .map_err(|error| DisplayMuxError::PeerUnavailable(format!("回應格式無效：{error}")))
}

fn sign(
    timestamp_seconds: u64,
    nonce: &str,
    action: &AgentAction,
    shared_key: &[u8],
) -> Result<String, DisplayMuxError> {
    let payload = signing_payload(timestamp_seconds, nonce, action)?;
    let mut mac = HmacSha256::new_from_slice(shared_key)
        .map_err(|_| DisplayMuxError::AuthenticationFailed)?;
    mac.update(&payload);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn signing_payload(
    timestamp_seconds: u64,
    nonce: &str,
    action: &AgentAction,
) -> Result<Vec<u8>, DisplayMuxError> {
    serde_json::to_vec(&(timestamp_seconds, nonce, action))
        .map_err(|error| DisplayMuxError::Backend(error.to_string()))
}

fn unix_time() -> Result<u64, DisplayMuxError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| DisplayMuxError::Backend(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An older host recomputes a reply's signature from the fields it knows.
    /// A reply that carries no diagnostics must therefore serialize exactly as
    /// it did before the field existed, or every older host would reject it.
    #[test]
    fn a_reply_without_diagnostics_serializes_as_before_the_field_existed() {
        let reply = AgentResponse {
            ready: true,
            message: "ready".to_owned(),
            protocol_version: AGENT_PROTOCOL_VERSION,
            ..AgentResponse::default()
        };

        let json = serde_json::to_string(&reply).unwrap();

        assert!(!json.contains("diagnostics"));
    }

    #[test]
    fn a_diagnostics_request_round_trips() {
        let json = serde_json::to_string(&AgentAction::DiagnosticsRequested).unwrap();

        assert_eq!(json, r#"{"type":"diagnostics_requested"}"#);
        assert_eq!(
            serde_json::from_str::<AgentAction>(&json).unwrap(),
            AgentAction::DiagnosticsRequested
        );
    }

    #[test]
    fn parses_both_common_mac_address_formats() {
        let colon = "AA:BB:CC:DD:EE:FF".parse::<MacAddress>().unwrap();
        let dash = "aa-bb-cc-dd-ee-ff".parse::<MacAddress>().unwrap();
        assert_eq!(colon, dash);
        assert_eq!(colon.to_string(), "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn rejects_incomplete_mac_address() {
        assert!(matches!(
            "AA:BB:CC".parse::<MacAddress>(),
            Err(DisplayMuxError::InvalidMacAddress(_))
        ));
    }

    #[test]
    fn a_locally_administered_address_never_names_the_host() {
        // Every address this Mac reports has the locally-administered bit set:
        // the Apple Silicon `anpi` devices, AWDL, the bridges and the Wi-Fi
        // privacy address. Which one is enumerated first is not stable, and it
        // had the machine identifying itself as three different hosts.
        for bytes in [
            [0xd2, 0xa3, 0x18, 0x8f, 0xc5, 0xf4],
            [0xd2, 0xa3, 0x18, 0x8f, 0xc5, 0xf5],
            [0x3a, 0xf0, 0xcc, 0x1c, 0x98, 0x35],
            [0x02, 0x00, 0x00, 0x00, 0x00, 0x00],
            [0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        ] {
            assert!(
                !identifies_the_machine(&mac_address::MacAddress::new(bytes)),
                "{bytes:02x?} must not name a host"
            );
        }
    }

    #[test]
    fn a_burned_in_address_still_names_the_host() {
        assert!(identifies_the_machine(&mac_address::MacAddress::new([
            0x2c, 0xf0, 0x5d, 0xe0, 0xc0, 0x29
        ])));
    }

    #[test]
    fn a_kept_id_survives_a_machine_whose_address_changed() {
        let first = LocalHostIdentity::with_id(
            "kept-id".to_owned(),
            "Mac".to_owned(),
            DestinationHost::Mac,
            Some("d2:a3:18:8f:c5:f4".to_owned()),
        );
        let later = LocalHostIdentity::with_id(
            first.id.clone(),
            "Mac".to_owned(),
            DestinationHost::Mac,
            Some("02:00:00:00:00:00".to_owned()),
        );

        assert_eq!(first.id, later.id);
        assert_ne!(first.mac_address, later.mac_address);
    }

    #[test]
    fn creates_stable_dns_safe_peer_identity() {
        assert_eq!(dns_label("Henry's MacBook.local"), "henry-s-macbook");
        assert_eq!(
            peer_id("Henry-PC", "windows", Some("AA:BB:CC:DD:EE:FF")),
            "aabbccddeeff-windows"
        );
    }

    #[test]
    fn prefers_private_ipv4_for_lan_connections() {
        let addresses = HashSet::from([
            "fe80::1234".parse().unwrap(),
            "192.168.1.25".parse().unwrap(),
            "127.0.0.1".parse().unwrap(),
        ]);
        assert_eq!(
            preferred_address(addresses.iter().copied()),
            Some("192.168.1.25".parse().unwrap())
        );
    }

    /// Anyone on the network reads an advertisement, and anyone can forge
    /// one, so the MAC address travels only in signed replies.
    #[test]
    fn the_advertisement_does_not_carry_the_mac_address() {
        let identity = LocalHostIdentity::with_id(
            "aabbccddeeff-windows".to_owned(),
            "Desk PC".to_owned(),
            DestinationHost::Windows,
            Some("AA:BB:CC:DD:EE:FF".to_owned()),
        );

        let properties = advertised_properties(&identity);

        assert_eq!(
            properties.get("id").map(String::as_str),
            Some("aabbccddeeff-windows")
        );
        assert!(!properties.contains_key("mac"));
    }

    #[test]
    fn only_local_network_addresses_are_accepted() {
        for local in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.9",
            "192.168.1.25",
            "169.254.10.20",
            "100.100.1.2",
            "::1",
            "fd7a:115c:a1e0::1",
            "fe80::1234",
            "::ffff:192.168.1.25",
        ] {
            assert!(
                is_local_network_address(local.parse().unwrap()),
                "{local} should be accepted"
            );
        }
        for remote in [
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "2001:4860::8888",
            "::ffff:8.8.8.8",
        ] {
            assert!(
                !is_local_network_address(remote.parse().unwrap()),
                "{remote} should be refused"
            );
        }
    }

    #[test]
    fn a_discovered_host_with_only_public_addresses_is_not_reachable() {
        let addresses = HashSet::from([
            "8.8.8.8".parse().unwrap(),
            "2001:4860::8888".parse().unwrap(),
        ]);
        assert_eq!(preferred_address(addresses.iter().copied()), None);
    }

    #[test]
    fn signed_request_rejects_tampering() {
        let key = b"a test key that is never persisted";
        let mut request = AgentRequest::signed(AgentAction::Ping, "nonce-1", key).unwrap();
        request.action = AgentAction::SwitchInput {
            monitor: None,
            input: DisplayInput::new(0x11).unwrap(),
        };
        assert_eq!(
            request.verify(key),
            Err(DisplayMuxError::AuthenticationFailed)
        );
    }

    #[test]
    fn an_unreadable_request_is_not_reported_as_a_wrong_password() {
        // A host that predates an action cannot read it. Blaming the pairing
        // password sends the user to change one that was right, and leaves the
        // version gap that actually caused it unmentioned.
        let response = rejection_response(&DisplayMuxError::UnreadableRequest);

        assert!(!response.ready);
        assert!(response.message.contains("版本"));
        assert!(!response.message.contains("配對密碼不一致"));
    }

    #[test]
    fn authentication_rejection_explains_pairing_password_mismatch() {
        let response = rejection_response(&DisplayMuxError::AuthenticationFailed);
        assert!(!response.ready);
        assert!(response.message.contains("配對密碼不一致"));
    }

    #[test]
    fn empty_legacy_response_is_not_reported_as_json_eof() {
        let error = parse_agent_response("").unwrap_err();
        assert!(matches!(error, DisplayMuxError::PeerUnavailable(_)));
        assert!(!error.to_string().contains("EOF"));
        assert!(error.to_string().contains("配對密碼"));
    }

    #[test]
    fn a_response_from_an_agent_without_identity_claims_still_deserializes() {
        let response: AgentResponse =
            serde_json::from_str(r#"{"ready":true,"message":"ok"}"#).unwrap();

        assert!(response.monitor_identity_links.is_empty());
    }

    #[test]
    fn display_inputs_notice_round_trips_with_a_vendor_indexed_list() {
        // The list this carries is the one a host could only read while the
        // display was showing it, including inputs MCCS has no code for.
        let action = AgentAction::DisplayInputsDiscovered {
            monitor: MonitorFingerprint::new("MSI", "3CF0", None::<String>),
            inputs: vec![
                DisplayInput::new(8).unwrap(),
                DisplayInput::new(14).unwrap(),
            ],
            vendor_indexed: true,
        };

        let encoded = serde_json::to_string(&action).unwrap();
        assert_eq!(
            serde_json::from_str::<AgentAction>(&encoded).unwrap(),
            action
        );
    }

    #[test]
    fn monitor_identities_changed_notice_round_trips_with_every_entry() {
        let action = AgentAction::MonitorIdentitiesChanged {
            links: vec![MonitorIdentityLink {
                alias: MonitorFingerprint::new("MSI", "7CF0", None::<String>),
                primary: Some(MonitorFingerprint::new("MSI", "3CF0", None::<String>)),
                updated_at_ms: 42,
            }],
        };

        let encoded = serde_json::to_string(&action).unwrap();
        assert_eq!(
            serde_json::from_str::<AgentAction>(&encoded).unwrap(),
            action
        );
    }

    #[test]
    fn a_withdrawn_identity_claim_round_trips_as_an_absent_primary() {
        let action = AgentAction::MonitorIdentitiesChanged {
            links: vec![MonitorIdentityLink {
                alias: MonitorFingerprint::new("MSI", "7CF0", None::<String>),
                primary: None,
                updated_at_ms: 42,
            }],
        };

        let encoded = serde_json::to_string(&action).unwrap();
        assert_eq!(
            serde_json::from_str::<AgentAction>(&encoded).unwrap(),
            action
        );
    }

    #[test]
    fn older_agent_response_without_display_route_remains_compatible() {
        let response: AgentResponse =
            serde_json::from_str(r#"{"ready":true,"message":"ready"}"#).unwrap();
        assert!(response.ready);
        assert!(response.display_route.is_none());
        assert!(response.display_routes.is_empty());
        assert_eq!(response.protocol_version, 0);
        assert!(response.host_order.is_empty());
        assert_eq!(response.host_order_updated_at_ms, 0);
        assert!(response.host_aliases.is_empty());
    }

    #[test]
    fn agent_response_carries_host_layout_for_peers_that_missed_a_change() {
        let response = AgentResponse {
            host_order: vec!["2cf05de0c029-windows".to_owned()],
            host_order_updated_at_ms: 42,
            host_aliases: vec![HostAlias {
                host_id: "2cf05de0c029-windows".to_owned(),
                name: "遊戲電腦".to_owned(),
                updated_at_ms: 7,
            }],
            ..AgentResponse::default()
        };

        let serialized = serde_json::to_string(&response).unwrap();

        assert_eq!(
            serde_json::from_str::<AgentResponse>(&serialized).unwrap(),
            response
        );
    }

    #[test]
    fn pre_v2_switch_input_request_without_monitor_field_still_deserializes() {
        let action: AgentAction =
            serde_json::from_str(r#"{"type":"switch_input","input":17}"#).unwrap();
        assert_eq!(
            action,
            AgentAction::SwitchInput {
                monitor: None,
                input: DisplayInput::new(0x11).unwrap(),
            }
        );
    }

    #[test]
    fn switch_input_with_monitor_serializes_the_monitor_field() {
        let action = AgentAction::SwitchInput {
            monitor: Some(MonitorFingerprint::new("ACM", "1234", Some("SERIAL-1"))),
            input: DisplayInput::new(0x11).unwrap(),
        };
        let serialized = serde_json::to_value(&action).unwrap();
        assert!(serialized.get("monitor").is_some());
    }

    #[test]
    fn host_appearances_changed_notice_round_trips_with_every_entry() {
        let action = AgentAction::HostAppearancesChanged {
            appearances: vec![HostAppearance {
                host_id: "2cf05de0c029-windows".to_owned(),
                icon: "gamepad".to_owned(),
                color: "orange".to_owned(),
                updated_at_ms: 1_757_000_000_000,
            }],
        };

        let serialized = serde_json::to_string(&action).unwrap();

        assert!(serialized.contains(r#""type":"host_appearances_changed""#));
        assert!(serialized.contains(r#""icon":"gamepad""#));
        assert_eq!(
            serde_json::from_str::<AgentAction>(&serialized).unwrap(),
            action
        );
    }

    #[test]
    fn host_aliases_changed_notice_round_trips_with_every_entry() {
        let action = AgentAction::HostAliasesChanged {
            aliases: vec![HostAlias {
                host_id: "2cf05de0c029-windows".to_owned(),
                name: "遊戲電腦".to_owned(),
                updated_at_ms: 1_757_000_000_000,
            }],
        };

        let serialized = serde_json::to_string(&action).unwrap();

        assert!(serialized.contains(r#""type":"host_aliases_changed""#));
        assert!(serialized.contains(r#""hostId":"2cf05de0c029-windows""#));
        assert_eq!(
            serde_json::from_str::<AgentAction>(&serialized).unwrap(),
            action
        );
    }

    #[test]
    fn input_labels_changed_notice_round_trips_with_every_entry() {
        let action = AgentAction::InputLabelsChanged {
            labels: vec![InputLabel {
                monitor: MonitorFingerprint::new("MSI", "3CF0", None::<String>),
                input: DisplayInput::new(8).unwrap(),
                label: "USB-C".to_owned(),
                updated_at_ms: 1_757_000_000_000,
            }],
        };

        let serialized = serde_json::to_string(&action).unwrap();

        assert!(serialized.contains(r#""type":"input_labels_changed""#));
        assert!(serialized.contains(r#""updatedAtMs":1757000000000"#));
        assert_eq!(
            serde_json::from_str::<AgentAction>(&serialized).unwrap(),
            action
        );
    }

    #[test]
    fn a_response_from_an_agent_without_input_labels_still_deserializes() {
        let response: AgentResponse =
            serde_json::from_str(r#"{"ready":true,"message":"ok"}"#).unwrap();

        assert!(response.input_labels.is_empty());
    }

    #[test]
    fn host_order_changed_notice_round_trips_with_its_order_and_timestamp() {
        let action = AgentAction::HostOrderChanged {
            order: vec![
                "2cf05de0c029-windows".to_owned(),
                "aabbccddeeff-mac".to_owned(),
            ],
            updated_at_ms: 1_757_000_000_000,
        };

        let serialized = serde_json::to_string(&action).unwrap();

        assert!(serialized.contains(r#""type":"host_order_changed""#));
        assert_eq!(
            serde_json::from_str::<AgentAction>(&serialized).unwrap(),
            action
        );
    }

    #[test]
    fn local_host_identity_matches_the_id_peers_discover() {
        let identity = LocalHostIdentity::from_parts(
            "Henry-PC".to_owned(),
            DestinationHost::Windows,
            Some("AA:BB:CC:DD:EE:FF".to_owned()),
        );

        assert_eq!(identity.id, "aabbccddeeff-windows");
        assert_eq!(identity.name, "Henry-PC");
    }

    #[test]
    fn active_input_changed_notice_round_trips_with_its_monitor_and_input() {
        let action = AgentAction::ActiveInputChanged {
            monitor: MonitorFingerprint::new("MSI", "3CF0", None::<String>),
            input: DisplayInput::new(0x08).unwrap(),
        };

        let serialized = serde_json::to_string(&action).unwrap();

        assert!(serialized.contains(r#""type":"active_input_changed""#));
        assert_eq!(
            serde_json::from_str::<AgentAction>(&serialized).unwrap(),
            action
        );
    }

    #[test]
    fn local_input_confirmed_notice_round_trips_with_its_sender_monitor_and_input() {
        let action = AgentAction::LocalInputConfirmed {
            host_id: "2cf05de0c029-windows".to_owned(),
            monitor: MonitorFingerprint::new("MSI", "3CF0", None::<String>),
            input: DisplayInput::new(0x08).unwrap(),
        };

        let serialized = serde_json::to_string(&action).unwrap();

        assert!(serialized.contains(r#""type":"local_input_confirmed""#));
        assert_eq!(
            serde_json::from_str::<AgentAction>(&serialized).unwrap(),
            action
        );
    }

    #[test]
    fn host_inputs_notice_round_trips_an_explicit_clear() {
        let action = AgentAction::HostInputsChanged {
            assignments: vec![AgentHostInput {
                host_id: "2cf05de0c029-windows".to_owned(),
                monitor: MonitorFingerprint::new("MSI", "3CF0", None::<String>),
                input: None,
                updated_at_ms: 1_757_000_000_000,
            }],
        };

        let serialized = serde_json::to_string(&action).unwrap();

        assert!(serialized.contains(r#""type":"host_inputs_changed""#));
        assert!(serialized.contains(r#""input":null"#));
        assert_eq!(
            serde_json::from_str::<AgentAction>(&serialized).unwrap(),
            action
        );
    }

    #[test]
    fn a_response_from_an_agent_without_host_inputs_still_deserializes() {
        let response: AgentResponse =
            serde_json::from_str(r#"{"ready":true,"message":"ok"}"#).unwrap();

        assert!(response.host_inputs.is_empty());
    }

    /// An agent that predates `confirmed` omits it, and its reports must stay
    /// readable — as unconfirmed, the conservative reading.
    #[test]
    fn display_route_without_confirmed_deserializes_as_unconfirmed() {
        let route: AgentDisplayRoute = serde_json::from_str(
            r#"{"monitor":{"manufacturer_id":"MSI","product_code":"3CF0","serial_number":null},"input":8}"#,
        )
        .unwrap();

        assert!(!route.confirmed);
        assert_eq!(route.input.value(), 0x08);
    }

    /// Following a discovered address turns on this proof, and discovery is
    /// unauthenticated — so everything an impostor could try has to fail.
    #[test]
    fn only_a_reply_from_the_paired_host_proves_the_pairing() {
        let key = b"pairing-secret";
        let signed = AgentResponse {
            ready: true,
            protocol_version: AGENT_PROTOCOL_VERSION,
            message: "authenticated".to_owned(),
            ..AgentResponse::default()
        }
        .signed_for("nonce-a", key);

        assert!(signed.proves_pairing("nonce-a", key));
        assert!(
            !signed.proves_pairing("nonce-b", key),
            "a reply was accepted for a request it does not answer"
        );
        assert!(
            !signed.proves_pairing("nonce-a", b"a-different-secret"),
            "a reply signed with the wrong password was accepted"
        );
        assert!(
            !AgentResponse::default().proves_pairing("nonce-a", key),
            "an unsigned reply was taken as proof"
        );

        for tampered in [
            AgentResponse {
                protocol_version: AGENT_PROTOCOL_VERSION - 1,
                ..signed.clone()
            },
            AgentResponse {
                message: "tampered".to_owned(),
                ..signed.clone()
            },
            AgentResponse {
                ready: false,
                ..signed.clone()
            },
        ] {
            assert!(
                !tampered.proves_pairing("nonce-a", key),
                "a modified response payload retained a valid signature"
            );
        }
    }

    #[test]
    fn pairing_passwords_are_stretched_and_domain_separated() {
        let first = derive_pairing_key("a sufficiently long pairing password");
        let same = derive_pairing_key("a sufficiently long pairing password");
        let different = derive_pairing_key("a different sufficiently long password");

        assert_eq!(first, same);
        assert_ne!(first, different);
        assert_ne!(first.as_slice(), b"a sufficiently long pairing password");
    }

    #[test]
    fn a_signed_but_incompatible_response_is_rejected() {
        let key = b"pairing-secret";
        let response = AgentResponse {
            ready: true,
            protocol_version: AGENT_PROTOCOL_VERSION - 1,
            ..AgentResponse::default()
        }
        .signed_for("nonce-a", key);

        assert!(response.proves_pairing("nonce-a", key));
        assert_eq!(
            verify_agent_response(&response, "nonce-a", key),
            Err(DisplayMuxError::UnreadableRequest)
        );
    }

    /// The listener binds the reply to the request it answers, so the caller
    /// can tell the paired host from anything else that accepted the socket.
    #[tokio::test]
    async fn the_listener_signs_what_it_sends_back() {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let key = Arc::<[u8]>::from(&b"pairing-secret"[..]);
        let server = AgentServer::new(address, Arc::clone(&key));
        tokio::spawn(async move {
            server
                .run(|_| async {
                    AgentResponse {
                        ready: true,
                        protocol_version: AGENT_PROTOCOL_VERSION,
                        ..AgentResponse::default()
                    }
                })
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let endpoint = PeerEndpoint {
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: address.port(),
        };
        let response = AgentClient::new(endpoint, Arc::clone(&key))
            .request(AgentAction::Ping, "nonce-a")
            .await
            .unwrap();

        assert!(response.proves_pairing("nonce-a", &key));
    }

    /// Anyone on the network can open this socket, and nothing over it is
    /// authenticated until a whole line has arrived — so a caller that sends
    /// no line must be given up on rather than held open.
    #[tokio::test]
    async fn a_caller_that_sends_nothing_is_given_up_on() {
        use tokio::io::AsyncReadExt as _;

        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let server = AgentServer::new(address, Arc::<[u8]>::from(&b"pairing-secret"[..]))
            .with_request_timeout(Duration::from_millis(150));
        tokio::spawn(async move { server.run(|_| async { AgentResponse::default() }).await });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut silent = TcpStream::connect(address).await.unwrap();
        let mut buffer = Vec::new();
        // The server closes its side once the deadline passes, so the read
        // completes on end-of-file. Without the deadline this waits for ever.
        let outcome = timeout(Duration::from_secs(5), silent.read_to_end(&mut buffer)).await;

        assert!(
            outcome.is_ok(),
            "a connection that sent nothing was still being held open"
        );
    }

    /// Nothing is authenticated before a request line arrives, so the number
    /// of connections anyone can hold open must be bounded, per address and
    /// in total.
    #[test]
    fn open_connections_are_limited_per_address_and_in_total() {
        let limits = ConnectionLimits::new(3, 2);
        let first = IpAddr::from([192, 168, 1, 10]);
        let second = IpAddr::from([192, 168, 1, 11]);

        let a = limits.try_admit(first).expect("first connection");
        let _b = limits.try_admit(first).expect("second connection");
        assert!(limits.try_admit(first).is_none(), "per-address cap ignored");

        let _c = limits.try_admit(second).expect("another address");
        assert!(limits.try_admit(second).is_none(), "total cap ignored");

        drop(a);
        assert!(
            limits.try_admit(first).is_some(),
            "a closed connection did not free its slot"
        );
    }

    #[tokio::test]
    async fn a_connection_over_the_limit_is_closed_at_once() {
        use tokio::io::AsyncReadExt as _;

        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let server = AgentServer::new(address, Arc::<[u8]>::from(&b"pairing-secret"[..]))
            .with_connection_limits(4, 1);
        tokio::spawn(async move { server.run(|_| async { AgentResponse::default() }).await });
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Holds this address's only slot for the full request timeout.
        let _held = TcpStream::connect(address).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut refused = TcpStream::connect(address).await.unwrap();
        let mut buffer = Vec::new();
        let outcome = timeout(Duration::from_secs(1), refused.read_to_end(&mut buffer)).await;

        assert!(
            outcome.is_ok(),
            "a connection over the limit was held open instead of refused"
        );
    }

    /// The cache used to be emptied outright once it reached a count, which
    /// let every nonce in it through a second time.
    #[test]
    fn expiring_a_nonce_does_not_forget_one_that_can_still_be_replayed() {
        let skew = MAX_CLOCK_SKEW.as_secs();
        let now = 1_000_000_u64;
        let mut seen = HashMap::from([
            ("stale".to_owned(), now - skew - 1),
            ("recent".to_owned(), now - 1),
        ]);

        seen.retain(|_, at| now.saturating_sub(*at) <= skew);

        assert!(
            !seen.contains_key("stale"),
            "a nonce too old to be accepted anyway was kept"
        );
        assert!(
            seen.contains_key("recent"),
            "a nonce that could still be replayed was forgotten"
        );
    }

    #[tokio::test]
    async fn wake_packet_has_the_expected_shape() {
        let mac = "01:23:45:67:89:AB".parse::<MacAddress>().unwrap();
        let mut packet = [0_u8; 102];
        packet[..6].fill(0xff);
        for chunk in packet[6..].chunks_exact_mut(6) {
            chunk.copy_from_slice(&mac.octets());
        }
        assert!(packet[..6].iter().all(|byte| *byte == 0xff));
        assert!(packet[6..]
            .chunks_exact(6)
            .all(|chunk| chunk == mac.octets()));
    }
}
