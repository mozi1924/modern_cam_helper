//! Microsoft Modern Webcam proprietary HID protocol (`0x24` Feature -> `0x27` Input).

use std::fmt;

use crate::{CMD_REPORT_ID, HID_PAYLOAD_LEN, HID_REPORT_LEN, ProtocolError, RESP_REPORT_ID};

/// Known HID opcodes verified against Microsoft Modern Webcam (`PID 0x0840`).
///
/// IMPORTANT: Setter opcodes (`0x03`, `0x05`, `0x09`, `0xAE`) interpret an
/// argument count of `0x00` as writing `0`; never probe setters with `[op, 0x00]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HidOpcode {
    GetFirmwareVersion = 0x01,
    GetSerialNumber = 0x02,
    SetHdr = 0x03,
    GetHdr = 0x04,
    SetRetouch = 0x05,
    GetRetouch = 0x06,
    SetMic = 0x09,
    GetMic = 0x0A,
    GetColorName = 0x0B,
    GetCapabilities = 0x0C,
    GetHwInfo = 0x0E,
    ResetUserSettings = 0xAE,
}

impl HidOpcode {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Decoded firmware version (`[major, minor, build, rev]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FirmwareVersion(pub [u8; 4]);

impl fmt::Display for FirmwareVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d] = self.0;
        write!(f, "{a}.{b}.{c}.{d} ({a:02X} {b:02X} {c:02X} {d:02X})")
    }
}

/// Snapshot of all vendor HID properties stored on the webcam.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HidDeviceState {
    pub firmware: Option<FirmwareVersion>,
    pub serial: Option<String>,
    pub color_name: Option<String>,
    pub hdr: Option<bool>,
    pub retouch: Option<bool>,
    pub mic: Option<bool>,
}

/// Parsed `0x27` HID Input Report response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidResponse {
    pub echo: u8,
    pub status: u8,
    pub data: Vec<u8>,
}

impl HidResponse {
    pub fn is_ok(&self) -> bool {
        self.status == 0
    }

    /// Interprets the first payload byte as a boolean flag (`!= 0`).
    pub fn as_bool(&self) -> Result<bool, ProtocolError> {
        self.data
            .first()
            .copied()
            .map(|v| v != 0)
            .ok_or(ProtocolError::MalformedData(
                "expected 1-byte boolean payload",
            ))
    }

    /// Parses the 4-byte firmware version payload.
    pub fn as_firmware(&self) -> Result<FirmwareVersion, ProtocolError> {
        if self.data.len() < 4 {
            return Err(ProtocolError::MalformedData(
                "expected 4-byte firmware version",
            ));
        }
        Ok(FirmwareVersion([
            self.data[0],
            self.data[1],
            self.data[2],
            self.data[3],
        ]))
    }

    /// Decodes a NUL/whitespace-padded ASCII string payload.
    pub fn as_ascii_trimmed(&self) -> String {
        parse_ascii_trimmed(&self.data)
    }
}

/// Encodes a raw payload (excluding Report ID) into a 64-byte Feature Report `0x24` frame.
pub fn encode_feature_report(payload: &[u8]) -> Result<[u8; HID_REPORT_LEN], ProtocolError> {
    if payload.is_empty() || payload.len() > HID_PAYLOAD_LEN {
        return Err(ProtocolError::InvalidPayloadLength(payload.len()));
    }
    let mut frame = [0u8; HID_REPORT_LEN];
    frame[0] = CMD_REPORT_ID;
    frame[1..1 + payload.len()].copy_from_slice(payload);
    Ok(frame)
}

/// Builds `[op, 0x00]` payload for query opcodes.
pub const fn encode_query_payload(op: HidOpcode) -> [u8; 2] {
    [op.as_u8(), 0x00]
}

/// Builds `[op, 0x01, val]` payload for 1-byte boolean setter opcodes.
pub const fn encode_bool_setter_payload(op: HidOpcode, enabled: bool) -> [u8; 3] {
    [op.as_u8(), 0x01, if enabled { 1 } else { 0 }]
}

/// Builds `[0xAE, 0x01, 0x01]` payload for resetting HID user settings.
pub const fn encode_reset_payload() -> [u8; 3] {
    [HidOpcode::ResetUserSettings.as_u8(), 0x01, 0x01]
}

/// Parses a `0x27` Input Report buffer `[0x27, echo, status, len, data...]`.
pub fn parse_input_report(buf: &[u8]) -> Result<HidResponse, ProtocolError> {
    if buf.len() < 4 {
        return Err(ProtocolError::ReportTooShort(buf.len()));
    }
    if buf[0] != RESP_REPORT_ID {
        return Err(ProtocolError::UnexpectedReportId(buf[0]));
    }
    let echo = buf[1];
    let status = buf[2];
    let declared_len = buf[3] as usize;
    let available = buf.len() - 4;
    if declared_len > available {
        return Err(ProtocolError::TruncatedResponseData {
            declared: declared_len,
            available,
        });
    }
    let data = buf[4..4 + declared_len].to_vec();
    Ok(HidResponse { echo, status, data })
}

/// Decodes a byte slice as printable ASCII, trimming trailing NUL bytes and whitespace.
pub fn parse_ascii_trimmed(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_feature_report_with_leading_id_and_zero_padding() {
        let frame = encode_feature_report(&[0x03, 0x01, 0x01]).unwrap();
        assert_eq!(frame.len(), 64);
        assert_eq!(frame[0], 0x24);
        assert_eq!(&frame[1..4], &[0x03, 0x01, 0x01]);
        assert!(frame[4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn rejects_invalid_payload_lengths() {
        assert_eq!(
            encode_feature_report(&[]),
            Err(ProtocolError::InvalidPayloadLength(0))
        );
        let too_long = [0u8; 64];
        assert_eq!(
            encode_feature_report(&too_long),
            Err(ProtocolError::InvalidPayloadLength(64))
        );
    }

    #[test]
    fn parses_real_firmware_and_serial_responses() {
        let mut fw_buf = [0u8; 64];
        fw_buf[..8].copy_from_slice(&[0x27, 0x01, 0x00, 0x04, 0x0B, 0x15, 0x00, 0x02]);
        let resp = parse_input_report(&fw_buf).unwrap();
        assert!(resp.is_ok());
        assert_eq!(resp.echo, 0x01);
        let fw = resp.as_firmware().unwrap();
        assert_eq!(fw.to_string(), "11.21.0.2 (0B 15 00 02)");

        let mut sn_buf = [0u8; 64];
        sn_buf[..4].copy_from_slice(&[0x27, 0x02, 0x00, 0x0E]);
        sn_buf[4..18].copy_from_slice(b"0C33348221101N");
        let sn_resp = parse_input_report(&sn_buf).unwrap();
        assert_eq!(sn_resp.as_ascii_trimmed(), "0C33348221101N");
    }

    #[test]
    fn parses_bool_and_reset_responses() {
        let hdr_on = [0x27, 0x04, 0x00, 0x01, 0x01];
        let resp = parse_input_report(&hdr_on).unwrap();
        assert!(resp.as_bool().unwrap());

        let reset_ok = [0x27, 0xAE, 0x00, 0x00];
        let resp = parse_input_report(&reset_ok).unwrap();
        assert!(resp.is_ok());
        assert!(resp.data.is_empty());
    }

    #[test]
    fn rejects_malformed_input_reports() {
        assert_eq!(
            parse_input_report(&[0x27, 0x01]),
            Err(ProtocolError::ReportTooShort(2))
        );
        assert_eq!(
            parse_input_report(&[0x9B, 0x01, 0x00, 0x01]),
            Err(ProtocolError::UnexpectedReportId(0x9B))
        );
        assert_eq!(
            parse_input_report(&[0x27, 0x01, 0x00, 0x10, 0x01]),
            Err(ProtocolError::TruncatedResponseData {
                declared: 16,
                available: 1
            })
        );
    }
}
