//! Cross-platform USB interface topology inspection and hotplug monitoring using `nusb`.

use std::collections::HashSet;
use std::thread::JoinHandle;

use futures_lite::future;
use futures_lite::stream::StreamExt;
use moderncam_protocol::is_supported_device;
use moderncam_protocol::sync::{UsbInterfaceSummary, has_audio_interface};
use nusb::hotplug::HotplugEvent;
use nusb::{DeviceId, DeviceInfo, MaybeFuture};

use crate::error::DeviceError;

/// Enumerated USB metadata and active interface descriptors for the webcam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDeviceTopology {
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial_number: Option<String>,
    pub product_name: Option<String>,
    pub interfaces: Vec<UsbInterfaceSummary>,
}

impl UsbDeviceTopology {
    pub fn from_device_info(info: &DeviceInfo) -> Self {
        let mut interfaces: Vec<UsbInterfaceSummary> = info
            .interfaces()
            .map(|iface| UsbInterfaceSummary {
                number: iface.interface_number(),
                class: iface.class(),
                subclass: iface.subclass(),
            })
            .collect();
        interfaces.sort_by_key(|i| i.number);
        Self {
            vendor_id: info.vendor_id(),
            product_id: info.product_id(),
            serial_number: info.serial_number().map(ToOwned::to_owned),
            product_name: info.product_string().map(ToOwned::to_owned),
            interfaces,
        }
    }

    /// Returns `true` when USB Audio (`bInterfaceClass = 1`) is exposed in this enumeration.
    pub fn has_audio_interface(&self) -> bool {
        has_audio_interface(&self.interfaces)
    }
}

/// Hotplug notifications emitted for supported Modern Webcam devices.
#[derive(Debug, Clone)]
pub enum HotplugNotice {
    Connected(UsbDeviceTopology),
    Disconnected,
}

/// Queries the currently connected USB devices without opening/claiming any interfaces.
pub fn query_usb_topology() -> Result<Option<UsbDeviceTopology>, DeviceError> {
    let devices = nusb::list_devices()
        .wait()
        .map_err(|e| DeviceError::Usb(e.to_string()))?;

    for info in devices {
        if is_supported_device(info.vendor_id(), info.product_id()) {
            return Ok(Some(UsbDeviceTopology::from_device_info(&info)));
        }
    }
    Ok(None)
}

/// Handle keeping the USB hotplug background thread alive.
///
/// Note: `nusb::HotplugWatch` blocks waiting for OS events, so dropping this handle
/// detaches rather than blocking indefinitely; the OS cleans up on exit.
#[derive(Debug)]
pub struct HotplugWatcherHandle {
    _thread: JoinHandle<()>,
}

/// Spawns a background thread listening to USB connect/disconnect events via `nusb::watch_devices`.
pub fn spawn_hotplug_watcher<F>(mut on_notice: F) -> Result<HotplugWatcherHandle, DeviceError>
where
    F: FnMut(HotplugNotice) + Send + 'static,
{
    let mut tracked_ids: HashSet<DeviceId> = HashSet::new();
    if let Ok(devices) = nusb::list_devices().wait() {
        for info in devices {
            if is_supported_device(info.vendor_id(), info.product_id()) {
                tracked_ids.insert(info.id());
            }
        }
    }

    let mut watch = nusb::watch_devices().map_err(|e| DeviceError::Usb(e.to_string()))?;

    let thread = std::thread::Builder::new()
        .name("moderncam-usb-hotplug".into())
        .spawn(move || {
            future::block_on(async {
                while let Some(event) = watch.next().await {
                    match event {
                        HotplugEvent::Connected(info) => {
                            if is_supported_device(info.vendor_id(), info.product_id()) {
                                tracked_ids.insert(info.id());
                                let topo = UsbDeviceTopology::from_device_info(&info);
                                on_notice(HotplugNotice::Connected(topo));
                            }
                        }
                        HotplugEvent::Disconnected(id) => {
                            if tracked_ids.remove(&id) {
                                on_notice(HotplugNotice::Disconnected);
                            }
                        }
                    }
                }
            });
        })
        .map_err(|e| DeviceError::Usb(format!("failed to spawn hotplug thread: {e}")))?;

    Ok(HotplugWatcherHandle { _thread: thread })
}
