//! Non-RT application controller orchestrating Core Audio routes, library, lyrics, offline analysis,
//! live RT engine, model store, and diagnostics (§4.2, §9.4).
//!
//! Strict separation of concerns:
//! - Runs strictly off the real-time audio thread.
//! - Applies route reconfiguration sequence: stop/mute -> re-enumerate -> prepare off-RT -> open -> verify active format.
//! - Handles hotplug changes via Core Audio HAL listener thread: if fixed input (Nothing USB-C) disappears,
//!   immediately stops mic capture and emits `AppEvent::RouteLost` with zero silent fallback to Mac mic!

use chrono::Utc;
use crossbeam_channel::{Receiver, Sender};
use karaok_analysis::run_track_preparation;
use karaok_audio_device::{
    resolve_input_route, resolve_output_route, ActiveStreamHandle, AudioHalBackend,
};
use karaok_audio_engine::{AudioEngineHandle, RtMicInputHandle};
use karaok_diagnostics::{DiagnosticLog, LogLevel, RedactedDiagnosticExport, RtCounters};
use karaok_library::{LibraryStore, StoragePaths};
use karaok_lyrics::{
    evaluate_viewport, mark_cue_instrumental, parse_lyrics_text, place_cue_manually,
    LyricsViewportState,
};
use karaok_media::{copy_original_to_managed_store, probe_media_file};
use karaok_model_store::ModelStore;
use karaok_types::{
    ActiveStreamFormat, AnalysisJob, AppEvent, ArtifactKind, AudioDeviceInfo, AudioPreference,
    DeviceSelector, ErrorCode, KaraokError, KaraokResult, LatencyOrigin, LatencyProfile,
    LyricsDocument, MeterSnapshot, MixParams, ModelManifest, MusicalScale, PitchClass, Playlist,
    PlaylistItem, ReviewStatus, RouteStatus, RtCountersSnapshot, Track, TrackAvailability,
    TransportSnapshot, VocalActivitySummary, VoiceDspParams, VoicePreset,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioRouteState {
    pub inputs: Vec<AudioDeviceInfo>,
    pub outputs: Vec<AudioDeviceInfo>,
    pub preference: AudioPreference,
    pub input_status: RouteStatus,
    pub output_status: RouteStatus,
    pub input_status_message_fr: Option<String>,
    pub output_status_message_fr: Option<String>,
    pub active_input_format: Option<ActiveStreamFormat>,
    pub active_output_format: Option<ActiveStreamFormat>,
    pub mic_capture_active: bool,
    pub voice_dsp_latency_ms: f64,
    pub loaded_track_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackDetailView {
    pub track: Track,
    pub stems_ready: bool,
    pub separation_method: Option<String>,
    pub latest_job: Option<AnalysisJob>,
    pub vocal_activity: Option<VocalActivitySummary>,
    pub lyrics: Option<LyricsDocument>,
}

/// Typed versioned commands accepted via FFI JSON dispatch (§9.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ControllerCommand {
    GetAudioState,
    RefreshDevices,
    SelectInput {
        selector: DeviceSelector,
        confirm_reconnect: bool,
    },
    SelectOutput {
        selector: DeviceSelector,
    },
    SetInputChannel {
        channel_index: u32,
    },
    StartMicCapture,
    StopMicCapture,
    SetVoiceParams {
        params: VoiceDspParams,
    },
    ApplyVoicePreset {
        preset: VoicePreset,
    },
    SetMixParams {
        params: MixParams,
    },
    ImportTrack {
        file_path: String,
    },
    ListTracks {
        search: Option<String>,
        favorites_only: bool,
    },
    GetTrackDetail {
        track_id: Uuid,
    },
    UpdateTrackMetadata {
        track_id: Uuid,
        title: String,
        artist: String,
        album: Option<String>,
        is_favorite: bool,
        tonic: Option<PitchClass>,
        scale: Option<MusicalScale>,
    },
    DeleteTrack {
        track_id: Uuid,
        delete_managed_copy: bool,
    },
    ListPlaylists,
    CreatePlaylist {
        name: String,
    },
    AddTrackToPlaylist {
        playlist_id: Uuid,
        track_id: Uuid,
    },
    ListPlaylistItems {
        playlist_id: Uuid,
    },
    RemovePlaylistItem {
        item_id: Uuid,
    },
    ReorderPlaylistItems {
        playlist_id: Uuid,
        ordered_item_ids: Vec<Uuid>,
    },
    StartPreparation {
        track_id: Uuid,
    },
    CancelPreparation {
        track_id: Uuid,
    },
    SetLyricsText {
        track_id: Uuid,
        raw_text: String,
        provenance: String,
        language: Option<String>,
    },
    PlaceLyricCue {
        track_id: Uuid,
        cue_id: String,
        start_s: f64,
        end_s: Option<f64>,
    },
    MarkCueInstrumental {
        track_id: Uuid,
        cue_id: String,
        is_instrumental: bool,
    },
    SetLyricsOffset {
        track_id: Uuid,
        global_offset_ms: i64,
    },
    ValidateLyrics {
        track_id: Uuid,
    },
    EvaluateLyricsViewport {
        track_id: Uuid,
        source_position_s: f64,
        max_blocks: usize,
    },
    LoadTrackForPlayback {
        track_id: Uuid,
        #[serde(default)]
        use_stems: Option<bool>,
        /// Monotonic client request generation. Older concurrent loads are
        /// rejected before they can replace or restart a newer selection.
        #[serde(default)]
        request_generation: Option<u64>,
    },
    Play {
        #[serde(default)]
        request_generation: Option<u64>,
    },
    Pause,
    Seek {
        position_s: f64,
        #[serde(default)]
        request_generation: Option<u64>,
    },
    ListModels,
    ImportVerifiedModel {
        file_path: String,
        manifest: Box<ModelManifest>,
    },
    GetDiagnostics,
    ListLatencyProfiles,
    PurgeCache,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ok", rename_all = "snake_case")]
pub enum ControllerResponse {
    Success { data: serde_json::Value },
    Error { error: KaraokError },
}

pub struct AppController {
    paths: StoragePaths,
    store: Mutex<LibraryStore>,
    model_store: Mutex<ModelStore>,
    hal: Arc<dyn AudioHalBackend>,
    counters: Arc<RtCounters>,
    log: Arc<DiagnosticLog>,

    preference: Mutex<AudioPreference>,
    device_generation: AtomicU64,
    input_was_lost: AtomicBool,
    input_status: Mutex<(RouteStatus, Option<String>)>,
    output_status: Mutex<(RouteStatus, Option<String>)>,

    /// Serializes all route transitions (hotplug, select input/output, mic start/stop - B5).
    route_lock: Mutex<()>,
    engine_state: Mutex<Arc<karaok_audio_engine::AtomicEngineState>>,
    engine: Mutex<Option<AudioEngineHandle>>,
    mic_return_slot: Arc<Mutex<Option<RtMicInputHandle>>>,
    output_stream: Mutex<Option<Box<dyn ActiveStreamHandle>>>,
    input_stream: Mutex<Option<Box<dyn ActiveStreamHandle>>>,
    active_input_uid: Mutex<Option<String>>,
    loaded_track_id: Mutex<Option<Uuid>>,
    latest_playback_request: AtomicU64,

    active_jobs: Mutex<HashMap<Uuid, Arc<AtomicBool>>>,
    event_tx: Sender<AppEvent>,
    event_rx: Receiver<AppEvent>,

    shutdown_flag: Arc<AtomicBool>,
    _hal_listener_guard: Mutex<Option<Box<dyn karaok_audio_device::HalSubscription>>>,
    hotplug_thread: Mutex<Option<JoinHandle<()>>>,
}

impl AppController {
    pub fn new(paths: StoragePaths, hal: Arc<dyn AudioHalBackend>) -> KaraokResult<Arc<Self>> {
        let store = LibraryStore::open(&paths)?;
        let pref = store.load_audio_preference()?;
        let model_store = ModelStore::open(paths.models_dir.clone())?;
        let counters = Arc::new(RtCounters::new());
        let log = Arc::new(DiagnosticLog::new(256));
        let (event_tx, event_rx) = crossbeam_channel::bounded(256);

        let (hal_rx, hal_guard) = hal.subscribe_changes().unwrap_or_else(|_| {
            let (_tx, rx) = crossbeam_channel::bounded(1);
            (
                rx,
                Box::new(karaok_audio_device::NoopHalSubscription)
                    as Box<dyn karaok_audio_device::HalSubscription>,
            )
        });

        let initial_state = Arc::new(karaok_audio_engine::AtomicEngineState::new());
        let controller = Arc::new(Self {
            paths,
            store: Mutex::new(store),
            model_store: Mutex::new(model_store),
            hal,
            counters,
            log,
            preference: Mutex::new(pref),
            device_generation: AtomicU64::new(1),
            input_was_lost: AtomicBool::new(false),
            input_status: Mutex::new((RouteStatus::Unconfigured, None)),
            output_status: Mutex::new((RouteStatus::Unconfigured, None)),
            route_lock: Mutex::new(()),
            engine_state: Mutex::new(initial_state),
            engine: Mutex::new(None),
            mic_return_slot: Arc::new(Mutex::new(None)),
            output_stream: Mutex::new(None),
            input_stream: Mutex::new(None),
            active_input_uid: Mutex::new(None),
            loaded_track_id: Mutex::new(None),
            latest_playback_request: AtomicU64::new(0),
            active_jobs: Mutex::new(HashMap::new()),
            event_tx,
            event_rx,
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            _hal_listener_guard: Mutex::new(Some(hal_guard)),
            hotplug_thread: Mutex::new(None),
        });

        // Initial device scan and output engine preparation (monitoring stays strictly OFF!)
        controller.reconcile_devices(false);

        // Spawn non-blocking HAL hotplug listener loop
        let weak_ctrl = Arc::downgrade(&controller);
        let shutdown = Arc::clone(&controller.shutdown_flag);
        if let Ok(handle) = thread::Builder::new()
            .name("karaok-hal-hotplug".into())
            .spawn(move || {
                while !shutdown.load(Ordering::Relaxed) {
                    match hal_rx.recv_timeout(std::time::Duration::from_millis(250)) {
                        Ok(sig) => {
                            let mut devices_list_changed =
                                sig == karaok_audio_device::HalChangeSignal::DevicesListChanged;
                            // Drain bursty HAL signals before reconciling once
                            while let Ok(next) = hal_rx.try_recv() {
                                if next == karaok_audio_device::HalChangeSignal::DevicesListChanged {
                                    devices_list_changed = true;
                                }
                            }
                            if let Some(ctrl) = weak_ctrl.upgrade() {
                                // Subscribe newly hotplugged devices & prune removed ones off-HAL-callback (P2-I2)
                                if devices_list_changed {
                                    if let Ok(mut guard) = ctrl._hal_listener_guard.lock() {
                                        if let Some(ref mut sub) = *guard {
                                            sub.sync_device_listeners();
                                        }
                                    }
                                }
                                ctrl.reconcile_devices(true);
                            } else {
                                break;
                            }
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
        {
            if let Ok(mut g) = controller.hotplug_thread.lock() {
                *g = Some(handle);
            }
        }

        controller.log.record(
            LogLevel::Info,
            "controller",
            "Contrôleur Karaok Party initialisé hors ligne (monitoring désactivé par défaut).",
        );

        Ok(controller)
    }

    /// Explicit graceful shutdown invoked before dropping the controller handle (B5).
    /// Cancels all active analysis workers, stops audio streams, and joins the hotplug thread safely.
    pub fn shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
        if let Ok(jobs) = self.active_jobs.lock() {
            for flag in jobs.values() {
                flag.store(true, Ordering::Relaxed);
            }
        }
        if let Ok(_route_guard) = self.route_lock.lock() {
            self.stop_mic_capture_locked();
            if let Ok(mut s) = self.output_stream.lock() {
                *s = None;
            }
            if let Ok(mut e) = self.engine.lock() {
                *e = None;
            }
            if let Ok(mut m) = self.mic_return_slot.lock() {
                *m = None;
            }
        }
        if let Ok(mut guard) = self._hal_listener_guard.lock() {
            *guard = None;
        }
        if let Ok(mut g) = self.hotplug_thread.lock() {
            if let Some(h) = g.take() {
                // Never self-join if Drop runs on the hotplug thread itself (B5)!
                if thread::current().id() != h.thread().id() {
                    let _ = h.join();
                }
            }
        }
    }

    pub fn poll_events(&self) -> Vec<AppEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.event_rx.try_recv() {
            out.push(ev);
        }
        out
    }

    pub fn read_transport_snapshot(&self) -> TransportSnapshot {
        self.engine_state
            .lock()
            .map(|s| s.read_transport())
            .unwrap_or_default()
    }

    pub fn read_meter_snapshot(&self) -> MeterSnapshot {
        self.engine_state
            .lock()
            .map(|s| s.read_meters())
            .unwrap_or_default()
    }

    pub fn read_rt_counters(&self) -> RtCountersSnapshot {
        self.counters.snapshot()
    }

    fn emit(&self, ev: AppEvent) {
        let _ = self.event_tx.try_send(ev);
    }

    /// Re-enumerate Core Audio devices and apply strict hotplug & no-silent-fallback policy (§5.5, AC-04, B5).
    pub fn reconcile_devices(&self, from_hotplug: bool) {
        let Ok(_route_guard) = self.route_lock.lock() else {
            return;
        };
        let devices = match self.hal.enumerate_devices() {
            Ok(d) => d,
            Err(e) => {
                self.log
                    .record(LogLevel::Error, "audio-device", &e.message_fr);
                return;
            }
        };
        let gen = self.device_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let inputs: Vec<AudioDeviceInfo> = devices.iter().filter(|d| d.has_input()).cloned().collect();
        let outputs: Vec<AudioDeviceInfo> = devices.iter().filter(|d| d.has_output()).cloned().collect();

        let pref = self
            .preference
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();

        let was_lost = self.input_was_lost.load(Ordering::Relaxed);
        let in_res = resolve_input_route(&pref.input, &devices, was_lost);
        let out_res = resolve_output_route(&pref.output, &devices);

        // Handle lost fixed input device (e.g., Nothing Headphone (a) USB-C unplugged!)
        if in_res.status == RouteStatus::UnavailablePreferred {
            let was_capturing = self
                .input_stream
                .lock()
                .map(|g| g.is_some())
                .unwrap_or(false);
            self.stop_mic_capture_locked();
            self.input_was_lost.store(true, Ordering::Relaxed);
            if was_capturing || from_hotplug {
                let dev_name = match &pref.input {
                    DeviceSelector::Fixed { last_known_name, .. } => last_known_name.clone(),
                    DeviceSelector::SystemDefault => "Micro par défaut".to_string(),
                };
                self.emit(AppEvent::RouteLost {
                    role: "input".to_string(),
                    device_name: dev_name,
                    message_fr: in_res
                        .message_fr
                        .clone()
                        .unwrap_or_else(|| "Entrée microphone perdue.".to_string()),
                    requires_user_confirmation_to_resume: true,
                });
            }
        } else if in_res.status == RouteStatus::AwaitingReconnectConfirmation {
            // Do NOT auto-resume capture without user confirmation (§5.4, AC-04)
            self.stop_mic_capture_locked();
        } else if let Some(ref resolved_in) = in_res.device {
            // If currently capturing on SystemDefault and the resolved input UID or sample rate changed on hotplug,
            // refresh input stream safely and update resampler rate (B2, B8)!
            let active_in_fmt = self
                .input_stream
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|s| s.format()));
            let cur_uid = self.active_input_uid.lock().ok().and_then(|g| g.clone());
            if let Some(fmt) = active_in_fmt {
                let sr_changed = resolved_in
                    .nominal_sample_rate_hz
                    .is_some_and(|sr| (sr - fmt.sample_rate_hz).abs() > 1.0);
                let uid_changed = cur_uid.as_deref() != Some(resolved_in.uid.as_str());
                if from_hotplug && (sr_changed || uid_changed) {
                    self.force_disable_monitoring();
                    self.stop_mic_capture_locked();
                    let _ = self.open_mic_stream_locked(resolved_in);
                }
            }
        }

        if let Ok(mut g) = self.input_status.lock() {
            *g = (in_res.status, in_res.message_fr.clone());
        }

        // Handle lost output device -> stop active output stream FIRST (single-writer seqlock - P2-B1),
        // then mark engine route-lost and publish RouteLost directly; NEVER surprise-switch to speakers!
        if out_res.status == RouteStatus::UnavailablePreferred {
            if let Ok(mut s) = self.output_stream.lock() {
                *s = None;
            }
            if let Ok(mut eng) = self.engine.lock() {
                if let Some(ref mut e) = *eng {
                    e.mark_route_lost();
                }
            }
            if let Ok(st) = self.engine_state.lock() {
                st.publish_route_lost();
            }
            if from_hotplug {
                let dev_name = match &pref.output {
                    DeviceSelector::Fixed { last_known_name, .. } => last_known_name.clone(),
                    DeviceSelector::SystemDefault => "Sortie par défaut".to_string(),
                };
                self.emit(AppEvent::RouteLost {
                    role: "output".to_string(),
                    device_name: dev_name,
                    message_fr: out_res
                        .message_fr
                        .clone()
                        .unwrap_or_else(|| "Sortie audio perdue.".to_string()),
                    requires_user_confirmation_to_resume: true,
                });
            }
        } else if let Some(ref dev) = out_res.device {
            let current_out_fmt = self
                .output_stream
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|s| s.format()));
            let out_changed = current_out_fmt
                .as_ref()
                .map(|f| {
                    f.redacted_uid != dev.redacted_uid
                        || dev
                            .nominal_sample_rate_hz
                            .is_some_and(|sr| (sr - f.sample_rate_hz).abs() > 1.0)
                })
                .unwrap_or(true);
            if out_changed {
                // Safety (§5.5, I5): disable software monitoring on ANY hotplug output change
                if from_hotplug {
                    self.force_disable_monitoring();
                }
                let _ = self.rebuild_output_pipeline_locked(dev);
            }
        }

        if let Ok(mut g) = self.output_status.lock() {
            *g = (out_res.status, out_res.message_fr.clone());
        }

        self.emit(AppEvent::DevicesChanged {
            generation: gen,
            inputs,
            outputs,
        });
    }

    fn force_disable_monitoring(&self) {
        if let Ok(mut pref) = self.preference.lock() {
            if pref.voice_dsp.monitoring_enabled {
                pref.voice_dsp.monitoring_enabled = false;
                if let Ok(mut eng) = self.engine.lock() {
                    if let Some(ref mut e) = *eng {
                        e.set_voice_params(pref.voice_dsp);
                    }
                }
            }
        }
    }

    /// Caller MUST hold `self.route_lock` (B5).
    fn rebuild_output_pipeline_locked(&self, out_dev: &AudioDeviceInfo) -> KaraokResult<()> {
        self.counters.inc_route_transitions();
        let prev_pos = self.read_transport_snapshot().source_position_s;
        let prev_track = self.loaded_track_id.lock().ok().and_then(|g| *g);

        // 1. Stop existing input & output streams cleanly BEFORE creating new graph (§5.5)
        self.stop_mic_capture_locked();
        if let Ok(mut s) = self.output_stream.lock() {
            *s = None;
        }
        if let Ok(mut e) = self.engine.lock() {
            *e = None;
        }
        if let Ok(mut m) = self.mic_return_slot.lock() {
            *m = None;
        }

        let pref = self
            .preference
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let out_sr = out_dev.nominal_sample_rate_hz.unwrap_or(48_000.0).round() as u32;

        // 2. Prepare new RT engine completely off-RT
        let (mut handle, rt_out, rt_mic) = AudioEngineHandle::create(
            out_sr,
            out_sr,
            pref.voice_dsp,
            pref.mix,
            Arc::clone(&self.counters),
        );
        handle.set_input_channel(pref.input_channel_index);

        self.emit(AppEvent::RouteOpening {
            role: "output".into(),
            device_name: out_dev.name.clone(),
        });

        let stream = match self.hal.open_output_stream(&out_dev.uid, rt_out) {
            Ok(s) => s,
            Err(err) => {
                self.emit(AppEvent::RouteFailed {
                    role: "output".into(),
                    error: err.clone(),
                });
                return Err(err);
            }
        };

        let active_fmt = stream.format();
        let out_frames = active_fmt.buffer_frames as f64
            + active_fmt.driver_latency_frames.unwrap_or(0) as f64
            + active_fmt.safety_offset_frames.unwrap_or(0) as f64;
        let out_ms = (out_frames / active_fmt.sample_rate_hz.max(8000.0)) * 1000.0;
        handle.set_presentation_latency_ms(out_ms);

        let profile = LatencyProfile {
            id: Uuid::new_v4(),
            route_fingerprint: format!(
                "out:{}:{:.0}Hz:{}f:pitch={}",
                active_fmt.redacted_uid,
                active_fmt.sample_rate_hz,
                active_fmt.buffer_frames,
                pref.voice_dsp.pitch_enabled
            ),
            input_name: match &pref.input {
                DeviceSelector::Fixed { last_known_name, .. } => last_known_name.clone(),
                DeviceSelector::SystemDefault => "Entrée système par défaut".into(),
            },
            output_name: active_fmt.device_name.clone(),
            sample_rate_hz: active_fmt.sample_rate_hz,
            buffer_frames: active_fmt.buffer_frames,
            pitch_correction_enabled: pref.voice_dsp.pitch_enabled,
            reverb_enabled: pref.voice_dsp.reverb_enabled,
            origin: if active_fmt.is_simulated {
                LatencyOrigin::Simulated
            } else {
                LatencyOrigin::DriverEstimateOnly
            },
            input_driver_ms: None,
            output_driver_ms: Some(out_ms),
            dsp_algorithmic_ms: handle.voice_dsp_latency_ms(),
            total_estimated_ms: Some(out_ms + handle.voice_dsp_latency_ms()),
            measured_p50_ms: None,
            measured_p95_ms: None,
            measured_p99_ms: None,
            os_version: "macOS".into(),
            protocol_notes: "Estimation pilote Core Audio + latence algorithmique DSP (mesure boucle acoustique non effectuée sur cette route).".into(),
            measured_at: Utc::now(),
            valid: true,
        };
        if let Ok(store) = self.store.lock() {
            let _ = store.save_latency_profile(&profile);
        }

        if let Ok(mut st) = self.engine_state.lock() {
            *st = handle.state();
        }
        if let Ok(mut m) = self.mic_return_slot.lock() {
            *m = Some(rt_mic);
        }
        if let Ok(mut os) = self.output_stream.lock() {
            *os = Some(stream);
        }
        if let Ok(mut e) = self.engine.lock() {
            *e = Some(handle);
        }

        // Preserve loaded track and source position across route reconfiguration (§5.5, B7)
        if let Some(tid) = prev_track {
            let _ = self.load_track_for_playback(tid, None, None);
            if prev_pos > 0.05 {
                let _ = self.seek(prev_pos, None);
            }
        }

        self.emit(AppEvent::RouteReady {
            role: "output".into(),
            format: active_fmt,
        });
        Ok(())
    }

    fn stop_mic_capture_locked(&self) {
        if let Ok(mut e) = self.engine.lock() {
            if let Some(ref mut eng) = *e {
                eng.set_mic_capture_active(false);
            }
        }
        // Dropping input_stream stops AUHAL and returns RtMicInputHandle into `mic_return_slot` (B6)!
        if let Ok(mut s) = self.input_stream.lock() {
            *s = None;
        }
        if let Ok(mut u) = self.active_input_uid.lock() {
            *u = None;
        }
    }

    fn open_mic_stream_locked(&self, dev: &AudioDeviceInfo) -> KaraokResult<ActiveStreamFormat> {
        // Idempotent: if already capturing on the exact same device UID, return its active format (B6)
        if let (Ok(ins), Ok(cur_uid)) = (self.input_stream.lock(), self.active_input_uid.lock()) {
            if let Some(ref stream) = *ins {
                if cur_uid.as_deref() == Some(dev.uid.as_str()) {
                    return Ok(stream.format());
                }
            }
        }

        // Stop any previous input stream so its Drop cleanly returns `RtMicInputHandle` to `mic_return_slot`!
        self.stop_mic_capture_locked();

        let mic_handle = self
            .mic_return_slot
            .lock()
            .ok()
            .and_then(|mut g| g.take())
            .ok_or_else(|| {
                KaraokError::with_hint(
                    ErrorCode::DeviceUnavailable,
                    "Aucune sortie audio active pour héberger le flux micro.",
                    "Sélectionnez une sortie audio valide dans Son.",
                )
            })?;

        self.emit(AppEvent::RouteOpening {
            role: "input".into(),
            device_name: dev.name.clone(),
        });

        let stream = match self
            .hal
            .open_input_stream(&dev.uid, mic_handle, Arc::clone(&self.mic_return_slot))
        {
            Ok(s) => s,
            Err(err) => {
                self.emit(AppEvent::RouteFailed {
                    role: "input".into(),
                    error: err.clone(),
                });
                return Err(err);
            }
        };
        let fmt = stream.format();

        // Update RT asynchronous resampler with ACTUAL negotiated input sample rate (B2: 16/24/44.1/48 kHz!)
        if let Ok(mut e) = self.engine.lock() {
            if let Some(ref mut eng) = *e {
                eng.set_input_sample_rate(fmt.sample_rate_hz);
                eng.set_mic_capture_active(true);
            }
        }
        if let Ok(mut ins) = self.input_stream.lock() {
            *ins = Some(stream);
        }
        if let Ok(mut u) = self.active_input_uid.lock() {
            *u = Some(dev.uid.clone());
        }

        self.emit(AppEvent::RouteReady {
            role: "input".into(),
            format: fmt.clone(),
        });
        // Privacy (I14): NEVER log raw `device_name` (may contain user's first name); log redacted UID + transport!
        self.log.record(
            LogLevel::Info,
            "audio-device",
            &format!(
                "Capture micro active : [{}] transport={} ({:.0} Hz, {} ch, buf {}, sim={})",
                fmt.redacted_uid,
                fmt.transport.label_fr(),
                fmt.sample_rate_hz,
                fmt.channels,
                fmt.buffer_frames,
                fmt.is_simulated
            ),
        );
        Ok(fmt)
    }

    pub fn start_mic_capture(&self) -> KaraokResult<ActiveStreamFormat> {
        let _route_guard = self.route_lock.lock().map_err(|_| {
            KaraokError::new(ErrorCode::InternalError, "Verrou de route audio occupé.")
        })?;
        let devices = self.hal.enumerate_devices()?;
        let pref = self.preference.lock().map(|g| g.clone()).unwrap_or_default();
        let res = resolve_input_route(&pref.input, &devices, self.input_was_lost.load(Ordering::Relaxed));

        let dev = match res.status {
            RouteStatus::Ready => res.device.ok_or_else(|| {
                KaraokError::new(ErrorCode::DeviceUnavailable, "Entrée introuvable.")
            })?,
            RouteStatus::AwaitingReconnectConfirmation => {
                return Err(KaraokError::with_hint(
                    ErrorCode::DeviceUnavailable,
                    res.message_fr.unwrap_or_else(|| "Confirmation de reconnexion requise.".into()),
                    "Confirmez la réactivation de votre micro dans l'onglet Son.",
                ));
            }
            _ => {
                return Err(KaraokError::with_hint(
                    ErrorCode::DeviceUnavailable,
                    res.message_fr.unwrap_or_else(|| "Micro préféré indisponible.".into()),
                    "Connectez votre micro préféré ou sélectionnez explicitement une autre entrée dans Son.",
                ));
            }
        };

        self.open_mic_stream_locked(&dev)
    }

    pub fn stop_mic_capture(&self) {
        if let Ok(_route_guard) = self.route_lock.lock() {
            self.stop_mic_capture_locked();
        }
    }

    pub fn get_audio_state(&self) -> KaraokResult<AudioRouteState> {
        let devices = self.hal.enumerate_devices()?;
        let inputs = devices.iter().filter(|d| d.has_input()).cloned().collect();
        let outputs = devices.iter().filter(|d| d.has_output()).cloned().collect();
        let pref = self.preference.lock().map(|g| g.clone()).unwrap_or_default();
        let (input_status, input_status_message_fr) = self
            .input_status
            .lock()
            .map(|g| g.clone())
            .unwrap_or((RouteStatus::Unconfigured, None));
        let (output_status, output_status_message_fr) = self
            .output_status
            .lock()
            .map(|g| g.clone())
            .unwrap_or((RouteStatus::Unconfigured, None));
        let active_input_format = self
            .input_stream
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.format()));
        let active_output_format = self
            .output_stream
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.format()));
        let voice_dsp_latency_ms = self
            .engine
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|e| e.voice_dsp_latency_ms()))
            .unwrap_or(0.0);

        let loaded_track_id = self.loaded_track_id.lock().ok().and_then(|g| *g);
        Ok(AudioRouteState {
            inputs,
            outputs,
            preference: pref,
            input_status,
            output_status,
            input_status_message_fr,
            output_status_message_fr,
            mic_capture_active: active_input_format.is_some(),
            active_input_format,
            active_output_format,
            voice_dsp_latency_ms,
            loaded_track_id,
        })
    }

    pub fn import_track(&self, file_path: &str) -> KaraokResult<Track> {
        let src = PathBuf::from(file_path);
        let probed = probe_media_file(&src)?;

        let store = self.store.lock().map_err(|_| {
            KaraokError::new(ErrorCode::InternalError, "Base de données occupée.")
        })?;
        // Duplicate SHA-256 detection: reuse existing managed copy cleanly (§3.2, §10)
        if let Some(existing) = store.find_track_by_hash(&probed.sha256)? {
            return Ok(existing);
        }

        let managed_path = copy_original_to_managed_store(
            &src,
            &self.paths.originals_dir,
            &probed.sha256,
            &probed.format,
        )?;

        let now = Utc::now();
        let track = Track {
            id: Uuid::new_v4(),
            title: probed.title,
            artist: probed.artist,
            album: probed.album,
            duration_s: probed.duration_s,
            original_sha256: probed.sha256,
            managed_original_path: managed_path.to_string_lossy().to_string(),
            format: probed.format,
            sample_rate_hz: probed.sample_rate_hz,
            channels: probed.channels,
            encoder_delay_samples: probed.encoder_delay_samples,
            is_favorite: false,
            availability: TrackAvailability::Available,
            estimated_tonic: None,
            estimated_scale: None,
            key_confidence: None,
            key_ambiguous: true,
            created_at: now,
            updated_at: now,
        };
        store.upsert_track(&track)?;
        self.log.record(
            LogLevel::Info,
            "media",
            &format!(
                "Morceau importé ({:.1}s, {} Hz, {} ch, {}).",
                track.duration_s, track.sample_rate_hz, track.channels, track.format
            ),
        );
        Ok(track)
    }

    pub fn get_track_detail(&self, track_id: Uuid) -> KaraokResult<TrackDetailView> {
        let store = self.store.lock().map_err(|_| {
            KaraokError::new(ErrorCode::InternalError, "Base de données occupée.")
        })?;
        let track = store.get_track(track_id)?.ok_or_else(|| {
            KaraokError::new(ErrorCode::NotFound, "Morceau introuvable dans la bibliothèque.")
        })?;
        let voc_art = store.find_artifact(track_id, ArtifactKind::VocalStem, None)?;
        let acc_art = store.find_artifact(track_id, ArtifactKind::AccompanimentStem, None)?;
        let stems_ready = voc_art
            .as_ref()
            .is_some_and(|a| std::path::Path::new(&a.file_path).exists())
            && acc_art
                .as_ref()
                .is_some_and(|a| std::path::Path::new(&a.file_path).exists());
        let separation_method = voc_art.map(|a| format!("{} v{}", a.method, a.method_version));
        let latest_job = store.latest_analysis_job(track_id)?;
        let vocal_activity = store.get_vocal_activity(track_id)?;
        let lyrics = store.get_latest_lyrics(track_id)?;

        Ok(TrackDetailView {
            track,
            stems_ready,
            separation_method,
            latest_job,
            vocal_activity,
            lyrics,
        })
    }

    pub fn start_preparation(self: &Arc<Self>, track_id: Uuid) -> KaraokResult<Uuid> {
        // Verify track exists
        let mut track = {
            let store = self.store.lock().map_err(|_| {
                KaraokError::new(ErrorCode::InternalError, "Base de données occupée.")
            })?;
            store.get_track(track_id)?.ok_or_else(|| {
                KaraokError::new(ErrorCode::NotFound, "Morceau introuvable.")
            })?
        };

        let cancel_flag = Arc::new(AtomicBool::new(false));
        if let Ok(mut jobs) = self.active_jobs.lock() {
            if let Some(prev) = jobs.insert(track_id, Arc::clone(&cancel_flag)) {
                prev.store(true, Ordering::Relaxed);
            }
        }

        let ctrl = Arc::clone(self);
        let paths = self.paths.clone();
        let job_op_id = Uuid::new_v4();

        thread::Builder::new()
            .name("karaok-analysis-worker".into())
            .spawn(move || {
                // Dedicated SQLite connection in worker thread (WAL concurrent reads/writes, zero lock across RT!)
                let worker_store = match LibraryStore::open(&paths) {
                    Ok(s) => s,
                    Err(err) => {
                        ctrl.emit(AppEvent::Error { error: err });
                        return;
                    }
                };
                let model_store = match ModelStore::open(paths.models_dir.clone()) {
                    Ok(m) => m,
                    Err(err) => {
                        ctrl.emit(AppEvent::Error { error: err });
                        return;
                    }
                };
                let job_flag = Arc::clone(&cancel_flag);
                let res = run_track_preparation(
                    &mut track,
                    &worker_store,
                    &paths,
                    &model_store,
                    cancel_flag,
                    |job| {
                        ctrl.emit(AppEvent::PreparationProgress { job: job.clone() });
                    },
                );
                match res {
                    Ok(prep) => {
                        ctrl.emit(AppEvent::ArtifactReady {
                            track_id,
                            kind: ArtifactKind::VocalStem,
                        });
                        ctrl.emit(AppEvent::ArtifactReady {
                            track_id,
                            kind: ArtifactKind::AccompanimentStem,
                        });
                        if prep.vocal_activity.review_status != ReviewStatus::UserValidated {
                            ctrl.emit(AppEvent::LyricsNeedsReview {
                                track_id,
                                reason_fr: prep.job.status_detail_fr.clone(),
                            });
                        }
                    }
                    Err(err) => {
                        if err.code != ErrorCode::JobCancelled {
                            ctrl.emit(AppEvent::Error { error: err });
                        }
                    }
                }
                // Only remove our own cancel token so a superseded job never removes a newer job's token (I8)
                if let Ok(mut jobs) = ctrl.active_jobs.lock() {
                    if jobs.get(&track_id).is_some_and(|existing| Arc::ptr_eq(existing, &job_flag)) {
                        jobs.remove(&track_id);
                    }
                }
            })
            .map_err(|e| {
                KaraokError::new(
                    ErrorCode::InternalError,
                    format!("Impossible de démarrer le worker d'analyse : {e}"),
                )
            })?;

        Ok(job_op_id)
    }

    pub fn cancel_preparation(&self, track_id: Uuid) -> bool {
        if let Ok(jobs) = self.active_jobs.lock() {
            if let Some(flag) = jobs.get(&track_id) {
                flag.store(true, Ordering::Relaxed);
                return true;
            }
        }
        false
    }

    pub fn load_track_for_playback(
        &self,
        track_id: Uuid,
        use_stems_override: Option<bool>,
        request_generation: Option<u64>,
    ) -> KaraokResult<TransportSnapshot> {
        if let Some(generation) = request_generation {
            let previous = self
                .latest_playback_request
                .fetch_max(generation, Ordering::AcqRel);
            if generation < previous {
                return Err(stale_playback_request_error());
            }
        }

        let (track, pcm_art, voc_art, acc_art) = {
            let store = self.store.lock().map_err(|_| {
                KaraokError::new(ErrorCode::InternalError, "Base de données occupée.")
            })?;
            let track = store.get_track(track_id)?.ok_or_else(|| {
                KaraokError::new(ErrorCode::NotFound, "Morceau introuvable.")
            })?;
            let pcm = store.find_artifact(track_id, ArtifactKind::CanonicalPcm, None)?;
            let voc = store.find_artifact(track_id, ArtifactKind::VocalStem, None)?;
            let acc = store.find_artifact(track_id, ArtifactKind::AccompanimentStem, None)?;
            (track, pcm, voc, acc)
        };

        // Ensure canonical PCM exists with a valid O(1) WAV header so playback never silently stalls (P2-N2, §3.2)
        let canonical_wav = match pcm_art {
            Some(art)
                if karaok_media::is_valid_canonical_wav_header(std::path::Path::new(&art.file_path)) =>
            {
                PathBuf::from(art.file_path)
            }
            _ => {
                let pcm_path = self
                    .paths
                    .pcm_cache_dir
                    .join(format!("orig-{}.wav", &track.original_sha256[..24.min(track.original_sha256.len())]));
                if !karaok_media::is_valid_canonical_wav_header(&pcm_path) {
                    karaok_media::decode_to_canonical_stereo_wav(
                        std::path::Path::new(&track.managed_original_path),
                        &pcm_path,
                        128,
                    )?;
                }
                pcm_path
            }
        };

        let voc_path = voc_art
            .map(|a| PathBuf::from(a.file_path))
            .filter(|p| p.exists());
        let acc_path = acc_art
            .map(|a| PathBuf::from(a.file_path))
            .filter(|p| p.exists());

        // Decoding a missing canonical WAV can be slow. A newer UI request
        // may have arrived while this one was preparing the file, so verify
        // again before mutating preferences, loaded-track state, or engine.
        self.ensure_current_playback_request(request_generation)?;

        if let Some(use_stems) = use_stems_override {
            if let Ok(mut pref) = self.preference.lock() {
                pref.mix.use_stems = use_stems;
                if let Ok(mut eng) = self.engine.lock() {
                    if let Some(ref mut e) = *eng {
                        e.set_mix_params(pref.mix);
                    }
                }
            }
        }

        if let Ok(mut lt) = self.loaded_track_id.lock() {
            *lt = Some(track_id);
        }

        let mut eng_guard = self.engine.lock().map_err(|_| {
            KaraokError::new(ErrorCode::InternalError, "Moteur audio indisponible.")
        })?;
        let eng = eng_guard.as_mut().ok_or_else(|| {
            KaraokError::with_hint(
                ErrorCode::DeviceUnavailable,
                "Aucune sortie audio active pour lire le morceau.",
                "Sélectionnez une sortie audio valide dans l'onglet Son.",
            )
        })?;

        eng.load_track(
            canonical_wav,
            voc_path,
            acc_path,
            track.duration_s,
            track.sample_rate_hz,
        );
        Ok(eng.state().read_transport())
    }

    pub fn play(&self, request_generation: Option<u64>) -> KaraokResult<TransportSnapshot> {
        self.ensure_current_playback_request(request_generation)?;
        let mut eng_guard = self.engine.lock().map_err(|_| {
            KaraokError::new(ErrorCode::InternalError, "Moteur audio indisponible.")
        })?;
        let eng = eng_guard.as_mut().ok_or_else(|| {
            KaraokError::new(ErrorCode::DeviceUnavailable, "Aucune sortie audio active.")
        })?;
        eng.play();
        Ok(eng.state().read_transport())
    }

    pub fn pause(&self) -> KaraokResult<TransportSnapshot> {
        let mut eng_guard = self.engine.lock().map_err(|_| {
            KaraokError::new(ErrorCode::InternalError, "Moteur audio indisponible.")
        })?;
        let eng = eng_guard.as_mut().ok_or_else(|| {
            KaraokError::new(ErrorCode::DeviceUnavailable, "Aucune sortie audio active.")
        })?;
        eng.pause();
        Ok(eng.state().read_transport())
    }

    pub fn seek(
        &self,
        position_s: f64,
        request_generation: Option<u64>,
    ) -> KaraokResult<TransportSnapshot> {
        self.ensure_current_playback_request(request_generation)?;
        let mut eng_guard = self.engine.lock().map_err(|_| {
            KaraokError::new(ErrorCode::InternalError, "Moteur audio indisponible.")
        })?;
        let eng = eng_guard.as_mut().ok_or_else(|| {
            KaraokError::new(ErrorCode::DeviceUnavailable, "Aucune sortie audio active.")
        })?;
        eng.seek(position_s);
        Ok(eng.state().read_transport())
    }

    fn ensure_current_playback_request(&self, request_generation: Option<u64>) -> KaraokResult<()> {
        if request_generation.is_some_and(|generation| {
            generation != self.latest_playback_request.load(Ordering::Acquire)
        }) {
            return Err(stale_playback_request_error());
        }
        Ok(())
    }

    /// Execute any typed `ControllerCommand` and return a structured `ControllerResponse`.
    pub fn dispatch(self: &Arc<Self>, cmd: ControllerCommand) -> ControllerResponse {
        match self.dispatch_inner(cmd) {
            Ok(data) => ControllerResponse::Success { data },
            Err(error) => ControllerResponse::Error { error },
        }
    }

    fn dispatch_inner(self: &Arc<Self>, cmd: ControllerCommand) -> KaraokResult<serde_json::Value> {
        match cmd {
            ControllerCommand::GetAudioState => to_json(self.get_audio_state()?),
            ControllerCommand::RefreshDevices => {
                self.reconcile_devices(false);
                to_json(self.get_audio_state()?)
            }
            ControllerCommand::SelectInput {
                selector,
                confirm_reconnect,
            } => {
                self.stop_mic_capture();
                let changed_selector = self
                    .preference
                    .lock()
                    .map(|p| p.input != selector)
                    .unwrap_or(true);
                if confirm_reconnect || changed_selector {
                    self.input_was_lost.store(false, Ordering::Relaxed);
                }
                if let Ok(mut pref) = self.preference.lock() {
                    pref.input = selector;
                    if let Ok(store) = self.store.lock() {
                        store.save_audio_preference(&pref)?;
                    }
                }
                self.reconcile_devices(false);
                to_json(self.get_audio_state()?)
            }
            ControllerCommand::SelectOutput { selector } => {
                // Safety: turn off monitoring on route change (§3.1)
                self.force_disable_monitoring();
                if let Ok(mut pref) = self.preference.lock() {
                    pref.output = selector;
                    if let Ok(store) = self.store.lock() {
                        store.save_audio_preference(&pref)?;
                    }
                }
                self.reconcile_devices(false);
                to_json(self.get_audio_state()?)
            }
            ControllerCommand::SetInputChannel { channel_index } => {
                if let Ok(mut pref) = self.preference.lock() {
                    pref.input_channel_index = channel_index;
                    if let Ok(store) = self.store.lock() {
                        store.save_audio_preference(&pref)?;
                    }
                }
                if let Ok(eng) = self.engine.lock() {
                    if let Some(ref e) = *eng {
                        e.set_input_channel(channel_index);
                    }
                }
                to_json(self.get_audio_state()?)
            }
            ControllerCommand::StartMicCapture => to_json(self.start_mic_capture()?),
            ControllerCommand::StopMicCapture => {
                self.stop_mic_capture();
                to_json(self.get_audio_state()?)
            }
            ControllerCommand::SetVoiceParams { params } => {
                let clean = params.clamped();
                if let Ok(mut pref) = self.preference.lock() {
                    pref.voice_dsp = clean;
                    if let Ok(store) = self.store.lock() {
                        let _ = store.save_audio_preference(&pref);
                    }
                }
                if let Ok(mut eng) = self.engine.lock() {
                    if let Some(ref mut e) = *eng {
                        e.set_voice_params(clean);
                    }
                }
                to_json(clean)
            }
            ControllerCommand::ApplyVoicePreset { preset } => {
                let updated = {
                    let mut pref = self.preference.lock().map_err(|_| {
                        KaraokError::new(ErrorCode::InternalError, "Verrou préférences occupé.")
                    })?;
                    let next = VoiceDspParams::from_preset_keeping_routing(preset, pref.voice_dsp);
                    pref.voice_dsp = next;
                    if let Ok(store) = self.store.lock() {
                        let _ = store.save_audio_preference(&pref);
                    }
                    next
                };
                if let Ok(mut eng) = self.engine.lock() {
                    if let Some(ref mut e) = *eng {
                        e.set_voice_params(updated);
                    }
                }
                to_json(updated)
            }
            ControllerCommand::SetMixParams { params } => {
                let clean = params.clamped();
                if let Ok(mut pref) = self.preference.lock() {
                    pref.mix = clean;
                    if let Ok(store) = self.store.lock() {
                        let _ = store.save_audio_preference(&pref);
                    }
                }
                if let Ok(mut eng) = self.engine.lock() {
                    if let Some(ref mut e) = *eng {
                        e.set_mix_params(clean);
                    }
                }
                to_json(clean)
            }
            ControllerCommand::ImportTrack { file_path } => to_json(self.import_track(&file_path)?),
            ControllerCommand::ListTracks {
                search,
                favorites_only,
            } => {
                let store = self.store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Base occupée.")
                })?;
                to_json(store.list_tracks(search.as_deref(), favorites_only)?)
            }
            ControllerCommand::GetTrackDetail { track_id } => {
                to_json(self.get_track_detail(track_id)?)
            }
            ControllerCommand::UpdateTrackMetadata {
                track_id,
                title,
                artist,
                album,
                is_favorite,
                tonic,
                scale,
            } => {
                let store = self.store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Base occupée.")
                })?;
                let mut track = store.get_track(track_id)?.ok_or_else(|| {
                    KaraokError::new(ErrorCode::NotFound, "Morceau introuvable.")
                })?;
                if !title.trim().is_empty() {
                    track.title = title.trim().to_string();
                }
                if !artist.trim().is_empty() {
                    track.artist = artist.trim().to_string();
                }
                track.album = album.filter(|s| !s.trim().is_empty());
                track.is_favorite = is_favorite;
                if tonic.is_some() {
                    track.estimated_tonic = tonic;
                    track.key_ambiguous = false;
                }
                if scale.is_some() {
                    track.estimated_scale = scale;
                }
                track.updated_at = Utc::now();
                store.upsert_track(&track)?;
                to_json(track)
            }
            ControllerCommand::DeleteTrack {
                track_id,
                delete_managed_copy,
            } => {
                let store = self.store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Base occupée.")
                })?;
                let track = store.get_track(track_id)?;
                let deleted = store.delete_track(track_id)?;
                if deleted && delete_managed_copy {
                    if let Some(t) = track {
                        // Only remove our managed copy inside Application Support/originals, NEVER external user files (§10)!
                        let p = PathBuf::from(&t.managed_original_path);
                        if p.starts_with(&self.paths.originals_dir) {
                            let _ = std::fs::remove_file(p);
                        }
                    }
                }
                to_json(deleted)
            }
            ControllerCommand::ListPlaylists => {
                let store = self.store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Base occupée.")
                })?;
                to_json(store.list_playlists()?)
            }
            ControllerCommand::CreatePlaylist { name } => {
                let store = self.store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Base occupée.")
                })?;
                let pl: Playlist = store.create_playlist(&name)?;
                to_json(pl)
            }
            ControllerCommand::AddTrackToPlaylist {
                playlist_id,
                track_id,
            } => {
                let store = self.store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Base occupée.")
                })?;
                let item: PlaylistItem = store.add_track_to_playlist(playlist_id, track_id)?;
                to_json(item)
            }
            ControllerCommand::ListPlaylistItems { playlist_id } => {
                let store = self.store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Base occupée.")
                })?;
                to_json(store.list_playlist_items(playlist_id)?)
            }
            ControllerCommand::RemovePlaylistItem { item_id } => {
                let store = self.store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Base occupée.")
                })?;
                to_json(store.remove_playlist_item(item_id)?)
            }
            ControllerCommand::ReorderPlaylistItems {
                playlist_id,
                ordered_item_ids,
            } => {
                let mut store = self.store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Base occupée.")
                })?;
                store.reorder_playlist_items(playlist_id, &ordered_item_ids)?;
                to_json(true)
            }
            ControllerCommand::StartPreparation { track_id } => {
                let op_id = self.start_preparation(track_id)?;
                to_json(op_id)
            }
            ControllerCommand::CancelPreparation { track_id } => {
                to_json(self.cancel_preparation(track_id))
            }
            ControllerCommand::SetLyricsText {
                track_id,
                raw_text,
                provenance,
                language,
            } => {
                let store = self.store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Base occupée.")
                })?;
                let track = store.get_track(track_id)?.ok_or_else(|| {
                    KaraokError::new(ErrorCode::NotFound, "Morceau introuvable.")
                })?;
                let prev_rev = store
                    .get_latest_lyrics(track_id)?
                    .map(|d| d.text_revision)
                    .unwrap_or(0);
                let mut report = parse_lyrics_text(
                    track_id,
                    &raw_text,
                    track.duration_s,
                    provenance,
                    language,
                    prev_rev,
                );
                // Attach any existing vocal activity summary without re-running stems (AC-09)!
                if let Some(vocal) = store.get_vocal_activity(track_id)? {
                    report.document.first_vocal_activity_s = vocal.first_vocal_activity_s;
                    report.document.first_sung_activity_s = vocal.first_sung_activity_s;
                    if report.document.granularity == karaok_types::LyricsGranularity::Untimed
                        && !vocal.no_singing_detected
                    {
                        karaok_analysis::align_untimed_lines_to_vocal_regions(
                            &mut report.document,
                            &vocal,
                            track.duration_s,
                        );
                    }
                }
                store.save_lyrics_document(&report.document)?;
                to_json(report)
            }
            ControllerCommand::PlaceLyricCue {
                track_id,
                cue_id,
                start_s,
                end_s,
            } => {
                let store = self.store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Base occupée.")
                })?;
                let track = store.get_track(track_id)?.ok_or_else(|| {
                    KaraokError::new(ErrorCode::NotFound, "Morceau introuvable.")
                })?;
                let mut doc = store.get_latest_lyrics(track_id)?.ok_or_else(|| {
                    KaraokError::new(ErrorCode::NotFound, "Aucun document de paroles pour ce morceau.")
                })?;
                place_cue_manually(&mut doc, &cue_id, start_s, end_s, track.duration_s)
                    .map_err(|m| KaraokError::new(ErrorCode::InvalidParameter, m))?;
                doc.text_revision += 1;
                store.save_lyrics_document(&doc)?;
                to_json(doc)
            }
            ControllerCommand::MarkCueInstrumental {
                track_id,
                cue_id,
                is_instrumental,
            } => {
                let store = self.store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Base occupée.")
                })?;
                let mut doc = store.get_latest_lyrics(track_id)?.ok_or_else(|| {
                    KaraokError::new(ErrorCode::NotFound, "Aucun document de paroles pour ce morceau.")
                })?;
                mark_cue_instrumental(&mut doc, &cue_id, is_instrumental)
                    .map_err(|m| KaraokError::new(ErrorCode::InvalidParameter, m))?;
                doc.text_revision += 1;
                store.save_lyrics_document(&doc)?;
                to_json(doc)
            }
            ControllerCommand::SetLyricsOffset {
                track_id,
                global_offset_ms,
            } => {
                let store = self.store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Base occupée.")
                })?;
                let mut doc = store.get_latest_lyrics(track_id)?.ok_or_else(|| {
                    KaraokError::new(ErrorCode::NotFound, "Aucun document de paroles.")
                })?;
                doc.global_offset_ms = global_offset_ms.clamp(-30_000, 30_000);
                doc.text_revision += 1;
                doc.updated_at = Utc::now();
                store.save_lyrics_document(&doc)?;
                to_json(doc)
            }
            ControllerCommand::ValidateLyrics { track_id } => {
                let store = self.store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Base occupée.")
                })?;
                let mut doc = store.get_latest_lyrics(track_id)?.ok_or_else(|| {
                    KaraokError::new(ErrorCode::NotFound, "Aucun document de paroles à valider.")
                })?;
                doc.status = ReviewStatus::UserValidated;
                doc.text_revision += 1;
                doc.updated_at = Utc::now();
                store.save_lyrics_document(&doc)?;
                to_json(doc)
            }
            ControllerCommand::EvaluateLyricsViewport {
                track_id,
                source_position_s,
                max_blocks,
            } => {
                let store = self.store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Base occupée.")
                })?;
                let doc = store.get_latest_lyrics(track_id)?.ok_or_else(|| {
                    KaraokError::new(ErrorCode::NotFound, "Aucune parole disponible.")
                })?;
                let vp: LyricsViewportState = evaluate_viewport(&doc, source_position_s, max_blocks);
                to_json(vp)
            }
            ControllerCommand::LoadTrackForPlayback {
                track_id,
                use_stems,
                request_generation,
            } => to_json(self.load_track_for_playback(
                track_id,
                use_stems,
                request_generation,
            )?),
            ControllerCommand::Play { request_generation } => {
                to_json(self.play(request_generation)?)
            }
            ControllerCommand::Pause => to_json(self.pause()?),
            ControllerCommand::Seek {
                position_s,
                request_generation,
            } => to_json(self.seek(position_s, request_generation)?),
            ControllerCommand::ListModels => {
                let ms = self.model_store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Registre modèles occupé.")
                })?;
                to_json(ms.list_manifests())
            }
            ControllerCommand::ImportVerifiedModel {
                file_path,
                manifest,
            } => {
                let mut ms = self.model_store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Registre modèles occupé.")
                })?;
                let installed = ms.import_verified_local_model(&PathBuf::from(file_path), *manifest)?;
                to_json(installed)
            }
            ControllerCommand::GetDiagnostics => {
                let export = RedactedDiagnosticExport::build(&self.counters, &self.log);
                to_json(export)
            }
            ControllerCommand::ListLatencyProfiles => {
                let store = self.store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Base occupée.")
                })?;
                to_json(store.list_latency_profiles()?)
            }
            ControllerCommand::PurgeCache => {
                let store = self.store.lock().map_err(|_| {
                    KaraokError::new(ErrorCode::InternalError, "Base occupée.")
                })?;
                let count = store.purge_reconstructible_cache(&self.paths)?;
                to_json(count)
            }
        }
    }
}

impl Drop for AppController {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn to_json<T: Serialize>(value: T) -> KaraokResult<serde_json::Value> {
    serde_json::to_value(value).map_err(|e| {
        KaraokError::new(
            ErrorCode::InternalError,
            format!("Sérialisation réponse échouée : {e}"),
        )
    })
}

fn stale_playback_request_error() -> KaraokError {
    KaraokError::new(
        ErrorCode::JobCancelled,
        "Chargement ignoré : un morceau plus récent a été sélectionné.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use karaok_audio_device::{HalChangeSignal, SimulatedHalBackend};
    use karaok_media::write_stereo_f32_wav_atomic;
    use std::f32::consts::TAU;
    use tempfile::tempdir;

    #[test]
    fn end_to_end_import_prepare_lyrics_and_hotplug_nothing_simulation() {
        let dir = tempdir().unwrap();
        let paths = StoragePaths::from_roots(dir.path().join("app"), dir.path().join("cache")).unwrap();
        let sim_hal = Arc::new(SimulatedHalBackend::new(
            SimulatedHalBackend::fixture_with_nothing_usb_and_builtin(),
        ));
        let ctrl = AppController::new(paths, sim_hal.clone()).unwrap();

        // 1. Select Nothing Headphone (a) USB-C explicitly by UID
        let audio_state = ctrl.get_audio_state().unwrap();
        let nothing_in = audio_state
            .inputs
            .iter()
            .find(|d| d.nothing_headphone_a_candidate)
            .unwrap()
            .clone();
        let resp = ctrl.dispatch(ControllerCommand::SelectInput {
            selector: DeviceSelector::Fixed {
                uid: nothing_in.uid.clone(),
                last_known_name: nothing_in.name.clone(),
                confirmed_by_user: true,
            },
            confirm_reconnect: false,
        });
        assert!(matches!(resp, ControllerResponse::Success { .. }));

        // Start mic capture explicitly -> succeeds on Nothing USB-C (simulated)
        let transitions_before = ctrl.read_rt_counters().route_transitions;
        let mic_fmt = ctrl.start_mic_capture().unwrap();
        assert_eq!(mic_fmt.device_name, "Nothing Headphone (a)");
        assert!(mic_fmt.is_simulated);

        // Regression B6: stop + restart mic MUST NOT rebuild output pipeline!
        ctrl.stop_mic_capture();
        let mic_fmt_again = ctrl.start_mic_capture().unwrap();
        assert_eq!(mic_fmt_again.device_name, "Nothing Headphone (a)");
        assert_eq!(
            ctrl.read_rt_counters().route_transitions,
            transitions_before,
            "Toggling microphone must NOT tear down output pipeline (B6)"
        );

        // Simulate unplugging Nothing Headphone (a) USB-C -> MUST stop mic and NOT fall back to Mac mic!
        let remaining: Vec<_> = audio_state
            .inputs
            .iter()
            .chain(audio_state.outputs.iter())
            .filter(|d| !d.nothing_headphone_a_candidate)
            .cloned()
            .collect();
        sim_hal.set_devices(remaining, HalChangeSignal::DevicesListChanged);
        ctrl.reconcile_devices(true);

        let after_unplug = ctrl.get_audio_state().unwrap();
        assert_eq!(after_unplug.input_status, RouteStatus::UnavailablePreferred);
        assert!(!after_unplug.mic_capture_active);

        // 2. Import synthetic WAV with 1.5s intro + 1.5s sung voice, attach untimed lyrics, run preparation
        let wav_path = dir.path().join("song.wav");
        let sr = 48_000u32;
        let frames = sr as usize * 3;
        let mut pcm = vec![0.0_f32; frames * 2];
        for i in 0..frames {
            let t = i as f32 / sr as f32;
            let bass_l = 0.2 * (TAU * 90.0 * t).sin();
            let bass_r = -0.2 * (TAU * 90.0 * t).sin();
            let voc = if t >= 1.5 {
                0.45 * (TAU * 440.0 * t).sin() + 0.15 * (TAU * 880.0 * t).sin()
            } else {
                0.0
            };
            pcm[i * 2] = bass_l + voc;
            pcm[i * 2 + 1] = bass_r + voc;
        }
        write_stereo_f32_wav_atomic(&wav_path, sr, &pcm).unwrap();

        let track = ctrl.import_track(wav_path.to_str().unwrap()).unwrap();

        // A superseded asynchronous UI load must never replace or restart the
        // latest requested track (regression: Swift generation guard used to
        // protect only UI state, not the Rust engine state).
        ctrl.load_track_for_playback(track.id, None, Some(2))
            .unwrap();
        let stale_load = ctrl
            .load_track_for_playback(track.id, None, Some(1))
            .unwrap_err();
        assert_eq!(stale_load.code, ErrorCode::JobCancelled);
        let stale_play = ctrl.play(Some(1)).unwrap_err();
        assert_eq!(stale_play.code, ErrorCode::JobCancelled);
        ctrl.play(Some(2)).unwrap();
        ctrl.pause().unwrap();

        let _ = ctrl.dispatch(ControllerCommand::SetLyricsText {
            track_id: track.id,
            raw_text: "Première ligne chantée\nDeuxième ligne lumineuse".into(),
            provenance: "user_pasted".into(),
            language: Some("fr".into()),
        });

        let _ = ctrl.start_preparation(track.id).unwrap();
        // Wait for async worker to reach terminal job state (Completed or NeedsReview)
        for _ in 0..100 {
            thread::sleep(std::time::Duration::from_millis(40));
            let det = ctrl.get_track_detail(track.id).unwrap();
            let finished = det.latest_job.as_ref().is_some_and(|j| {
                matches!(
                    j.state,
                    karaok_types::AnalysisJobState::Completed
                        | karaok_types::AnalysisJobState::NeedsReview
                )
            });
            if finished && det.stems_ready && det.vocal_activity.is_some() {
                let first_cue = det.lyrics.as_ref().unwrap().first_lyric_cue_s.unwrap();
                assert!(
                    first_cue >= 1.1,
                    "Aligned first cue must preserve 1.5s intro, got {first_cue:.2}s"
                );
                return;
            }
        }
        panic!("Offline preparation did not complete in time");
    }
}
