//! Cross-platform `hidapi` backend used on Linux (`hidraw`) and Windows (Win32 HID).

use std::thread;
use std::time::{Duration, Instant};

use hidapi::{HidApi, HidDevice};
use moderncam_protocol::{
    CONTROL_USAGE, CONTROL_USAGE_PAGE, HID_REPORT_LEN, MICROSOFT_VID, MODERN_WEBCAM_PID,
    RESP_REPORT_ID,
};

use crate::error::DeviceError;
use crate::hid::HidTransport;

/// HID transport backed by `hidapi`.
pub struct HidApiTransport {
    device: HidDevice,
}

impl HidApiTransport {
    pub fn open() -> Result<Self, DeviceError> {
        let api = HidApi::new().map_err(|e| DeviceError::Hid(e.to_string()))?;

        // Prefer the exact vendor control TLC (0xFF07:0x0212) required on Windows
        // where multi-collection descriptors create separate HID device paths.
        let mut preferred = None;
        let mut fallback = None;

        for info in api.device_list() {
            if info.vendor_id() != MICROSOFT_VID || info.product_id() != MODERN_WEBCAM_PID {
                continue;
            }
            if info.usage_page() == CONTROL_USAGE_PAGE && info.usage() == CONTROL_USAGE {
                preferred = Some(info.path());
                break;
            }
            if fallback.is_none() {
                fallback = Some(info.path());
            }
        }

        let path = preferred.or(fallback).ok_or(DeviceError::NotFound)?;
        let device = api
            .open_path(path)
            .map_err(|e| DeviceError::Hid(format!("failed to open HID path: {e}")))?;

        Ok(Self { device })
    }
}

impl HidTransport for HidApiTransport {
    fn send_feature_and_wait_input(
        &mut self,
        frame: &[u8; HID_REPORT_LEN],
        expected_echo: u8,
        timeout: Duration,
    ) -> Result<Vec<u8>, DeviceError> {
        // Drain any stale pending input reports.
        let mut drain_buf = [0u8; HID_REPORT_LEN];
        while let Ok(n) = self.device.read_timeout(&mut drain_buf, 0) {
            if n == 0 {
                break;
            }
        }

        if let Err(first_err) = self.device.send_feature_report(frame) {
            thread::sleep(Duration::from_millis(50));
            self.device.send_feature_report(frame).map_err(|e| {
                DeviceError::Hid(format!("send_feature_report failed ({first_err}): {e}"))
            })?;
        }

        let deadline = Instant::now() + timeout;
        let mut buf = [0u8; HID_REPORT_LEN];
        while Instant::now() < deadline {
            let remaining_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .clamp(1, 50) as i32;
            match self.device.read_timeout(&mut buf, remaining_ms) {
                Ok(n) if n >= 4 && buf[0] == RESP_REPORT_ID && buf[1] == expected_echo => {
                    return Ok(buf[..n].to_vec());
                }
                Ok(_) => continue,
                Err(e) => return Err(DeviceError::Hid(format!("read_timeout failed: {e}"))),
            }
        }

        Err(DeviceError::Timeout)
    }
}
