//! Background live camera preview pipeline backed by `cameras 0.3.2`.
//!
//! Runs enumeration, permission checks, format negotiation, and RGBA conversion off the UI
//! thread, exposing a single latest-frame slot so preview latency stays minimal.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cameras::pump::{self, Pump};
use cameras::{Capabilities, Device, FormatDescriptor, PixelFormat, Resolution, StreamConfig};
use crossbeam_channel::{Receiver, Sender, unbounded};

/// Decoded RGBA8 preview frame ready for GPU texture upload.
#[derive(Clone)]
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub seq: u64,
}

/// Camera entry surfaced to the UI camera selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewCameraInfo {
    pub id: String,
    pub name: String,
    pub is_modern_webcam: bool,
}

/// Current state of the video preview pipeline.
#[derive(Debug, Clone, PartialEq)]
pub enum PreviewStatus {
    Initializing,
    Streaming {
        device_name: String,
        width: u32,
        height: u32,
        fps: f32,
    },
    Paused {
        device_name: String,
    },
    WaitingReplug,
    PermissionDenied,
    NoCameraFound,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct PreviewSnapshot {
    pub status: PreviewStatus,
    pub cameras: Vec<PreviewCameraInfo>,
    pub selected_id: Option<String>,
}

impl Default for PreviewSnapshot {
    fn default() -> Self {
        Self {
            status: PreviewStatus::Initializing,
            cameras: Vec::new(),
            selected_id: None,
        }
    }
}

pub enum PreviewCmd {
    StartOrRefresh,
    Pause,
    Resume,
    StopForUnplug,
    ResumeAfterReplug,
    SelectCamera(String),
    Shutdown,
}

struct SharedState {
    snapshot: Mutex<PreviewSnapshot>,
    latest_frame: Mutex<Option<PreviewFrame>>,
    mirror: AtomicBool,
}

/// Thread-safe handle used by UI & hardware worker to inspect frames and control preview.
#[derive(Clone)]
pub struct PreviewHandle {
    cmd_tx: Sender<PreviewCmd>,
    shared: Arc<SharedState>,
}

impl PreviewHandle {
    pub fn snapshot(&self) -> PreviewSnapshot {
        self.shared
            .snapshot
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
    }

    pub fn take_latest_frame(&self) -> Option<PreviewFrame> {
        self.shared
            .latest_frame
            .lock()
            .ok()
            .and_then(|mut g| g.take())
    }

    pub fn set_mirror(&self, mirror: bool) {
        self.shared.mirror.store(mirror, Ordering::Relaxed);
    }

    pub fn start_or_refresh(&self) {
        let _ = self.cmd_tx.send(PreviewCmd::StartOrRefresh);
    }

    pub fn pause(&self) {
        let _ = self.cmd_tx.send(PreviewCmd::Pause);
    }

    pub fn resume(&self) {
        let _ = self.cmd_tx.send(PreviewCmd::Resume);
    }

    pub fn stop_for_unplug(&self) {
        let _ = self.cmd_tx.send(PreviewCmd::StopForUnplug);
    }

    pub fn resume_after_replug(&self) {
        let _ = self.cmd_tx.send(PreviewCmd::ResumeAfterReplug);
    }

    pub fn select_camera(&self, id: String) {
        let _ = self.cmd_tx.send(PreviewCmd::SelectCamera(id));
    }

    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(PreviewCmd::Shutdown);
    }
}

/// Owns the preview thread and joins it on drop.
pub struct PreviewManager {
    handle: PreviewHandle,
    thread: Option<JoinHandle<()>>,
}

impl PreviewManager {
    pub fn spawn(egui_ctx: egui::Context) -> Self {
        let (cmd_tx, cmd_rx) = unbounded();
        let shared = Arc::new(SharedState {
            snapshot: Mutex::new(PreviewSnapshot::default()),
            latest_frame: Mutex::new(None),
            mirror: AtomicBool::new(false),
        });
        let handle = PreviewHandle {
            cmd_tx,
            shared: Arc::clone(&shared),
        };

        let thread = thread::Builder::new()
            .name("moderncam-preview".into())
            .spawn(move || run_preview_loop(cmd_rx, shared, egui_ctx))
            .ok();

        handle.start_or_refresh();

        Self { handle, thread }
    }

    pub fn handle(&self) -> PreviewHandle {
        self.handle.clone()
    }
}

impl Drop for PreviewManager {
    fn drop(&mut self) {
        self.handle.shutdown();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct ActivePump {
    pump: Pump,
    device_name: String,
    paused: bool,
}

fn run_preview_loop(
    cmd_rx: Receiver<PreviewCmd>,
    shared: Arc<SharedState>,
    egui_ctx: egui::Context,
) {
    let mut active: Option<ActivePump> = None;
    let mut preferred_id: Option<String> = None;

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            PreviewCmd::Shutdown => {
                stop_active(&mut active, &shared);
                break;
            }
            PreviewCmd::StopForUnplug => {
                stop_active(&mut active, &shared);
                set_status(&shared, &egui_ctx, PreviewStatus::WaitingReplug);
            }
            PreviewCmd::Pause => {
                if let Some(act) = active.as_mut() {
                    pump::set_active(&act.pump, false);
                    act.paused = true;
                    set_status(
                        &shared,
                        &egui_ctx,
                        PreviewStatus::Paused {
                            device_name: act.device_name.clone(),
                        },
                    );
                }
            }
            PreviewCmd::Resume => {
                if let Some(act) = active.as_mut() {
                    pump::set_active(&act.pump, true);
                    act.paused = false;
                } else {
                    start_best_camera(&mut active, preferred_id.as_deref(), &shared, &egui_ctx);
                }
            }
            PreviewCmd::SelectCamera(id) => {
                preferred_id = Some(id);
                stop_active(&mut active, &shared);
                start_best_camera(&mut active, preferred_id.as_deref(), &shared, &egui_ctx);
            }
            PreviewCmd::StartOrRefresh => {
                if active.is_none() {
                    start_best_camera(&mut active, preferred_id.as_deref(), &shared, &egui_ctx);
                } else {
                    refresh_camera_list_only(&shared, &egui_ctx);
                }
            }
            PreviewCmd::ResumeAfterReplug => {
                stop_active(&mut active, &shared);
                // AVFoundation/V4L2/MF may register the video node slightly after USB connect.
                for attempt in 0..8 {
                    if start_best_camera(&mut active, preferred_id.as_deref(), &shared, &egui_ctx) {
                        break;
                    }
                    if attempt < 7 {
                        thread::sleep(Duration::from_millis(350));
                    }
                }
            }
        }
    }
}

fn stop_active(active: &mut Option<ActivePump>, shared: &Arc<SharedState>) {
    if let Some(act) = active.take() {
        pump::stop_and_join(act.pump);
    }
    if let Ok(mut f) = shared.latest_frame.lock() {
        *f = None;
    }
}

fn set_status(shared: &Arc<SharedState>, egui_ctx: &egui::Context, status: PreviewStatus) {
    if let Ok(mut snap) = shared.snapshot.lock() {
        snap.status = status;
    }
    egui_ctx.request_repaint();
}

fn refresh_camera_list_only(shared: &Arc<SharedState>, egui_ctx: &egui::Context) {
    if let Ok(devs) = cameras::devices() {
        let list = to_camera_infos(&devs);
        if let Ok(mut snap) = shared.snapshot.lock() {
            snap.cameras = list;
        }
        egui_ctx.request_repaint();
    }
}

fn start_best_camera(
    active: &mut Option<ActivePump>,
    preferred_id: Option<&str>,
    shared: &Arc<SharedState>,
    egui_ctx: &egui::Context,
) -> bool {
    set_status(shared, egui_ctx, PreviewStatus::Initializing);
    let devs = match cameras::devices() {
        Ok(list) => list,
        Err(cameras::Error::PermissionDenied) => {
            set_status(shared, egui_ctx, PreviewStatus::PermissionDenied);
            return false;
        }
        Err(err) => {
            set_status(shared, egui_ctx, PreviewStatus::Error(err.to_string()));
            return false;
        }
    };

    let infos = to_camera_infos(&devs);
    if infos.is_empty() {
        if let Ok(mut snap) = shared.snapshot.lock() {
            snap.cameras = infos;
            snap.selected_id = None;
            snap.status = PreviewStatus::NoCameraFound;
        }
        egui_ctx.request_repaint();
        return false;
    }

    let chosen = select_device(&devs, preferred_id);
    let Some(device) = chosen else {
        set_status(shared, egui_ctx, PreviewStatus::NoCameraFound);
        return false;
    };

    let selected_id = device.id.0.clone();
    let device_name = device.name.clone();
    if let Ok(mut snap) = shared.snapshot.lock() {
        snap.cameras = infos;
        snap.selected_id = Some(selected_id.clone());
    }

    let config = match choose_stream_config(&device) {
        Ok(cfg) => cfg,
        Err(err) => {
            set_status(shared, egui_ctx, PreviewStatus::Error(err));
            return false;
        }
    };

    let camera = match cameras::open(&device, config) {
        Ok(cam) => cam,
        Err(cameras::Error::PermissionDenied) => {
            set_status(shared, egui_ctx, PreviewStatus::PermissionDenied);
            return false;
        }
        Err(err) => {
            set_status(shared, egui_ctx, PreviewStatus::Error(err.to_string()));
            return false;
        }
    };

    let shared_for_cb = Arc::clone(shared);
    let ctx_for_cb = egui_ctx.clone();
    let dev_name_for_cb = device_name.clone();
    let mut seq: u64 = 0;
    let mut fps_window_start = Instant::now();
    let mut fps_frames: u32 = 0;
    let mut measured_fps: f32 = config.framerate as f32;

    let pump = pump::spawn(camera, move |frame| {
        let Ok(mut rgba) = cameras::to_rgba8(&frame) else {
            return;
        };
        if shared_for_cb.mirror.load(Ordering::Relaxed) {
            flip_rgba_horizontal(&mut rgba, frame.width as usize, frame.height as usize);
        }

        seq = seq.wrapping_add(1);
        fps_frames += 1;
        let elapsed = fps_window_start.elapsed();
        if elapsed >= Duration::from_millis(600) {
            measured_fps = fps_frames as f32 / elapsed.as_secs_f32();
            fps_frames = 0;
            fps_window_start = Instant::now();
        }

        if let Ok(mut slot) = shared_for_cb.latest_frame.lock() {
            *slot = Some(PreviewFrame {
                width: frame.width,
                height: frame.height,
                rgba,
                seq,
            });
        }
        if let Ok(mut snap) = shared_for_cb.snapshot.lock() {
            snap.status = PreviewStatus::Streaming {
                device_name: dev_name_for_cb.clone(),
                width: frame.width,
                height: frame.height,
                fps: measured_fps,
            };
        }
        ctx_for_cb.request_repaint();
    });

    *active = Some(ActivePump {
        pump,
        device_name,
        paused: false,
    });
    true
}

fn is_modern_webcam(dev: &Device) -> bool {
    let id_l = dev.id.0.to_ascii_lowercase();
    let name_l = dev.name.to_ascii_lowercase();
    (id_l.contains("045e") && id_l.contains("0840")) || name_l.contains("modern webcam")
}

fn to_camera_infos(devs: &[Device]) -> Vec<PreviewCameraInfo> {
    devs.iter()
        .map(|d| PreviewCameraInfo {
            id: d.id.0.clone(),
            name: d.name.clone(),
            is_modern_webcam: is_modern_webcam(d),
        })
        .collect()
}

fn select_device(devs: &[Device], preferred_id: Option<&str>) -> Option<Device> {
    if let Some(pid) = preferred_id
        && let Some(found) = devs.iter().find(|d| d.id.0 == pid)
    {
        return Some(found.clone());
    }
    devs.iter()
        .find(|d| is_modern_webcam(d))
        .cloned()
        .or_else(|| devs.first().cloned())
}

fn choose_stream_config(device: &Device) -> Result<StreamConfig, String> {
    let caps: Capabilities = cameras::probe(device).map_err(|e| e.to_string())?;
    let preferred_res = Resolution {
        width: 1280,
        height: 720,
    };

    // Prefer uncompressed or fast-decode formats at 1280x720@30fps for snappy low-latency preview.
    let format_rank = |pf: PixelFormat| -> u8 {
        match pf {
            PixelFormat::Bgra8 | PixelFormat::Rgba8 => 0,
            PixelFormat::Nv12 => 1,
            PixelFormat::Yuyv => 2,
            PixelFormat::Mjpeg => 3,
            _ => 4,
        }
    };

    if let Some(desc) = pick_720p_or_nearest(&caps.formats, preferred_res, format_rank) {
        let fps = desc.framerate_range.max.round().clamp(15.0, 30.0) as u32;
        return Ok(StreamConfig {
            resolution: desc.resolution,
            framerate: fps,
            pixel_format: desc.pixel_format,
        });
    }

    let fallback = StreamConfig {
        resolution: preferred_res,
        framerate: 30,
        pixel_format: PixelFormat::Nv12,
    };
    if let Some(desc) = cameras::best_format(&caps, &fallback) {
        let fps = desc.framerate_range.max.round().clamp(1.0, 30.0) as u32;
        return Ok(StreamConfig {
            resolution: desc.resolution,
            framerate: fps,
            pixel_format: desc.pixel_format,
        });
    }
    Err("camera reported no supported video formats".into())
}

fn pick_720p_or_nearest<F>(
    formats: &[FormatDescriptor],
    target: Resolution,
    rank: F,
) -> Option<FormatDescriptor>
where
    F: Fn(PixelFormat) -> u8,
{
    formats
        .iter()
        .filter(|f| f.framerate_range.max >= 24.0)
        .min_by_key(|f| {
            let exact_720 = if f.resolution == target { 0i64 } else { 1 };
            let aspect_16_9 = if f.resolution.width * 9 == f.resolution.height * 16 {
                0i64
            } else {
                1
            };
            let delta = (f.resolution.width as i64 - target.width as i64).abs()
                + (f.resolution.height as i64 - target.height as i64).abs();
            (exact_720, aspect_16_9, delta, rank(f.pixel_format))
        })
        .cloned()
}

fn flip_rgba_horizontal(buf: &mut [u8], width: usize, height: usize) {
    let row_bytes = width * 4;
    if row_bytes == 0 || buf.len() < row_bytes * height {
        return;
    }
    for row in buf.chunks_exact_mut(row_bytes).take(height) {
        let mut left = 0usize;
        let mut right = width.saturating_sub(1);
        while left < right {
            let l_off = left * 4;
            let r_off = right * 4;
            for i in 0..4 {
                row.swap(l_off + i, r_off + i);
            }
            left += 1;
            right -= 1;
        }
    }
}
