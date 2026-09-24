//! The tray (Windows) and menu-bar status item (macOS): open and quit, plus a
//! quick switch — every shared display to one host, or one display at a time —
//! without opening any window.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use super::{
    apply_active_group, build_host_switcher_state, host_group, read_settings,
    report_failure_if_allowed, run_host_switch, show_main_window, ui_text, AppRuntime,
    HostSwitcherMonitor, UiLocale, ACTIVE_ROUTE_CHANGED_EVENT, HOST_GROUPS_CHANGED_EVENT,
    HOST_NAMES_CHANGED_EVENT, HOST_ORDER_CHANGED_EVENT, MONITOR_IDENTITIES_CHANGED_EVENT,
    PEER_INPUTS_CHANGED_EVENT, SWITCH_NOTICE_EVENT,
};
use tauri::{
    ipc::Channel,
    menu::{CheckMenuItemBuilder, Menu, MenuBuilder, MenuItemBuilder, SubmenuBuilder},
    AppHandle, Emitter, Listener, Manager, Wry,
};

pub(super) const TRAY_ID: &str = "muxsu";
/// The small window that reports on a switch made from the menu.
pub(super) const NOTICE_WINDOW: &str = "switch-notice";
/// How long a switch may take before the window says it is working on it. Most
/// switches are done well inside this, and a panel that flashes up and away is
/// worse than none.
const WORKING_PANEL_DELAY: Duration = Duration::from_millis(1200);
const OPEN_ID: &str = "tray-open";
const QUIT_ID: &str = "tray-quit";
/// Menu ids carry the display and host they switch, split by a character
/// neither a display key nor a host id contains.
const ID_SEPARATOR: char = '\n';
const SWITCH_ONE_PREFIX: &str = "tray-one";
const SWITCH_ALL_PREFIX: &str = "tray-all";
const SHOW_GROUP_PREFIX: &str = "tray-group";

/// Whatever changes a host's name, order, port or the host a display shows
/// changes the menu too.
const MENU_EVENTS: [&str; 6] = [
    HOST_GROUPS_CHANGED_EVENT,
    ACTIVE_ROUTE_CHANGED_EVENT,
    HOST_ORDER_CHANGED_EVENT,
    HOST_NAMES_CHANGED_EVENT,
    PEER_INPUTS_CHANGED_EVENT,
    MONITOR_IDENTITIES_CHANGED_EVENT,
];

/// What the menu showed when it was last built. The main window asks for a
/// rebuild on every refresh, and replacing a menu that is open closes it on
/// Windows, so a rebuild that would change nothing is skipped.
static LAST_MENU: Mutex<Option<String>> = Mutex::new(None);

/// What the window was last told. It is created hidden and asks for this once
/// it is up, so the message has to outlive the click that caused it.
static LAST_NOTICE: Mutex<Option<SwitchNotice>> = Mutex::new(None);

/// The switch the menu started and is still waiting on. Switches run one at a
/// time on purpose — the first wakes a sleeping host so later ones find it
/// ready — so a second click while one is in flight is dropped rather than
/// queued, and the menu says as much until it is done.
static IN_FLIGHT: Mutex<Option<InFlight>> = Mutex::new(None);

/// Tells one run from the next, so a run that has already finished cannot
/// clear the state of the one that replaced it.
static NEXT_RUN: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
struct InFlight {
    run: u64,
    host_name: String,
}

/// Claims the one switch slot; `None` when a switch is already on its way.
fn begin_switch(host_name: &str) -> Option<u64> {
    let mut slot = match IN_FLIGHT.lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    if slot.is_some() {
        return None;
    }
    let run = NEXT_RUN.fetch_add(1, Ordering::Relaxed);
    *slot = Some(InFlight {
        run,
        host_name: host_name.to_owned(),
    });
    Some(run)
}

fn end_switch(run: u64) {
    let mut slot = match IN_FLIGHT.lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    if slot.as_ref().is_some_and(|flight| flight.run == run) {
        *slot = None;
    }
}

/// The host a switch is on its way to, for the menu to name.
fn switching_host() -> Option<String> {
    match IN_FLIGHT.lock() {
        Ok(slot) => slot.as_ref().map(|flight| flight.host_name.clone()),
        Err(poisoned) => poisoned
            .into_inner()
            .as_ref()
            .map(|flight| flight.host_name.clone()),
    }
}

fn is_running(run: u64) -> bool {
    match IN_FLIGHT.lock() {
        Ok(slot) => slot.as_ref().is_some_and(|flight| flight.run == run),
        Err(poisoned) => poisoned
            .into_inner()
            .as_ref()
            .is_some_and(|flight| flight.run == run),
    }
}

fn working_title(host_name: &str) -> String {
    match UiLocale::current() {
        UiLocale::TraditionalChinese => format!("正在切換到 {host_name}"),
        UiLocale::English => format!("Switching to {host_name}"),
    }
}

enum TrayAction {
    Open,
    Quit,
    SwitchOne {
        monitor_key: String,
        host_id: String,
    },
    SwitchAll {
        host_id: String,
    },
    /// An empty id goes back to showing every display and host.
    ShowGroup {
        group_id: String,
    },
}

fn parse_action(id: &str) -> Option<TrayAction> {
    match id {
        OPEN_ID => return Some(TrayAction::Open),
        QUIT_ID => return Some(TrayAction::Quit),
        _ => {}
    }
    let mut parts = id.split(ID_SEPARATOR);
    match (parts.next()?, parts.next(), parts.next(), parts.next()) {
        (SWITCH_ONE_PREFIX, Some(monitor_key), Some(host_id), None) => {
            Some(TrayAction::SwitchOne {
                monitor_key: monitor_key.to_owned(),
                host_id: host_id.to_owned(),
            })
        }
        (SWITCH_ALL_PREFIX, Some(host_id), None, None) => Some(TrayAction::SwitchAll {
            host_id: host_id.to_owned(),
        }),
        (SHOW_GROUP_PREFIX, Some(group_id), None, None) => Some(TrayAction::ShowGroup {
            group_id: group_id.to_owned(),
        }),
        _ => None,
    }
}

fn switch_one_id(monitor_key: &str, host_id: &str) -> String {
    format!("{SWITCH_ONE_PREFIX}{ID_SEPARATOR}{monitor_key}{ID_SEPARATOR}{host_id}")
}

fn switch_all_id(host_id: &str) -> String {
    format!("{SWITCH_ALL_PREFIX}{ID_SEPARATOR}{host_id}")
}

fn show_group_id(group_id: &str) -> String {
    format!("{SHOW_GROUP_PREFIX}{ID_SEPARATOR}{group_id}")
}

/// This computer's groups and the one being shown, for the menu.
fn current_groups(app: &AppHandle) -> (Vec<host_group::HostGroup>, String) {
    let state = app.state::<AppRuntime>();
    match read_settings(&state) {
        Ok(settings) => {
            let active =
                host_group::active_group(&settings.host_groups, &settings.active_host_group)
                    .map(|group| group.id.clone())
                    .unwrap_or_default();
            (settings.host_groups, active)
        }
        Err(error) => {
            tracing::warn!(error = %error, "unable to read the groups for the tray menu");
            (Vec::new(), String::new())
        }
    }
}

/// Displays a switch to `host_id` would change: those that have a port for it
/// and are not already showing it.
fn switch_targets<'a>(
    monitors: &'a [HostSwitcherMonitor],
    host_id: &str,
) -> Vec<&'a HostSwitcherMonitor> {
    monitors
        .iter()
        .filter(|monitor| {
            monitor
                .hosts
                .iter()
                .any(|host| host.id == host_id && host.available && !host.is_active)
        })
        .collect()
}

fn current_monitors(app: &AppHandle) -> Vec<HostSwitcherMonitor> {
    let state = app.state::<AppRuntime>();
    match read_settings(&state) {
        Ok(settings) => build_host_switcher_state(&state, &settings).monitors,
        Err(error) => {
            tracing::warn!(error = %error, "unable to read settings for the tray menu");
            Vec::new()
        }
    }
}

/// Menu text as the platform draws it: Windows reads `&` as the marker of a
/// keyboard accelerator, so a host called "R&D" would lose its ampersand.
fn label(text: &str) -> String {
    if cfg!(target_os = "windows") {
        text.replace('&', "&&")
    } else {
        text.to_owned()
    }
}

/// Everything the menu shows, in the language it shows it in.
fn signature(
    monitors: &[HostSwitcherMonitor],
    switching: Option<&str>,
    groups: &(Vec<host_group::HostGroup>, String),
) -> String {
    format!(
        "{}\u{0}{switching:?}\u{0}{monitors:?}\u{0}{groups:?}",
        ui_text("zh-TW", "en")
    )
}

/// The groups as check items, with the one being shown checked. "All displays"
/// leads, so there is always a way back out of a group.
fn group_items(
    app: &AppHandle,
    groups: &[host_group::HostGroup],
    active_id: &str,
) -> tauri::Result<Vec<tauri::menu::CheckMenuItem<Wry>>> {
    std::iter::once(
        CheckMenuItemBuilder::new(label(ui_text("全部螢幕與主機", "All displays and hosts")))
            .id(show_group_id(""))
            .checked(active_id.is_empty())
            .enabled(!active_id.is_empty())
            .build(app),
    )
    .chain(groups.iter().map(|group| {
        CheckMenuItemBuilder::new(label(&group.name))
            .id(show_group_id(&group.id))
            .checked(group.id == active_id)
            .enabled(group.id != active_id)
            .build(app)
    }))
    .collect()
}

/// The hosts of one display as check items; the host on screen is checked.
/// Nothing is clickable while a switch is on its way.
fn host_items(
    app: &AppHandle,
    monitor: &HostSwitcherMonitor,
    switching: bool,
) -> tauri::Result<Vec<tauri::menu::CheckMenuItem<Wry>>> {
    monitor
        .hosts
        .iter()
        .map(|host| {
            CheckMenuItemBuilder::new(label(&host.name))
                .id(switch_one_id(&monitor.monitor_key, &host.id))
                .checked(host.is_active)
                .enabled(!switching && host.available && !host.is_active)
                .build(app)
        })
        .collect()
}

/// The menu for the tray as it is created; later changes go through
/// `refresh_menu`.
pub(super) fn build_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let monitors = current_monitors(app);
    let switching = switching_host();
    let groups = current_groups(app);
    let menu = menu_for(app, &monitors, switching.as_deref(), &groups)?;
    remember(Some(signature(&monitors, switching.as_deref(), &groups)));
    Ok(menu)
}

fn remember(shown: Option<String>) {
    match LAST_MENU.lock() {
        Ok(mut last) => *last = shown,
        Err(poisoned) => *poisoned.into_inner() = shown,
    }
}

fn is_showing(shown: &str) -> bool {
    match LAST_MENU.lock() {
        Ok(last) => last.as_deref() == Some(shown),
        Err(_) => false,
    }
}

/// The heading above the hosts: what a click would do, or what the click
/// already made is doing.
fn heading_text(monitors: &[HostSwitcherMonitor], switching: Option<&str>) -> String {
    if let Some(host_name) = switching {
        return format!("{}…", working_title(host_name));
    }
    if monitors.len() > 1 {
        ui_text("全部螢幕切到", "Switch every display to").to_owned()
    } else {
        ui_text("共用螢幕切到", "Switch the shared display to").to_owned()
    }
}

fn menu_for(
    app: &AppHandle,
    monitors: &[HostSwitcherMonitor],
    switching: Option<&str>,
    groups: &(Vec<host_group::HostGroup>, String),
) -> tauri::Result<Menu<Wry>> {
    let mut menu = MenuBuilder::new(app);
    let busy = switching.is_some();
    match monitors {
        [] => {}
        [monitor] => {
            let heading = MenuItemBuilder::new(label(&heading_text(monitors, switching)))
                .enabled(false)
                .build(app)?;
            menu = menu.item(&heading);
            for item in host_items(app, monitor, busy)? {
                menu = menu.item(&item);
            }
            menu = menu.separator();
        }
        [first, ..] => {
            let heading = MenuItemBuilder::new(label(&heading_text(monitors, switching)))
                .enabled(false)
                .build(app)?;
            menu = menu.item(&heading);
            // Every display lists the same hosts in the same saved order.
            for host in &first.hosts {
                let showing_everywhere = monitors.iter().all(|monitor| {
                    monitor
                        .hosts
                        .iter()
                        .any(|option| option.id == host.id && option.is_active)
                });
                let item = CheckMenuItemBuilder::new(label(&host.name))
                    .id(switch_all_id(&host.id))
                    .checked(showing_everywhere)
                    .enabled(!busy && !switch_targets(monitors, &host.id).is_empty())
                    .build(app)?;
                menu = menu.item(&item);
            }
            menu = menu.separator();
            for monitor in monitors {
                let mut submenu = SubmenuBuilder::new(app, label(&monitor.name));
                for item in host_items(app, monitor, busy)? {
                    submenu = submenu.item(&item);
                }
                menu = menu.item(&submenu.build()?);
            }
            menu = menu.separator();
        }
    }
    let (defined, active_id) = groups;
    if !defined.is_empty() {
        let mut submenu = SubmenuBuilder::new(app, label(ui_text("群組", "Groups")));
        for item in group_items(app, defined, active_id)? {
            submenu = submenu.item(&item);
        }
        menu = menu.item(&submenu.build()?).separator();
    }
    menu.text(OPEN_ID, ui_text("開啟 MuxSU", "Open MuxSU"))
        .separator()
        .text(QUIT_ID, ui_text("結束 MuxSU", "Quit MuxSU"))
        .build()
}

/// Rebuilds the menu from the saved settings when what it shows has changed.
/// Cheap: it reads no display.
pub(super) fn refresh_menu(app: &AppHandle) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let monitors = current_monitors(app);
    let switching = switching_host();
    let groups = current_groups(app);
    let shown = signature(&monitors, switching.as_deref(), &groups);
    if is_showing(&shown) {
        return;
    }
    match menu_for(app, &monitors, switching.as_deref(), &groups) {
        Ok(menu) => match tray.set_menu(Some(menu)) {
            Ok(()) => remember(Some(shown)),
            Err(error) => tracing::warn!(error = %error, "unable to update the tray menu"),
        },
        Err(error) => tracing::warn!(error = %error, "unable to build the tray menu"),
    }
}

/// One display's part of a switch the menu asked for, carrying the names a
/// failure notification would need: the menu is gone by the time it is shown.
struct SwitchJob {
    monitor_key: String,
    monitor_name: String,
    host_id: String,
    host_name: String,
}

/// A menu click names a display and a host by id. A menu built from settings
/// that have since changed can name one that is gone, and the switch still
/// runs and reports its own error, so an unknown id stands in for its name.
fn job_for(monitors: &[HostSwitcherMonitor], monitor_key: &str, host_id: &str) -> SwitchJob {
    let monitor = monitors
        .iter()
        .find(|monitor| monitor.monitor_key == monitor_key);
    let host = monitor.and_then(|monitor| monitor.hosts.iter().find(|host| host.id == host_id));
    SwitchJob {
        monitor_key: monitor_key.to_owned(),
        monitor_name: monitor.map_or_else(|| monitor_key.to_owned(), |it| it.name.clone()),
        host_id: host_id.to_owned(),
        host_name: host.map_or_else(|| host_id.to_owned(), |it| it.name.clone()),
    }
}

/// What the window shows: a switch still on its way, or one that did not
/// happen. Both are for switches made from the menu, which has no window of
/// its own to report in.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(super) enum SwitchNotice {
    Working {
        /// Names the host the click asked for.
        title: String,
    },
    Failed {
        title: String,
        /// One line per display that would not switch.
        reasons: Vec<String>,
    },
}

/// What a failed switch says. `name_displays` when the click covered more than
/// one display and each reason has to say which display it belongs to.
fn failure_notice(
    locale: UiLocale,
    host_name: &str,
    failures: &[(String, String)],
    name_displays: bool,
) -> SwitchNotice {
    SwitchNotice::Failed {
        title: match locale {
            UiLocale::TraditionalChinese => format!("無法切換到 {host_name}"),
            UiLocale::English => format!("Unable to switch to {host_name}"),
        },
        reasons: failures
            .iter()
            .map(|(monitor, message)| match (name_displays, locale) {
                (false, _) => message.clone(),
                (true, UiLocale::TraditionalChinese) => format!("{monitor}：{message}"),
                (true, UiLocale::English) => format!("{monitor}: {message}"),
            })
            .collect(),
    }
}

/// What the window asks for as it loads.
pub(super) fn last_switch_notice() -> Option<SwitchNotice> {
    match LAST_NOTICE.lock() {
        Ok(last) => last.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

fn remember_notice(notice: SwitchNotice) {
    match LAST_NOTICE.lock() {
        Ok(mut last) => *last = Some(notice),
        Err(poisoned) => *poisoned.into_inner() = Some(notice),
    }
}

/// A switch from the menu has no window to report in, so it gets one: a small
/// panel over whatever is on screen. It is created hidden at startup and shown
/// here, because building a window takes long enough to be seen.
fn show_notice(app: &AppHandle, notice: SwitchNotice) {
    remember_notice(notice.clone());
    let Some(window) = app.get_webview_window(NOTICE_WINDOW) else {
        tracing::warn!("the switch notice window is unavailable");
        return;
    };
    // A window still up from an earlier notice has already read its message.
    if let Err(error) = app.emit_to(NOTICE_WINDOW, SWITCH_NOTICE_EVENT, &notice) {
        tracing::warn!(error = %error, "unable to hand the switch notice window its message");
    }
    if let Err(error) = window.show() {
        tracing::warn!(error = %error, "unable to show the switch notice window");
        return;
    }
    // A panel that steals focus mid-switch would be in the way; only one that
    // asks to be dismissed takes it.
    if matches!(notice, SwitchNotice::Failed { .. }) {
        if let Err(error) = window.set_focus() {
            tracing::warn!(error = %error, "unable to focus the switch notice window");
        }
    }
}

fn hide_notice(app: &AppHandle) {
    let Some(window) = app.get_webview_window(NOTICE_WINDOW) else {
        return;
    };
    if let Err(error) = window.hide() {
        tracing::warn!(error = %error, "unable to hide the switch notice window");
    }
}

/// Puts the menu back in step with whatever the switch left behind. Clicking a
/// check item flips its tick on the spot, whatever the switch then does, so
/// this runs even when nothing changed.
fn rebuild_menu(app: &AppHandle) {
    remember(None);
    refresh_menu(app);
}

/// Runs switches the menu asked for, one display at a time: the first switch
/// wakes a sleeping host, so later ones find it ready. Nothing on screen waits
/// for these, so the menu and a small panel carry what happened.
fn run_switches(app: AppHandle, jobs: Vec<SwitchJob>) {
    // Every job of one click targets the same host, so it is named once.
    let Some(host_name) = jobs.first().map(|job| job.host_name.clone()) else {
        return;
    };
    let Some(run) = begin_switch(&host_name) else {
        // A second click while a switch is on its way. The click already
        // flipped a tick that the running switch has not earned, so the menu
        // is put back — and it names what it is waiting for.
        tracing::info!("ignored a menu switch while another was still running");
        rebuild_menu(&app);
        return;
    };
    // The menu now says what it is doing, and every host in it is unclickable.
    rebuild_menu(&app);
    show_working_panel_if_slow(app.clone(), run, host_name.clone());
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppRuntime>();
        let mut switched = false;
        let mut failures: Vec<(String, String)> = Vec::new();
        let name_displays = jobs.len() > 1;
        for job in jobs {
            // The panel names the host, not the stage, so nothing reads these.
            let progress = Channel::new(|_| Ok(()));
            match run_host_switch(job.monitor_key, job.host_id, progress, &state).await {
                Ok(_) => switched = true,
                Err(message) => {
                    tracing::warn!(error = %message, "a switch from the tray menu failed");
                    report_failure_if_allowed(&app, &message);
                    failures.push((job.monitor_name, message));
                }
            }
        }
        // Before the menu is rebuilt, so it is drawn idle rather than busy.
        end_switch(run);
        rebuild_menu(&app);
        if switched {
            // Every window shows which host is active.
            if let Err(error) = app.emit(ACTIVE_ROUTE_CHANGED_EVENT, ()) {
                tracing::warn!(error = %error, "unable to notify windows of a switch");
            }
        }
        // Last, so the menu already shows what really happened by the time the
        // panel sends anyone back to it.
        if failures.is_empty() {
            hide_notice(&app);
        } else {
            let notice = failure_notice(UiLocale::current(), &host_name, &failures, name_displays);
            show_notice(&app, notice);
        }
    });
}

/// Says the app is working on it, but only once the switch has taken longer
/// than a switch usually does.
fn show_working_panel_if_slow(app: AppHandle, run: u64, host_name: String) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(WORKING_PANEL_DELAY).await;
        if !is_running(run) {
            return;
        }
        show_notice(
            &app,
            SwitchNotice::Working {
                title: working_title(&host_name),
            },
        );
    });
}

pub(super) fn handle_menu_event(app: &AppHandle, id: &str) {
    match parse_action(id) {
        Some(TrayAction::Open) => show_main_window(app),
        Some(TrayAction::Quit) => app.exit(0),
        Some(TrayAction::SwitchOne {
            monitor_key,
            host_id,
        }) => {
            let monitors = current_monitors(app);
            run_switches(
                app.clone(),
                vec![job_for(&monitors, &monitor_key, &host_id)],
            );
        }
        Some(TrayAction::ShowGroup { group_id }) => {
            let state = app.state::<AppRuntime>();
            if let Err(error) = apply_active_group(&state, app, &group_id) {
                tracing::warn!(error = %error, "unable to show that group from the tray menu");
            }
        }
        Some(TrayAction::SwitchAll { host_id }) => {
            let monitors = current_monitors(app);
            let jobs = switch_targets(&monitors, &host_id)
                .into_iter()
                .map(|monitor| job_for(&monitors, &monitor.monitor_key, &host_id))
                .collect();
            run_switches(app.clone(), jobs);
        }
        None => {}
    }
}

/// Keeps the menu current with changes made here or by a paired host.
pub(super) fn follow_changes(app: &AppHandle) {
    for event in MENU_EVENTS {
        let handle = app.clone();
        app.listen_any(event, move |_| refresh_menu(&handle));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_round_trip() {
        match parse_action(&switch_one_id("DMO/3410/DEMO-001", "peer-1")) {
            Some(TrayAction::SwitchOne {
                monitor_key,
                host_id,
            }) => {
                assert_eq!(monitor_key, "DMO/3410/DEMO-001");
                assert_eq!(host_id, "peer-1");
            }
            _ => panic!("expected a single-display switch"),
        }
        match parse_action(&switch_all_id("local")) {
            Some(TrayAction::SwitchAll { host_id }) => assert_eq!(host_id, "local"),
            _ => panic!("expected an every-display switch"),
        }
        // Group ids come from `next_nonce`, which uses hyphens; the separator
        // here is a newline, so one cannot be read as the other.
        match parse_action(&show_group_id("4821-1790000000-7")) {
            Some(TrayAction::ShowGroup { group_id }) => assert_eq!(group_id, "4821-1790000000-7"),
            _ => panic!("expected a group to be shown"),
        }
    }

    #[test]
    fn the_all_displays_item_carries_an_empty_group_id() {
        match parse_action(&show_group_id("")) {
            Some(TrayAction::ShowGroup { group_id }) => assert!(group_id.is_empty()),
            _ => panic!("expected the every-display entry"),
        }
    }

    fn failed_parts(notice: &SwitchNotice) -> (&str, &[String]) {
        match notice {
            SwitchNotice::Failed { title, reasons } => (title, reasons),
            SwitchNotice::Working { .. } => panic!("expected a failure"),
        }
    }

    #[test]
    fn a_single_failed_display_is_shown_without_naming_it() {
        let failures = vec![("VG252Q".to_owned(), "螢幕沒有回應。".to_owned())];
        let notice = failure_notice(UiLocale::TraditionalChinese, "ITX-PC", &failures, false);
        let (title, reasons) = failed_parts(&notice);
        assert_eq!(title, "無法切換到 ITX-PC");
        assert_eq!(reasons, ["螢幕沒有回應。".to_owned()]);
    }

    #[test]
    fn failures_across_displays_name_the_display_that_failed() {
        let failures = vec![
            ("VG252Q".to_owned(), "No response.".to_owned()),
            ("MPG 274U E16M".to_owned(), "No HDMI 2 input.".to_owned()),
        ];
        let notice = failure_notice(UiLocale::English, "ITX-PC", &failures, true);
        let (title, reasons) = failed_parts(&notice);
        assert_eq!(title, "Unable to switch to ITX-PC");
        assert_eq!(
            reasons,
            [
                "VG252Q: No response.".to_owned(),
                "MPG 274U E16M: No HDMI 2 input.".to_owned(),
            ]
        );
    }

    #[test]
    fn the_window_can_still_read_the_message_after_the_click() {
        remember_notice(SwitchNotice::Working {
            title: "正在切換到 ITX-PC".to_owned(),
        });
        let stored = last_switch_notice().expect("a notice was remembered");
        assert!(matches!(stored, SwitchNotice::Working { .. }));
    }

    #[test]
    fn only_one_switch_runs_at_a_time() {
        let first = begin_switch("ITX-PC").expect("the slot was free");
        assert!(begin_switch("MacBook-Pro").is_none());
        assert_eq!(switching_host().as_deref(), Some("ITX-PC"));

        // A run that has already finished cannot clear the one after it.
        end_switch(first + 999);
        assert!(is_running(first));

        end_switch(first);
        assert!(switching_host().is_none());
        assert!(begin_switch("MacBook-Pro").is_some());
        end_switch(first + 1);
    }

    #[test]
    fn the_heading_names_the_host_a_switch_is_on_its_way_to() {
        // Which language it is in is the locale's business; that it names the
        // host rather than offering another one is this function's.
        let busy = heading_text(&[], Some("ITX-PC"));
        assert!(busy.contains("ITX-PC"), "{busy}");
        assert!(busy.ends_with('…'), "{busy}");
        assert_ne!(busy, heading_text(&[], None));
    }

    #[test]
    fn a_job_falls_back_to_ids_when_the_menu_named_what_is_gone() {
        let job = job_for(&[], "DMO/3410/DEMO-001", "peer-1");
        assert_eq!(job.monitor_key, "DMO/3410/DEMO-001");
        assert_eq!(job.monitor_name, "DMO/3410/DEMO-001");
        assert_eq!(job.host_id, "peer-1");
        assert_eq!(job.host_name, "peer-1");
    }

    #[test]
    fn unknown_or_malformed_ids_do_nothing() {
        assert!(parse_action("tray-something").is_none());
        assert!(
            parse_action(&format!("{SWITCH_ONE_PREFIX}{ID_SEPARATOR}only-a-display")).is_none()
        );
        assert!(parse_action(&format!(
            "{SWITCH_ALL_PREFIX}{ID_SEPARATOR}a{ID_SEPARATOR}b"
        ))
        .is_none());
    }
}
