//! USB interface topology inspection and microphone / reset re-enumeration state machine.
//!
//! Core invariant:
//! `mic_pending_replug = (hid_mic_enabled != usb_audio_interface_present)`
//!
//! Because firmware `0x09` (mic toggle) and `0xAE` (reset user settings) do not trigger a
//! USB bus reset themselves, while the microphone toggle alters the USB descriptor set
//! (interfaces `#3/#4, class=1 Audio`), any mismatch between the persisted HID mic flag
//! and the currently enumerated USB interfaces requires one physical unplug/replug.

/// Summary of one USB interface from the active configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UsbInterfaceSummary {
    pub number: u8,
    pub class: u8,
    pub subclass: u8,
}

/// Human-readable USB interface class name.
pub const fn interface_class_name(class: u8) -> &'static str {
    match class {
        1 => "Audio",
        2 => "CDC",
        3 => "HID",
        8 => "MassStorage",
        14 => "Video",
        0xEF => "Misc/IAD",
        _ => "Other",
    }
}

/// Returns `true` if any USB Audio interface (`bInterfaceClass == 1`) is currently enumerated.
pub fn has_audio_interface(interfaces: &[UsbInterfaceSummary]) -> bool {
    interfaces.iter().any(|iface| iface.class == 1)
}

/// Consistency state between the persisted HID mic setting and active USB Audio descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MicActivationStatus {
    /// HID mic is ON and USB Audio interfaces (`#3/#4`) are present.
    ActiveSynced,
    /// HID mic is OFF and USB Audio interfaces are absent.
    InactiveSynced,
    /// HID mic is ON in flash, but USB Audio interfaces are not yet enumerated (replug needed).
    PendingEnableReplug,
    /// HID mic is OFF in flash, but USB Audio interfaces are still exposed (replug needed).
    PendingDisableReplug,
    /// Could not determine both sides yet.
    #[default]
    Unknown,
}

impl MicActivationStatus {
    pub const fn needs_replug(self) -> bool {
        matches!(self, Self::PendingEnableReplug | Self::PendingDisableReplug)
    }

    pub const fn is_synced(self) -> bool {
        matches!(self, Self::ActiveSynced | Self::InactiveSynced)
    }

    pub const fn label_zh(self) -> &'static str {
        match self {
            Self::ActiveSynced => "已生效",
            Self::InactiveSynced => "已关闭",
            Self::PendingEnableReplug => "待插拔启用",
            Self::PendingDisableReplug => "待插拔关闭",
            Self::Unknown => "未知",
        }
    }
}

/// Evaluates `MicActivationStatus` from the HID `mic` flag and USB Audio interface presence.
pub const fn evaluate_mic_status(
    hid_mic: Option<bool>,
    has_audio_iface: bool,
) -> MicActivationStatus {
    match (hid_mic, has_audio_iface) {
        (Some(true), true) => MicActivationStatus::ActiveSynced,
        (Some(false), false) => MicActivationStatus::InactiveSynced,
        (Some(true), false) => MicActivationStatus::PendingEnableReplug,
        (Some(false), true) => MicActivationStatus::PendingDisableReplug,
        (None, _) => MicActivationStatus::Unknown,
    }
}

/// Reason why a physical USB replug is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplugReason {
    MicToggled,
    ResetChangedMic,
    PreExistingMismatch,
}

/// Tracks the guided physical unplug/replug lifecycle for GUI & CLI feedback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplugPhase {
    #[default]
    Idle,
    NeedsReplug {
        target_mic: bool,
        reason: ReplugReason,
    },
    DeviceRemovedWaitingReturn {
        target_mic: bool,
        reason: ReplugReason,
    },
    JustSynced {
        mic_enabled: bool,
    },
}

/// Deterministic state machine tracking `mic` / `reset` vs hotplug disconnect/reconnect.
#[derive(Debug, Clone, Default)]
pub struct ReplugStateMachine {
    phase: ReplugPhase,
}

impl ReplugStateMachine {
    pub fn phase(&self) -> ReplugPhase {
        self.phase
    }

    /// Updates state after querying/applying HID settings and inspecting USB interfaces.
    pub fn on_snapshot(
        &mut self,
        hid_mic: Option<bool>,
        has_audio_iface: bool,
        reason_hint: ReplugReason,
    ) {
        let status = evaluate_mic_status(hid_mic, has_audio_iface);
        match (status, self.phase) {
            (MicActivationStatus::PendingEnableReplug, _) => {
                self.phase = ReplugPhase::NeedsReplug {
                    target_mic: true,
                    reason: self.carry_reason(reason_hint),
                };
            }
            (MicActivationStatus::PendingDisableReplug, _) => {
                self.phase = ReplugPhase::NeedsReplug {
                    target_mic: false,
                    reason: self.carry_reason(reason_hint),
                };
            }
            (MicActivationStatus::ActiveSynced, ReplugPhase::DeviceRemovedWaitingReturn { .. })
            | (MicActivationStatus::ActiveSynced, ReplugPhase::NeedsReplug { .. }) => {
                self.phase = ReplugPhase::JustSynced { mic_enabled: true };
            }
            (
                MicActivationStatus::InactiveSynced,
                ReplugPhase::DeviceRemovedWaitingReturn { .. },
            )
            | (MicActivationStatus::InactiveSynced, ReplugPhase::NeedsReplug { .. }) => {
                self.phase = ReplugPhase::JustSynced { mic_enabled: false };
            }
            (MicActivationStatus::ActiveSynced | MicActivationStatus::InactiveSynced, _) => {
                self.phase = ReplugPhase::Idle;
            }
            (MicActivationStatus::Unknown, _) => {}
        }
    }

    /// Marks that the device was physically unplugged while awaiting replug.
    pub fn on_device_disconnected(&mut self) {
        if let ReplugPhase::NeedsReplug { target_mic, reason } = self.phase {
            self.phase = ReplugPhase::DeviceRemovedWaitingReturn { target_mic, reason };
        }
    }

    /// Clears the transient `JustSynced` state back to `Idle` once the UI has displayed a toast.
    pub fn acknowledge_synced(&mut self) {
        if matches!(self.phase, ReplugPhase::JustSynced { .. }) {
            self.phase = ReplugPhase::Idle;
        }
    }

    fn carry_reason(&self, fallback: ReplugReason) -> ReplugReason {
        match self.phase {
            ReplugPhase::NeedsReplug { reason, .. }
            | ReplugPhase::DeviceRemovedWaitingReturn { reason, .. } => reason,
            _ => fallback,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_audio_interface_from_usb_summaries() {
        let without_audio = vec![
            UsbInterfaceSummary {
                number: 0,
                class: 14,
                subclass: 1,
            },
            UsbInterfaceSummary {
                number: 1,
                class: 14,
                subclass: 2,
            },
            UsbInterfaceSummary {
                number: 2,
                class: 3,
                subclass: 0,
            },
        ];
        assert!(!has_audio_interface(&without_audio));

        let mut with_audio = without_audio.clone();
        with_audio.push(UsbInterfaceSummary {
            number: 3,
            class: 1,
            subclass: 1,
        });
        assert!(has_audio_interface(&with_audio));
    }

    #[test]
    fn replug_state_machine_handles_mic_toggle_and_replug_cycle() {
        let mut sm = ReplugStateMachine::default();

        // Initially synced ON.
        sm.on_snapshot(Some(true), true, ReplugReason::PreExistingMismatch);
        assert_eq!(sm.phase(), ReplugPhase::Idle);

        // User toggles mic OFF -> Audio interface is still enumerated -> needs replug.
        sm.on_snapshot(Some(false), true, ReplugReason::MicToggled);
        assert_eq!(
            sm.phase(),
            ReplugPhase::NeedsReplug {
                target_mic: false,
                reason: ReplugReason::MicToggled,
            }
        );

        // User unplugs device.
        sm.on_device_disconnected();
        assert_eq!(
            sm.phase(),
            ReplugPhase::DeviceRemovedWaitingReturn {
                target_mic: false,
                reason: ReplugReason::MicToggled,
            }
        );

        // User plugs device back in -> Audio interface gone, mic=false -> synced!
        sm.on_snapshot(Some(false), false, ReplugReason::PreExistingMismatch);
        assert_eq!(sm.phase(), ReplugPhase::JustSynced { mic_enabled: false });

        sm.acknowledge_synced();
        assert_eq!(sm.phase(), ReplugPhase::Idle);
    }

    #[test]
    fn reset_changing_mic_triggers_replug_even_without_mic_button() {
        let mut sm = ReplugStateMachine::default();
        // Reset ran while Audio was absent, and reset restored mic=true in flash.
        sm.on_snapshot(Some(true), false, ReplugReason::ResetChangedMic);
        assert_eq!(
            sm.phase(),
            ReplugPhase::NeedsReplug {
                target_mic: true,
                reason: ReplugReason::ResetChangedMic,
            }
        );
    }
}
