//! Background hardware worker coordinating USB hotplug, debounced UVC writes,
//! HID queries, reset reconciliation, and physical privacy shutter polling.

use std::collections::HashMap;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};
use elegance::BadgeTone;
use moderncam_device::{DeviceSnapshot, HotplugNotice, ModernCamController, spawn_hotplug_watcher};
use moderncam_protocol::sync::{ReplugPhase, ReplugReason, ReplugStateMachine};
use moderncam_protocol::uvc::{UvcControlKey, UvcControlUpdate};

use crate::preview::PreviewHandle;

const SLIDER_DEBOUNCE: Duration = Duration::from_millis(35);
const PRIVACY_POLL_INTERVAL: Duration = Duration::from_millis(900);

/// Commands sent from the UI thread to the background hardware worker.
pub enum WorkerCmd {
    RefreshAll,
    SetHdr(bool),
    SetRetouch(bool),
    SetMic(bool),
    SetUvc(UvcControlUpdate),
    ResetUvcDefaults,
    ResetHidSettings,
    Hotplug(HotplugNotice),
    Shutdown,
}

/// Non-blocking toast notification emitted by the worker for the UI to render.
#[derive(Debug, Clone)]
pub struct GuiToast {
    pub title: String,
    pub description: String,
    pub tone: BadgeTone,
}

/// Events sent from the background hardware worker to the UI thread.
#[derive(Debug, Clone)]
pub enum WorkerEvent {
    Snapshot {
        snapshot: Box<DeviceSnapshot>,
        replug_phase: ReplugPhase,
        force_sync_controls: bool,
    },
    Toast(GuiToast),
}

#[derive(Clone)]
pub struct WorkerHandle {
    cmd_tx: Sender<WorkerCmd>,
    event_rx: Receiver<WorkerEvent>,
}

impl WorkerHandle {
    pub fn refresh_all(&self) {
        let _ = self.cmd_tx.send(WorkerCmd::RefreshAll);
    }

    pub fn set_hdr(&self, enabled: bool) {
        let _ = self.cmd_tx.send(WorkerCmd::SetHdr(enabled));
    }

    pub fn set_retouch(&self, enabled: bool) {
        let _ = self.cmd_tx.send(WorkerCmd::SetRetouch(enabled));
    }

    pub fn set_mic(&self, enabled: bool) {
        let _ = self.cmd_tx.send(WorkerCmd::SetMic(enabled));
    }

    pub fn set_uvc(&self, update: UvcControlUpdate) {
        let _ = self.cmd_tx.send(WorkerCmd::SetUvc(update));
    }

    pub fn reset_uvc_defaults(&self) {
        let _ = self.cmd_tx.send(WorkerCmd::ResetUvcDefaults);
    }

    pub fn reset_hid_settings(&self) {
        let _ = self.cmd_tx.send(WorkerCmd::ResetHidSettings);
    }

    pub fn try_recv_event(&self) -> Option<WorkerEvent> {
        self.event_rx.try_recv().ok()
    }

    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(WorkerCmd::Shutdown);
    }
}

pub struct HardwareWorker {
    handle: WorkerHandle,
    thread: Option<JoinHandle<()>>,
}

impl HardwareWorker {
    pub fn spawn(egui_ctx: egui::Context, preview: PreviewHandle) -> Self {
        let (cmd_tx, cmd_rx) = unbounded();
        let (event_tx, event_rx) = unbounded();

        let hotplug_tx = cmd_tx.clone();
        let _hotplug_watcher = spawn_hotplug_watcher(move |notice| {
            let _ = hotplug_tx.send(WorkerCmd::Hotplug(notice));
        })
        .ok();

        let thread = thread::Builder::new()
            .name("moderncam-hw-worker".into())
            .spawn(move || {
                let _keep_watcher = _hotplug_watcher;
                run_worker_loop(cmd_rx, event_tx, egui_ctx, preview);
            })
            .ok();

        let handle = WorkerHandle { cmd_tx, event_rx };
        handle.refresh_all();

        Self { handle, thread }
    }

    pub fn handle(&self) -> WorkerHandle {
        self.handle.clone()
    }
}

impl Drop for HardwareWorker {
    fn drop(&mut self) {
        self.handle.shutdown();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run_worker_loop(
    cmd_rx: Receiver<WorkerCmd>,
    event_tx: Sender<WorkerEvent>,
    egui_ctx: egui::Context,
    preview: PreviewHandle,
) {
    let mut ctrl = ModernCamController::new();
    let mut replug_sm = ReplugStateMachine::default();
    let mut pending_uvc: HashMap<UvcControlKey, UvcControlUpdate> = HashMap::new();
    let mut debounce_deadline: Option<Instant> = None;
    let mut last_privacy_poll = Instant::now();

    loop {
        let timeout = match debounce_deadline {
            Some(dl) => dl.saturating_duration_since(Instant::now()),
            None => Duration::from_millis(250),
        };

        match cmd_rx.recv_timeout(timeout) {
            Ok(cmd) => {
                if !handle_command(
                    cmd,
                    &mut ctrl,
                    &mut replug_sm,
                    &mut pending_uvc,
                    &mut debounce_deadline,
                    &event_tx,
                    &egui_ctx,
                    &preview,
                ) {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        // Flush coalesced slider writes once debounce deadline arrives.
        if let Some(dl) = debounce_deadline
            && Instant::now() >= dl
            && !pending_uvc.is_empty()
        {
            debounce_deadline = None;
            let updates: Vec<UvcControlUpdate> = pending_uvc.drain().map(|(_, v)| v).collect();
            let mut failed_msg = None;
            for upd in updates {
                if let Err(err) = ctrl.apply_uvc(upd) {
                    failed_msg = Some(err.to_string());
                }
            }
            if let Some(msg) = failed_msg {
                emit_toast(&event_tx, &egui_ctx, "画质调节失败", msg, BadgeTone::Danger);
            }
            emit_snapshot(
                &ctrl,
                &mut replug_sm,
                ReplugReason::PreExistingMismatch,
                false,
                &event_tx,
                &egui_ctx,
            );
        }

        // Periodic low-overhead poll for the physical privacy shutter.
        if ctrl.snapshot().connected && last_privacy_poll.elapsed() >= PRIVACY_POLL_INTERVAL {
            last_privacy_poll = Instant::now();
            let before = ctrl
                .snapshot()
                .uvc_state
                .as_ref()
                .and_then(|s| s.privacy_shutter);
            let after = ctrl.poll_privacy_shutter();
            if before != after || !ctrl.snapshot().connected {
                emit_snapshot(
                    &ctrl,
                    &mut replug_sm,
                    ReplugReason::PreExistingMismatch,
                    false,
                    &event_tx,
                    &egui_ctx,
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_command(
    cmd: WorkerCmd,
    ctrl: &mut ModernCamController,
    replug_sm: &mut ReplugStateMachine,
    pending_uvc: &mut HashMap<UvcControlKey, UvcControlUpdate>,
    debounce_deadline: &mut Option<Instant>,
    event_tx: &Sender<WorkerEvent>,
    egui_ctx: &egui::Context,
    preview: &PreviewHandle,
) -> bool {
    match cmd {
        WorkerCmd::Shutdown => return false,
        WorkerCmd::SetUvc(upd) => {
            pending_uvc.insert(upd.key(), upd);
            if debounce_deadline.is_none() {
                *debounce_deadline = Some(Instant::now() + SLIDER_DEBOUNCE);
            }
        }
        WorkerCmd::RefreshAll => {
            let _ = ctrl.refresh_all();
            emit_snapshot(
                ctrl,
                replug_sm,
                ReplugReason::PreExistingMismatch,
                true,
                event_tx,
                egui_ctx,
            );
        }
        WorkerCmd::SetHdr(on) => match ctrl.set_hdr(on) {
            Ok(_) => {
                emit_snapshot(
                    ctrl,
                    replug_sm,
                    ReplugReason::PreExistingMismatch,
                    true,
                    event_tx,
                    egui_ctx,
                );
                emit_toast(
                    event_tx,
                    egui_ctx,
                    "HDR 已更新",
                    if on { "已开启 HDR" } else { "已关闭 HDR" },
                    BadgeTone::Ok,
                );
            }
            Err(err) => {
                emit_toast(
                    event_tx,
                    egui_ctx,
                    "HDR 设置失败",
                    err.to_string(),
                    BadgeTone::Danger,
                );
                emit_snapshot(
                    ctrl,
                    replug_sm,
                    ReplugReason::PreExistingMismatch,
                    true,
                    event_tx,
                    egui_ctx,
                );
            }
        },
        WorkerCmd::SetRetouch(on) => match ctrl.set_retouch(on) {
            Ok(_) => {
                emit_snapshot(
                    ctrl,
                    replug_sm,
                    ReplugReason::PreExistingMismatch,
                    true,
                    event_tx,
                    egui_ctx,
                );
                emit_toast(
                    event_tx,
                    egui_ctx,
                    "美颜 (Retouch) 已更新",
                    if on {
                        "已开启美颜"
                    } else {
                        "已关闭美颜"
                    },
                    BadgeTone::Ok,
                );
            }
            Err(err) => {
                emit_toast(
                    event_tx,
                    egui_ctx,
                    "美颜设置失败",
                    err.to_string(),
                    BadgeTone::Danger,
                );
                emit_snapshot(
                    ctrl,
                    replug_sm,
                    ReplugReason::PreExistingMismatch,
                    true,
                    event_tx,
                    egui_ctx,
                );
            }
        },
        WorkerCmd::SetMic(on) => match ctrl.set_mic(on) {
            Ok(_) => {
                emit_snapshot(
                    ctrl,
                    replug_sm,
                    ReplugReason::MicToggled,
                    true,
                    event_tx,
                    egui_ctx,
                );
                let tone = if ctrl.snapshot().mic_status.needs_replug() {
                    BadgeTone::Warning
                } else {
                    BadgeTone::Ok
                };
                emit_toast(
                    event_tx,
                    egui_ctx,
                    "麦克风配置已写入",
                    ctrl.snapshot().mic_status.label_zh(),
                    tone,
                );
            }
            Err(err) => {
                emit_toast(
                    event_tx,
                    egui_ctx,
                    "麦克风设置失败",
                    err.to_string(),
                    BadgeTone::Danger,
                );
                emit_snapshot(
                    ctrl,
                    replug_sm,
                    ReplugReason::PreExistingMismatch,
                    true,
                    event_tx,
                    egui_ctx,
                );
            }
        },
        WorkerCmd::ResetUvcDefaults => {
            pending_uvc.clear();
            *debounce_deadline = None;
            match ctrl.reset_uvc_defaults() {
                Ok(_) => {
                    emit_snapshot(
                        ctrl,
                        replug_sm,
                        ReplugReason::PreExistingMismatch,
                        true,
                        event_tx,
                        egui_ctx,
                    );
                    emit_toast(
                        event_tx,
                        egui_ctx,
                        "画质参数已恢复默认",
                        "所有 UVC 图像与曝光设置已恢复出厂默认",
                        BadgeTone::Ok,
                    );
                }
                Err(err) => emit_toast(
                    event_tx,
                    egui_ctx,
                    "UVC 重置失败",
                    err.to_string(),
                    BadgeTone::Danger,
                ),
            }
        }
        WorkerCmd::ResetHidSettings => match ctrl.reset_hid_settings() {
            Ok(_) => {
                emit_snapshot(
                    ctrl,
                    replug_sm,
                    ReplugReason::ResetChangedMic,
                    true,
                    event_tx,
                    egui_ctx,
                );
                if ctrl.snapshot().mic_status.needs_replug() {
                    emit_toast(
                        event_tx,
                        egui_ctx,
                        "固件设置已重置 — 需插拔生效",
                        "麦克风配置与当前 USB 描述符不一致，请重新插拔摄像头",
                        BadgeTone::Warning,
                    );
                } else {
                    emit_toast(
                        event_tx,
                        egui_ctx,
                        "设备固件设置已重置",
                        "HID 0xAE 恢复默认完成，USB 描述符状态一致",
                        BadgeTone::Ok,
                    );
                }
            }
            Err(err) => emit_toast(
                event_tx,
                egui_ctx,
                "固件设置重置失败",
                err.to_string(),
                BadgeTone::Danger,
            ),
        },
        WorkerCmd::Hotplug(notice) => match notice {
            HotplugNotice::Disconnected => {
                pending_uvc.clear();
                *debounce_deadline = None;
                ctrl.mark_disconnected();
                replug_sm.on_device_disconnected();
                preview.stop_for_unplug();
                emit_snapshot(
                    ctrl,
                    replug_sm,
                    ReplugReason::PreExistingMismatch,
                    true,
                    event_tx,
                    egui_ctx,
                );
            }
            HotplugNotice::Connected(_) => {
                thread::sleep(Duration::from_millis(400));
                let _ = ctrl.refresh_all();
                emit_snapshot(
                    ctrl,
                    replug_sm,
                    ReplugReason::PreExistingMismatch,
                    true,
                    event_tx,
                    egui_ctx,
                );
                if let ReplugPhase::JustSynced { mic_enabled } = replug_sm.phase() {
                    emit_toast(
                        event_tx,
                        egui_ctx,
                        "摄像头已重新连接",
                        if mic_enabled {
                            "麦克风 Audio 接口已启用并生效"
                        } else {
                            "麦克风 Audio 接口已移除"
                        },
                        BadgeTone::Ok,
                    );
                    replug_sm.acknowledge_synced();
                }
                preview.resume_after_replug();
            }
        },
    }
    true
}

fn emit_snapshot(
    ctrl: &ModernCamController,
    replug_sm: &mut ReplugStateMachine,
    reason_hint: ReplugReason,
    force_sync_controls: bool,
    event_tx: &Sender<WorkerEvent>,
    egui_ctx: &egui::Context,
) {
    let snap = ctrl.snapshot().clone();
    let hid_mic = snap.hid.as_ref().and_then(|h| h.mic);
    let has_audio = snap
        .topology
        .as_ref()
        .is_some_and(|t| t.has_audio_interface());
    replug_sm.on_snapshot(hid_mic, has_audio, reason_hint);

    let _ = event_tx.send(WorkerEvent::Snapshot {
        snapshot: Box::new(snap),
        replug_phase: replug_sm.phase(),
        force_sync_controls,
    });
    egui_ctx.request_repaint();
}

fn emit_toast(
    event_tx: &Sender<WorkerEvent>,
    egui_ctx: &egui::Context,
    title: impl Into<String>,
    description: impl Into<String>,
    tone: BadgeTone,
) {
    let _ = event_tx.send(WorkerEvent::Toast(GuiToast {
        title: title.into(),
        description: description.into(),
        tone,
    }));
    egui_ctx.request_repaint();
}
