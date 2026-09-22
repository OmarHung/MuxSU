/// Every command in `generate_handler!`. Listing them makes each one need an
/// `allow-*` permission in a window's capability, so a window can call only
/// the commands it is granted rather than all of them.
const COMMANDS: &[&str] = &[
    "set_locale",
    "discover_peers",
    "select_peer",
    "remove_peer",
    "add_shared_monitor",
    "remove_shared_monitor",
    "get_settings",
    "get_host_switcher_state",
    "get_host_order",
    "set_host_order",
    "get_host_names",
    "set_host_name",
    "get_host_appearances",
    "set_host_appearance",
    "set_input_label",
    "set_monitor_identity_link",
    "set_local_input",
    "reset_settings",
    "exchange_host_layout",
    "hide_host_switcher",
    "check_host_switcher_shortcut",
    "complete_onboarding",
    "get_input_options",
    "save_settings",
    "check_for_update",
    "install_update",
    "get_dashboard_state",
    "probe_peer",
    "get_host_presence",
    "refresh_host_presence",
    "wake_peer",
    "switch_host",
    "diagnostics_status",
    "set_diagnostics_consent",
    "prepare_diagnostic_report",
    "send_diagnostic_report",
    "save_diagnostic_report",
    "refresh_tray",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("failed to run tauri-build");
}
