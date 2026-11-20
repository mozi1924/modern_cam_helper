//! Unified controller combining USB topology, proprietary HID, and UVC controls.

use moderncam_protocol::hid::HidDeviceState;
use moderncam_protocol::sync::{MicActivationStatus, evaluate_mic_status};
use moderncam_protocol::uvc::{UvcCapabilities, UvcControlUpdate, UvcState};

use crate::error::DeviceError;
use crate::hid::HidClient;
use crate::topology::{UsbDeviceTopology, query_usb_topology};
use crate::uvc::{UvcTransport, open_default_uvc};

/// Complete state snapshot of the connected Microsoft Modern Webcam.
#[derive(Debug, Clone, Default)]
pub struct DeviceSnapshot {
    pub connected: bool,
    pub topology: Option<UsbDeviceTopology>,
    pub hid: Option<HidDeviceState>,
    pub uvc_caps: Option<UvcCapabilities>,
    pub uvc_state: Option<UvcState>,
    pub mic_status: MicActivationStatus,
    pub last_error: Option<String>,
}

/// Stateful facade coordinating HID, UVC, and USB topology inspection.
pub struct ModernCamController {
    hid: Option<HidClient>,
    uvc: Option<Box<dyn UvcTransport>>,
    snapshot: DeviceSnapshot,
}

impl std::fmt::Debug for ModernCamController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModernCamController")
            .field("connected", &self.snapshot.connected)
            .finish_non_exhaustive()
    }
}

impl Default for ModernCamController {
    fn default() -> Self {
        Self {
            hid: None,
            uvc: None,
            snapshot: DeviceSnapshot {
                mic_status: MicActivationStatus::Unknown,
                ..DeviceSnapshot::default()
            },
        }
    }
}

impl ModernCamController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> &DeviceSnapshot {
        &self.snapshot
    }

    /// Drops open HID/UVC handles immediately when a USB disconnect is detected.
    pub fn mark_disconnected(&mut self) {
        self.hid = None;
        self.uvc = None;
        self.snapshot = DeviceSnapshot {
            connected: false,
            mic_status: MicActivationStatus::Unknown,
            ..DeviceSnapshot::default()
        };
    }

    /// Connects (if needed) and refreshes USB topology, HID state, and UVC state/capabilities.
    pub fn refresh_all(&mut self) -> Result<&DeviceSnapshot, DeviceError> {
        let topology = query_usb_topology()?;
        let Some(topo) = topology else {
            self.mark_disconnected();
            return Err(DeviceError::NotFound);
        };

        let mut errs = Vec::new();

        if self.hid.is_none() {
            match HidClient::open() {
                Ok(client) => self.hid = Some(client),
                Err(err) => errs.push(format!("HID: {err}")),
            }
        }
        let hid_state = if let Some(hid) = self.hid.as_mut() {
            match hid.query_all() {
                Ok(s) => Some(s),
                Err(err) => {
                    if matches!(err, DeviceError::Disconnected) {
                        self.hid = None;
                    }
                    errs.push(format!("HID query: {err}"));
                    None
                }
            }
        } else {
            None
        };

        if self.uvc.is_none() {
            match open_default_uvc() {
                Ok(uvc) => self.uvc = Some(uvc),
                Err(err) => errs.push(format!("UVC: {err}")),
            }
        }
        let (uvc_caps, uvc_state) = if let Some(uvc) = self.uvc.as_mut() {
            let caps = uvc.capabilities().ok();
            match uvc.read_state() {
                Ok(st) => (caps, Some(st)),
                Err(err) => {
                    if matches!(err, DeviceError::Disconnected) {
                        self.uvc = None;
                    }
                    errs.push(format!("UVC read: {err}"));
                    (caps, None)
                }
            }
        } else {
            (None, None)
        };

        let mic_status = evaluate_mic_status(
            hid_state.as_ref().and_then(|h| h.mic),
            topo.has_audio_interface(),
        );

        self.snapshot = DeviceSnapshot {
            connected: true,
            topology: Some(topo),
            hid: hid_state,
            uvc_caps,
            uvc_state,
            mic_status,
            last_error: if errs.is_empty() {
                None
            } else {
                Some(errs.join(" | "))
            },
        };
        Ok(&self.snapshot)
    }

    pub fn set_hdr(&mut self, enabled: bool) -> Result<&DeviceSnapshot, DeviceError> {
        self.ensure_hid()?;
        let res = self.hid.as_mut().expect("ensured").set_hdr(enabled);
        self.handle_hid_write_result(res)?;
        self.refresh_hid_and_mic_status()
    }

    pub fn set_retouch(&mut self, enabled: bool) -> Result<&DeviceSnapshot, DeviceError> {
        self.ensure_hid()?;
        let res = self.hid.as_mut().expect("ensured").set_retouch(enabled);
        self.handle_hid_write_result(res)?;
        self.refresh_hid_and_mic_status()
    }

    pub fn set_mic(&mut self, enabled: bool) -> Result<&DeviceSnapshot, DeviceError> {
        self.ensure_hid()?;
        let res = self.hid.as_mut().expect("ensured").set_mic(enabled);
        self.handle_hid_write_result(res)?;
        self.refresh_hid_and_mic_status()
    }

    /// Sends HID `0xAE` Reset User Settings, then re-queries all HID settings & USB Audio
    /// interface topology to determine whether microphone settings require a replug.
    pub fn reset_hid_settings(&mut self) -> Result<&DeviceSnapshot, DeviceError> {
        self.ensure_hid()?;
        let res = self.hid.as_mut().expect("ensured").reset_user_settings();
        self.handle_hid_write_result(res)?;
        self.refresh_hid_and_mic_status()
    }

    pub fn apply_uvc(&mut self, update: UvcControlUpdate) -> Result<&DeviceSnapshot, DeviceError> {
        self.ensure_uvc()?;
        let res = self.uvc.as_mut().expect("ensured").apply(update);
        if let Err(err) = res {
            if matches!(err, DeviceError::Disconnected) {
                self.uvc = None;
            }
            return Err(err);
        }
        self.refresh_uvc_state()
    }

    pub fn reset_uvc_defaults(&mut self) -> Result<&DeviceSnapshot, DeviceError> {
        self.ensure_uvc()?;
        let res = self.uvc.as_mut().expect("ensured").reset_defaults();
        if let Err(err) = res {
            if matches!(err, DeviceError::Disconnected) {
                self.uvc = None;
            }
            return Err(err);
        }
        self.refresh_uvc_state()
    }

    /// Refreshes the privacy shutter state without doing full HID/USB enumeration.
    pub fn poll_privacy_shutter(&mut self) -> Option<bool> {
        let uvc = self.uvc.as_mut()?;
        match uvc.read_privacy_shutter() {
            Ok(Some(closed)) => {
                if let Some(st) = self.snapshot.uvc_state.as_mut() {
                    st.privacy_shutter = Some(closed);
                }
                Some(closed)
            }
            Ok(None) => None,
            Err(DeviceError::Disconnected) => {
                self.mark_disconnected();
                None
            }
            Err(_) => None,
        }
    }

    fn ensure_hid(&mut self) -> Result<(), DeviceError> {
        if self.hid.is_none() {
            self.hid = Some(HidClient::open()?);
        }
        Ok(())
    }

    fn ensure_uvc(&mut self) -> Result<(), DeviceError> {
        if self.uvc.is_none() {
            self.uvc = Some(open_default_uvc()?);
        }
        Ok(())
    }

    fn handle_hid_write_result(&mut self, res: Result<(), DeviceError>) -> Result<(), DeviceError> {
        if let Err(err) = res {
            if matches!(err, DeviceError::Disconnected) {
                self.hid = None;
            }
            return Err(err);
        }
        Ok(())
    }

    fn refresh_hid_and_mic_status(&mut self) -> Result<&DeviceSnapshot, DeviceError> {
        let hid_state = self.hid.as_mut().expect("hid").query_all()?;
        let topo = query_usb_topology()?;
        let has_audio = topo
            .as_ref()
            .is_some_and(UsbDeviceTopology::has_audio_interface);
        self.snapshot.mic_status = evaluate_mic_status(hid_state.mic, has_audio);
        self.snapshot.hid = Some(hid_state);
        if let Some(t) = topo {
            self.snapshot.topology = Some(t);
            self.snapshot.connected = true;
        }
        self.snapshot.last_error = None;
        Ok(&self.snapshot)
    }

    fn refresh_uvc_state(&mut self) -> Result<&DeviceSnapshot, DeviceError> {
        let uvc = self.uvc.as_mut().expect("uvc");
        let caps = uvc.capabilities().ok();
        let st = uvc.read_state()?;
        if let Some(c) = caps {
            self.snapshot.uvc_caps = Some(c);
        }
        self.snapshot.uvc_state = Some(st);
        self.snapshot.last_error = None;
        Ok(&self.snapshot)
    }
}
