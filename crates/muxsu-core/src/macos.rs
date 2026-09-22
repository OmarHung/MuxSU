use std::{thread, time::Duration};

use ddc::Ddc;
use ddc_macos::Monitor;

use crate::{
    capabilities, edid,
    macos_connection::{self, DisplayLink},
    DisplayInput, DisplayMuxError, MonitorControl, MonitorDescriptor, MonitorFingerprint,
    MonitorId, MonitorResolution,
};

const INPUT_SELECT_VCP_CODE: u8 = 0x60;
/// macOS Type-C/USB-C DDC/CI transports intermittently return malformed
/// packets (e.g. "invalid DDC/CI length") even when the channel is
/// otherwise healthy; a short retry with a fresh monitor lookup resolves
/// most of these, matching the standard mitigation used by ddcutil and
/// similar tools for unreliable DDC buses.
const DDC_RETRY_ATTEMPTS: u32 = 3;
const DDC_RETRY_DELAY: Duration = Duration::from_millis(80);

/// Some monitor firmware (observed on an MStar-driven MSI display) ACKs a
/// VCP 0x60 SET without ever applying it, silently discarding the input
/// switch while still reporting success. Reading the input back after the
/// write catches this so callers see a real failure instead of a false
/// "switched" result.
const WRITE_VERIFY_DELAY: Duration = Duration::from_millis(300);
const WRITE_VERIFY_ATTEMPTS: u32 = 2;

fn with_ddc_retry<T>(
    mut operation: impl FnMut() -> Result<T, DisplayMuxError>,
) -> Result<T, DisplayMuxError> {
    let mut last_error = None;
    for attempt in 0..DDC_RETRY_ATTEMPTS {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) => {
                if attempt + 1 < DDC_RETRY_ATTEMPTS {
                    thread::sleep(DDC_RETRY_DELAY);
                }
                last_error = Some(error);
            }
        }
    }
    Err(last_error.expect("loop runs at least DDC_RETRY_ATTEMPTS >= 1 time"))
}

/// macOS DDC/CI adapter. `ddc-macos` chooses the Intel IOKit or Apple Silicon
/// display service at runtime, so the same implementation covers Mac mini,
/// MacBook Air and MacBook Pro connection paths that expose DDC.
pub struct MacOsMonitorController;

impl MacOsMonitorController {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for MacOsMonitorController {
    fn default() -> Self {
        Self::new()
    }
}

impl MonitorControl for MacOsMonitorController {
    fn enumerate(&self) -> Result<Vec<MonitorDescriptor>, DisplayMuxError> {
        let monitors = enumerate_external_online_monitors()?;
        let links = macos_connection::display_links();
        Ok(monitors
            .iter()
            .map(|monitor| descriptor(monitor, &links))
            .collect())
    }

    fn read_input(&self, monitor_id: &MonitorId) -> Result<DisplayInput, DisplayMuxError> {
        with_ddc_retry(|| {
            let mut monitor = find_monitor(monitor_id)?;
            let value = monitor
                .get_vcp_feature(INPUT_SELECT_VCP_CODE)
                .map_err(backend_error)?;
            DisplayInput::new(u32::from(value.value()))
        })
    }

    fn supported_inputs(
        &self,
        monitor_id: &MonitorId,
    ) -> Result<Vec<DisplayInput>, DisplayMuxError> {
        with_ddc_retry(|| {
            let mut monitor = find_monitor(monitor_id)?;
            let raw = monitor.capabilities_string().map_err(backend_error)?;
            let inputs = capabilities::parse_input_sources(&raw);
            if inputs.is_empty() {
                return Err(DisplayMuxError::Backend(
                    "macOS 顯示器 capabilities 未宣告 VCP 0x60 輸入值".to_owned(),
                ));
            }
            Ok(inputs)
        })
    }

    fn input_value_maximum(&self, monitor_id: &MonitorId) -> Result<Option<u32>, DisplayMuxError> {
        with_ddc_retry(|| {
            let mut monitor = find_monitor(monitor_id)?;
            let value = monitor
                .get_vcp_feature(INPUT_SELECT_VCP_CODE)
                .map_err(backend_error)?;
            Ok(Some(u32::from(value.maximum())))
        })
    }

    fn write_input(
        &self,
        monitor_id: &MonitorId,
        input: DisplayInput,
    ) -> Result<(), DisplayMuxError> {
        with_ddc_retry(|| {
            let mut monitor = find_monitor(monitor_id)?;
            monitor
                .set_vcp_feature(INPUT_SELECT_VCP_CODE, input.value() as u16)
                .map_err(backend_error)
        })?;

        // A transient read failure here does not prove the switch failed - it
        // just means we couldn't confirm it. Only a *clean* read that clearly
        // disagrees with the requested input, repeated across every verify
        // attempt, is treated as a real failure; anything else falls back to
        // trusting the write that already reported success above.
        for _ in 0..WRITE_VERIFY_ATTEMPTS {
            thread::sleep(WRITE_VERIFY_DELAY);
            let confirmed = with_ddc_retry(|| {
                let mut monitor = find_monitor(monitor_id)?;
                let value = monitor
                    .get_vcp_feature(INPUT_SELECT_VCP_CODE)
                    .map_err(backend_error)?;
                DisplayInput::new(u32::from(value.value()))
            });
            match confirmed {
                Ok(value) if value == input => return Ok(()),
                Ok(_) => continue,
                Err(_) => return Ok(()),
            }
        }

        Err(DisplayMuxError::Backend(format!(
            "顯示器未執行輸入切換指令（要求 {:#x}）：這台顯示器的韌體可能不支援透過 DDC/CI 遠端切換輸入源",
            input.value()
        )))
    }
}

fn find_monitor(monitor_id: &MonitorId) -> Result<Monitor, DisplayMuxError> {
    let matches = enumerate_external_online_monitors()?
        .into_iter()
        .filter(|monitor| id_for(&fingerprint(monitor, monitor.edid().as_deref())) == *monitor_id)
        .collect::<Vec<_>>();

    match matches.len() {
        0 => Err(DisplayMuxError::MonitorNoLongerAvailable(
            monitor_id.as_str().to_owned(),
        )),
        1 => Ok(matches.into_iter().next().expect("length checked")),
        count => Err(DisplayMuxError::AmbiguousTarget { count }),
    }
}

fn enumerate_external_online_monitors() -> Result<Vec<Monitor>, DisplayMuxError> {
    Ok(Monitor::enumerate()
        .map_err(backend_error)?
        .into_iter()
        .filter(|monitor| {
            let display = monitor.handle();
            display.is_online() && !display.is_builtin()
        })
        .collect())
}

fn descriptor(monitor: &Monitor, links: &[DisplayLink]) -> MonitorDescriptor {
    let raw_edid = monitor.edid();
    let fingerprint = fingerprint(monitor, raw_edid.as_deref());
    let (max_resolution, resolution_source) =
        edid::preferred_resolution(raw_edid.as_deref(), core_graphics_resolution(monitor));
    let connection = macos_connection::connection_for(links, &fingerprint, |edid| {
        fingerprint_from_edid(edid).ok()
    });

    MonitorDescriptor {
        id: id_for(&fingerprint),
        name: monitor.description(),
        fingerprint,
        active: true,
        built_in: false,
        max_resolution,
        resolution_source,
        connection,
    }
}

fn fingerprint(monitor: &Monitor, raw_edid: Option<&[u8]>) -> MonitorFingerprint {
    let handle = monitor.handle();
    raw_edid
        .and_then(|value| match fingerprint_from_edid(value) {
            Ok(fingerprint) => Some(fingerprint),
            Err(error) => {
                tracing::warn!(
                    monitor = %monitor.description(),
                    error = %error,
                    "macOS monitor EDID is unusable; using CoreGraphics identity"
                );
                None
            }
        })
        .unwrap_or_else(|| {
            fingerprint_from_native_ids(
                handle.vendor_number(),
                handle.model_number(),
                monitor.serial_number(),
            )
        })
}

fn id_for(fingerprint: &MonitorFingerprint) -> MonitorId {
    MonitorId::new(format!("macos:{}", fingerprint.stable_key()))
}

fn fingerprint_from_edid(edid: &[u8]) -> Result<MonitorFingerprint, DisplayMuxError> {
    if !edid::is_valid(edid) {
        return Err(DisplayMuxError::Backend(
            "顯示器 EDID 標頭、長度或 checksum 無效，無法安全識別裝置".to_owned(),
        ));
    }

    let manufacturer = u16::from_be_bytes([edid[8], edid[9]]);
    let manufacturer_id = [
        manufacturer_character((manufacturer >> 10) & 0x1f),
        manufacturer_character((manufacturer >> 5) & 0x1f),
        manufacturer_character(manufacturer & 0x1f),
    ]
    .into_iter()
    .collect::<String>();
    let product_code = format!("{:04X}", u16::from_le_bytes([edid[10], edid[11]]));
    let serial = u32::from_le_bytes([edid[12], edid[13], edid[14], edid[15]]);

    Ok(MonitorFingerprint::new(
        manufacturer_id,
        product_code,
        (serial != 0).then(|| serial.to_string()),
    ))
}

fn fingerprint_from_native_ids(
    vendor_number: u32,
    model_number: u32,
    serial_number: Option<String>,
) -> MonitorFingerprint {
    let manufacturer = vendor_number as u16;
    let manufacturer_id = [
        manufacturer_character((manufacturer >> 10) & 0x1f),
        manufacturer_character((manufacturer >> 5) & 0x1f),
        manufacturer_character(manufacturer & 0x1f),
    ]
    .into_iter()
    .collect::<String>();

    MonitorFingerprint::new(
        manufacturer_id,
        format!("{:04X}", model_number),
        serial_number,
    )
}

fn manufacturer_character(value: u16) -> char {
    if (1..=26).contains(&value) {
        char::from_u32(u32::from(value) + 64).unwrap_or('?')
    } else {
        '?'
    }
}

fn backend_error(error: impl std::fmt::Display) -> DisplayMuxError {
    DisplayMuxError::Backend(format!(
        "macOS 無法透過目前的 HDMI／USB-C／Thunderbolt 路徑使用 DDC/CI：{error}"
    ))
}

fn core_graphics_resolution(monitor: &Monitor) -> Option<MonitorResolution> {
    let mode = monitor.handle().display_mode()?;
    let width = u32::try_from(mode.pixel_width()).ok()?;
    let height = u32::try_from(mode.pixel_height()).ok()?;
    (width > 0 && height > 0).then(|| MonitorResolution::new(width, height))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn ddc_retry_succeeds_after_a_transient_failure() {
        let attempts = Cell::new(0);
        let result = with_ddc_retry(|| {
            attempts.set(attempts.get() + 1);
            if attempts.get() < 2 {
                Err(DisplayMuxError::Backend("invalid DDC/CI length".to_owned()))
            } else {
                Ok(42)
            }
        });
        assert_eq!(result, Ok(42));
        assert_eq!(attempts.get(), 2);
    }

    #[test]
    fn ddc_retry_gives_up_after_exhausting_attempts() {
        let attempts = Cell::new(0);
        let result = with_ddc_retry(|| {
            attempts.set(attempts.get() + 1);
            Err::<(), _>(DisplayMuxError::Backend("invalid DDC/CI length".to_owned()))
        });
        assert!(result.is_err());
        assert_eq!(attempts.get(), DDC_RETRY_ATTEMPTS);
    }

    fn finalize_edid(edid: &mut [u8; 128]) {
        edid[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
        edid[127] = 0_u8.wrapping_sub(
            edid[..127]
                .iter()
                .fold(0_u8, |sum, byte| sum.wrapping_add(*byte)),
        );
    }

    #[test]
    fn parses_asus_edid_identity() {
        let mut edid = [0_u8; 128];
        let manufacturer = (1_u16 << 10) | (21_u16 << 5) | 19_u16;
        [edid[8], edid[9]] = manufacturer.to_be_bytes();
        [edid[10], edid[11]] = 0x3554_u16.to_le_bytes();
        [edid[12], edid[13], edid[14], edid[15]] = 278_504_u32.to_le_bytes();
        finalize_edid(&mut edid);

        let fingerprint = fingerprint_from_edid(&edid).unwrap();

        assert_eq!(fingerprint.manufacturer_id, "AUS");
        assert_eq!(fingerprint.product_code, "3554");
        assert_eq!(fingerprint.serial_number.as_deref(), Some("278504"));
    }

    #[test]
    fn builds_matching_identity_from_core_graphics_when_edid_is_missing() {
        let vendor_number = (1_u32 << 10) | (21_u32 << 5) | 19_u32;

        let fingerprint =
            fingerprint_from_native_ids(vendor_number, 0x3554, Some("278504".to_owned()));

        assert_eq!(fingerprint.manufacturer_id, "AUS");
        assert_eq!(fingerprint.product_code, "3554");
        assert_eq!(fingerprint.serial_number.as_deref(), Some("278504"));
    }
}
