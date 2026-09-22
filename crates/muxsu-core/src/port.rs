use crate::{DiscoveredPeer, DisplayInput, DisplayMuxError, MonitorDescriptor, MonitorId};

pub trait MonitorControl {
    fn enumerate(&self) -> Result<Vec<MonitorDescriptor>, DisplayMuxError>;

    fn read_input(&self, monitor: &MonitorId) -> Result<DisplayInput, DisplayMuxError>;

    fn supported_inputs(&self, monitor: &MonitorId) -> Result<Vec<DisplayInput>, DisplayMuxError>;

    /// The maximum value the display reports for VCP 0x60. Some firmware
    /// (e.g. MStar-based MSI panels) ignores the MCCS input codes it
    /// advertises and instead selects inputs by a private index in
    /// `1..=maximum`; callers use this to build that fallback list.
    fn input_value_maximum(&self, _monitor: &MonitorId) -> Result<Option<u32>, DisplayMuxError> {
        Ok(None)
    }

    fn write_input(&self, monitor: &MonitorId, input: DisplayInput) -> Result<(), DisplayMuxError>;
}

pub trait PeerDiscovery {
    fn peers(&self) -> Result<Vec<DiscoveredPeer>, DisplayMuxError>;
}
