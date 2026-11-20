//! Cross-platform standard UVC camera control transport.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

use moderncam_protocol::uvc::{UvcCapabilities, UvcControlUpdate, UvcState};

use crate::error::DeviceError;

/// Platform abstraction for reading/writing UVC Camera Terminal & Processing Unit controls
/// concurrently with live video capture.
pub trait UvcTransport: Send {
    fn capabilities(&mut self) -> Result<UvcCapabilities, DeviceError>;
    fn read_state(&mut self) -> Result<UvcState, DeviceError>;
    fn apply(&mut self, update: UvcControlUpdate) -> Result<(), DeviceError>;
    fn reset_defaults(&mut self) -> Result<(), DeviceError>;

    /// Lightweight read of only the physical privacy shutter state (when supported).
    fn read_privacy_shutter(&mut self) -> Result<Option<bool>, DeviceError> {
        Ok(self.read_state()?.privacy_shutter)
    }
}

/// Opens the platform-appropriate UVC control transport for Microsoft Modern Webcam.
pub fn open_default_uvc() -> Result<Box<dyn UvcTransport>, DeviceError> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::MacOsUvcTransport::open()?))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(linux::LinuxUvcTransport::open()?))
    }
    #[cfg(target_os = "windows")]
    {
        Ok(Box::new(windows::WindowsUvcTransport::open()?))
    }
}
