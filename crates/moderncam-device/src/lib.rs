//! Cross-platform hardware control layer for Microsoft Modern Webcam.
//!
//! Layers three complementary transport mechanisms so camera control works
//! concurrently with live video streaming and without root/driver replacement:
//! - USB topology & hotplug monitoring via `nusb` (all platforms)
//! - Proprietary HID (`0x24` Feature / `0x27` Input) via targeted `IOHIDManager`
//!   on macOS and `hidapi` on Linux/Windows
//! - Standard UVC controls via EP0 `IOUSBDeviceInterface182` on macOS,
//!   `v4l` on Linux, and DirectShow via `cameras` (`controls`) on Windows.

pub mod controller;
pub mod error;
pub mod hid;
pub mod topology;
pub mod uvc;

pub use controller::{DeviceSnapshot, ModernCamController};
pub use error::DeviceError;
pub use topology::{HotplugNotice, UsbDeviceTopology, query_usb_topology, spawn_hotplug_watcher};
