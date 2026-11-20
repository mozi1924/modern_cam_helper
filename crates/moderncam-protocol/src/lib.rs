//! Pure-Rust protocol definitions, USB/UVC descriptor parsers, and
//! microphone/reset re-enumeration state machines for Microsoft Modern Webcam.

pub mod hid;
pub mod sync;
pub mod uvc;

use thiserror::Error;

/// Microsoft USB Vendor ID.
pub const MICROSOFT_VID: u16 = 0x045E;

/// Microsoft Modern Webcam (1080p) USB Product ID.
pub const MODERN_WEBCAM_PID: u16 = 0x0840;

/// Microsoft 4K Modern Webcam USB Product ID (reserved for future extension).
pub const MODERN_WEBCAM_4K_PID: u16 = 0x085D;

/// Supported webcam product IDs.
pub const SUPPORTED_PIDS: &[u16] = &[MODERN_WEBCAM_PID];

/// HID top-level collection Usage Page for Accessory Center control channel.
pub const CONTROL_USAGE_PAGE: u16 = 0xFF07;

/// HID top-level collection Usage ID for Modern Webcam control channel.
pub const CONTROL_USAGE: u16 = 0x0212;

/// Feature Report ID used to send commands to the webcam.
pub const CMD_REPORT_ID: u8 = 0x24;

/// Input Report ID used by the webcam to push asynchronous command responses.
pub const RESP_REPORT_ID: u8 = 0x27;

/// Total HID packet size on the wire (1 byte report ID + 63 bytes payload).
pub const HID_REPORT_LEN: usize = 64;

/// Maximum command payload length excluding the leading Report ID byte.
pub const HID_PAYLOAD_LEN: usize = 63;

/// Errors produced while encoding, decoding, or validating protocol packets.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("payload length {0} invalid (expected 1..={HID_PAYLOAD_LEN})")]
    InvalidPayloadLength(usize),

    #[error("input report too short ({0} bytes, need at least 4)")]
    ReportTooShort(usize),

    #[error("unexpected input report id 0x{0:02X} (expected 0x{RESP_REPORT_ID:02X})")]
    UnexpectedReportId(u8),

    #[error("command echo mismatch: expected 0x{expected:02X}, got 0x{actual:02X}")]
    EchoMismatch { expected: u8, actual: u8 },

    #[error("device reported failure status 0x{status:02X} for opcode 0x{opcode:02X}")]
    DeviceStatusError { opcode: u8, status: u8 },

    #[error("response data length {declared} exceeds available bytes {available}")]
    TruncatedResponseData { declared: usize, available: usize },

    #[error("malformed response data: {0}")]
    MalformedData(&'static str),
}

/// Checks whether a `(vid, pid)` pair corresponds to a supported webcam model.
pub fn is_supported_device(vid: u16, pid: u16) -> bool {
    vid == MICROSOFT_VID && SUPPORTED_PIDS.contains(&pid)
}
