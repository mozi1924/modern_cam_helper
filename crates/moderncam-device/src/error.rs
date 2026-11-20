//! Typed errors for hardware discovery, HID/UVC transport, and verification.

use moderncam_protocol::ProtocolError;
use thiserror::Error;

/// Errors returned by cross-platform device discovery and control operations.
#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("Microsoft Modern Webcam (VID 0x045E, PID 0x0840) not found")]
    NotFound,

    #[error("permission denied accessing device: {0}")]
    PermissionDenied(String),

    #[error("device offline or disconnected")]
    Disconnected,

    #[error("operation timed out waiting for device response")]
    Timeout,

    #[error("HID transport error: {0}")]
    Hid(String),

    #[error("UVC control error: {0}")]
    Uvc(String),

    #[error("USB topology/hotplug error: {0}")]
    Usb(String),

    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    #[error("unsupported on this platform or device: {0}")]
    Unsupported(&'static str),
}
