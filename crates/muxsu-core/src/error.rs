use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DisplayMuxError {
    #[error("找不到設定的共用螢幕；未變更任何螢幕")]
    TargetNotFound,

    #[error("找到 {count} 台符合共用螢幕指紋的裝置；為避免誤控，已停止操作")]
    AmbiguousTarget { count: usize },

    #[error("找不到先前列舉的螢幕裝置：{0}")]
    MonitorNoLongerAvailable(String),

    #[error("無效的螢幕輸入值：0x{0:02X}")]
    InvalidInput(u32),

    #[error("無法辨識螢幕輸入值：{0}；請輸入 0x01 至 0xFF，或使用十進位 1 至 255")]
    InvalidInputCode(String),

    #[error("此平台尚未提供螢幕控制功能")]
    UnsupportedPlatform,

    #[error("MAC 位址格式無效：{0}")]
    InvalidMacAddress(String),

    #[error("無法送出網路喚醒封包：{0}")]
    WakeFailed(String),

    #[error("無法連線至另一台主機：{0}")]
    PeerUnavailable(String),

    #[error("另一台主機拒絕了未通過驗證的要求")]
    AuthenticationFailed,

    #[error("要求已過期或可能被重播")]
    StaleRequest,

    /// The request could not be read at all. Almost always a host running a
    /// version that predates whatever was sent, which must not be reported as
    /// a wrong pairing password: the user then changes a password that was
    /// right, and the version gap stays.
    #[error("另一台主機無法解讀這個要求，通常是兩台主機版本不同")]
    UnreadableRequest,

    #[error("無法完成螢幕操作：{0}")]
    Backend(String),

    /// A setting the user just entered, rejected with a message that already
    /// names the problem. Backend's "無法完成螢幕操作" prefix belongs to a
    /// display that would not respond; on a shortcut or a pairing password it
    /// sends the reader looking at the display for a fault that is not there.
    #[error("{zh}")]
    Rejected { zh: String, en: String },
}

impl DisplayMuxError {
    pub fn localized_message(&self, traditional_chinese: bool) -> String {
        if traditional_chinese {
            return self.to_string();
        }

        match self {
            Self::TargetNotFound =>
                "The configured shared display was not found; no display was changed".to_owned(),
            Self::AmbiguousTarget { count } => format!(
                "Found {count} displays matching the shared display fingerprint; stopped to prevent controlling the wrong display"
            ),
            Self::MonitorNoLongerAvailable(id) =>
                format!("The previously enumerated display is no longer available: {id}"),
            Self::InvalidInput(value) => format!("Invalid display input value: 0x{value:02X}"),
            Self::InvalidInputCode(value) => format!(
                "Unrecognized display input value: {value}; enter 0x01 through 0xFF or decimal 1 through 255"
            ),
            Self::UnsupportedPlatform =>
                "Display control is not available on this platform".to_owned(),
            Self::InvalidMacAddress(value) => format!("Invalid MAC address: {value}"),
            Self::WakeFailed(detail) => format!(
                "Unable to send the Wake-on-LAN packet: {}",
                english_detail(detail)
            ),
            Self::PeerUnavailable(detail) => format!(
                "Unable to connect to the other host: {}",
                english_detail(detail)
            ),
            Self::AuthenticationFailed =>
                "The other host rejected an unauthenticated request".to_owned(),
            Self::StaleRequest => "The request expired or may have been replayed".to_owned(),
            Self::UnreadableRequest =>
                "The other host could not read this request, usually because the two hosts are on different versions"
                    .to_owned(),
            Self::Backend(detail) => format!(
                "Unable to complete the display operation: {}",
                english_detail(detail)
            ),
            Self::Rejected { en, .. } => en.clone(),
        }
    }
}

fn english_detail(detail: &str) -> String {
    const PREFIXES: [(&str, &str); 13] = [
        ("回應格式無效：", "Invalid response format: "),
        (
            "macOS 無法透過目前的 HDMI／USB-C／Thunderbolt 路徑使用 DDC/CI：",
            "macOS cannot use DDC/CI through the current HDMI/USB-C/Thunderbolt path: ",
        ),
        (
            "無法讀取共用螢幕目前的輸入來源：",
            "Unable to read the shared display's current input: ",
        ),
        (
            "無法切換共用螢幕輸入來源：",
            "Unable to switch the shared display input: ",
        ),
        (
            "無法連線 Windows WMI 螢幕資料：",
            "Unable to connect to Windows WMI display data: ",
        ),
        (
            "無法讀取 Windows 螢幕 EDID：",
            "Unable to read the Windows display EDID: ",
        ),
        (
            "無法判斷 Windows 內建螢幕：",
            "Unable to identify the Windows built-in display: ",
        ),
        (
            "無法解析 Windows 螢幕裝置路徑：",
            "Unable to parse the Windows display device path: ",
        ),
        (
            "無法取得 Windows 邏輯螢幕資訊：",
            "Unable to get Windows logical display information: ",
        ),
        (
            "無法取得 Windows 實體螢幕裝置路徑：",
            "Unable to get the Windows physical display device path: ",
        ),
        (
            "無法取得實體螢幕數量：",
            "Unable to get the physical display count: ",
        ),
        (
            "無法列舉實體螢幕控制介面：",
            "Unable to enumerate physical display control interfaces: ",
        ),
        (
            "無法列舉 Windows 邏輯螢幕：",
            "Unable to enumerate Windows logical displays: ",
        ),
    ];
    for (zh_tw, en) in PREFIXES {
        if let Some(rest) = detail.strip_prefix(zh_tw) {
            return format!("{en}{rest}");
        }
    }
    match detail {
        "無法讀取區域網路搜尋結果" => "Unable to read local network discovery results".to_owned(),
        "連線逾時" => "Connection timed out".to_owned(),
        "回應逾時" => "Response timed out".to_owned(),
        "廣播位址格式無效" => "Invalid broadcast address".to_owned(),
        "Windows 未提供螢幕裝置路徑；為避免誤控，已停止操作" =>
            "Windows did not provide a display device path; stopped to prevent controlling the wrong display".to_owned(),
        "顯示器 EDID 標頭、長度或 checksum 無效，無法安全識別裝置" =>
            "The display EDID header, length, or checksum is invalid, so the device cannot be identified safely".to_owned(),
        _ => detail.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_core_errors_in_both_supported_languages() {
        let error = DisplayMuxError::AmbiguousTarget { count: 2 };

        assert!(error.localized_message(true).contains("找到 2 台"));
        assert!(error.localized_message(false).contains("Found 2 displays"));

        let macos = DisplayMuxError::Backend(
            "macOS 無法透過目前的 HDMI／USB-C／Thunderbolt 路徑使用 DDC/CI：checksum mismatch"
                .to_owned(),
        );
        let message = macos.localized_message(false);
        assert!(message.contains("current HDMI/USB-C/Thunderbolt path"));
        assert!(!message.contains("無法"));
    }

    /// A rejected setting says what is wrong with it. Wrapping that in "unable
    /// to complete the display operation" points at hardware that is fine.
    #[test]
    fn a_rejected_setting_is_not_reported_as_a_display_failure() {
        let error = DisplayMuxError::Rejected {
            zh: "這是作業系統保留的快捷鍵".to_owned(),
            en: "The operating system keeps this shortcut for itself".to_owned(),
        };

        assert_eq!(error.localized_message(true), "這是作業系統保留的快捷鍵");
        assert_eq!(
            error.localized_message(false),
            "The operating system keeps this shortcut for itself"
        );
        assert!(!error.localized_message(true).contains("螢幕操作"));
        assert!(!error.localized_message(false).contains("display operation"));
    }
}
