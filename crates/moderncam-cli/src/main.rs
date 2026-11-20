//! Command-line tool for inspecting and controlling Microsoft Modern Webcam.

use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use moderncam_device::{
    HotplugNotice, ModernCamController, query_usb_topology, spawn_hotplug_watcher,
};
use moderncam_protocol::sync::{MicActivationStatus, interface_class_name};
use moderncam_protocol::uvc::{ControlRange, PowerLineFreq, UvcControlUpdate};

#[derive(Parser)]
#[command(
    name = "moderncam-cli",
    about = "Cross-platform CLI for Microsoft Modern Webcam (HID + UVC + USB topology)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print full device info, HID flags, USB interfaces, Mic status, and UVC controls.
    Dump,
    /// List active USB interfaces and report whether the Mic Audio interface is active.
    Interfaces,
    /// Query or modify proprietary HID settings.
    Hid {
        #[command(subcommand)]
        action: HidAction,
    },
    /// Query, modify, or reset standard UVC image controls.
    Uvc {
        #[command(subcommand)]
        action: UvcAction,
    },
    /// Send HID 0xAE (reset user settings) and verify Mic vs USB Audio interface consistency.
    Reset {
        /// Wait for physical unplug/replug if Mic state differs from active USB Audio interfaces.
        #[arg(long)]
        wait_replug: bool,
    },
}

#[derive(Subcommand)]
enum HidAction {
    Get { field: HidGetField },
    Set { field: HidBoolField, value: BoolArg },
    Toggle { field: HidBoolField },
}

#[derive(Clone, Copy, ValueEnum)]
enum HidGetField {
    Fw,
    Serial,
    Color,
    Hdr,
    Retouch,
    Mic,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum HidBoolField {
    Hdr,
    Retouch,
    Mic,
}

#[derive(Clone, Copy, ValueEnum)]
enum BoolArg {
    On,
    Off,
    #[value(name = "1")]
    One,
    #[value(name = "0")]
    Zero,
}

impl BoolArg {
    fn as_bool(self) -> bool {
        matches!(self, Self::On | Self::One)
    }
}

#[derive(Subcommand)]
enum UvcAction {
    /// Show all UVC control values and hardware ranges.
    List,
    /// Set one UVC control value.
    Set {
        control: UvcControlArg,
        value: String,
    },
    /// Reset all UVC controls to hardware defaults (instant, no replug).
    Reset,
}

#[derive(Clone, Copy, ValueEnum)]
enum UvcControlArg {
    Brightness,
    Contrast,
    Saturation,
    Sharpness,
    WbAuto,
    WbTemp,
    AeAuto,
    Exposure,
    Flicker,
    Zoom,
}

fn main() -> ExitCode {
    env_logger::init();
    let cli = Cli::parse();

    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Interfaces => {
            let topo = query_usb_topology()
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "Microsoft Modern Webcam not found on USB bus".to_string())?;
            print_interfaces(&topo);
            Ok(())
        }
        Command::Dump => {
            let mut ctrl = ModernCamController::new();
            let snap = ctrl.refresh_all().map_err(|e| e.to_string())?;
            print_snapshot(snap);
            Ok(())
        }
        Command::Hid { action } => run_hid(action),
        Command::Uvc { action } => run_uvc(action),
        Command::Reset { wait_replug } => run_reset(wait_replug),
    }
}

fn run_hid(action: HidAction) -> Result<(), String> {
    let mut ctrl = ModernCamController::new();
    let snap = ctrl.refresh_all().map_err(|e| e.to_string())?.clone();
    let hid = snap
        .hid
        .as_ref()
        .ok_or_else(|| "HID interface unavailable".to_string())?;

    match action {
        HidAction::Get { field } => {
            match field {
                HidGetField::Fw => println!(
                    "fw: {}",
                    hid.firmware
                        .map(|f| f.to_string())
                        .unwrap_or_else(|| "unavailable".into())
                ),
                HidGetField::Serial => {
                    println!("serial: {}", hid.serial.as_deref().unwrap_or("unavailable"))
                }
                HidGetField::Color => println!(
                    "color: {}",
                    hid.color_name.as_deref().unwrap_or("unavailable")
                ),
                HidGetField::Hdr => println!("hdr: {}", fmt_opt_bool(hid.hdr)),
                HidGetField::Retouch => println!("retouch: {}", fmt_opt_bool(hid.retouch)),
                HidGetField::Mic => {
                    println!(
                        "mic: {} ({})",
                        fmt_opt_bool(hid.mic),
                        snap.mic_status.label_zh()
                    )
                }
            }
            Ok(())
        }
        HidAction::Set { field, value } => apply_hid_bool(&mut ctrl, field, value.as_bool()),
        HidAction::Toggle { field } => {
            let current = match field {
                HidBoolField::Hdr => hid.hdr,
                HidBoolField::Retouch => hid.retouch,
                HidBoolField::Mic => hid.mic,
            }
            .ok_or_else(|| "could not read current HID flag".to_string())?;
            apply_hid_bool(&mut ctrl, field, !current)
        }
    }
}

fn apply_hid_bool(
    ctrl: &mut ModernCamController,
    field: HidBoolField,
    target: bool,
) -> Result<(), String> {
    let snap = match field {
        HidBoolField::Hdr => ctrl.set_hdr(target),
        HidBoolField::Retouch => ctrl.set_retouch(target),
        HidBoolField::Mic => ctrl.set_mic(target),
    }
    .map_err(|e| e.to_string())?;

    let name = match field {
        HidBoolField::Hdr => "hdr",
        HidBoolField::Retouch => "retouch",
        HidBoolField::Mic => "mic",
    };
    println!("{name} -> {} (verified)", if target { "on" } else { "off" });
    if field == HidBoolField::Mic {
        println!("mic status: {}", snap.mic_status.label_zh());
        if snap.mic_status.needs_replug() {
            println!(
                "NOTE: Unplug and replug the webcam USB cable for Audio interfaces to update."
            );
        }
    }
    Ok(())
}

fn run_uvc(action: UvcAction) -> Result<(), String> {
    let mut ctrl = ModernCamController::new();
    let snap = ctrl.refresh_all().map_err(|e| e.to_string())?.clone();

    match action {
        UvcAction::List => {
            print_uvc(snap.uvc_caps.as_ref(), snap.uvc_state.as_ref());
            Ok(())
        }
        UvcAction::Reset => {
            let updated = ctrl.reset_uvc_defaults().map_err(|e| e.to_string())?;
            println!("UVC controls reset to hardware defaults.");
            print_uvc(updated.uvc_caps.as_ref(), updated.uvc_state.as_ref());
            Ok(())
        }
        UvcAction::Set { control, value } => {
            let update = parse_uvc_update(control, &value)?;
            let updated = ctrl.apply_uvc(update).map_err(|e| e.to_string())?;
            println!("Updated {control_name:?}.", control_name = update.key());
            print_uvc(updated.uvc_caps.as_ref(), updated.uvc_state.as_ref());
            Ok(())
        }
    }
}

fn run_reset(wait_replug: bool) -> Result<(), String> {
    let mut ctrl = ModernCamController::new();
    let before = ctrl.refresh_all().map_err(|e| e.to_string())?.clone();
    let prev_mic = before.hid.as_ref().and_then(|h| h.mic);

    let after = ctrl
        .reset_hid_settings()
        .map_err(|e| e.to_string())?
        .clone();
    println!("HID 0xAE reset completed.");
    if let Some(hid) = &after.hid {
        println!(
            "  HDR={}  Retouch={}  Mic={} (was {})",
            fmt_opt_bool(hid.hdr),
            fmt_opt_bool(hid.retouch),
            fmt_opt_bool(hid.mic),
            fmt_opt_bool(prev_mic),
        );
    }
    println!("  Mic USB status: {}", after.mic_status.label_zh());

    if after.mic_status.needs_replug() {
        println!(
            "WARNING: HID mic setting differs from active USB Audio interfaces; unplug & replug USB required."
        );
        if wait_replug {
            wait_for_replug_sync(&mut ctrl)?;
        }
    }
    Ok(())
}

fn wait_for_replug_sync(ctrl: &mut ModernCamController) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    let _watcher = spawn_hotplug_watcher(move |notice| {
        let _ = tx.send(notice);
    })
    .map_err(|e| e.to_string())?;

    println!("Waiting up to 60s for webcam unplug and replug...");
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut saw_disconnect = false;

    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(HotplugNotice::Disconnected) => {
                saw_disconnect = true;
                ctrl.mark_disconnected();
                println!("  Device unplugged; please plug it back in...");
            }
            Ok(HotplugNotice::Connected(_)) => {
                if !saw_disconnect {
                    continue;
                }
                std::thread::sleep(Duration::from_millis(400));
                if let Ok(snap) = ctrl.refresh_all() {
                    println!(
                        "  Device reconnected! Mic status: {}",
                        snap.mic_status.label_zh()
                    );
                    if matches!(
                        snap.mic_status,
                        MicActivationStatus::ActiveSynced | MicActivationStatus::InactiveSynced
                    ) {
                        return Ok(());
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    Err("timed out waiting for webcam replug".into())
}

fn parse_uvc_update(control: UvcControlArg, raw: &str) -> Result<UvcControlUpdate, String> {
    let parse_bool = |s: &str| match s.to_ascii_lowercase().as_str() {
        "on" | "1" | "true" => Ok(true),
        "off" | "0" | "false" => Ok(false),
        _ => Err(format!("invalid boolean '{s}' (expected on/off/1/0)")),
    };
    let parse_i32 = |s: &str| {
        s.parse::<i32>()
            .map_err(|e| format!("invalid integer '{s}': {e}"))
    };

    match control {
        UvcControlArg::Brightness => Ok(UvcControlUpdate::Brightness(parse_i32(raw)?)),
        UvcControlArg::Contrast => Ok(UvcControlUpdate::Contrast(parse_i32(raw)?)),
        UvcControlArg::Saturation => Ok(UvcControlUpdate::Saturation(parse_i32(raw)?)),
        UvcControlArg::Sharpness => Ok(UvcControlUpdate::Sharpness(parse_i32(raw)?)),
        UvcControlArg::WbAuto => Ok(UvcControlUpdate::WhiteBalanceAuto(parse_bool(raw)?)),
        UvcControlArg::WbTemp => Ok(UvcControlUpdate::WhiteBalanceTemp(parse_i32(raw)?)),
        UvcControlArg::AeAuto => Ok(UvcControlUpdate::AutoExposure(parse_bool(raw)?)),
        UvcControlArg::Exposure => Ok(UvcControlUpdate::ExposureTime(parse_i32(raw)?)),
        UvcControlArg::Flicker => {
            let freq = match raw.to_ascii_lowercase().as_str() {
                "off" | "disabled" | "0" => PowerLineFreq::Disabled,
                "50" | "50hz" | "1" => PowerLineFreq::Hz50,
                "60" | "60hz" | "2" => PowerLineFreq::Hz60,
                "auto" | "3" => PowerLineFreq::Auto,
                _ => {
                    return Err(format!(
                        "invalid flicker mode '{raw}' (expected 50/60/auto/off)"
                    ));
                }
            };
            Ok(UvcControlUpdate::PowerLineFreq(freq))
        }
        UvcControlArg::Zoom => Ok(UvcControlUpdate::Zoom(parse_i32(raw)?)),
    }
}

fn print_snapshot(snap: &moderncam_device::DeviceSnapshot) {
    if let Some(topo) = &snap.topology {
        println!(
            "Device: {} (VID=0x{:04X} PID=0x{:04X} S/N={})",
            topo.product_name
                .as_deref()
                .unwrap_or("Microsoft Modern Webcam"),
            topo.vendor_id,
            topo.product_id,
            topo.serial_number.as_deref().unwrap_or("?"),
        );
    }

    if let Some(hid) = &snap.hid {
        println!("\n[HID Settings]");
        println!(
            "  Firmware : {}",
            hid.firmware
                .map(|f| f.to_string())
                .unwrap_or_else(|| "?".into())
        );
        println!("  Serial   : {}", hid.serial.as_deref().unwrap_or("?"));
        println!("  Color    : {}", hid.color_name.as_deref().unwrap_or("?"));
        println!("  HDR      : {}", fmt_opt_bool(hid.hdr));
        println!("  Retouch  : {}", fmt_opt_bool(hid.retouch));
        println!(
            "  Mic      : {} ({})",
            fmt_opt_bool(hid.mic),
            snap.mic_status.label_zh()
        );
    }

    if let Some(topo) = &snap.topology {
        println!();
        print_interfaces(topo);
    }

    println!();
    print_uvc(snap.uvc_caps.as_ref(), snap.uvc_state.as_ref());

    if let Some(err) = &snap.last_error {
        println!("\n[Warnings]\n  {err}");
    }
}

fn print_interfaces(topo: &moderncam_device::UsbDeviceTopology) {
    println!("[USB Interfaces]");
    for iface in &topo.interfaces {
        println!(
            "  #{:<2} class={:3} ({})",
            iface.number,
            iface.class,
            interface_class_name(iface.class)
        );
    }
    println!(
        "  Audio Interface: {}",
        if topo.has_audio_interface() {
            "active"
        } else {
            "absent"
        }
    );
}

fn print_uvc(
    caps: Option<&moderncam_protocol::uvc::UvcCapabilities>,
    state: Option<&moderncam_protocol::uvc::UvcState>,
) {
    println!("[UVC Controls]");
    let Some(st) = state else {
        println!("  unavailable");
        return;
    };
    let fmt_row = |name: &str, val: Option<i32>, range: Option<ControlRange>| {
        let v = val.map(|x| x.to_string()).unwrap_or_else(|| "-".into());
        let r = range
            .map(|r| {
                format!(
                    "range={}..={} step={} def={}",
                    r.min, r.max, r.step, r.default
                )
            })
            .unwrap_or_default();
        println!("  {name:<16}: {v:<8} {r}");
    };
    fmt_row("Brightness", st.brightness, caps.and_then(|c| c.brightness));
    fmt_row("Contrast", st.contrast, caps.and_then(|c| c.contrast));
    fmt_row("Saturation", st.saturation, caps.and_then(|c| c.saturation));
    fmt_row("Sharpness", st.sharpness, caps.and_then(|c| c.sharpness));
    println!(
        "  {:<16}: {}",
        "WB Auto",
        fmt_opt_bool(st.white_balance_auto)
    );
    fmt_row(
        "WB Temp (K)",
        st.white_balance_temp,
        caps.and_then(|c| c.white_balance_temp),
    );
    println!("  {:<16}: {}", "AE Auto", fmt_opt_bool(st.auto_exposure));
    fmt_row(
        "Exposure Time",
        st.exposure_time,
        caps.and_then(|c| c.exposure_time),
    );
    println!(
        "  {:<16}: {}",
        "Anti-Flicker",
        st.power_line_freq
            .map(|f| f.label_zh().to_string())
            .unwrap_or_else(|| "-".into())
    );
    fmt_row("Zoom", st.zoom, caps.and_then(|c| c.zoom));
    println!(
        "  {:<16}: {}",
        "Privacy Shutter",
        match st.privacy_shutter {
            Some(true) => "CLOSED",
            Some(false) => "OPEN",
            None => "N/A",
        }
    );
}

fn fmt_opt_bool(val: Option<bool>) -> &'static str {
    match val {
        Some(true) => "on",
        Some(false) => "off",
        None => "?",
    }
}
