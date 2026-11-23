//! Real-time Rust audio engine: master output clock, stems/original mixer, async mic drift resampler,
//! streaming disk media feeder, and lock-free Seqlock Transport/Meter publishing (§6, REQ-009, REQ-013, REQ-016).
//!
//! Strict RT callback guarantees:
//! - ZERO heap allocations, ZERO mutex locks, ZERO disk/SQLite/network/ML calls in `render_stereo_interleaved` or `push_interleaved`.
//! - Playback PCM & stems stream in bounded 4096-frame chunks from disk on an off-RT thread into `rtrb`.
//! - `SetInputSampleRate(f64)` updates the asynchronous drift resampler dynamically for 16/24/44.1/48/96 kHz mics (B2).
//! - `AtomicEngineState` uses a lock-free seqlock preventing torn TransportSnapshot reads (I5).
//! - Control-side authoritative `duration_s` & EOF handling fix immediate `load -> seek` and post-EOF replay (B7).

use karaok_diagnostics::RtCounters;
use karaok_types::{
    compute_presentation_source_position_s, MeterSnapshot, MixParams, PresentationQuality,
    TransportSnapshot, TransportState, VoiceDspParams,
};
use karaok_voice_dsp::{MasterLimiter, SmoothedValue, VoiceDspChain};
use rtrb::{Consumer, Producer, RingBuffer};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const PLAYBACK_RING_FRAMES: usize = 48_000; // 1.0s bounded pre-buffer at 48 kHz
const MIC_RING_SAMPLES: usize = 16_384; // ~340 ms bounded mono mic buffer at 48 kHz
const MIC_TARGET_OCCUPANCY_SAMPLES: usize = 512; // ~10.7 ms target jitter/drift cushion
const MAX_RT_BLOCK_FRAMES: usize = 4096;
const FEEDER_CHUNK_FRAMES: usize = 4096;

/// Packed audio chunk streamed from the off-RT media feeder thread into the RT output callback.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlaybackFrame {
    pub generation: u64,
    pub source_frame_index: u64,
    pub orig_l: f32,
    pub orig_r: f32,
    pub acc_l: f32,
    pub acc_r: f32,
    pub voc_l: f32,
    pub voc_r: f32,
    pub has_stems: bool,
}

/// Lock-free command pushed from control thread to RT render thread.
#[derive(Debug, Clone, Copy)]
pub enum RtCommand {
    SetVoiceParams(VoiceDspParams),
    SetMixParams(MixParams),
    SetTransportState {
        state: TransportState,
        generation: u64,
        source_position_s: f64,
    },
    SetTrackMetadata {
        duration_s: f64,
        source_sample_rate: u32,
        stems_available: bool,
    },
    SetPresentationLatencyMs(f64),
    SetMicActive(bool),
    SetInputSampleRate(f64),
    ResetDriftEstimator,
}

/// Shared lock-free atomic snapshot store with seqlock coherence for `TransportSnapshot` (I5).
pub struct AtomicEngineState {
    // Seqlock counter: odd while writer updates transport fields, even when coherent.
    transport_seq: AtomicU64,
    generation: AtomicU64,
    transport_state: AtomicU8,
    source_position_bits: AtomicU64,
    duration_bits: AtomicU64,
    monotonic_host_ns: AtomicU64,
    playback_ratio_bits: AtomicU64,
    presentation_latency_ms_bits: AtomicU64,
    presentation_quality: AtomicU8,
    using_stems: AtomicBool,
    stems_available: AtomicBool,

    // Meters & pitch telemetry (independent single-value atomics, transient drop allowed §9.4)
    input_pre_peak: AtomicU32,
    input_pre_rms: AtomicU32,
    input_clipped: AtomicBool,
    voice_post_peak: AtomicU32,
    voice_post_rms: AtomicU32,
    output_peak: AtomicU32,
    output_clipped: AtomicBool,
    detected_f0_hz: AtomicU32,
    target_f0_hz: AtomicU32,
    pitch_confidence: AtomicU32,
    voiced: AtomicBool,
    pitch_correction_cents: AtomicU32,
    active_mic_capture: AtomicBool,

    epoch: Instant,
}

impl AtomicEngineState {
    pub fn new() -> Self {
        Self {
            transport_seq: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            transport_state: AtomicU8::new(TransportState::Stopped as u8),
            source_position_bits: AtomicU64::new(0.0_f64.to_bits()),
            duration_bits: AtomicU64::new(0.0_f64.to_bits()),
            monotonic_host_ns: AtomicU64::new(0),
            playback_ratio_bits: AtomicU64::new(1.0_f64.to_bits()),
            presentation_latency_ms_bits: AtomicU64::new(0.0_f64.to_bits()),
            presentation_quality: AtomicU8::new(PresentationQuality::Uncertain as u8),
            using_stems: AtomicBool::new(false),
            stems_available: AtomicBool::new(false),
            input_pre_peak: AtomicU32::new(0),
            input_pre_rms: AtomicU32::new(0),
            input_clipped: AtomicBool::new(false),
            voice_post_peak: AtomicU32::new(0),
            voice_post_rms: AtomicU32::new(0),
            output_peak: AtomicU32::new(0),
            output_clipped: AtomicBool::new(false),
            detected_f0_hz: AtomicU32::new(0),
            target_f0_hz: AtomicU32::new(0),
            pitch_confidence: AtomicU32::new(0),
            voiced: AtomicBool::new(false),
            pitch_correction_cents: AtomicU32::new(0),
            active_mic_capture: AtomicBool::new(false),
            epoch: Instant::now(),
        }
    }

    #[inline]
    pub fn monotonic_ns(&self) -> u64 {
        self.epoch.elapsed().as_nanos() as u64
    }

    pub fn publish_transport(&self, snap: &TransportSnapshot) {
        // Begin seqlock write (odd). Mask to even base as defense-in-depth so readers can never wedge (P2-B1).
        let s = self.transport_seq.load(Ordering::Relaxed) & !1;
        self.transport_seq.store(s.wrapping_add(1), Ordering::Release);
        std::sync::atomic::fence(Ordering::Release);

        self.generation.store(snap.generation, Ordering::Relaxed);
        self.transport_state
            .store(snap.state as u8, Ordering::Relaxed);
        self.source_position_bits
            .store(snap.source_position_s.to_bits(), Ordering::Relaxed);
        self.duration_bits
            .store(snap.duration_s.to_bits(), Ordering::Relaxed);
        self.monotonic_host_ns
            .store(snap.monotonic_host_ns, Ordering::Relaxed);
        self.playback_ratio_bits
            .store(snap.playback_ratio.to_bits(), Ordering::Relaxed);
        self.presentation_latency_ms_bits
            .store(snap.presentation_latency_ms.to_bits(), Ordering::Relaxed);
        self.presentation_quality
            .store(snap.quality as u8, Ordering::Relaxed);
        self.using_stems.store(snap.using_stems, Ordering::Relaxed);
        self.stems_available
            .store(snap.stems_available, Ordering::Relaxed);

        // End seqlock write (even)
        std::sync::atomic::fence(Ordering::Release);
        self.transport_seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// Directly mark `RouteLost` from control thread after stopping a lost output stream (I5).
    pub fn publish_route_lost(&self) {
        let mut snap = self.read_transport();
        snap.state = TransportState::RouteLost;
        snap.quality = PresentationQuality::Uncertain;
        snap.monotonic_host_ns = self.monotonic_ns();
        self.publish_transport(&snap);
    }

    pub fn read_transport(&self) -> TransportSnapshot {
        for _ in 0..8 {
            let seq1 = self.transport_seq.load(Ordering::Acquire);
            if seq1 & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            let generation = self.generation.load(Ordering::Relaxed);
            let state = match self.transport_state.load(Ordering::Relaxed) {
                1 => TransportState::Playing,
                2 => TransportState::Paused,
                3 => TransportState::Buffering,
                4 => TransportState::RouteLost,
                _ => TransportState::Stopped,
            };
            let quality = match self.presentation_quality.load(Ordering::Relaxed) {
                0 => PresentationQuality::BackendTimestamp,
                1 => PresentationQuality::OutputCursorCompensated,
                _ => PresentationQuality::Uncertain,
            };
            let snap = TransportSnapshot {
                generation,
                state,
                source_position_s: f64::from_bits(
                    self.source_position_bits.load(Ordering::Relaxed),
                ),
                duration_s: f64::from_bits(self.duration_bits.load(Ordering::Relaxed)),
                monotonic_host_ns: self.monotonic_host_ns.load(Ordering::Relaxed),
                playback_ratio: f64::from_bits(self.playback_ratio_bits.load(Ordering::Relaxed)),
                presentation_latency_ms: f64::from_bits(
                    self.presentation_latency_ms_bits.load(Ordering::Relaxed),
                ),
                quality,
                using_stems: self.using_stems.load(Ordering::Relaxed),
                stems_available: self.stems_available.load(Ordering::Relaxed),
            };
            std::sync::atomic::fence(Ordering::Acquire);
            let seq2 = self.transport_seq.load(Ordering::Acquire);
            if seq1 == seq2 {
                return snap;
            }
        }
        TransportSnapshot::default()
    }

    pub fn publish_meters(&self, m: &MeterSnapshot) {
        self.input_pre_peak
            .store(m.input_pre_peak.to_bits(), Ordering::Relaxed);
        self.input_pre_rms
            .store(m.input_pre_rms.to_bits(), Ordering::Relaxed);
        self.input_clipped
            .store(m.input_clipped, Ordering::Relaxed);
        self.voice_post_peak
            .store(m.voice_post_peak.to_bits(), Ordering::Relaxed);
        self.voice_post_rms
            .store(m.voice_post_rms.to_bits(), Ordering::Relaxed);
        self.output_peak
            .store(m.output_peak.to_bits(), Ordering::Relaxed);
        self.output_clipped
            .store(m.output_clipped, Ordering::Relaxed);
        self.detected_f0_hz
            .store(m.detected_f0_hz.to_bits(), Ordering::Relaxed);
        self.target_f0_hz
            .store(m.target_f0_hz.to_bits(), Ordering::Relaxed);
        self.pitch_confidence
            .store(m.pitch_confidence.to_bits(), Ordering::Relaxed);
        self.voiced.store(m.voiced, Ordering::Relaxed);
        self.pitch_correction_cents
            .store(m.pitch_correction_cents.to_bits(), Ordering::Relaxed);
        self.active_mic_capture
            .store(m.active_mic_capture, Ordering::Relaxed);
    }

    pub fn read_meters(&self) -> MeterSnapshot {
        MeterSnapshot {
            input_pre_peak: f32::from_bits(self.input_pre_peak.load(Ordering::Relaxed)),
            input_pre_rms: f32::from_bits(self.input_pre_rms.load(Ordering::Relaxed)),
            input_clipped: self.input_clipped.load(Ordering::Relaxed),
            voice_post_peak: f32::from_bits(self.voice_post_peak.load(Ordering::Relaxed)),
            voice_post_rms: f32::from_bits(self.voice_post_rms.load(Ordering::Relaxed)),
            output_peak: f32::from_bits(self.output_peak.load(Ordering::Relaxed)),
            output_clipped: self.output_clipped.load(Ordering::Relaxed),
            detected_f0_hz: f32::from_bits(self.detected_f0_hz.load(Ordering::Relaxed)),
            target_f0_hz: f32::from_bits(self.target_f0_hz.load(Ordering::Relaxed)),
            pitch_confidence: f32::from_bits(self.pitch_confidence.load(Ordering::Relaxed)),
            voiced: self.voiced.load(Ordering::Relaxed),
            pitch_correction_cents: f32::from_bits(
                self.pitch_correction_cents.load(Ordering::Relaxed),
            ),
            active_mic_capture: self.active_mic_capture.load(Ordering::Relaxed),
        }
    }
}

impl Default for AtomicEngineState {
    fn default() -> Self {
        Self::new()
    }
}

/// Adaptive asynchronous fractional resampler compensating both nominal sample-rate differences
/// (e.g. 16/24/44.1 kHz input -> 48 kHz output) AND clock drift between distinct devices (§6.4, B2, AC-15).
pub struct AsyncDriftResampler {
    nominal_ratio: f64,
    current_ratio: f64,
    integral_err: f64,
    frac_pos: f64,
    prev_sample: f32,
    curr_sample: f32,
    target_occupancy: f64,
    primed: bool,
}

impl AsyncDriftResampler {
    pub fn new(input_sample_rate: f64, output_sample_rate: f64, target_occupancy: usize) -> Self {
        let nom = (input_sample_rate / output_sample_rate.max(8000.0)).clamp(0.15, 4.0);
        Self {
            nominal_ratio: nom,
            current_ratio: nom,
            integral_err: 0.0,
            frac_pos: 0.0,
            prev_sample: 0.0,
            curr_sample: 0.0,
            target_occupancy: target_occupancy.max(64) as f64,
            primed: false,
        }
    }

    pub fn set_sample_rates(&mut self, input_sr: f64, output_sr: f64) {
        self.nominal_ratio = (input_sr / output_sr.max(8000.0)).clamp(0.15, 4.0);
        // Scale target occupancy proportionally to input rate so cushion stays ~10 ms
        self.target_occupancy = ((input_sr * 0.0107).round()).clamp(128.0, 2048.0);
        self.reset();
    }

    pub fn reset(&mut self) {
        self.current_ratio = self.nominal_ratio;
        self.integral_err = 0.0;
        self.frac_pos = 0.0;
        self.prev_sample = 0.0;
        self.curr_sample = 0.0;
        self.primed = false;
    }

    pub fn current_ratio(&self) -> f64 {
        self.current_ratio
    }

    /// Consume from lock-free mic `Consumer<f32>` into `out_mono` (`frames` samples),
    /// adapting `current_ratio` smoothly around `nominal_ratio`.
    pub fn pull_resampled(
        &mut self,
        mic_cons: &mut Consumer<f32>,
        out_mono: &mut [f32],
    ) -> (bool, usize) {
        let available = mic_cons.slots();
        if !self.primed {
            if available < (self.target_occupancy as usize / 2).max(32) {
                out_mono.fill(0.0);
                return (false, available);
            }
            self.curr_sample = mic_cons.pop().unwrap_or(0.0);
            self.prev_sample = self.curr_sample;
            self.primed = true;
        }

        // Midpoint occupancy accounts for nominal input consumption over this output block
        let expected_consumed = out_mono.len() as f64 * self.nominal_ratio;
        let midpoint_occ = (mic_cons.slots() as f64) - (expected_consumed * 0.5);
        let err = (midpoint_occ - self.target_occupancy) / self.target_occupancy;
        let bounded_err = err.clamp(-1.0, 1.0);
        self.integral_err = (self.integral_err + bounded_err * 0.0002).clamp(-0.004, 0.004);
        let correction = (bounded_err * 0.003 + self.integral_err).clamp(-0.006, 0.006);
        self.current_ratio = self.nominal_ratio * (1.0 + correction);

        let mut underrun = false;
        for sample in out_mono.iter_mut() {
            while self.frac_pos >= 1.0 {
                self.frac_pos -= 1.0;
                self.prev_sample = self.curr_sample;
                match mic_cons.pop() {
                    Ok(s) => self.curr_sample = s,
                    Err(_) => {
                        self.curr_sample *= 0.92;
                        underrun = true;
                    }
                }
            }
            let frac = self.frac_pos as f32;
            *sample = self.prev_sample + (self.curr_sample - self.prev_sample) * frac;
            self.frac_pos += self.current_ratio;
        }

        // Trim runaway backlog on OS pause in O(1) via read_chunk
        let max_backlog = (self.target_occupancy as usize) * 8;
        if mic_cons.slots() > max_backlog {
            let trim = mic_cons.slots() - (self.target_occupancy as usize) * 2;
            if let Ok(chunk) = mic_cons.read_chunk(trim) {
                chunk.commit_all();
            }
        }

        (underrun, mic_cons.slots())
    }
}

/// Real-time input producer handle owned by the Core Audio input callback thread (returned on drop for reuse — B6).
pub struct RtMicInputHandle {
    mic_prod: Producer<f32>,
    counters: Arc<RtCounters>,
    input_channel_index: Arc<AtomicU32>,
    active: Arc<AtomicBool>,
}

impl RtMicInputHandle {
    /// Called strictly inside the Core Audio input callback. ZERO allocations, ZERO locks.
    pub fn push_interleaved(&mut self, interleaved: &[f32], channels: usize) {
        self.counters.inc_input_callbacks();
        if !self.active.load(Ordering::Relaxed) || channels == 0 {
            return;
        }
        let ch = (self.input_channel_index.load(Ordering::Relaxed) as usize).min(channels - 1);
        let frames = interleaved.len() / channels;
        let mut overruns = 0u64;
        for f in 0..frames {
            let s = interleaved[f * channels + ch];
            let clean = if s.is_finite() { s } else { 0.0 };
            if self.mic_prod.push(clean).is_err() {
                overruns += 1;
            }
        }
        if overruns > 0 {
            self.counters.inc_mic_overruns();
        }
    }

    pub fn push_mono(&mut self, mono: &[f32]) {
        self.push_interleaved(mono, 1);
    }
}

/// Real-time output render processor owned exclusively by the Core Audio output callback thread.
pub struct RtOutputProcessor {
    output_sample_rate: f64,
    cmd_cons: Consumer<RtCommand>,
    playback_cons: Consumer<PlaybackFrame>,
    mic_cons: Consumer<f32>,
    state: Arc<AtomicEngineState>,
    counters: Arc<RtCounters>,

    voice_dsp: VoiceDspChain,
    limiter: MasterLimiter,
    drift_resampler: AsyncDriftResampler,

    // Smoothed mix controls & click-free fades (§6.1, I3)
    mix_params: MixParams,
    acc_gain: SmoothedValue,
    orig_voc_gain: SmoothedValue,
    stems_blend: SmoothedValue,
    monitor_gain: SmoothedValue,
    mic_active_fade: SmoothedValue,
    transport_fade: SmoothedValue,

    // Master output timeline state
    transport_state: TransportState,
    generation: u64,
    source_sample_rate: u32,
    duration_s: f64,
    rendered_source_s: f64,
    presentation_latency_ms: f64,
    stems_available: bool,
    mic_active: bool,
    mic_dsp_needs_reset: bool,

    // Preallocated scratch buffers (ZERO RT allocation)
    mic_mono_scratch: Vec<f32>,
    voice_l_scratch: Vec<f32>,
    voice_r_scratch: Vec<f32>,
}

impl RtOutputProcessor {
    fn drain_commands(&mut self) {
        while let Ok(cmd) = self.cmd_cons.pop() {
            match cmd {
                RtCommand::SetVoiceParams(vp) => {
                    let clean = vp.clamped();
                    self.voice_dsp.set_params(clean);
                    let mon_target = if clean.monitoring_enabled && !clean.mic_muted {
                        clean.monitoring_level
                    } else {
                        0.0
                    };
                    self.monitor_gain.set_target(mon_target);
                }
                RtCommand::SetMixParams(mp) => {
                    let clean = mp.clamped();
                    self.mix_params = clean;
                    self.acc_gain.set_target(clean.accompaniment_gain);
                    self.orig_voc_gain.set_target(clean.original_vocal_gain);
                    self.stems_blend.set_target(if clean.use_stems && self.stems_available {
                        1.0
                    } else {
                        0.0
                    });
                    self.limiter.set_ceiling_db(clean.master_limiter_ceiling_db);
                }
                RtCommand::SetTransportState {
                    state,
                    generation,
                    source_position_s,
                } => {
                    if generation != self.generation {
                        self.generation = generation;
                        self.rendered_source_s = source_position_s.max(0.0);
                        // O(1) bounded drop of stale frames belonging to previous seek generation (I4)
                        let slots = self.playback_cons.slots();
                        if slots > 0 {
                            if let Ok(chunk) = self.playback_cons.read_chunk(slots) {
                                let (s1, s2) = chunk.as_slices();
                                let mut stale = 0usize;
                                for f in s1.iter().chain(s2.iter()) {
                                    if f.generation < generation {
                                        stale += 1;
                                    } else {
                                        break;
                                    }
                                }
                                chunk.commit(stale);
                            }
                        }
                    }
                    self.transport_state = state;
                    self.transport_fade
                        .set_target(if state == TransportState::Playing {
                            1.0
                        } else {
                            0.0
                        });
                }
                RtCommand::SetTrackMetadata {
                    duration_s,
                    source_sample_rate,
                    stems_available,
                } => {
                    self.duration_s = duration_s.max(0.0);
                    self.source_sample_rate = source_sample_rate.max(8000);
                    self.stems_available = stems_available;
                    self.stems_blend
                        .set_target(if self.mix_params.use_stems && stems_available {
                            1.0
                        } else {
                            0.0
                        });
                }
                RtCommand::SetPresentationLatencyMs(ms) => {
                    if ms.is_finite() && ms >= 0.0 {
                        self.presentation_latency_ms = ms;
                    }
                }
                RtCommand::SetMicActive(active) => {
                    self.mic_active = active;
                    if active {
                        self.mic_dsp_needs_reset = false;
                        self.mic_active_fade.set_target(1.0);
                    } else {
                        // Ramp out cleanly before resetting DSP state (zero hard-zero click - I3!)
                        self.mic_active_fade.set_target(0.0);
                        self.mic_dsp_needs_reset = true;
                        self.drift_resampler.reset();
                    }
                }
                RtCommand::SetInputSampleRate(in_sr) => {
                    if in_sr.is_finite() && in_sr >= 8000.0 {
                        self.drift_resampler
                            .set_sample_rates(in_sr, self.output_sample_rate);
                        // Drain stale samples from previous rate in O(1)
                        let slots = self.mic_cons.slots();
                        if slots > 0 {
                            if let Ok(chunk) = self.mic_cons.read_chunk(slots) {
                                chunk.commit_all();
                            }
                        }
                    }
                }
                RtCommand::ResetDriftEstimator => {
                    self.drift_resampler.reset();
                }
            }
        }
    }

    /// Render `total_frames` interleaved stereo output samples (`out_interleaved.len() >= total_frames * 2`).
    /// Processes arbitrarily large host buffers in chunks of `MAX_RT_BLOCK_FRAMES` (I4).
    /// STRICTLY RT-SAFE: zero allocations, zero locks, deterministic timing!
    pub fn render_stereo_interleaved(&mut self, out_interleaved: &mut [f32], total_frames: usize) {
        let start = Instant::now();
        self.counters.inc_output_callbacks();
        self.drain_commands();

        let cap_frames = out_interleaved.len() / 2;
        let frames_to_render = total_frames.min(cap_frames);
        if frames_to_render == 0 {
            return;
        }

        let mut offset = 0usize;
        let mut last_voice_telem = karaok_voice_dsp::VoiceBlockTelemetry::default();
        let mut out_peak = 0.0_f32;
        let mut out_clipped = false;
        let mut total_sanitized = 0u64;
        let mut underrun_this_block = false;
        let source_sr = self.source_sample_rate.max(8000) as f64;

        while offset < frames_to_render {
            let chunk_frames = (frames_to_render - offset).min(MAX_RT_BLOCK_FRAMES);

            // 1. Pull resampled mic block (or smooth fade-out tail on disable - I3)
            let run_voice = self.mic_active || self.mic_active_fade.current() > 1e-4;
            if run_voice {
                if self.mic_active {
                    let (mic_underrun, occ) = self.drift_resampler.pull_resampled(
                        &mut self.mic_cons,
                        &mut self.mic_mono_scratch[..chunk_frames],
                    );
                    if mic_underrun {
                        self.counters.inc_mic_underruns();
                    }
                    self.counters.set_mic_ring_occupancy(occ as u32);
                    self.counters
                        .set_mic_resample_ratio(self.drift_resampler.current_ratio());
                } else {
                    self.mic_mono_scratch[..chunk_frames].fill(0.0);
                }
                last_voice_telem = self.voice_dsp.process_block(
                    &self.mic_mono_scratch[..chunk_frames],
                    &mut self.voice_l_scratch[..chunk_frames],
                    &mut self.voice_r_scratch[..chunk_frames],
                );
            } else {
                if self.mic_dsp_needs_reset {
                    self.voice_dsp.reset();
                    self.mic_dsp_needs_reset = false;
                }
                self.voice_l_scratch[..chunk_frames].fill(0.0);
                self.voice_r_scratch[..chunk_frames].fill(0.0);
            }

            // 2. Mix music + smoothed monitored voice + lookahead Master Limiter
            for i in 0..chunk_frames {
                let tfade = self.transport_fade.next();
                let (music_l, music_r) = if self.transport_state == TransportState::Playing {
                    let mut popped: Option<PlaybackFrame> = None;
                    while let Ok(f) = self.playback_cons.pop() {
                        if f.generation == self.generation {
                            popped = Some(f);
                            break;
                        }
                    }
                    if let Some(frame) = popped {
                        self.rendered_source_s = frame.source_frame_index as f64 / source_sr;
                        let acc_g = self.acc_gain.next();
                        let voc_g = self.orig_voc_gain.next();
                        let blend = if frame.has_stems {
                            self.stems_blend.next()
                        } else {
                            let _ = self.stems_blend.next();
                            0.0
                        };
                        let stem_l = frame.acc_l * acc_g + frame.voc_l * voc_g;
                        let stem_r = frame.acc_r * acc_g + frame.voc_r * voc_g;
                        let orig_l = frame.orig_l * acc_g;
                        let orig_r = frame.orig_r * acc_g;
                        let ml = orig_l * (1.0 - blend) + stem_l * blend;
                        let mr = orig_r * (1.0 - blend) + stem_r * blend;
                        (ml * tfade, mr * tfade)
                    } else {
                        if self.duration_s > 0.0 && self.rendered_source_s >= self.duration_s - 0.03
                        {
                            self.transport_state = TransportState::Stopped;
                            self.rendered_source_s = self.duration_s;
                        } else {
                            underrun_this_block = true;
                        }
                        (0.0, 0.0)
                    }
                } else {
                    let _ = self.acc_gain.next();
                    let _ = self.orig_voc_gain.next();
                    let _ = self.stems_blend.next();
                    (0.0, 0.0)
                };

                let mon_g = self.monitor_gain.next() * self.mic_active_fade.next();
                let mix_l = music_l + self.voice_l_scratch[i] * mon_g;
                let mix_r = music_r + self.voice_r_scratch[i] * mon_g;

                let (lim_l, lim_r, san, clipped) = self.limiter.process(mix_l, mix_r);
                total_sanitized += san;
                out_clipped |= clipped;
                out_peak = out_peak.max(lim_l.abs()).max(lim_r.abs());

                let out_idx = (offset + i) * 2;
                out_interleaved[out_idx] = lim_l;
                out_interleaved[out_idx + 1] = lim_r;
            }

            offset += chunk_frames;
        }

        if underrun_this_block {
            self.counters.inc_music_underruns();
        }
        self.counters.add_non_finite_sanitizations(total_sanitized);

        // 3. Publish seqlock-coherent TransportSnapshot (subtract presentation latency ONLY while Playing - I5!)
        let limiter_ms = (self.limiter.latency_samples() as f64 / self.output_sample_rate) * 1000.0;
        let total_pres_ms = self.presentation_latency_ms + limiter_ms;
        let applied_delay_s = if self.transport_state == TransportState::Playing {
            total_pres_ms / 1000.0
        } else {
            0.0
        };
        let presentation_source_s = compute_presentation_source_position_s(
            self.rendered_source_s,
            applied_delay_s,
            1.0,
            self.duration_s,
            false,
        );

        let quality = if self.transport_state == TransportState::RouteLost {
            PresentationQuality::Uncertain
        } else {
            PresentationQuality::OutputCursorCompensated
        };

        self.state.publish_transport(&TransportSnapshot {
            generation: self.generation,
            state: self.transport_state,
            source_position_s: presentation_source_s,
            duration_s: self.duration_s,
            monotonic_host_ns: self.state.monotonic_ns(),
            playback_ratio: 1.0,
            presentation_latency_ms: total_pres_ms,
            quality,
            using_stems: self.mix_params.use_stems && self.stems_available,
            stems_available: self.stems_available,
        });

        self.state.publish_meters(&MeterSnapshot {
            input_pre_peak: last_voice_telem.pre_peak,
            input_pre_rms: last_voice_telem.pre_rms,
            input_clipped: last_voice_telem.input_clipped,
            voice_post_peak: last_voice_telem.post_peak,
            voice_post_rms: last_voice_telem.post_rms,
            output_peak: out_peak,
            output_clipped: out_clipped,
            detected_f0_hz: last_voice_telem.detected_f0_hz,
            target_f0_hz: last_voice_telem.target_f0_hz,
            pitch_confidence: last_voice_telem.pitch_confidence,
            voiced: last_voice_telem.voiced,
            pitch_correction_cents: last_voice_telem.pitch_correction_cents,
            active_mic_capture: self.mic_active,
        });

        let elapsed_us = start.elapsed().as_micros() as u32;
        let budget_us = ((frames_to_render as f64 / self.output_sample_rate) * 1_000_000.0) as u32;
        self.counters
            .record_render_duration_us(elapsed_us, budget_us);
    }

    pub fn voice_dsp_latency_ms(&self) -> f64 {
        self.voice_dsp.algorithmic_latency_ms()
            + (self.limiter.latency_samples() as f64 / self.output_sample_rate) * 1000.0
    }
}

/// Commands sent from control thread to the off-RT streaming media feeder thread.
#[derive(Debug)]
enum FeederCommand {
    LoadTrack {
        generation: u64,
        canonical_wav: PathBuf,
        vocal_stem_wav: Option<PathBuf>,
        accompaniment_stem_wav: Option<PathBuf>,
        output_sample_rate: u32,
        start_position_s: f64,
    },
    Seek {
        generation: u64,
        position_s: f64,
    },
    SetPlaying(bool),
    Stop,
    Shutdown,
}

/// High-level non-RT control handle owning the streaming media feeder thread and lock-free RT queues.
pub struct AudioEngineHandle {
    cmd_prod: Producer<RtCommand>,
    feeder_tx: crossbeam_channel::Sender<FeederCommand>,
    feeder_thread: Option<JoinHandle<()>>,
    state: Arc<AtomicEngineState>,
    counters: Arc<RtCounters>,
    input_channel_index: Arc<AtomicU32>,
    mic_input_active: Arc<AtomicBool>,
    generation: u64,
    output_sample_rate: u32,
    voice_dsp_latency_ms: f64,
    // Authoritative control-side track metadata preventing stale RT reads (B7)
    control_duration_s: f64,
    control_playing: bool,
    control_loaded: bool,
}

impl AudioEngineHandle {
    pub fn create(
        output_sample_rate: u32,
        input_sample_rate: u32,
        initial_voice: VoiceDspParams,
        initial_mix: MixParams,
        counters: Arc<RtCounters>,
    ) -> (Self, RtOutputProcessor, RtMicInputHandle) {
        let (cmd_prod, cmd_cons) = RingBuffer::<RtCommand>::new(256);
        let (playback_prod, playback_cons) = RingBuffer::<PlaybackFrame>::new(PLAYBACK_RING_FRAMES);
        let (mic_prod, mic_cons) = RingBuffer::<f32>::new(MIC_RING_SAMPLES);

        let state = Arc::new(AtomicEngineState::new());
        let input_channel_index = Arc::new(AtomicU32::new(0));
        let mic_input_active = Arc::new(AtomicBool::new(false));

        let out_sr_f = output_sample_rate.max(8000) as f32;
        let voice_dsp = VoiceDspChain::new(out_sr_f, initial_voice.clamped());
        let limiter = MasterLimiter::new(out_sr_f, initial_mix.clamped().master_limiter_ceiling_db);
        let voice_dsp_latency_ms = voice_dsp.algorithmic_latency_ms()
            + (limiter.latency_samples() as f64 / out_sr_f as f64) * 1000.0;

        let clean_mix = initial_mix.clamped();
        let rt_output = RtOutputProcessor {
            output_sample_rate: output_sample_rate.max(8000) as f64,
            cmd_cons,
            playback_cons,
            mic_cons,
            state: Arc::clone(&state),
            counters: Arc::clone(&counters),
            voice_dsp,
            limiter,
            drift_resampler: AsyncDriftResampler::new(
                input_sample_rate.max(8000) as f64,
                output_sample_rate.max(8000) as f64,
                MIC_TARGET_OCCUPANCY_SAMPLES,
            ),
            mix_params: clean_mix,
            acc_gain: SmoothedValue::new(clean_mix.accompaniment_gain, out_sr_f, 15.0),
            orig_voc_gain: SmoothedValue::new(clean_mix.original_vocal_gain, out_sr_f, 15.0),
            stems_blend: SmoothedValue::new(0.0, out_sr_f, 25.0),
            monitor_gain: SmoothedValue::new(0.0, out_sr_f, 15.0),
            mic_active_fade: SmoothedValue::new(0.0, out_sr_f, 12.0),
            transport_fade: SmoothedValue::new(0.0, out_sr_f, 12.0),
            transport_state: TransportState::Stopped,
            generation: 0,
            source_sample_rate: output_sample_rate.max(8000),
            duration_s: 0.0,
            rendered_source_s: 0.0,
            presentation_latency_ms: 0.0,
            stems_available: false,
            mic_active: false,
            mic_dsp_needs_reset: false,
            mic_mono_scratch: vec![0.0; MAX_RT_BLOCK_FRAMES],
            voice_l_scratch: vec![0.0; MAX_RT_BLOCK_FRAMES],
            voice_r_scratch: vec![0.0; MAX_RT_BLOCK_FRAMES],
        };

        let rt_mic = RtMicInputHandle {
            mic_prod,
            counters: Arc::clone(&counters),
            input_channel_index: Arc::clone(&input_channel_index),
            active: Arc::clone(&mic_input_active),
        };

        let (feeder_tx, feeder_rx) = crossbeam_channel::bounded::<FeederCommand>(32);
        let feeder_thread = thread::Builder::new()
            .name("karaok-media-feeder".into())
            .spawn(move || run_media_feeder(feeder_rx, playback_prod))
            .ok();

        let handle = Self {
            cmd_prod,
            feeder_tx,
            feeder_thread,
            state,
            counters,
            input_channel_index,
            mic_input_active,
            generation: 0,
            output_sample_rate: output_sample_rate.max(8000),
            voice_dsp_latency_ms,
            control_duration_s: 0.0,
            control_playing: false,
            control_loaded: false,
        };

        (handle, rt_output, rt_mic)
    }

    pub fn state(&self) -> Arc<AtomicEngineState> {
        Arc::clone(&self.state)
    }

    pub fn counters(&self) -> Arc<RtCounters> {
        Arc::clone(&self.counters)
    }

    pub fn output_sample_rate(&self) -> u32 {
        self.output_sample_rate
    }

    pub fn voice_dsp_latency_ms(&self) -> f64 {
        self.voice_dsp_latency_ms
    }

    pub fn send_rt_command(&mut self, cmd: RtCommand) -> bool {
        self.cmd_prod.push(cmd).is_ok()
    }

    pub fn set_voice_params(&mut self, params: VoiceDspParams) -> bool {
        self.send_rt_command(RtCommand::SetVoiceParams(params.clamped()))
    }

    pub fn set_mix_params(&mut self, params: MixParams) -> bool {
        self.send_rt_command(RtCommand::SetMixParams(params.clamped()))
    }

    pub fn set_input_channel(&self, channel_index: u32) {
        self.input_channel_index
            .store(channel_index, Ordering::Relaxed);
    }

    pub fn set_input_sample_rate(&mut self, input_sr_hz: f64) -> bool {
        self.send_rt_command(RtCommand::SetInputSampleRate(input_sr_hz))
    }

    pub fn set_mic_capture_active(&mut self, active: bool) {
        self.mic_input_active.store(active, Ordering::Relaxed);
        let _ = self.send_rt_command(RtCommand::SetMicActive(active));
    }

    pub fn set_presentation_latency_ms(&mut self, ms: f64) {
        let _ = self.send_rt_command(RtCommand::SetPresentationLatencyMs(ms));
    }

    pub fn load_track(
        &mut self,
        canonical_wav: PathBuf,
        vocal_stem_wav: Option<PathBuf>,
        accompaniment_stem_wav: Option<PathBuf>,
        duration_s: f64,
        source_sample_rate: u32,
    ) -> u64 {
        self.generation += 1;
        let gen = self.generation;
        self.control_duration_s = duration_s.max(0.0);
        self.control_playing = false;
        self.control_loaded = true;
        let stems_available = vocal_stem_wav.is_some() && accompaniment_stem_wav.is_some();
        let _ = self.send_rt_command(RtCommand::SetTrackMetadata {
            duration_s: self.control_duration_s,
            source_sample_rate,
            stems_available,
        });
        let _ = self.send_rt_command(RtCommand::SetTransportState {
            state: TransportState::Paused,
            generation: gen,
            source_position_s: 0.0,
        });
        let _ = self.feeder_tx.send(FeederCommand::LoadTrack {
            generation: gen,
            canonical_wav,
            vocal_stem_wav,
            accompaniment_stem_wav,
            output_sample_rate: self.output_sample_rate,
            start_position_s: 0.0,
        });
        gen
    }

    pub fn play(&mut self) -> u64 {
        if !self.control_loaded {
            return self.generation;
        }
        let snap = self.state.read_transport();
        // If playback previously reached EOF (Stopped at end), automatically seek to 0.0 to allow replay (B7)!
        if snap.state == TransportState::Stopped
            && self.control_duration_s > 0.0
            && snap.source_position_s >= self.control_duration_s - 0.05
        {
            self.control_playing = true;
            return self.seek(0.0);
        }
        self.control_playing = true;
        let pos = snap.source_position_s;
        let gen = self.generation;
        let _ = self.feeder_tx.send(FeederCommand::SetPlaying(true));
        let _ = self.send_rt_command(RtCommand::SetTransportState {
            state: TransportState::Playing,
            generation: gen,
            source_position_s: pos,
        });
        gen
    }

    pub fn pause(&mut self) -> u64 {
        self.control_playing = false;
        let pos = self.state.read_transport().source_position_s;
        let gen = self.generation;
        let _ = self.feeder_tx.send(FeederCommand::SetPlaying(false));
        let _ = self.send_rt_command(RtCommand::SetTransportState {
            state: TransportState::Paused,
            generation: gen,
            source_position_s: pos,
        });
        gen
    }

    pub fn stop(&mut self) -> u64 {
        self.control_playing = false;
        self.generation += 1;
        let gen = self.generation;
        let _ = self.feeder_tx.send(FeederCommand::Stop);
        let _ = self.send_rt_command(RtCommand::SetTransportState {
            state: TransportState::Stopped,
            generation: gen,
            source_position_s: 0.0,
        });
        gen
    }

    /// Seek using authoritative control-side `control_duration_s` and `control_playing` (B7).
    pub fn seek(&mut self, position_s: f64) -> u64 {
        let clamped = position_s.clamp(0.0, self.control_duration_s.max(0.0));
        self.generation += 1;
        let gen = self.generation;
        let target_state = if self.control_playing {
            TransportState::Playing
        } else {
            TransportState::Paused
        };
        let _ = self.send_rt_command(RtCommand::SetTransportState {
            state: target_state,
            generation: gen,
            source_position_s: clamped,
        });
        let _ = self.feeder_tx.send(FeederCommand::Seek {
            generation: gen,
            position_s: clamped,
        });
        gen
    }

    /// Stop feeder and inform RT processor of route loss.
    /// Callers MUST stop/dispose the active output audio stream BEFORE calling `state.publish_route_lost()`
    /// so `AtomicEngineState::publish_transport` preserves strict single-writer exclusion (P2-B1).
    pub fn mark_route_lost(&mut self) {
        self.control_playing = false;
        let pos = self.state.read_transport().source_position_s;
        let _ = self.feeder_tx.send(FeederCommand::SetPlaying(false));
        let _ = self.send_rt_command(RtCommand::SetTransportState {
            state: TransportState::RouteLost,
            generation: self.generation,
            source_position_s: pos,
        });
    }
}

impl Drop for AudioEngineHandle {
    fn drop(&mut self) {
        let _ = self.feeder_tx.send(FeederCommand::Shutdown);
        if let Some(handle) = self.feeder_thread.take() {
            let _ = handle.join();
        }
    }
}

/// Bounded-memory disk-streaming stereo f32 WAV reader (I6 — never loads whole multi-hour track into RAM!).
struct StreamingStereoWav {
    reader: hound::WavReader<BufReader<File>>,
    sample_rate: u32,
    total_frames: u64,
    chunk_start_frame: u64,
    chunk_buf: Vec<f32>, // interleaved stereo frames for current window
}

impl StreamingStereoWav {
    fn open(path: &Path) -> Result<Self, hound::Error> {
        let reader = hound::WavReader::open(path)?;
        let spec = reader.spec();
        let total_frames = (reader.duration() as u64).max(1);
        Ok(Self {
            reader,
            sample_rate: spec.sample_rate.max(8000),
            total_frames,
            chunk_start_frame: u64::MAX,
            chunk_buf: Vec::with_capacity(FEEDER_CHUNK_FRAMES * 2),
        })
    }

    fn seek_and_load_chunk(&mut self, start_frame: u64) {
        self.chunk_buf.clear();
        let clamped = start_frame.min(self.total_frames);
        self.chunk_start_frame = clamped;
        if clamped >= self.total_frames {
            return;
        }
        if self.reader.seek(clamped as u32).is_err() {
            return;
        }
        // Read up to FEEDER_CHUNK_FRAMES + 2 frames (extra frame for linear interpolation lookahead)
        let samples_to_read = (FEEDER_CHUNK_FRAMES + 2) * 2;
        for sample in self.reader.samples::<f32>().take(samples_to_read) {
            let v = sample.unwrap_or(0.0);
            self.chunk_buf.push(if v.is_finite() { v } else { 0.0 });
        }
    }

    #[inline]
    fn sample_lerp(&mut self, frame_idx: u64, frac: f32) -> (f32, f32) {
        if frame_idx >= self.total_frames {
            return (0.0, 0.0);
        }
        let chunk_frames = (self.chunk_buf.len() / 2) as u64;
        if self.chunk_start_frame == u64::MAX
            || frame_idx < self.chunk_start_frame
            || frame_idx + 1 >= self.chunk_start_frame.saturating_add(chunk_frames)
        {
            self.seek_and_load_chunk(frame_idx);
        }
        let rel0 = frame_idx.saturating_sub(self.chunk_start_frame) as usize;
        let available = self.chunk_buf.len() / 2;
        if rel0 >= available {
            return (0.0, 0.0);
        }
        let rel1 = (rel0 + 1).min(available - 1);
        let b0 = rel0 * 2;
        let b1 = rel1 * 2;
        let l = self.chunk_buf[b0] + (self.chunk_buf[b1] - self.chunk_buf[b0]) * frac;
        let r = self.chunk_buf[b0 + 1] + (self.chunk_buf[b1 + 1] - self.chunk_buf[b0 + 1]) * frac;
        (l, r)
    }
}

/// Dedicated off-RT streaming media feeder thread (bounded RAM — I6).
fn run_media_feeder(
    rx: crossbeam_channel::Receiver<FeederCommand>,
    mut prod: Producer<PlaybackFrame>,
) {
    struct LoadedStream {
        generation: u64,
        source_sr: u32,
        output_sr: u32,
        orig: StreamingStereoWav,
        acc: Option<StreamingStereoWav>,
        voc: Option<StreamingStereoWav>,
        source_cursor: f64,
    }

    let open_loaded = |generation: u64,
                       canonical_wav: PathBuf,
                       vocal_stem_wav: Option<PathBuf>,
                       accompaniment_stem_wav: Option<PathBuf>,
                       output_sample_rate: u32,
                       start_position_s: f64|
     -> Option<LoadedStream> {
        let orig = StreamingStereoWav::open(&canonical_wav).ok()?;
        let source_sr = orig.sample_rate;
        let voc = vocal_stem_wav
            .as_deref()
            .and_then(|p| StreamingStereoWav::open(p).ok());
        let acc = accompaniment_stem_wav
            .as_deref()
            .and_then(|p| StreamingStereoWav::open(p).ok());
        let start_cursor = (start_position_s * source_sr as f64)
            .clamp(0.0, orig.total_frames as f64);
        Some(LoadedStream {
            generation,
            source_sr,
            output_sr: output_sample_rate.max(8000),
            orig,
            acc,
            voc,
            source_cursor: start_cursor,
        })
    };

    let mut loaded: Option<LoadedStream> = None;
    let mut playing = false;

    let handle_cmd = |cmd: FeederCommand,
                      loaded: &mut Option<LoadedStream>,
                      playing: &mut bool|
     -> bool {
        match cmd {
            FeederCommand::Shutdown => return true,
            FeederCommand::Stop => {
                *playing = false;
                *loaded = None;
            }
            FeederCommand::SetPlaying(p) => *playing = p,
            FeederCommand::Seek {
                generation,
                position_s,
            } => {
                if let Some(ref mut audio) = loaded {
                    audio.generation = generation;
                    audio.source_cursor = (position_s * audio.source_sr as f64)
                        .clamp(0.0, audio.orig.total_frames as f64);
                }
            }
            FeederCommand::LoadTrack {
                generation,
                canonical_wav,
                vocal_stem_wav,
                accompaniment_stem_wav,
                output_sample_rate,
                start_position_s,
            } => {
                *loaded = open_loaded(
                    generation,
                    canonical_wav,
                    vocal_stem_wav,
                    accompaniment_stem_wav,
                    output_sample_rate,
                    start_position_s,
                );
            }
        }
        false
    };

    loop {
        while let Ok(cmd) = rx.try_recv() {
            if handle_cmd(cmd, &mut loaded, &mut playing) {
                return;
            }
        }

        if let Some(ref mut audio) = loaded {
            let total_source_frames = audio.orig.total_frames;
            let step = audio.source_sr as f64 / audio.output_sr as f64;
            let has_stems = audio.acc.is_some() && audio.voc.is_some();

            let mut pushed = 0usize;
            while prod.slots() > 0
                && (audio.source_cursor as u64) < total_source_frames
                && pushed < 2048
            {
                let idx0 = audio.source_cursor as u64;
                let frac = (audio.source_cursor - idx0 as f64) as f32;

                let (orig_l, orig_r) = audio.orig.sample_lerp(idx0, frac);
                let (acc_l, acc_r) = match &mut audio.acc {
                    Some(s) => s.sample_lerp(idx0, frac),
                    None => (orig_l, orig_r),
                };
                let (voc_l, voc_r) = match &mut audio.voc {
                    Some(s) => s.sample_lerp(idx0, frac),
                    None => (0.0, 0.0),
                };

                let frame = PlaybackFrame {
                    generation: audio.generation,
                    source_frame_index: idx0,
                    orig_l,
                    orig_r,
                    acc_l,
                    acc_r,
                    voc_l,
                    voc_r,
                    has_stems,
                };
                if prod.push(frame).is_err() {
                    break;
                }
                audio.source_cursor += step;
                pushed += 1;
            }

            if pushed == 0 {
                let wait = if playing {
                    Duration::from_millis(2)
                } else {
                    Duration::from_millis(10)
                };
                if let Ok(cmd) = rx.recv_timeout(wait) {
                    if handle_cmd(cmd, &mut loaded, &mut playing) {
                        return;
                    }
                }
            }
        } else {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(cmd) => {
                    if handle_cmd(cmd, &mut loaded, &mut playing) {
                        return;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ac15_async_drift_resampler_stabilizes_buffer_under_clock_drift() {
        let (mut prod, mut cons) = RingBuffer::<f32>::new(MIC_RING_SAMPLES);
        let mut resampler = AsyncDriftResampler::new(48_000.0, 48_000.0, 512);
        let block = 256usize;
        let mut out = vec![0.0_f32; block];

        for _ in 0..512 {
            let _ = prod.push(0.1);
        }

        let mut frac_accum = 0.0_f64;
        let mut max_occ = 0usize;
        let mut min_occ = usize::MAX;
        let mut underruns = 0u64;

        for step in 0..22_500 {
            frac_accum += 256.0 * 1.0001;
            let to_push = frac_accum.floor() as usize;
            frac_accum -= to_push as f64;
            for _ in 0..to_push {
                let _ = prod.push(0.25);
            }
            let (ur, occ) = resampler.pull_resampled(&mut cons, &mut out);
            if ur {
                underruns += 1;
            }
            if step > 500 {
                max_occ = max_occ.max(occ);
                min_occ = min_occ.min(occ);
            }
        }

        assert_eq!(underruns, 0, "Drifting clocks must not underrun");
        assert!(
            max_occ < 1024 && min_occ > 256,
            "Buffer occupancy must stay tightly bounded around 512 (observed min={min_occ}, max={max_occ})"
        );
        assert!(
            (resampler.current_ratio() - 1.0001).abs() < 0.0005,
            "Ratio should track +100ppm drift, got {}",
            resampler.current_ratio()
        );
    }

    #[test]
    fn b2_dynamic_non_48khz_input_resampling_handles_16khz_and_44100hz_mics() {
        // Test Bluetooth HFP 16 kHz mic feeding 48 kHz output (ratio = 1/3)
        let (mut prod, mut cons) = RingBuffer::<f32>::new(MIC_RING_SAMPLES);
        let mut resampler = AsyncDriftResampler::new(48_000.0, 48_000.0, 512);
        resampler.set_sample_rates(16_000.0, 48_000.0);

        for _ in 0..200 {
            let _ = prod.push(0.2);
        }
        let mut out = vec![0.0_f32; 256];
        let mut frac = 0.0_f64;
        let mut underruns = 0u64;
        for _ in 0..1000 {
            frac += 256.0 * (16_000.0 / 48_000.0);
            let n = frac.floor() as usize;
            frac -= n as f64;
            for _ in 0..n {
                let _ = prod.push(0.2);
            }
            let (ur, _) = resampler.pull_resampled(&mut cons, &mut out);
            if ur {
                underruns += 1;
            }
        }
        assert_eq!(underruns, 0);
        assert!((resampler.current_ratio() - (1.0 / 3.0)).abs() < 0.01);
    }

    #[test]
    fn ac07_ac11_and_b7_stems_immediate_seek_and_eof_replay() {
        let counters = Arc::new(RtCounters::new());
        let mut voice = VoiceDspParams::preset_natural();
        voice.monitoring_enabled = false;
        let mut mix = MixParams {
            use_stems: true,
            accompaniment_gain: 1.0,
            original_vocal_gain: 0.0,
            ..Default::default()
        };

        let (mut handle, mut rt_out, mut rt_mic) =
            AudioEngineHandle::create(48_000, 48_000, voice, mix, Arc::clone(&counters));

        let tmp = tempfile::tempdir().unwrap();
        let orig_path = tmp.path().join("orig.wav");
        let acc_path = tmp.path().join("acc.wav");
        let voc_path = tmp.path().join("voc.wav");

        let frames = 48_000usize;
        let write_const = |p: &PathBuf, val: f32| {
            let spec = hound::WavSpec {
                channels: 2,
                sample_rate: 48_000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let mut w = hound::WavWriter::create(p, spec).unwrap();
            for _ in 0..(frames * 2) {
                w.write_sample(val).unwrap();
            }
            w.finalize().unwrap();
        };
        write_const(&orig_path, 0.5);
        write_const(&acc_path, 0.2);
        write_const(&voc_path, 0.4);

        // Regression B7: immediate load_track -> seek(0.5) BEFORE any RT callback runs must NOT clamp to 0.0!
        handle.load_track(orig_path, Some(voc_path), Some(acc_path), 1.0, 48_000);
        handle.seek(0.5);
        handle.play();
        handle.set_mic_capture_active(true);

        thread::sleep(Duration::from_millis(60));

        let silence = vec![0.0_f32; 256];
        let mut out_buf = vec![0.0_f32; 512];
        for _ in 0..20 {
            rt_mic.push_mono(&silence);
            rt_out.render_stereo_interleaved(&mut out_buf, 256);
        }

        let snap = handle.state().read_transport();
        assert_eq!(snap.state, TransportState::Playing);
        assert!(
            snap.source_position_s >= 0.52,
            "Immediate load->seek(0.5)->play must start at >=0.5s, got {}",
            snap.source_position_s
        );
        let last_sample = out_buf[out_buf.len() - 1];
        assert!(
            (last_sample - 0.2).abs() < 0.03,
            "With vocal_gain=0, output must equal accompaniment (0.2), got {last_sample}"
        );

        // Raise vocal gain to 1.0 -> 0.2 + 0.4 = 0.6
        mix.original_vocal_gain = 1.0;
        handle.set_mix_params(mix);
        for _ in 0..20 {
            rt_mic.push_mono(&silence);
            rt_out.render_stereo_interleaved(&mut out_buf, 256);
        }
        let after_vocal_up = out_buf[out_buf.len() - 1];
        assert!(
            (after_vocal_up - 0.6).abs() < 0.04,
            "Expected 0.6, got {after_vocal_up}"
        );

        // Run to EOF (1.0s), verify Stopped, then call play() -> MUST replay cleanly from 0.0s (B7)!
        for _ in 0..180 {
            rt_out.render_stereo_interleaved(&mut out_buf, 256);
        }
        assert_eq!(handle.state().read_transport().state, TransportState::Stopped);

        handle.play();
        thread::sleep(Duration::from_millis(50));
        for _ in 0..10 {
            rt_out.render_stereo_interleaved(&mut out_buf, 256);
        }
        let replayed = handle.state().read_transport();
        assert_eq!(replayed.state, TransportState::Playing);
        assert!(
            replayed.source_position_s < 0.25,
            "Post-EOF play() must restart near 0.0s, got {}",
            replayed.source_position_s
        );
    }

    #[test]
    fn seqlock_even_mask_prevents_odd_wedging_on_route_lost() {
        let st = AtomicEngineState::new();
        // Simulate corrupted odd parity prior to publish_route_lost (defense-in-depth P2-B1)
        st.transport_seq.store(7, Ordering::Release);
        st.publish_route_lost();
        assert_eq!(st.transport_seq.load(Ordering::Acquire) & 1, 0);
        let snap = st.read_transport();
        assert_eq!(snap.state, TransportState::RouteLost);
        assert_eq!(snap.quality, PresentationQuality::Uncertain);
    }
}
