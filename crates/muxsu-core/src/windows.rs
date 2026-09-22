use std::{collections::HashMap, ffi::c_void, mem::size_of, ptr, thread, time::Duration};

use serde::Deserialize;
use windows_sys::core::BOOL;
use windows_sys::Win32::{
    Devices::Display::{
        CapabilitiesRequestAndCapabilitiesReply, DestroyPhysicalMonitor,
        GetCapabilitiesStringLength, GetNumberOfPhysicalMonitorsFromHMONITOR,
        GetPhysicalMonitorsFromHMONITOR, GetVCPFeatureAndVCPFeatureReply, SetVCPFeature,
        PHYSICAL_MONITOR,
    },
    Foundation::{LPARAM, RECT},
    Graphics::Gdi::{
        EnumDisplayDevicesW, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, DEVMODEW,
        DISPLAY_DEVICEW, HDC, HMONITOR, MONITORINFOEXW,
    },
    UI::WindowsAndMessaging::EDD_GET_DEVICE_INTERFACE_NAME,
};
use wmi::WMIConnection;

use crate::{
    capabilities, windows_connection, DisplayInput, DisplayMuxError, MonitorControl,
    MonitorDescriptor, MonitorFingerprint, MonitorId, MonitorResolution, PowerState,
    ResolutionSource,
};

const INPUT_SOURCE_VCP_CODE: u8 = 0x60;
const POWER_MODE_VCP_CODE: u8 = 0xd6;
const MAX_CAPABILITIES_LENGTH: u32 = 64 * 1024;

/// Windows reports transient DDC/CI faults on a bus that is otherwise
/// healthy — most often `ERROR_GRAPHICS_DDCCI_INVALID_MESSAGE_COMMAND`
/// (`0xC0262585`), observed on an MSI MPG 274U in the seconds after an input
/// change, while the display is still re-syncing and answering badly. A short
/// retry with a freshly opened physical-monitor handle clears them; this is
/// the same mitigation the macOS path and ddcutil already use, and without it
/// one bad reply fails a switch a paired host is relying on.
const DDC_RETRY_ATTEMPTS: u32 = 3;
const DDC_RETRY_DELAY: Duration = Duration::from_millis(120);

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

pub struct WindowsMonitorController;

impl WindowsMonitorController {
    pub fn new() -> Result<Self, DisplayMuxError> {
        Ok(Self)
    }

    fn enumerate_native(&self) -> Result<Vec<NativeMonitor>, DisplayMuxError> {
        let wmi_monitors = query_wmi_monitors()?;
        let logical_monitors = enumerate_logical_monitors()?;
        let mut native_monitors = Vec::new();

        for logical in logical_monitors {
            let details = monitor_logical_details(logical)?;
            let identity = parse_device_path(&details.device_path)?;
            let wmi_monitor = wmi_monitors
                .get(&identity.wmi_instance_key)
                .ok_or_else(|| {
                    DisplayMuxError::Backend(format!(
                        "無法取得 {} 的 EDID 序號；為避免誤控，已停止列舉",
                        details.device_path
                    ))
                })?;
            let physical_monitors = physical_monitors(logical)?;
            let raw_edid = windows_connection::read_cached_edid(&identity.wmi_instance_key);
            let connection = windows_connection::connection(
                wmi_monitor.video_output_technology,
                raw_edid.as_deref(),
            );

            for (index, physical) in physical_monitors.into_iter().enumerate() {
                let description_buffer = physical.szPhysicalMonitorDescription;
                let description = wide_string(&description_buffer);
                let name = decode_edid_text(&wmi_monitor.user_friendly_name)
                    .filter(|value| !value.is_empty())
                    .unwrap_or(description);
                let id = MonitorId::new(format!(
                    "{}::physical:{index}",
                    details.device_path.to_ascii_uppercase()
                ));

                native_monitors.push(NativeMonitor {
                    descriptor: MonitorDescriptor {
                        id,
                        name,
                        fingerprint: MonitorFingerprint::new(
                            &identity.manufacturer_id,
                            &identity.product_code,
                            decode_edid_text(&wmi_monitor.serial_number_id),
                        ),
                        active: wmi_monitor.active,
                        built_in: wmi_monitor.built_in,
                        max_resolution: details.max_resolution,
                        resolution_source: details
                            .max_resolution
                            .map(|_| ResolutionSource::WindowsDisplayMode),
                        connection: connection.clone(),
                    },
                    handle: physical.hPhysicalMonitor,
                });
            }
        }

        Ok(native_monitors)
    }

    fn find_native(&self, id: &MonitorId) -> Result<NativeMonitor, DisplayMuxError> {
        self.enumerate_native()?
            .into_iter()
            .find(|monitor| monitor.descriptor.id == *id)
            .ok_or_else(|| DisplayMuxError::MonitorNoLongerAvailable(id.as_str().to_owned()))
    }

    /// Returns the display's `(current, maximum)` reply for VCP 0x60.
    fn read_input_reply(&self, monitor: &MonitorId) -> Result<(u32, u32), DisplayMuxError> {
        with_ddc_retry(|| {
            let native = self.find_native(monitor)?;
            let mut code_type = 0;
            let mut current = 0;
            let mut maximum = 0;

            // SAFETY: `native.handle` is an owned, live physical-monitor handle. All out-pointers
            // reference initialized local `u32` values for the duration of the call.
            let succeeded = unsafe {
                GetVCPFeatureAndVCPFeatureReply(
                    native.handle,
                    INPUT_SOURCE_VCP_CODE,
                    &mut code_type,
                    &mut current,
                    &mut maximum,
                )
            };
            if succeeded == 0 {
                return Err(last_windows_error("無法讀取共用螢幕目前的輸入來源"));
            }

            Ok((current, maximum))
        })
    }

    /// The display's raw MCCS capabilities string, which names both the
    /// inputs it takes and the power states it takes.
    fn read_capabilities(&self, monitor: &MonitorId) -> Result<Vec<u8>, DisplayMuxError> {
        let native = self.find_native(monitor)?;
        let mut length = 0_u32;
        // SAFETY: the physical-monitor handle is live and `length` is a valid out-pointer.
        if unsafe { GetCapabilitiesStringLength(native.handle, &mut length) } == 0 || length == 0 {
            return Err(last_windows_error("無法取得螢幕 MCCS capabilities 長度"));
        }
        if length > MAX_CAPABILITIES_LENGTH {
            return Err(DisplayMuxError::Backend(format!(
                "螢幕回報的 MCCS capabilities 長度不合理：{length} bytes"
            )));
        }
        let mut raw = vec![0_u8; length as usize];
        // SAFETY: `raw` contains `length` writable bytes and the monitor handle remains live.
        if unsafe {
            CapabilitiesRequestAndCapabilitiesReply(native.handle, raw.as_mut_ptr(), length)
        } == 0
        {
            return Err(last_windows_error("無法讀取螢幕 MCCS capabilities"));
        }
        let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
        raw.truncate(end);
        Ok(raw)
    }
}

impl MonitorControl for WindowsMonitorController {
    fn enumerate(&self) -> Result<Vec<MonitorDescriptor>, DisplayMuxError> {
        Ok(self
            .enumerate_native()?
            .into_iter()
            .map(|monitor| monitor.descriptor.clone())
            .collect())
    }

    fn read_input(&self, monitor: &MonitorId) -> Result<DisplayInput, DisplayMuxError> {
        let (current, _) = self.read_input_reply(monitor)?;
        DisplayInput::new(current)
    }

    fn input_value_maximum(&self, monitor: &MonitorId) -> Result<Option<u32>, DisplayMuxError> {
        let (_, maximum) = self.read_input_reply(monitor)?;
        Ok(Some(maximum))
    }

    fn supported_inputs(&self, monitor: &MonitorId) -> Result<Vec<DisplayInput>, DisplayMuxError> {
        with_ddc_retry(|| {
            let raw = self.read_capabilities(monitor)?;
            let inputs = capabilities::parse_input_sources(&raw);
            if inputs.is_empty() {
                return Err(DisplayMuxError::Backend(
                    "Windows 顯示器 capabilities 未宣告 VCP 0x60 輸入值".to_owned(),
                ));
            }
            Ok(inputs)
        })
    }

    fn write_input(&self, monitor: &MonitorId, input: DisplayInput) -> Result<(), DisplayMuxError> {
        with_ddc_retry(|| {
            let native = self.find_native(monitor)?;

            // SAFETY: `native.handle` is an owned, live physical-monitor handle, VCP 0x60 is the
            // MCCS input-source feature, and `DisplayInput` restricts values to one byte.
            let succeeded =
                unsafe { SetVCPFeature(native.handle, INPUT_SOURCE_VCP_CODE, input.value()) };
            if succeeded == 0 {
                return Err(last_windows_error("無法切換共用螢幕輸入來源"));
            }

            Ok(())
        })
    }

    fn write_power_state(
        &self,
        monitor: &MonitorId,
        state: PowerState,
    ) -> Result<(), DisplayMuxError> {
        with_ddc_retry(|| {
            let native = self.find_native(monitor)?;

            // SAFETY: `native.handle` is an owned, live physical-monitor handle, VCP 0xD6 is the
            // MCCS power-mode feature, and `PowerState` only ever yields one of its defined values.
            let succeeded = unsafe {
                SetVCPFeature(
                    native.handle,
                    POWER_MODE_VCP_CODE,
                    u32::from(state.vcp_value()),
                )
            };
            if succeeded == 0 {
                return Err(last_windows_error("無法變更共用螢幕電源狀態"));
            }

            Ok(())
        })
    }
}

struct NativeMonitor {
    descriptor: MonitorDescriptor,
    handle: *mut c_void,
}

impl Drop for NativeMonitor {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: this type exclusively owns the physical-monitor handle and drops it once.
            let succeeded = unsafe { DestroyPhysicalMonitor(self.handle) };
            if succeeded == 0 {
                tracing::warn!(
                    monitor_id = self.descriptor.id.as_str(),
                    "failed to release physical monitor handle"
                );
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WmiMonitorId {
    instance_name: String,
    active: bool,
    serial_number_id: Vec<u16>,
    user_friendly_name: Vec<u16>,
    #[serde(skip)]
    built_in: bool,
    #[serde(skip)]
    video_output_technology: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WmiMonitorConnectionParams {
    instance_name: String,
    video_output_technology: u32,
}

fn query_wmi_monitors() -> Result<HashMap<String, WmiMonitorId>, DisplayMuxError> {
    let connection = WMIConnection::with_namespace_path("ROOT\\WMI")
        .map_err(|error| backend_error("無法連線 Windows WMI 螢幕資料", error))?;
    let monitors: Vec<WmiMonitorId> = connection
        .raw_query(
            "SELECT InstanceName, Active, SerialNumberID, UserFriendlyName FROM WmiMonitorID",
        )
        .map_err(|error| backend_error("無法讀取 Windows 螢幕 EDID", error))?;
    let connections: Vec<WmiMonitorConnectionParams> = connection
        .raw_query("SELECT InstanceName, VideoOutputTechnology FROM WmiMonitorConnectionParams")
        .map_err(|error| backend_error("無法判斷 Windows 內建螢幕", error))?;
    let connection_types = connections
        .into_iter()
        .map(|params| {
            (
                normalize_wmi_instance(&params.instance_name),
                params.video_output_technology,
            )
        })
        .collect::<HashMap<_, _>>();
    Ok(monitors
        .into_iter()
        .map(|mut monitor| {
            let key = normalize_wmi_instance(&monitor.instance_name);
            monitor.video_output_technology = connection_types.get(&key).copied();
            monitor.built_in = monitor
                .video_output_technology
                .is_some_and(is_internal_output);
            (key, monitor)
        })
        .collect())
}

fn is_internal_output(technology: u32) -> bool {
    // DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS, DISPLAYPORT_EMBEDDED,
    // UDI_EMBEDDED, or INTERNAL.
    matches!(technology, 6 | 11 | 13 | 0x8000_0000)
}

struct DeviceIdentity {
    manufacturer_id: String,
    product_code: String,
    wmi_instance_key: String,
}

fn parse_device_path(device_path: &str) -> Result<DeviceIdentity, DisplayMuxError> {
    let normalized = device_path
        .trim_start_matches("\\\\?\\")
        .to_ascii_uppercase();
    let parts = normalized.split('#').collect::<Vec<_>>();
    if parts.len() < 3 || parts[0] != "DISPLAY" || parts[1].len() < 4 {
        return Err(DisplayMuxError::Backend(format!(
            "無法解析 Windows 螢幕裝置路徑：{device_path}"
        )));
    }

    let hardware_id = parts[1];
    Ok(DeviceIdentity {
        manufacturer_id: hardware_id[..3].to_owned(),
        product_code: hardware_id[3..].to_owned(),
        wmi_instance_key: format!("DISPLAY\\{}\\{}", hardware_id, parts[2]),
    })
}

fn normalize_wmi_instance(instance: &str) -> String {
    instance
        .strip_suffix("_0")
        .unwrap_or(instance)
        .to_ascii_uppercase()
}

fn decode_edid_text(values: &[u16]) -> Option<String> {
    let text = values
        .iter()
        .copied()
        .take_while(|value| *value != 0)
        .filter_map(|value| char::from_u32(value as u32))
        .collect::<String>()
        .trim()
        .to_owned();
    (!text.is_empty()).then_some(text)
}

fn wide_string(values: &[u16]) -> String {
    String::from_utf16_lossy(
        &values
            .iter()
            .copied()
            .take_while(|value| *value != 0)
            .collect::<Vec<_>>(),
    )
}

struct LogicalMonitorDetails {
    device_path: String,
    max_resolution: Option<MonitorResolution>,
}

fn monitor_max_resolution(device_name: &[u16]) -> Option<MonitorResolution> {
    let mut mode_num = 0;
    let mut devmode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let mut max_width = 0u32;
    let mut max_height = 0u32;

    loop {
        // SAFETY: `device_name` is null-terminated UTF-16, `devmode` has `dmSize` initialized.
        let ok = unsafe { EnumDisplaySettingsW(device_name.as_ptr(), mode_num, &mut devmode) };
        if ok == 0 {
            break;
        }
        let w = devmode.dmPelsWidth;
        let h = devmode.dmPelsHeight;
        let area = (w as u64) * (h as u64);
        let current_max_area = (max_width as u64) * (max_height as u64);
        if area > current_max_area || (area == current_max_area && w > max_width) {
            max_width = w;
            max_height = h;
        }
        mode_num += 1;
    }

    if max_width > 0 && max_height > 0 {
        Some(MonitorResolution::new(max_width, max_height))
    } else {
        None
    }
}

fn monitor_logical_details(monitor: HMONITOR) -> Result<LogicalMonitorDetails, DisplayMuxError> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;

    // SAFETY: `info` has the required `cbSize`, remains live for the call, and the cast is valid
    // because MONITORINFOEXW begins with MONITORINFO as required by Win32.
    let info_succeeded = unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) };
    if info_succeeded == 0 {
        return Err(last_windows_error("無法取得 Windows 邏輯螢幕資訊"));
    }

    let mut device = DISPLAY_DEVICEW {
        cb: size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };

    // SAFETY: `info.szDevice` is a null-terminated buffer populated by GetMonitorInfoW and
    // `device` is initialized with the correct structure size.
    let device_succeeded = unsafe {
        EnumDisplayDevicesW(
            info.szDevice.as_ptr(),
            0,
            &mut device,
            EDD_GET_DEVICE_INTERFACE_NAME,
        )
    };
    if device_succeeded == 0 {
        return Err(last_windows_error("無法取得 Windows 實體螢幕裝置路徑"));
    }

    let device_path = wide_string(&device.DeviceID);
    if device_path.is_empty() {
        return Err(DisplayMuxError::Backend(
            "Windows 未提供螢幕裝置路徑；為避免誤控，已停止操作".to_owned(),
        ));
    }

    let max_resolution = monitor_max_resolution(&info.szDevice);

    Ok(LogicalMonitorDetails {
        device_path,
        max_resolution,
    })
}

fn physical_monitors(monitor: HMONITOR) -> Result<Vec<PHYSICAL_MONITOR>, DisplayMuxError> {
    let mut count = 0;

    // SAFETY: `count` is a valid out-pointer and `monitor` came from EnumDisplayMonitors.
    let count_succeeded = unsafe { GetNumberOfPhysicalMonitorsFromHMONITOR(monitor, &mut count) };
    if count_succeeded == 0 {
        return Err(last_windows_error("無法取得實體螢幕數量"));
    }

    let mut physical = (0..count)
        .map(|_| PHYSICAL_MONITOR::default())
        .collect::<Vec<_>>();

    // SAFETY: the vector has exactly `count` initialized slots and remains allocated for the call.
    let enumerate_succeeded =
        unsafe { GetPhysicalMonitorsFromHMONITOR(monitor, count, physical.as_mut_ptr()) };
    if enumerate_succeeded == 0 {
        return Err(last_windows_error("無法列舉實體螢幕控制介面"));
    }

    Ok(physical)
}

fn enumerate_logical_monitors() -> Result<Vec<HMONITOR>, DisplayMuxError> {
    unsafe extern "system" fn callback(
        monitor: HMONITOR,
        _device_context: HDC,
        _bounds: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        // SAFETY: `data` is the pointer to the live Vec passed to EnumDisplayMonitors below;
        // Win32 invokes callbacks synchronously before that Vec leaves scope.
        let monitors = unsafe { &mut *(data as *mut Vec<HMONITOR>) };
        monitors.push(monitor);
        1
    }

    let mut monitors = Vec::new();
    let data = &mut monitors as *mut Vec<HMONITOR> as LPARAM;

    // SAFETY: null HDC/clip enumerate all desktop monitors, callback has the required ABI, and
    // `data` points to a live Vec for the synchronous duration of the call.
    let succeeded =
        unsafe { EnumDisplayMonitors(ptr::null_mut(), ptr::null(), Some(callback), data) };
    if succeeded == 0 {
        return Err(last_windows_error("無法列舉 Windows 邏輯螢幕"));
    }

    Ok(monitors)
}

fn last_windows_error(action: &str) -> DisplayMuxError {
    DisplayMuxError::Backend(format!("{action}：{}", std::io::Error::last_os_error()))
}

fn backend_error(action: &str, error: impl std::fmt::Display) -> DisplayMuxError {
    DisplayMuxError::Backend(format!("{action}：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_monitor_interface_path() {
        let identity = parse_device_path(
            r"\\?\DISPLAY#AUS3554#5&5405411&0&UID4353#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}",
        )
        .expect("path parses");

        assert_eq!(identity.manufacturer_id, "AUS");
        assert_eq!(identity.product_code, "3554");
        assert_eq!(
            identity.wmi_instance_key,
            r"DISPLAY\AUS3554\5&5405411&0&UID4353"
        );
    }

    #[test]
    fn normalizes_wmi_instance_suffix() {
        assert_eq!(
            normalize_wmi_instance(r"DISPLAY\AUS3554\5&5405411&0&UID4353_0"),
            r"DISPLAY\AUS3554\5&5405411&0&UID4353"
        );
    }

    #[test]
    fn identifies_only_embedded_windows_output_technologies_as_internal() {
        for technology in [6, 11, 13, 0x8000_0000] {
            assert!(is_internal_output(technology));
        }
        for technology in [4, 5, 10, 12, 16] {
            assert!(!is_internal_output(technology));
        }
    }
}
