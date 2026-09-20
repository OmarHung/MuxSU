use std::io::{self, Write};

use anyhow::{bail, Context, Result};
use muxsu_core::{DisplayInput, SwitchMode};

#[cfg(target_os = "windows")]
use muxsu_core::{
    windows::WindowsMonitorController, DisplayMuxProfile, DisplayMuxService, MonitorControl,
    MonitorFingerprint, SwitchOutcome,
};

fn main() -> Result<()> {
    run(std::env::args().skip(1))
}

fn run(mut args: impl Iterator<Item = String>) -> Result<()> {
    let command = args.next().unwrap_or_else(|| "help".to_owned());

    match command.as_str() {
        "list" => match args.next().as_deref() {
            None => list_monitors(),
            // Machine-readable form of the same scan, for tools that would
            // otherwise have to parse the human output. Everything one `list`
            // reports was enumerated together, which is what lets
            // `scripts/identity-oracle.mjs import` record that two identities
            // are two panels rather than two modes of one.
            Some("--json") => list_monitors_as_json(),
            Some(other) => bail!("不支援的參數：{other}"),
        },
        "switch" => {
            let manufacturer = args.next().context("缺少 manufacturer ID")?;
            let product = args.next().context("缺少 product code")?;
            let serial = args
                .next()
                .context("缺少 serial number；沒有序號時請輸入 -")?;
            let requested =
                DisplayInput::parse_code(&args.next().context("缺少輸入值，例如 0x0F")?)?;
            let mode = match args.next().as_deref() {
                None => SwitchMode::Apply,
                Some("--dry-run") => SwitchMode::DryRun,
                Some(other) => bail!("不支援的參數：{other}"),
            };
            switch_to(manufacturer, product, serial, requested, mode)
        }
        "help" | "--help" | "-h" => print_help(),
        other => bail!("不支援的命令：{other}"),
    }
}

#[cfg(target_os = "windows")]
fn controller() -> Result<WindowsMonitorController> {
    WindowsMonitorController::new().context("無法初始化 Windows 螢幕控制器")
}

#[cfg(target_os = "windows")]
fn list_monitors() -> Result<()> {
    let monitors = controller()?.enumerate()?;
    let stdout = io::stdout();
    let mut output = stdout.lock();

    for monitor in monitors {
        writeln!(
            output,
            "{} | {} / {} / {} | {}",
            monitor.name,
            monitor.fingerprint.manufacturer_id,
            monitor.fingerprint.product_code,
            monitor
                .fingerprint
                .serial_number
                .as_deref()
                .unwrap_or("serial unavailable"),
            monitor.id.as_str()
        )?;
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn list_monitors() -> Result<()> {
    bail!(muxsu_core::DisplayMuxError::UnsupportedPlatform)
}

#[cfg(target_os = "windows")]
fn list_monitors_as_json() -> Result<()> {
    let monitors = controller()?.enumerate()?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(output, "{}", serde_json::to_string_pretty(&monitors)?)?;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn list_monitors_as_json() -> Result<()> {
    bail!(muxsu_core::DisplayMuxError::UnsupportedPlatform)
}

#[cfg(target_os = "windows")]
fn switch_to(
    manufacturer: String,
    product: String,
    serial: String,
    requested: DisplayInput,
    mode: SwitchMode,
) -> Result<()> {
    let serial = (serial != "-").then_some(serial);
    let profile = DisplayMuxProfile {
        shared_monitor: MonitorFingerprint::new(manufacturer, product, serial),
    };
    let service = DisplayMuxService::new(controller()?, profile);
    let outcome = service.switch_to_input(requested, mode)?;
    let stdout = io::stdout();
    let mut output = stdout.lock();

    match outcome {
        SwitchOutcome::DryRun {
            target,
            current,
            requested,
        } => writeln!(
            output,
            "dry-run：{} 將由 0x{:02X} 切換至 0x{:02X}；未寫入",
            target.name,
            current.value(),
            requested.value()
        )?,
        SwitchOutcome::AlreadySelected { target, input } => writeln!(
            output,
            "{} 已使用輸入 0x{:02X}；未重複寫入",
            target.name,
            input.value()
        )?,
        SwitchOutcome::Switched {
            target,
            previous,
            selected,
        } => writeln!(
            output,
            "{} 已由 0x{:02X} 切換至 0x{:02X}",
            target.name,
            previous.value(),
            selected.value()
        )?,
    }

    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn switch_to(
    _manufacturer: String,
    _product: String,
    _serial: String,
    _requested: muxsu_core::DisplayInput,
    _mode: SwitchMode,
) -> Result<()> {
    bail!(muxsu_core::DisplayMuxError::UnsupportedPlatform)
}

fn print_help() -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(output, "MuxSU CLI")?;
    writeln!(output, "  muxsu-cli list [--json]")?;
    writeln!(
        output,
        "  muxsu-cli switch <manufacturer> <product> <serial|-> <input> [--dry-run]"
    )?;
    Ok(())
}
