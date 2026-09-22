use crate::{
    DiscoveredPeer, DisplayInput, DisplayMuxError, MonitorDescriptor, MonitorId, PowerState,
};

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

    /// The values the display declares for VCP 0xD6 in its capabilities
    /// string. `None` when it names the feature without a value list, or does
    /// not name it at all: both mean nothing is known, and neither may be
    /// read as "it takes none".
    fn supported_power_states(
        &self,
        _monitor: &MonitorId,
    ) -> Result<Option<Vec<u32>>, DisplayMuxError> {
        Ok(None)
    }

    /// Writes the display's own power state (VCP 0xD6), leaving every input
    /// selection as it is.
    ///
    /// The default refuses rather than reports success: a controller with no
    /// power channel must not let a caller believe it darkened a panel it
    /// never touched.
    fn write_power_state(
        &self,
        _monitor: &MonitorId,
        _state: PowerState,
    ) -> Result<(), DisplayMuxError> {
        Err(DisplayMuxError::UnsupportedPlatform)
    }
}

pub trait PeerDiscovery {
    fn peers(&self) -> Result<Vec<DiscoveredPeer>, DisplayMuxError>;
}
