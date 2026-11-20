//! Main `eframe` application UI for Microsoft Modern Webcam Control & Live Preview.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use eframe::egui;
use elegance::{
    Accent, Badge, BadgeTone, BuiltInTheme, Button, ButtonSize, Callout, CalloutTone, Card,
    CollapsingSection, IndicatorState, SegmentedControl, SegmentedSize, Select, Slider, StatusPill,
    Switch, Theme, Toast, Toasts, glyphs,
};
use moderncam_device::DeviceSnapshot;
use moderncam_protocol::sync::{
    MicActivationStatus, ReplugPhase, ReplugReason, interface_class_name,
};
use moderncam_protocol::uvc::{ControlRange, PowerLineFreq, UvcControlKey, UvcControlUpdate};

use crate::fonts;
use crate::preview::{PreviewHandle, PreviewManager, PreviewStatus};
use crate::worker::{HardwareWorker, WorkerEvent, WorkerHandle};

const USER_EDIT_LOCK_WINDOW: Duration = Duration::from_millis(300);

/// Local editable mirror of hardware controls so UI drags remain immediate and jitter-free.
#[derive(Debug, Clone)]
struct LocalControls {
    hdr: bool,
    retouch: bool,
    mic: bool,
    brightness: i32,
    contrast: i32,
    saturation: i32,
    sharpness: i32,
    zoom: i32,
    wb_auto: bool,
    wb_temp: i32,
    ae_auto: bool,
    exposure_time: i32,
    flicker: PowerLineFreq,
}

impl Default for LocalControls {
    fn default() -> Self {
        Self {
            hdr: false,
            retouch: false,
            mic: true,
            brightness: 128,
            contrast: 128,
            saturation: 32,
            sharpness: 2,
            zoom: 0,
            wb_auto: true,
            wb_temp: 4500,
            ae_auto: true,
            exposure_time: 156,
            flicker: PowerLineFreq::Auto,
        }
    }
}

pub struct ModernCamApp {
    theme: BuiltInTheme,
    _preview_mgr: PreviewManager,
    preview: PreviewHandle,
    _hw_worker: HardwareWorker,
    worker: WorkerHandle,

    snapshot: DeviceSnapshot,
    replug_phase: ReplugPhase,
    controls: LocalControls,
    last_edit: HashMap<UvcControlKey, Instant>,

    mirror_preview: bool,
    selected_camera_id: String,
    preview_texture: Option<egui::TextureHandle>,
    last_frame_seq: u64,
}

impl ModernCamApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let theme = BuiltInTheme::Slate;
        theme.theme().install(&cc.egui_ctx);
        fonts::install_cjk_fallback(&cc.egui_ctx);

        let preview_mgr = PreviewManager::spawn(cc.egui_ctx.clone());
        let preview = preview_mgr.handle();
        let hw_worker = HardwareWorker::spawn(cc.egui_ctx.clone(), preview.clone());
        let worker = hw_worker.handle();

        Self {
            theme,
            _preview_mgr: preview_mgr,
            preview,
            _hw_worker: hw_worker,
            worker,
            snapshot: DeviceSnapshot::default(),
            replug_phase: ReplugPhase::Idle,
            controls: LocalControls::default(),
            last_edit: HashMap::new(),
            mirror_preview: false,
            selected_camera_id: String::new(),
            preview_texture: None,
            last_frame_seq: 0,
        }
    }

    fn drain_worker_events(&mut self, ctx: &egui::Context) {
        while let Some(event) = self.worker.try_recv_event() {
            match event {
                WorkerEvent::Snapshot {
                    snapshot,
                    replug_phase,
                    force_sync_controls,
                } => {
                    self.snapshot = *snapshot;
                    self.replug_phase = replug_phase;
                    self.sync_local_controls(force_sync_controls);
                }
                WorkerEvent::Toast(t) => {
                    Toast::new(t.title)
                        .description(t.description)
                        .tone(t.tone)
                        .show(ctx);
                }
            }
        }
    }

    fn sync_local_controls(&mut self, force: bool) {
        if let Some(hid) = &self.snapshot.hid {
            if let Some(v) = hid.hdr {
                self.controls.hdr = v;
            }
            if let Some(v) = hid.retouch {
                self.controls.retouch = v;
            }
            if let Some(v) = hid.mic {
                self.controls.mic = v;
            }
        }
        let Some(uvc) = &self.snapshot.uvc_state else {
            return;
        };
        let can_sync = |key: UvcControlKey, last_edit: &HashMap<UvcControlKey, Instant>| {
            force
                || last_edit
                    .get(&key)
                    .is_none_or(|t| t.elapsed() >= USER_EDIT_LOCK_WINDOW)
        };

        if can_sync(UvcControlKey::Brightness, &self.last_edit)
            && let Some(v) = uvc.brightness
        {
            self.controls.brightness = v;
        }
        if can_sync(UvcControlKey::Contrast, &self.last_edit)
            && let Some(v) = uvc.contrast
        {
            self.controls.contrast = v;
        }
        if can_sync(UvcControlKey::Saturation, &self.last_edit)
            && let Some(v) = uvc.saturation
        {
            self.controls.saturation = v;
        }
        if can_sync(UvcControlKey::Sharpness, &self.last_edit)
            && let Some(v) = uvc.sharpness
        {
            self.controls.sharpness = v;
        }
        if can_sync(UvcControlKey::Zoom, &self.last_edit)
            && let Some(v) = uvc.zoom
        {
            self.controls.zoom = v;
        }
        if can_sync(UvcControlKey::WhiteBalanceAuto, &self.last_edit)
            && let Some(v) = uvc.white_balance_auto
        {
            self.controls.wb_auto = v;
        }
        if can_sync(UvcControlKey::WhiteBalanceTemp, &self.last_edit)
            && let Some(v) = uvc.white_balance_temp
        {
            self.controls.wb_temp = v;
        }
        if can_sync(UvcControlKey::AutoExposure, &self.last_edit)
            && let Some(v) = uvc.auto_exposure
        {
            self.controls.ae_auto = v;
        }
        if can_sync(UvcControlKey::ExposureTime, &self.last_edit)
            && let Some(v) = uvc.exposure_time
        {
            self.controls.exposure_time = v;
        }
        if can_sync(UvcControlKey::PowerLineFreq, &self.last_edit)
            && let Some(v) = uvc.power_line_freq
        {
            self.controls.flicker = v;
        }
    }

    fn send_uvc(&mut self, update: UvcControlUpdate) {
        self.last_edit.insert(update.key(), Instant::now());
        self.worker.set_uvc(update);
    }

    fn upload_latest_preview_frame(&mut self, ctx: &egui::Context) {
        let Some(frame) = self.preview.take_latest_frame() else {
            return;
        };
        if frame.seq == self.last_frame_seq {
            return;
        }
        self.last_frame_seq = frame.seq;
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.rgba,
        );
        match &mut self.preview_texture {
            Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
            None => {
                self.preview_texture =
                    Some(ctx.load_texture("webcam_preview", image, egui::TextureOptions::LINEAR));
            }
        }
    }
}

impl eframe::App for ModernCamApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_worker_events(&ctx);
        self.upload_latest_preview_frame(&ctx);

        let preview_snap = self.preview.snapshot();
        if self.selected_camera_id.is_empty()
            && let Some(sel) = &preview_snap.selected_id
        {
            self.selected_camera_id = sel.clone();
        }

        self.render_header(ui);

        egui::CentralPanel::default().show(ui, |ui| {
            self.render_replug_callout(ui);

            let avail = ui.available_size();
            let gap = 14.0;
            let left_w = ((avail.x - gap) * 0.56).clamp(380.0, 760.0);
            let right_w = (avail.x - gap - left_w).max(320.0);
            let connected = self.snapshot.connected;
            let has_hid = connected && self.snapshot.hid.is_some();
            let has_uvc = connected && self.snapshot.uvc_state.is_some();

            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(left_w, avail.y),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("left_scroll")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                self.render_preview_column(ui, &preview_snap);
                                ui.add_space(10.0);
                                self.render_device_info_card(ui, has_hid, has_uvc);
                                ui.add_space(12.0);
                            });
                    },
                );
                ui.add_space(gap);
                ui.allocate_ui_with_layout(
                    egui::vec2(right_w, avail.y),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        self.render_controls_column(ui);
                    },
                );
            });
        });

        Toasts::new().render(&ctx);
    }
}

impl ModernCamApp {
    fn render_header(&mut self, ui: &mut egui::Ui) {
        let theme = Theme::current(ui.ctx());
        let bg = theme.palette.bg;

        let resp = egui::Panel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(bg)
                    .inner_margin(egui::Margin::symmetric(16, 10)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Label::new(
                        egui::RichText::new("Microsoft Modern Webcam 控制台")
                            .color(theme.palette.text)
                            .size(18.0)
                            .strong(),
                    ));
                    ui.add_space(12.0);

                    let usb_ind = if self.snapshot.connected {
                        IndicatorState::On
                    } else if matches!(
                        self.replug_phase,
                        ReplugPhase::DeviceRemovedWaitingReturn { .. }
                    ) {
                        IndicatorState::Connecting
                    } else {
                        IndicatorState::Off
                    };

                    let mic_ind = match self.snapshot.mic_status {
                        MicActivationStatus::ActiveSynced => IndicatorState::On,
                        MicActivationStatus::InactiveSynced => IndicatorState::Idle,
                        MicActivationStatus::PendingEnableReplug
                        | MicActivationStatus::PendingDisableReplug => IndicatorState::Connecting,
                        MicActivationStatus::Unknown => IndicatorState::Off,
                    };

                    let shutter_ind = match self
                        .snapshot
                        .uvc_state
                        .as_ref()
                        .and_then(|s| s.privacy_shutter)
                    {
                        Some(false) => IndicatorState::On,
                        Some(true) => IndicatorState::Off,
                        None => IndicatorState::Idle,
                    };

                    ui.add(
                        StatusPill::new()
                            .item("USB", usb_ind)
                            .item("麦克风", mic_ind)
                            .item("挡板", shutter_ind),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Select internally switches to vertical() (top_down LEFT), so isolate it
                        // inside a fixed-width left_to_right allocation to anchor it on the right.
                        let mut theme_changed = false;
                        ui.allocate_ui_with_layout(
                            egui::vec2(106.0, 26.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                let options =
                                    BuiltInTheme::all().into_iter().map(|t| (t, t.label()));
                                if ui
                                    .add(
                                        Select::new("header_theme_select", &mut self.theme)
                                            .options(options)
                                            .width(106.0)
                                            .size(ButtonSize::Small),
                                    )
                                    .changed()
                                {
                                    theme_changed = true;
                                }
                            },
                        );
                        if theme_changed {
                            self.theme.theme().install(ui.ctx());
                        }
                        ui.add_space(8.0);
                        if ui
                            .add(
                                Button::icon_text(glyphs::REFRESH, "刷新")
                                    .outline()
                                    .size(ButtonSize::Small),
                            )
                            .clicked()
                        {
                            self.worker.refresh_all();
                            self.preview.start_or_refresh();
                        }
                    });
                });
            });

        let header_rect = resp.response.rect;
        ui.painter().hline(
            header_rect.x_range(),
            header_rect.bottom() - 0.5,
            egui::Stroke::new(1.0, theme.palette.border),
        );
    }

    fn render_replug_callout(&self, ui: &mut egui::Ui) {
        let banner = match self.replug_phase {
            ReplugPhase::NeedsReplug { target_mic, reason } => {
                let title = if target_mic {
                    "需要重新插拔摄像头 USB 以启用麦克风"
                } else {
                    "需要重新插拔摄像头 USB 以彻底关闭麦克风"
                };
                let prefix = match reason {
                    ReplugReason::ResetChangedMic => "执行设备重置 (HID 0xAE) 后，",
                    ReplugReason::MicToggled => "切换麦克风开关后，",
                    ReplugReason::PreExistingMismatch => "检测到",
                };
                let body = format!(
                    "{prefix}设备闪存配置（mic={}）与当前 USB 接口枚举不一致。由于固件不支持软件重启，请拔插一次摄像头 USB 线，程序正自动监听重连并将在接入后自动恢复预览。",
                    if target_mic { "开" } else { "关" }
                );
                Some((title, body))
            }
            ReplugPhase::DeviceRemovedWaitingReturn { target_mic, .. } => Some((
                "已检测到摄像头拔出，等待重新插入…",
                format!(
                    "请将 Microsoft Modern Webcam 重新插入 USB 接口，系统将校验麦克风 Audio 接口是否{}。",
                    if target_mic {
                        "成功暴露"
                    } else {
                        "已移除"
                    }
                ),
            )),
            _ if self.snapshot.mic_status.needs_replug() => Some((
                "麦克风配置待插拔生效",
                "请拔插一次摄像头 USB 线以刷新 USB 描述符。".to_string(),
            )),
            _ => None,
        };

        if let Some((title, body)) = banner {
            Callout::new(CalloutTone::Warning)
                .title(title)
                .body(body)
                .tinted()
                .multiline()
                .show(ui, |_| {});
            ui.add_space(10.0);
        }
    }

    fn render_preview_column(
        &mut self,
        ui: &mut egui::Ui,
        preview_snap: &crate::preview::PreviewSnapshot,
    ) {
        let theme = Theme::current(ui.ctx());

        Card::new().heading("实时画面预览").show(ui, |ui| {
            // Top info & camera selector bar
            ui.horizontal(|ui| {
                if !preview_snap.cameras.is_empty() {
                    let options: Vec<(String, String)> = preview_snap
                        .cameras
                        .iter()
                        .map(|c| {
                            let label = if c.is_modern_webcam {
                                format!("★ {}", c.name)
                            } else {
                                c.name.clone()
                            };
                            (c.id.clone(), label)
                        })
                        .collect();
                    let prev_sel = self.selected_camera_id.clone();
                    ui.add(
                        Select::new("preview_cam_select", &mut self.selected_camera_id)
                            .options(options)
                            .width(230.0)
                            .size(ButtonSize::Small),
                    );
                    if self.selected_camera_id != prev_sel && !self.selected_camera_id.is_empty() {
                        self.preview.select_camera(self.selected_camera_id.clone());
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    match &preview_snap.status {
                        PreviewStatus::Streaming {
                            width, height, fps, ..
                        } => {
                            ui.add(
                                Badge::new(format!("{width}×{height} · {fps:.0} fps"), BadgeTone::Ok)
                                    .preserve_case(),
                            );
                        }
                        PreviewStatus::Paused { .. } => {
                            ui.add(Badge::new("已暂停", BadgeTone::Neutral).preserve_case());
                        }
                        PreviewStatus::Initializing => {
                            ui.add(Badge::new("正在初始化…", BadgeTone::Info).preserve_case());
                        }
                        PreviewStatus::WaitingReplug => {
                            ui.add(Badge::new("等待重插", BadgeTone::Warning).preserve_case());
                        }
                        PreviewStatus::PermissionDenied => {
                            ui.add(Badge::new("缺少摄像头权限", BadgeTone::Danger).preserve_case());
                        }
                        PreviewStatus::NoCameraFound => {
                            ui.add(Badge::new("未检测到摄像头", BadgeTone::Warning).preserve_case());
                        }
                        PreviewStatus::Error(_) => {
                            ui.add(Badge::new("预览异常", BadgeTone::Danger).preserve_case());
                        }
                    }
                });
            });

            ui.add_space(8.0);

            // 16:9 preview viewport capped at 420px so Device Info fits cleanly below on launch.
            let canvas_w = ui.available_width();
            let canvas_h = (canvas_w * 9.0 / 16.0).min(420.0);
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(canvas_w, canvas_h), egui::Sense::hover());

            ui.painter()
                .rect_filled(rect, 8.0, egui::Color32::from_rgb(14, 17, 23));

            let is_streaming = matches!(preview_snap.status, PreviewStatus::Streaming { .. });
            if is_streaming || matches!(preview_snap.status, PreviewStatus::Paused { .. }) {
                if let Some(tex) = &self.preview_texture {
                    let tex_size = tex.size_vec2();
                    let scale = (rect.width() / tex_size.x).min(rect.height() / tex_size.y);
                    let draw_size = tex_size * scale;
                    let img_rect = egui::Rect::from_center_size(rect.center(), draw_size);
                    egui::Image::new((tex.id(), draw_size))
                        .corner_radius(6.0)
                        .paint_at(ui, img_rect);
                }
            } else {
                // Status overlay when stream is inactive
                let msg = match &preview_snap.status {
                    PreviewStatus::Initializing => "正在打开摄像头预览并检查系统权限…",
                    PreviewStatus::WaitingReplug => "设备已断开，等待重新插入后自动恢复预览…",
                    PreviewStatus::PermissionDenied => {
                        "系统拒绝了摄像头访问权限。\n请在「系统设置 → 隐私与安全性 → 摄像头」中授权后点击重试。"
                    }
                    PreviewStatus::NoCameraFound => "未找到可用视频设备，请检查 USB 连接。",
                    PreviewStatus::Error(e) => e.as_str(),
                    _ => "",
                };
                ui.scope_builder(egui::UiBuilder::new().max_rect(rect.shrink(20.0)), |ui| {
                    ui.centered_and_justified(|ui| {
                        ui.vertical_centered(|ui| {
                            ui.add(egui::Label::new(
                                egui::RichText::new(msg)
                                    .color(theme.palette.text_muted)
                                    .size(14.0),
                            ));
                            if matches!(
                                preview_snap.status,
                                PreviewStatus::PermissionDenied
                                    | PreviewStatus::NoCameraFound
                                    | PreviewStatus::Error(_)
                            ) {
                                ui.add_space(10.0);
                                if ui
                                    .add(
                                        Button::icon_text(glyphs::REFRESH, "重试预览")
                                            .accent(Accent::Blue),
                                    )
                                    .clicked()
                                {
                                    self.preview.start_or_refresh();
                                }
                            }
                        });
                    });
                });
            }

            // Physical privacy shutter overlay warning
            if self
                .snapshot
                .uvc_state
                .as_ref()
                .and_then(|s| s.privacy_shutter)
                == Some(true)
            {
                let badge_rect = egui::Rect::from_center_size(
                    egui::pos2(rect.center().x, rect.bottom() - 28.0),
                    egui::vec2(300.0, 28.0),
                );
                ui.painter().rect_filled(
                    badge_rect,
                    6.0,
                    egui::Color32::from_rgba_unmultiplied(180, 83, 9, 225),
                );
                ui.painter().text(
                    badge_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "物理隐私挡板已关闭 — 请向右侧拨开镜头挡板",
                    egui::FontId::proportional(13.0),
                    egui::Color32::WHITE,
                );
            }

            ui.add_space(10.0);

            // Bottom preview controls
            ui.horizontal(|ui| {
                match &preview_snap.status {
                    PreviewStatus::Paused { .. }
                        if ui
                            .add(
                                Button::icon_text(glyphs::PLAY, "继续预览")
                                    .accent(Accent::Green)
                                    .size(ButtonSize::Small),
                            )
                            .clicked() =>
                    {
                        self.preview.resume();
                    }
                    PreviewStatus::Streaming { .. }
                        if ui
                            .add(
                                Button::icon_text(glyphs::PAUSE, "暂停预览")
                                    .outline()
                                    .size(ButtonSize::Small),
                            )
                            .clicked() =>
                    {
                        self.preview.pause();
                    }
                    _ => {}
                }

                ui.add_space(8.0);
                if ui
                    .add(Switch::new(&mut self.mirror_preview, "水平镜像"))
                    .changed()
                {
                    self.preview.set_mirror(self.mirror_preview);
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let shutter_text = match self
                        .snapshot
                        .uvc_state
                        .as_ref()
                        .and_then(|s| s.privacy_shutter)
                    {
                        Some(true) => ("隐私挡板：已关闭", BadgeTone::Warning),
                        Some(false) => ("隐私挡板：已打开", BadgeTone::Ok),
                        None => ("挡板状态未知", BadgeTone::Neutral),
                    };
                    ui.add(Badge::new(shutter_text.0, shutter_text.1).preserve_case());
                });
            });
        });
    }

    fn render_controls_column(&mut self, ui: &mut egui::Ui) {
        let connected = self.snapshot.connected;
        let has_hid = connected && self.snapshot.hid.is_some();
        let has_uvc = connected && self.snapshot.uvc_state.is_some();

        egui::ScrollArea::vertical()
            .id_salt("right_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

                // 1. Proprietary HID Settings Card
                Card::new().heading("专属功能 (HID)").show(ui, |ui| {
                    if ui
                        .add(Switch::new(&mut self.controls.hdr, "HDR 高动态范围").enabled(has_hid))
                        .changed()
                    {
                        self.worker.set_hdr(self.controls.hdr);
                    }

                    if ui
                        .add(
                            Switch::new(&mut self.controls.retouch, "Retouch 面部美颜")
                                .enabled(has_hid),
                        )
                        .changed()
                    {
                        self.worker.set_retouch(self.controls.retouch);
                    }

                    ui.horizontal(|ui| {
                        if ui
                            .add(Switch::new(&mut self.controls.mic, "内置麦克风").enabled(has_hid))
                            .changed()
                        {
                            self.worker.set_mic(self.controls.mic);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let tone = match self.snapshot.mic_status {
                                MicActivationStatus::ActiveSynced => BadgeTone::Ok,
                                MicActivationStatus::InactiveSynced => BadgeTone::Neutral,
                                MicActivationStatus::PendingEnableReplug
                                | MicActivationStatus::PendingDisableReplug => BadgeTone::Warning,
                                MicActivationStatus::Unknown => BadgeTone::Neutral,
                            };
                            ui.add(
                                Badge::new(self.snapshot.mic_status.label_zh(), tone)
                                    .preserve_case(),
                            );
                        });
                    });
                });

                // 2. Basic UVC Image Settings Card
                let caps = self.snapshot.uvc_caps.clone().unwrap_or_default();
                Card::new().heading("基础画质 (UVC)").show(ui, |ui| {
                    ui.add_enabled_ui(has_uvc, |ui| {
                        if let Some(r) = caps.brightness {
                            let mut val = self.controls.brightness;
                            if self.render_uvc_slider_row(ui, "亮度", &mut val, r, "", None) {
                                self.controls.brightness = val;
                                self.send_uvc(UvcControlUpdate::Brightness(val));
                            }
                        }
                        if let Some(r) = caps.contrast {
                            let mut val = self.controls.contrast;
                            if self.render_uvc_slider_row(ui, "对比度", &mut val, r, "", None) {
                                self.controls.contrast = val;
                                self.send_uvc(UvcControlUpdate::Contrast(val));
                            }
                        }
                        if let Some(r) = caps.saturation {
                            let mut val = self.controls.saturation;
                            if self.render_uvc_slider_row(ui, "饱和度", &mut val, r, "", None) {
                                self.controls.saturation = val;
                                self.send_uvc(UvcControlUpdate::Saturation(val));
                            }
                        }
                        if let Some(r) = caps.sharpness {
                            let mut val = self.controls.sharpness;
                            if self.render_uvc_slider_row(ui, "锐度", &mut val, r, "", None) {
                                self.controls.sharpness = val;
                                self.send_uvc(UvcControlUpdate::Sharpness(val));
                            }
                        }
                        if let Some(r) = caps.zoom {
                            let mut val = self.controls.zoom;
                            if self.render_uvc_slider_row(ui, "数字缩放", &mut val, r, "", None)
                            {
                                self.controls.zoom = val;
                                self.send_uvc(UvcControlUpdate::Zoom(val));
                            }
                        }
                        if caps.brightness.is_none()
                            && caps.contrast.is_none()
                            && caps.saturation.is_none()
                            && caps.sharpness.is_none()
                        {
                            let theme = Theme::current(ui.ctx());
                            ui.add(egui::Label::new(
                                theme.muted_text("未检测到可用 UVC 画质控制"),
                            ));
                        }
                    });
                });

                // 3. White Balance, Exposure, and Anti-Flicker Card
                Card::new()
                    .heading("白平衡、曝光与防闪烁 (UVC)")
                    .show(ui, |ui| {
                        ui.add_enabled_ui(has_uvc, |ui| {
                            if caps.white_balance_auto
                                && ui
                                    .add(Switch::new(&mut self.controls.wb_auto, "自动白平衡"))
                                    .changed()
                            {
                                self.send_uvc(UvcControlUpdate::WhiteBalanceAuto(
                                    self.controls.wb_auto,
                                ));
                            }

                            if let Some(r) = caps.white_balance_temp {
                                let manual_wb =
                                    has_uvc && (!caps.white_balance_auto || !self.controls.wb_auto);
                                ui.add_enabled_ui(manual_wb, |ui| {
                                    let mut val = self.controls.wb_temp;
                                    if self
                                        .render_uvc_slider_row(ui, "色温", &mut val, r, " K", None)
                                    {
                                        self.controls.wb_temp = val;
                                        self.send_uvc(UvcControlUpdate::WhiteBalanceTemp(val));
                                    }

                                    ui.horizontal_wrapped(|ui| {
                                        for (label, kelvin) in [
                                            ("暖光 3200K", 3200),
                                            ("自然 4500K", 4500),
                                            ("日光 5500K", 5500),
                                            ("冷白 6500K", 6500),
                                        ] {
                                            if ui
                                                .add(
                                                    Button::new(label)
                                                        .outline()
                                                        .size(ButtonSize::Small),
                                                )
                                                .clicked()
                                            {
                                                let clamped = r.clamp_snap(kelvin);
                                                self.controls.wb_auto = false;
                                                self.controls.wb_temp = clamped;
                                                self.send_uvc(UvcControlUpdate::WhiteBalanceTemp(
                                                    clamped,
                                                ));
                                            }
                                        }
                                    });
                                });
                            }

                            ui.add_space(6.0);
                            if caps.auto_exposure
                                && ui
                                    .add(Switch::new(&mut self.controls.ae_auto, "自动曝光"))
                                    .changed()
                            {
                                self.send_uvc(UvcControlUpdate::AutoExposure(
                                    self.controls.ae_auto,
                                ));
                            }

                            if let Some(r) = caps.exposure_time {
                                let manual_ae =
                                    has_uvc && (!caps.auto_exposure || !self.controls.ae_auto);
                                ui.add_enabled_ui(manual_ae, |ui| {
                                    let mut val = self.controls.exposure_time;
                                    let fmt_ms: fn(f64) -> String =
                                        |v| format!("{:.1} ms", v / 10.0);
                                    if self.render_uvc_slider_row(
                                        ui,
                                        "曝光时间",
                                        &mut val,
                                        r,
                                        "",
                                        Some(fmt_ms),
                                    ) {
                                        self.controls.exposure_time = val;
                                        self.send_uvc(UvcControlUpdate::ExposureTime(val));
                                    }
                                });
                            }

                            if let Some(plf_cap) = caps.power_line_freq {
                                ui.add_space(6.0);
                                let theme = Theme::current(ui.ctx());
                                ui.add(egui::Label::new(theme.muted_text("防闪烁 (抗频闪)")));
                                let mut options: Vec<(PowerLineFreq, &'static str)> = Vec::new();
                                if plf_cap.auto {
                                    options.push((PowerLineFreq::Auto, "自动"));
                                }
                                if plf_cap.hz50 {
                                    options.push((PowerLineFreq::Hz50, "50 Hz"));
                                }
                                if plf_cap.hz60 {
                                    options.push((PowerLineFreq::Hz60, "60 Hz"));
                                }
                                if plf_cap.disabled {
                                    options.push((PowerLineFreq::Disabled, "关闭"));
                                }
                                if !options.is_empty() {
                                    let mut idx = options
                                        .iter()
                                        .position(|(f, _)| *f == self.controls.flicker)
                                        .unwrap_or(0);
                                    let labels: Vec<&str> =
                                        options.iter().map(|(_, l)| *l).collect();
                                    if ui
                                        .add(
                                            SegmentedControl::new(&mut idx, labels)
                                                .id_salt("flicker_seg")
                                                .size(SegmentedSize::Small)
                                                .fill()
                                                .accent(Accent::Blue),
                                        )
                                        .changed()
                                    {
                                        let chosen = options[idx].0;
                                        self.controls.flicker = chosen;
                                        self.send_uvc(UvcControlUpdate::PowerLineFreq(chosen));
                                    }
                                }
                            }
                        });
                    });

                ui.add_space(12.0);
            });
    }

    fn render_device_info_card(&self, ui: &mut egui::Ui, has_hid: bool, has_uvc: bool) {
        Card::new().show(ui, |ui| {
            CollapsingSection::new("device_info_section", "设备信息与维护")
                .default_open(true)
                .show(ui, |ui| {
                    let theme = Theme::current(ui.ctx());
                    if let Some(hid) = &self.snapshot.hid {
                        ui.horizontal_wrapped(|ui| {
                            info_row(
                                ui,
                                &theme,
                                "固件",
                                &hid.firmware
                                    .map(|f| f.to_string())
                                    .unwrap_or_else(|| "未知".into()),
                            );
                            ui.add_space(10.0);
                            info_row(
                                ui,
                                &theme,
                                "序列号",
                                hid.serial.as_deref().unwrap_or("未知"),
                            );
                            ui.add_space(10.0);
                            info_row(
                                ui,
                                &theme,
                                "外观",
                                hid.color_name.as_deref().unwrap_or("未知"),
                            );
                        });
                    } else {
                        ui.add(egui::Label::new(
                            theme.muted_text("未连接设备或 HID 通道不可用"),
                        ));
                    }

                    if let Some(topo) = &self.snapshot.topology {
                        ui.add_space(6.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.add(egui::Label::new(theme.muted_text("USB 接口：")));
                            for iface in &topo.interfaces {
                                let tone = match iface.class {
                                    1 => BadgeTone::Ok,
                                    3 => BadgeTone::Info,
                                    _ => BadgeTone::Neutral,
                                };
                                let label = format!(
                                    "#{} {}",
                                    iface.number,
                                    interface_class_name(iface.class)
                                );
                                ui.add(Badge::new(label, tone).preserve_case());
                            }
                        });
                    }

                    ui.add_space(8.0);
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add(
                                Button::new("重置画质参数 (UVC)")
                                    .outline()
                                    .size(ButtonSize::Small)
                                    .enabled(has_uvc),
                            )
                            .clicked()
                        {
                            self.worker.reset_uvc_defaults();
                        }

                        if ui
                            .add(
                                Button::new("重置固件配置 (HID 0xAE)")
                                    .accent(Accent::Red)
                                    .outline()
                                    .size(ButtonSize::Small)
                                    .enabled(has_hid),
                            )
                            .clicked()
                        {
                            self.worker.reset_hid_settings();
                        }
                    });
                });
        });
    }

    fn render_uvc_slider_row(
        &self,
        ui: &mut egui::Ui,
        label: &str,
        value: &mut i32,
        range: ControlRange,
        suffix: &str,
        value_fmt: Option<fn(f64) -> String>,
    ) -> bool {
        let theme = Theme::current(ui.ctx());
        let mut changed = false;

        ui.horizontal(|ui| {
            ui.add(egui::Label::new(
                egui::RichText::new(label)
                    .color(theme.palette.text)
                    .size(theme.typography.label),
            ));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let is_default = *value == range.default;
                if ui
                    .add(
                        Button::new("默认")
                            .outline()
                            .size(ButtonSize::Small)
                            .enabled(!is_default),
                    )
                    .clicked()
                {
                    *value = range.default;
                    changed = true;
                }
            });
        });

        let mut slider = Slider::new(value, range.min..=range.max)
            .step(range.step.max(1) as f64)
            .show_value(true)
            .desired_width(ui.available_width())
            .accent(Accent::Blue);

        if !suffix.is_empty() {
            slider = slider.suffix(suffix);
        }
        if let Some(fmt) = value_fmt {
            slider = slider.value_fmt(fmt);
        }

        if ui.add(slider).changed() {
            changed = true;
        }
        ui.add_space(2.0);
        changed
    }
}

fn info_row(ui: &mut egui::Ui, theme: &Theme, key: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(theme.muted_text(format!("{key}："))));
        ui.add(egui::Label::new(
            egui::RichText::new(value)
                .color(theme.palette.text)
                .monospace(),
        ));
    });
}
