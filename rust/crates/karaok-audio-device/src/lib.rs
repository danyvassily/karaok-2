//! Unified macOS Core Audio HAL / AUHAL device enumeration, hotplug listeners, and route policy (§5, ADR-0002).
//!
//! Guarantees:
//! - Enumerates ALL inputs and outputs exposed by Core Audio HAL separately with real UIDs,
//!   channels, sample rates, buffer ranges, latencies, and alive state. Unknown properties remain `None`.
//! - NEVER modifies macOS global default devices.
//! - Resolves `DeviceSelector::SystemDefault` dynamically vs `DeviceSelector::Fixed { uid }`.
//! - When a `Fixed(uid)` preferred mic (e.g. Nothing Headphone (a) USB-C) is absent or unplugged,
//!   returns `RouteStatus::UnavailablePreferred` and NEVER silently falls back to the Mac built-in mic!
//! - On reconnect of a lost fixed device, enters `RouteStatus::AwaitingReconnectConfirmation`.
//! - Provides `SimulatedHalBackend` explicitly labeled `is_simulated = true` for deterministic hotplug testing.

use crossbeam_channel::{Receiver, Sender};
use karaok_audio_engine::{RtMicInputHandle, RtOutputProcessor};
use karaok_types::{
    redact_device_uid, ActiveStreamFormat, AudioDeviceInfo, DeviceSelector, ErrorCode, KaraokError,
    KaraokResult, RouteStatus, TransportType,
};
use std::sync::{Arc, Mutex};

#[cfg(target_os = "macos")]
pub mod macos_hal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HalChangeSignal {
    DevicesListChanged,
    DefaultInputChanged,
    DefaultOutputChanged,
    DevicePropertyChanged,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteResolution {
    pub status: RouteStatus,
    pub device: Option<AudioDeviceInfo>,
    pub message_fr: Option<String>,
}

/// Pure, deterministic route resolution policy (§5.1, §5.5, AC-03, AC-04).
/// Separated from hardware I/O so every edge case (Nothing USB-C unplug/replug, rename, default change)
/// is exhaustively testable!
pub fn resolve_input_route(
    selector: &DeviceSelector,
    devices: &[AudioDeviceInfo],
    was_lost_before: bool,
) -> RouteResolution {
    let inputs: Vec<&AudioDeviceInfo> = devices.iter().filter(|d| d.has_input() && d.is_alive).collect();
    match selector {
        DeviceSelector::SystemDefault => {
            if let Some(dev) = inputs.iter().find(|d| d.is_default_input).copied().or_else(|| inputs.first().copied()) {
                RouteResolution {
                    status: RouteStatus::Ready,
                    device: Some(dev.clone()),
                    message_fr: None,
                }
            } else {
                RouteResolution {
                    status: RouteStatus::UnavailablePreferred,
                    device: None,
                    message_fr: Some("Aucune entrée microphone disponible sur ce Mac.".to_string()),
                }
            }
        }
        DeviceSelector::Fixed {
            uid,
            last_known_name,
            ..
        } => {
            if let Some(dev) = inputs.iter().find(|d| d.uid == *uid).copied() {
                if was_lost_before {
                    RouteResolution {
                        status: RouteStatus::AwaitingReconnectConfirmation,
                        device: Some(dev.clone()),
                        message_fr: Some(format!(
                            "« {} » est de nouveau connecté. Confirmez la reprise avant de réactiver le micro.",
                            dev.name
                        )),
                    }
                } else {
                    RouteResolution {
                        status: RouteStatus::Ready,
                        device: Some(dev.clone()),
                        message_fr: None,
                    }
                }
            } else {
                // INVARIANT AC-04: NEVER silently fall back to Mac built-in mic!
                RouteResolution {
                    status: RouteStatus::UnavailablePreferred,
                    device: None,
                    message_fr: Some(format!(
                        "Micro préféré indisponible : « {last_known_name} ». Aucun basculement automatique vers le micro du Mac n'a été effectué ; choisissez une entrée de secours ou rebranchez votre casque."
                    )),
                }
            }
        }
    }
}

/// Pure route resolution for music/mix output (§5.5).
pub fn resolve_output_route(
    selector: &DeviceSelector,
    devices: &[AudioDeviceInfo],
) -> RouteResolution {
    let outputs: Vec<&AudioDeviceInfo> = devices.iter().filter(|d| d.has_output() && d.is_alive).collect();
    match selector {
        DeviceSelector::SystemDefault => {
            if let Some(dev) = outputs.iter().find(|d| d.is_default_output).copied().or_else(|| outputs.first().copied()) {
                RouteResolution {
                    status: RouteStatus::Ready,
                    device: Some(dev.clone()),
                    message_fr: None,
                }
            } else {
                RouteResolution {
                    status: RouteStatus::UnavailablePreferred,
                    device: None,
                    message_fr: Some("Aucune sortie audio disponible sur ce Mac.".to_string()),
                }
            }
        }
        DeviceSelector::Fixed {
            uid,
            last_known_name,
            ..
        } => {
            if let Some(dev) = outputs.iter().find(|d| d.uid == *uid).copied() {
                RouteResolution {
                    status: RouteStatus::Ready,
                    device: Some(dev.clone()),
                    message_fr: None,
                }
            } else {
                RouteResolution {
                    status: RouteStatus::UnavailablePreferred,
                    device: None,
                    message_fr: Some(format!(
                        "Sortie préférée indisponible : « {last_known_name} ». Lecture en pause ; sélectionnez une sortie pour reprendre (aucun basculement surprise sur les haut-parleurs)."
                    )),
                }
            }
        }
    }
}

/// Trait representing a running audio stream that cleanly stops and releases resources on drop.
pub trait ActiveStreamHandle: Send {
    fn format(&self) -> ActiveStreamFormat;
}

/// Trait representing an active HAL change listener guard supporting dynamic hotplug device sync (P2-I2).
pub trait HalSubscription: Send {
    /// Synchronize per-device property listeners when the OS device list changes (hotplug add/remove).
    fn sync_device_listeners(&mut self) {}
}

pub struct NoopHalSubscription;
impl HalSubscription for NoopHalSubscription {}

/// Backend abstraction isolating Core Audio HAL hardware calls from deterministic test mocks.
pub trait AudioHalBackend: Send + Sync {
    fn enumerate_devices(&self) -> KaraokResult<Vec<AudioDeviceInfo>>;

    fn open_output_stream(
        &self,
        device_uid: &str,
        processor: RtOutputProcessor,
    ) -> KaraokResult<Box<dyn ActiveStreamHandle>>;

    /// Open input stream, returning the `RtMicInputHandle` back into `mic_return_slot` when dropped
    /// (after the audio unit has fully stopped) so toggling mic never tears down playback (B6).
    fn open_input_stream(
        &self,
        device_uid: &str,
        mic_handle: RtMicInputHandle,
        mic_return_slot: Arc<Mutex<Option<RtMicInputHandle>>>,
    ) -> KaraokResult<Box<dyn ActiveStreamHandle>>;

    fn subscribe_changes(
        &self,
    ) -> KaraokResult<(Receiver<HalChangeSignal>, Box<dyn HalSubscription>)>;
}

/// Create the real macOS Core Audio HAL backend on macOS targets.
#[cfg(target_os = "macos")]
pub fn default_system_backend() -> Arc<dyn AudioHalBackend> {
    Arc::new(macos_hal::MacOsCoreAudioBackend::new())
}

/// Explicitly labeled simulated HAL backend for deterministic tests (NEVER presented as physical hardware).
pub struct SimulatedHalBackend {
    devices: Arc<Mutex<Vec<AudioDeviceInfo>>>,
    listeners: Arc<Mutex<Vec<Sender<HalChangeSignal>>>>,
}

impl SimulatedHalBackend {
    pub fn new(initial_devices: Vec<AudioDeviceInfo>) -> Self {
        Self {
            devices: Arc::new(Mutex::new(initial_devices)),
            listeners: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn set_devices(&self, devices: Vec<AudioDeviceInfo>, signal: HalChangeSignal) {
        if let Ok(mut g) = self.devices.lock() {
            *g = devices;
        }
        if let Ok(listeners) = self.listeners.lock() {
            for tx in listeners.iter() {
                let _ = tx.try_send(signal);
            }
        }
    }

    /// Standard simulated test fixture mirroring the documented Nothing Headphone (a) USB-C profile (§1.1).
    pub fn fixture_with_nothing_usb_and_builtin() -> Vec<AudioDeviceInfo> {
        let nothing_in_uid = "sim-usb-nothing-headphone-a-in".to_string();
        let nothing_out_uid = "sim-usb-nothing-headphone-a-out".to_string();
        let builtin_in_uid = "sim-builtin-mac-mic".to_string();
        let builtin_out_uid = "sim-builtin-mac-speakers".to_string();
        vec![
            AudioDeviceInfo {
                redacted_uid: redact_device_uid(&nothing_in_uid),
                uid: nothing_in_uid,
                name: "Nothing Headphone (a)".to_string(),
                manufacturer: Some("Nothing Tech".to_string()),
                transport: TransportType::Usb,
                input_channels: 1,
                output_channels: 0,
                nominal_sample_rate_hz: Some(48_000.0),
                available_sample_rate_ranges_hz: vec![(48_000.0, 48_000.0)],
                buffer_frame_size: Some(256),
                buffer_frame_size_range: Some((64, 1024)),
                input_latency_frames: Some(48),
                output_latency_frames: None,
                safety_offset_frames: Some(32),
                is_alive: true,
                is_default_input: false,
                is_default_output: false,
                nothing_headphone_a_candidate: true,
            },
            AudioDeviceInfo {
                redacted_uid: redact_device_uid(&nothing_out_uid),
                uid: nothing_out_uid,
                name: "Nothing Headphone (a)".to_string(),
                manufacturer: Some("Nothing Tech".to_string()),
                transport: TransportType::Usb,
                input_channels: 0,
                output_channels: 2,
                nominal_sample_rate_hz: Some(48_000.0),
                available_sample_rate_ranges_hz: vec![(48_000.0, 48_000.0)],
                buffer_frame_size: Some(256),
                buffer_frame_size_range: Some((64, 1024)),
                input_latency_frames: None,
                output_latency_frames: Some(64),
                safety_offset_frames: Some(32),
                is_alive: true,
                is_default_input: false,
                is_default_output: false,
                nothing_headphone_a_candidate: true,
            },
            AudioDeviceInfo {
                redacted_uid: redact_device_uid(&builtin_in_uid),
                uid: builtin_in_uid,
                name: "Micro MacBook Pro".to_string(),
                manufacturer: Some("Apple Inc.".to_string()),
                transport: TransportType::BuiltIn,
                input_channels: 1,
                output_channels: 0,
                nominal_sample_rate_hz: Some(48_000.0),
                available_sample_rate_ranges_hz: vec![(44_100.0, 48_000.0)],
                buffer_frame_size: Some(256),
                buffer_frame_size_range: Some((64, 2048)),
                input_latency_frames: Some(32),
                output_latency_frames: None,
                safety_offset_frames: Some(16),
                is_alive: true,
                is_default_input: true,
                is_default_output: false,
                nothing_headphone_a_candidate: false,
            },
            AudioDeviceInfo {
                redacted_uid: redact_device_uid(&builtin_out_uid),
                uid: builtin_out_uid,
                name: "Haut-parleurs MacBook Pro".to_string(),
                manufacturer: Some("Apple Inc.".to_string()),
                transport: TransportType::BuiltIn,
                input_channels: 0,
                output_channels: 2,
                nominal_sample_rate_hz: Some(48_000.0),
                available_sample_rate_ranges_hz: vec![(44_100.0, 96_000.0)],
                buffer_frame_size: Some(256),
                buffer_frame_size_range: Some((64, 2048)),
                input_latency_frames: None,
                output_latency_frames: Some(48),
                safety_offset_frames: Some(16),
                is_alive: true,
                is_default_input: false,
                is_default_output: true,
                nothing_headphone_a_candidate: false,
            },
        ]
    }
}

struct SimulatedStreamHandle {
    fmt: ActiveStreamFormat,
    _keep_output: Option<RtOutputProcessor>,
    keep_input: Option<RtMicInputHandle>,
    mic_return_slot: Option<Arc<Mutex<Option<RtMicInputHandle>>>>,
}

impl Drop for SimulatedStreamHandle {
    fn drop(&mut self) {
        if let (Some(handle), Some(slot)) = (self.keep_input.take(), self.mic_return_slot.take()) {
            if let Ok(mut g) = slot.lock() {
                *g = Some(handle);
            }
        }
    }
}

impl ActiveStreamHandle for SimulatedStreamHandle {
    fn format(&self) -> ActiveStreamFormat {
        self.fmt.clone()
    }
}

impl AudioHalBackend for SimulatedHalBackend {
    fn enumerate_devices(&self) -> KaraokResult<Vec<AudioDeviceInfo>> {
        let guard = self.devices.lock().map_err(|_| {
            KaraokError::new(ErrorCode::InternalError, "Verrou inventaire simulé indisponible.")
        })?;
        Ok(guard.clone())
    }

    fn open_output_stream(
        &self,
        device_uid: &str,
        processor: RtOutputProcessor,
    ) -> KaraokResult<Box<dyn ActiveStreamHandle>> {
        let devs = self.enumerate_devices()?;
        let dev = devs
            .into_iter()
            .find(|d| d.uid == device_uid && d.has_output() && d.is_alive)
            .ok_or_else(|| {
                KaraokError::new(ErrorCode::DeviceUnavailable, "Sortie simulée introuvable.")
            })?;
        Ok(Box::new(SimulatedStreamHandle {
            fmt: ActiveStreamFormat {
                device_name: dev.name,
                redacted_uid: dev.redacted_uid,
                transport: dev.transport,
                sample_rate_hz: dev.nominal_sample_rate_hz.unwrap_or(48_000.0),
                channels: dev.output_channels,
                buffer_frames: dev.buffer_frame_size.unwrap_or(256),
                driver_latency_frames: dev.output_latency_frames,
                safety_offset_frames: dev.safety_offset_frames,
                is_simulated: true,
            },
            _keep_output: Some(processor),
            keep_input: None,
            mic_return_slot: None,
        }))
    }

    fn open_input_stream(
        &self,
        device_uid: &str,
        mic_handle: RtMicInputHandle,
        mic_return_slot: Arc<Mutex<Option<RtMicInputHandle>>>,
    ) -> KaraokResult<Box<dyn ActiveStreamHandle>> {
        let devs = match self.enumerate_devices() {
            Ok(d) => d,
            Err(e) => {
                if let Ok(mut slot) = mic_return_slot.lock() {
                    *slot = Some(mic_handle);
                }
                return Err(e);
            }
        };
        let Some(dev) = devs
            .into_iter()
            .find(|d| d.uid == device_uid && d.has_input() && d.is_alive)
        else {
            // Return RT mic handle on failure identically to macos_hal.rs (P2-N1)
            if let Ok(mut slot) = mic_return_slot.lock() {
                *slot = Some(mic_handle);
            }
            return Err(KaraokError::new(
                ErrorCode::DeviceUnavailable,
                "Entrée simulée introuvable.",
            ));
        };
        Ok(Box::new(SimulatedStreamHandle {
            fmt: ActiveStreamFormat {
                device_name: dev.name,
                redacted_uid: dev.redacted_uid,
                transport: dev.transport,
                sample_rate_hz: dev.nominal_sample_rate_hz.unwrap_or(48_000.0),
                channels: dev.input_channels,
                buffer_frames: dev.buffer_frame_size.unwrap_or(256),
                driver_latency_frames: dev.input_latency_frames,
                safety_offset_frames: dev.safety_offset_frames,
                is_simulated: true,
            },
            _keep_output: None,
            keep_input: Some(mic_handle),
            mic_return_slot: Some(mic_return_slot),
        }))
    }

    fn subscribe_changes(
        &self,
    ) -> KaraokResult<(Receiver<HalChangeSignal>, Box<dyn HalSubscription>)> {
        let (tx, rx) = crossbeam_channel::bounded(32);
        if let Ok(mut g) = self.listeners.lock() {
            g.push(tx);
        }
        Ok((rx, Box::new(NoopHalSubscription)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ac04_nothing_unplug_never_silently_falls_back_and_requires_confirmation_on_reconnect() {
        let devs = SimulatedHalBackend::fixture_with_nothing_usb_and_builtin();
        let nothing_uid = devs[0].uid.clone();
        let sel = DeviceSelector::Fixed {
            uid: nothing_uid.clone(),
            last_known_name: "Nothing Headphone (a)".into(),
            confirmed_by_user: true,
        };

        // Connected initially -> Ready with Nothing (even though Mac built-in is system default!)
        let r1 = resolve_input_route(&sel, &devs, false);
        assert_eq!(r1.status, RouteStatus::Ready);
        assert_eq!(r1.device.unwrap().uid, nothing_uid);

        // Unplug Nothing -> Mac built-in is still present, MUST NOT fall back to it!
        let remaining: Vec<_> = devs.iter().filter(|d| d.uid != nothing_uid).cloned().collect();
        let r2 = resolve_input_route(&sel, &remaining, false);
        assert_eq!(r2.status, RouteStatus::UnavailablePreferred);
        assert!(r2.device.is_none());
        assert!(r2.message_fr.unwrap().contains("Aucun basculement automatique"));

        // Reconnect Nothing -> must require explicit user confirmation before resuming!
        let r3 = resolve_input_route(&sel, &devs, true);
        assert_eq!(r3.status, RouteStatus::AwaitingReconnectConfirmation);
        assert_eq!(r3.device.unwrap().uid, nothing_uid);
    }

    #[test]
    fn ac03_fixed_uid_survives_rename_and_system_default_follows_role() {
        let mut devs = SimulatedHalBackend::fixture_with_nothing_usb_and_builtin();
        let nothing_uid = devs[0].uid.clone();
        devs[0].name = "Nothing Headphone (a) Renommé".into();

        let fixed = DeviceSelector::Fixed {
            uid: nothing_uid.clone(),
            last_known_name: "Nothing Headphone (a)".into(),
            confirmed_by_user: true,
        };
        let res_fixed = resolve_input_route(&fixed, &devs, false);
        assert_eq!(res_fixed.status, RouteStatus::Ready);
        assert_eq!(res_fixed.device.unwrap().name, "Nothing Headphone (a) Renommé");

        let sys_def = resolve_input_route(&DeviceSelector::SystemDefault, &devs, false);
        assert_eq!(sys_def.status, RouteStatus::Ready);
        assert_eq!(sys_def.device.unwrap().name, "Micro MacBook Pro");
    }
}
