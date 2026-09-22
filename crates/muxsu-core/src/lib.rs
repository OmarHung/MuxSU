mod capabilities;
mod connection;
mod domain;
#[cfg(any(target_os = "macos", target_os = "windows", test))]
// Windows only reads the interface; resolution still comes from display modes.
#[cfg_attr(target_os = "windows", allow(dead_code))]
mod edid;
mod error;
mod network;
mod port;
mod service;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
mod macos_connection;
#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(any(target_os = "windows", test))]
mod windows_connection;

pub use connection::{
    candidate_inputs, input_matches_sink, DdcRisk, HostOutput, MonitorConnection, SinkInterface,
};
pub use domain::{
    DestinationHost, DiscoveredPeer, DisplayInput, DisplayMuxProfile, MonitorDescriptor,
    MonitorFingerprint, MonitorId, MonitorResolution, ResolutionSource, SwitchMode, SwitchOutcome,
};
pub use error::DisplayMuxError;
pub use network::{
    derive_pairing_key, is_local_network_address, AgentAction, AgentClient, AgentDisplayRoute,
    AgentHostInput, AgentRequest, AgentResponse, AgentServer, HostAlias, HostAppearance,
    InputLabel, LocalHostIdentity, MacAddress, MdnsPeerDiscovery, MonitorIdentityLink,
    PeerEndpoint, WakeTarget, AGENT_PROTOCOL_VERSION, DEFAULT_AGENT_PORT,
};
pub use port::{MonitorControl, PeerDiscovery};
pub use service::DisplayMuxService;
