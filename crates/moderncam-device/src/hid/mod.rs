//! Cross-platform HID transport (`0x24` Feature Report / `0x27` Input Report)
//! and high-level Modern Webcam HID command client.

#[cfg(not(target_os = "macos"))]
mod hidapi_backend;
#[cfg(target_os = "macos")]
mod macos;

use std::thread;
use std::time::Duration;

use moderncam_protocol::HID_REPORT_LEN;
use moderncam_protocol::hid::{
    HidDeviceState, HidOpcode, HidResponse, encode_bool_setter_payload, encode_feature_report,
    encode_query_payload, encode_reset_payload, parse_input_report,
};

use crate::error::DeviceError;

const DEFAULT_CMD_TIMEOUT: Duration = Duration::from_secs(3);

/// Platform-specific raw HID transport for Feature `0x24` / Input `0x27` exchange.
pub trait HidTransport: Send {
    /// Sends a 64-byte Feature Report (`frame[0] == 0x24`) and waits for the
    /// matching Input Report `0x27` response.
    fn send_feature_and_wait_input(
        &mut self,
        frame: &[u8; HID_REPORT_LEN],
        expected_echo: u8,
        timeout: Duration,
    ) -> Result<Vec<u8>, DeviceError>;
}

/// Opens the platform-appropriate HID transport for Microsoft Modern Webcam.
pub fn open_default_hid() -> Result<Box<dyn HidTransport>, DeviceError> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(macos::MacOsHidTransport::open()?))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(Box::new(hidapi_backend::HidApiTransport::open()?))
    }
}

/// High-level proprietary HID command client wrapping any [`HidTransport`].
pub struct HidClient {
    transport: Box<dyn HidTransport>,
}

impl std::fmt::Debug for HidClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HidClient").finish_non_exhaustive()
    }
}

impl HidClient {
    pub fn open() -> Result<Self, DeviceError> {
        Ok(Self {
            transport: open_default_hid()?,
        })
    }

    pub fn from_transport(transport: Box<dyn HidTransport>) -> Self {
        Self { transport }
    }

    /// Sends a command payload (without Report ID `0x24`) and parses the `0x27` response.
    pub fn command(
        &mut self,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<HidResponse, DeviceError> {
        let frame = encode_feature_report(payload)?;
        let expected_echo = payload[0];
        let raw = self
            .transport
            .send_feature_and_wait_input(&frame, expected_echo, timeout)?;
        let resp = parse_input_report(&raw)?;
        if resp.echo != expected_echo {
            return Err(moderncam_protocol::ProtocolError::EchoMismatch {
                expected: expected_echo,
                actual: resp.echo,
            }
            .into());
        }
        if !resp.is_ok() {
            return Err(moderncam_protocol::ProtocolError::DeviceStatusError {
                opcode: expected_echo,
                status: resp.status,
            }
            .into());
        }
        Ok(resp)
    }

    pub fn query_raw(&mut self, op: HidOpcode) -> Result<HidResponse, DeviceError> {
        let payload = encode_query_payload(op);
        self.command(&payload, DEFAULT_CMD_TIMEOUT)
    }

    pub fn query_bool(&mut self, op: HidOpcode) -> Result<bool, DeviceError> {
        self.query_raw(op)?.as_bool().map_err(Into::into)
    }

    /// Queries firmware version, serial number, hardware color, HDR, Retouch, and Mic.
    pub fn query_all(&mut self) -> Result<HidDeviceState, DeviceError> {
        let firmware = self
            .query_raw(HidOpcode::GetFirmwareVersion)
            .and_then(|r| r.as_firmware().map_err(Into::into))
            .ok();
        let serial = self
            .query_raw(HidOpcode::GetSerialNumber)
            .map(|r| r.as_ascii_trimmed())
            .ok();
        let color_name = self
            .query_raw(HidOpcode::GetColorName)
            .map(|r| r.as_ascii_trimmed())
            .ok();
        let hdr = self.query_bool(HidOpcode::GetHdr).ok();
        let retouch = self.query_bool(HidOpcode::GetRetouch).ok();
        let mic = self.query_bool(HidOpcode::GetMic).ok();

        if firmware.is_none()
            && serial.is_none()
            && hdr.is_none()
            && retouch.is_none()
            && mic.is_none()
        {
            return Err(DeviceError::Hid(
                "all HID queries failed for Modern Webcam".into(),
            ));
        }

        Ok(HidDeviceState {
            firmware,
            serial,
            color_name,
            hdr,
            retouch,
            mic,
        })
    }

    pub fn set_hdr(&mut self, enabled: bool) -> Result<(), DeviceError> {
        self.set_bool_and_confirm(HidOpcode::SetHdr, HidOpcode::GetHdr, enabled)
    }

    pub fn set_retouch(&mut self, enabled: bool) -> Result<(), DeviceError> {
        self.set_bool_and_confirm(HidOpcode::SetRetouch, HidOpcode::GetRetouch, enabled)
    }

    pub fn set_mic(&mut self, enabled: bool) -> Result<(), DeviceError> {
        self.set_bool_and_confirm(HidOpcode::SetMic, HidOpcode::GetMic, enabled)
    }

    /// Sends HID `0xAE` (Reset User Settings).
    ///
    /// Does not reboot USB on Modern Webcam; caller must re-query HID state and
    /// inspect USB Audio interfaces to detect whether `mic` changed.
    pub fn reset_user_settings(&mut self) -> Result<(), DeviceError> {
        let payload = encode_reset_payload();
        self.command(&payload, DEFAULT_CMD_TIMEOUT)?;
        Ok(())
    }

    fn set_bool_and_confirm(
        &mut self,
        set_op: HidOpcode,
        get_op: HidOpcode,
        enabled: bool,
    ) -> Result<(), DeviceError> {
        let payload = encode_bool_setter_payload(set_op, enabled);
        self.command(&payload, DEFAULT_CMD_TIMEOUT)?;

        // Poll readback briefly to confirm persisted value matches.
        for _ in 0..8 {
            if let Ok(actual) = self.query_bool(get_op)
                && actual == enabled
            {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(80));
        }
        Err(DeviceError::Hid(format!(
            "readback confirmation failed after setting {set_op:?} -> {enabled}"
        )))
    }
}
