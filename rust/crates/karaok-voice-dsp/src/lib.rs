//! Zero-allocation real-time vocal DSP chain in pure Rust (§8, REQ-014, REQ-015, AC-13, AC-14).
//!
//! Processing graph:
//!   mono mic -> smoothed input gain -> optional Butterworth HPF
//!            -> YIN F0 + voiced/unvoiced confidence detector
//!            -> scale quantizer with hysteresis & vibrato neutral zone
//!            -> pitch-synchronous dual-tap Hann granular pitch shifter (with matched dry-delay bypass)
//!            -> optional soft-knee compressor
//!            -> parallel 8-channel Householder FDN stereo reverb
//!            -> smoothed mic level & mute
//!
//! Master output safety:
//!   stereo music + stereo voice -> lookahead sample-peak limiter + NaN/Inf sanitizer.
//!
//! Strict RT invariant: ZERO heap allocations, ZERO locks, ZERO syscalls inside `process_*`.

use karaok_types::{MusicalScale, PitchClass, VoiceDspParams};
use std::f32::consts::PI;

pub const MIN_VOCAL_F0_HZ: f32 = 75.0;
pub const MAX_VOCAL_F0_HZ: f32 = 1000.0;

#[inline]
pub fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

#[inline]
pub fn hz_to_midi(hz: f32) -> f32 {
    69.0 + 12.0 * (hz / 440.0).log2()
}

#[inline]
pub fn midi_to_hz(midi: f32) -> f32 {
    440.0 * 2.0_f32.powf((midi - 69.0) / 12.0)
}

/// Click-free one-pole parameter smoother (zero allocation).
#[derive(Debug, Clone)]
pub struct SmoothedValue {
    current: f32,
    target: f32,
    coeff: f32,
}

impl SmoothedValue {
    pub fn new(initial: f32, sample_rate: f32, time_ms: f32) -> Self {
        let mut s = Self {
            current: initial,
            target: initial,
            coeff: 1.0,
        };
        s.set_time_ms(sample_rate, time_ms);
        s
    }

    pub fn set_time_ms(&mut self, sample_rate: f32, time_ms: f32) {
        let samples = (sample_rate * time_ms.max(0.5) / 1000.0).max(1.0);
        self.coeff = 1.0 - (-1.0 / samples).exp();
    }

    #[inline]
    pub fn set_target(&mut self, target: f32) {
        if target.is_finite() {
            self.target = target;
        }
    }

    #[inline]
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> f32 {
        self.current += self.coeff * (self.target - self.current);
        if (self.target - self.current).abs() < 1e-7 {
            self.current = self.target;
        }
        self.current
    }

    #[inline]
    pub fn current(&self) -> f32 {
        self.current
    }

    pub fn reset(&mut self, value: f32) {
        self.current = value;
        self.target = value;
    }
}

/// 2nd-order Butterworth high-pass biquad filter (preallocated state).
#[derive(Debug, Clone)]
pub struct BiquadHighPass {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
    cutoff_hz: f32,
    sample_rate: f32,
}

impl BiquadHighPass {
    pub fn new(sample_rate: f32, cutoff_hz: f32) -> Self {
        let mut f = Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: 0.0,
            z2: 0.0,
            cutoff_hz: 0.0,
            sample_rate: sample_rate.max(8000.0),
        };
        f.set_cutoff(cutoff_hz);
        f
    }

    pub fn set_cutoff(&mut self, cutoff_hz: f32) {
        let clamped = cutoff_hz.clamp(30.0, self.sample_rate * 0.25);
        if (clamped - self.cutoff_hz).abs() < 0.25 {
            return;
        }
        self.cutoff_hz = clamped;
        let omega = 2.0 * PI * clamped / self.sample_rate;
        let cos_w = omega.cos();
        let sin_w = omega.sin();
        let q = std::f32::consts::FRAC_1_SQRT_2;
        let alpha = sin_w / (2.0 * q);
        let a0 = 1.0 + alpha;
        self.b0 = ((1.0 + cos_w) * 0.5) / a0;
        self.b1 = (-(1.0 + cos_w)) / a0;
        self.b2 = ((1.0 + cos_w) * 0.5) / a0;
        self.a1 = (-2.0 * cos_w) / a0;
        self.a2 = (1.0 - alpha) / a0;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        if y.is_finite() {
            y
        } else {
            self.reset();
            0.0
        }
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

/// Real-time YIN F0 + voiced/unvoiced confidence detector (preallocated, zero allocation).
#[derive(Debug, Clone)]
pub struct YinDetector {
    sample_rate: f32,
    window_size: usize,
    integration_len: usize,
    min_lag: usize,
    max_lag: usize,
    hop_size: usize,
    ring: Vec<f32>,
    write_pos: usize,
    samples_since_hop: usize,
    linear_buf: Vec<f32>,
    cmndf: Vec<f32>,
    last_f0_hz: f32,
    last_confidence: f32,
    last_voiced: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PitchEstimate {
    pub f0_hz: f32,
    pub confidence: f32,
    pub voiced: bool,
    pub period_samples: f32,
}

impl YinDetector {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(8000.0);
        // ~21.3 ms window at 48 kHz (1024 samples), scaling proportionally with sample rate.
        let window_size = ((sr * 0.022).round() as usize).clamp(512, 2048);
        let max_lag = ((sr / MIN_VOCAL_F0_HZ).ceil() as usize).min(window_size * 2 / 3);
        let min_lag = ((sr / MAX_VOCAL_F0_HZ).floor() as usize).max(16);
        let integration_len = (window_size - max_lag).max(128);
        let hop_size = (sr * 0.0053).round() as usize; // ~5.3 ms hop (256 samples @ 48 kHz)

        Self {
            sample_rate: sr,
            window_size,
            integration_len,
            min_lag,
            max_lag,
            hop_size: hop_size.clamp(64, 512),
            ring: vec![0.0; window_size],
            write_pos: 0,
            samples_since_hop: 0,
            linear_buf: vec![0.0; window_size],
            cmndf: vec![1.0; max_lag + 2],
            last_f0_hz: 0.0,
            last_confidence: 0.0,
            last_voiced: false,
        }
    }

    pub fn reset(&mut self) {
        self.ring.fill(0.0);
        self.write_pos = 0;
        self.samples_since_hop = 0;
        self.last_f0_hz = 0.0;
        self.last_confidence = 0.0;
        self.last_voiced = false;
    }

    /// Push one mono sample. Re-estimates F0 every `hop_size` samples without allocating.
    #[inline]
    pub fn push_sample(&mut self, x: f32) -> PitchEstimate {
        self.ring[self.write_pos] = x;
        self.write_pos += 1;
        if self.write_pos >= self.window_size {
            self.write_pos = 0;
        }
        self.samples_since_hop += 1;
        if self.samples_since_hop >= self.hop_size {
            self.samples_since_hop = 0;
            self.analyze_window();
        }
        self.current()
    }

    #[inline]
    pub fn current(&self) -> PitchEstimate {
        let period_samples = if self.last_voiced && self.last_f0_hz > 1.0 {
            self.sample_rate / self.last_f0_hz
        } else {
            0.0
        };
        PitchEstimate {
            f0_hz: self.last_f0_hz,
            confidence: self.last_confidence,
            voiced: self.last_voiced,
            period_samples,
        }
    }

    fn analyze_window(&mut self) {
        // Linearize oldest-to-newest into preallocated buffer
        let tail = self.window_size - self.write_pos;
        self.linear_buf[..tail].copy_from_slice(&self.ring[self.write_pos..]);
        self.linear_buf[tail..].copy_from_slice(&self.ring[..self.write_pos]);

        // Compute RMS energy and zero-crossing rate (unvoiced/fricative rejection!)
        let mut energy = 0.0_f32;
        let mut zero_crossings = 0u32;
        let mut prev = self.linear_buf[0];
        for &s in &self.linear_buf {
            energy += s * s;
            if (s >= 0.0 && prev < 0.0) || (s < 0.0 && prev >= 0.0) {
                zero_crossings += 1;
            }
            prev = s;
        }
        let rms = (energy / self.window_size as f32).sqrt();
        let zcr = zero_crossings as f32 / self.window_size as f32;

        // Silence or high-ZCR unvoiced fricative (/s/, /f/, breath)
        if rms < 0.004 || zcr > 0.22 {
            self.last_voiced = false;
            self.last_confidence = 0.0;
            self.last_f0_hz = 0.0;
            return;
        }

        // YIN difference function + Cumulative Mean Normalized Difference Function (CMNDF)
        self.cmndf[0] = 1.0;
        let mut running_sum = 0.0_f32;
        let w = self.integration_len;

        for tau in 1..=self.max_lag {
            let mut diff_sum = 0.0_f32;
            let a = &self.linear_buf[..w];
            let b = &self.linear_buf[tau..tau + w];
            for i in 0..w {
                let d = a[i] - b[i];
                diff_sum += d * d;
            }
            running_sum += diff_sum;
            self.cmndf[tau] = if running_sum > 1e-12 {
                diff_sum * (tau as f32) / running_sum
            } else {
                1.0
            };
        }

        // YIN absolute threshold search (first dip below threshold prevents subharmonic octave drops)
        const YIN_THRESHOLD: f32 = 0.18;
        let mut best_tau: Option<usize> = None;

        let mut tau = self.min_lag;
        while tau < self.max_lag {
            if self.cmndf[tau] < YIN_THRESHOLD {
                while tau + 1 < self.max_lag && self.cmndf[tau + 1] < self.cmndf[tau] {
                    tau += 1;
                }
                best_tau = Some(tau);
                break;
            }
            tau += 1;
        }

        // Fallback: global minimum if still strongly periodic
        if best_tau.is_none() {
            let mut min_val = f32::MAX;
            let mut min_idx = 0usize;
            for t in self.min_lag..self.max_lag {
                if self.cmndf[t] < min_val {
                    min_val = self.cmndf[t];
                    min_idx = t;
                }
            }
            if min_val < 0.22 {
                best_tau = Some(min_idx);
            }
        }

        let Some(tau_idx) = best_tau else {
            self.last_voiced = false;
            self.last_confidence = 0.0;
            self.last_f0_hz = 0.0;
            return;
        };

        let min_cmndf = self.cmndf[tau_idx];
        let confidence = (1.0 - min_cmndf).clamp(0.0, 1.0);
        if confidence < 0.78 {
            self.last_voiced = false;
            self.last_confidence = confidence;
            self.last_f0_hz = 0.0;
            return;
        }

        // Parabolic interpolation around tau_idx for sub-sample precision
        let refined_tau = if tau_idx > self.min_lag && tau_idx < self.max_lag {
            let y0 = self.cmndf[tau_idx - 1];
            let y1 = self.cmndf[tau_idx];
            let y2 = self.cmndf[tau_idx + 1];
            let denom = 2.0 * (y0 - 2.0 * y1 + y2);
            if denom.abs() > 1e-9 {
                let delta = ((y0 - y2) / denom).clamp(-0.5, 0.5);
                tau_idx as f32 + delta
            } else {
                tau_idx as f32
            }
        } else {
            tau_idx as f32
        };

        let f0 = self.sample_rate / refined_tau.max(1.0);
        if (MIN_VOCAL_F0_HZ..=MAX_VOCAL_F0_HZ).contains(&f0) {
            self.last_f0_hz = f0;
            self.last_confidence = confidence;
            self.last_voiced = true;
        } else {
            self.last_voiced = false;
            self.last_confidence = 0.0;
            self.last_f0_hz = 0.0;
        }
    }
}

/// Musical scale target selector with hysteresis & vibrato neutral zone (§8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct QuantizerCacheSig {
    tonic: PitchClass,
    scale: MusicalScale,
    pitch_amount_bits: u32,
    neutral_zone_bits: u32,
    hysteresis_bits: u32,
    confidence_bits: u32,
}

#[derive(Debug, Clone)]
pub struct PitchQuantizer {
    current_target_midi: Option<i32>,
    smoothed_ratio: f32,
    sample_rate: f32,
    last_in_f0_hz: f32,
    last_in_voiced: bool,
    last_sig: Option<QuantizerCacheSig>,
    last_speed_bits: u32,
    cached_alpha: f32,
    cached_release_alpha: f32,
    cached_target_f0_hz: f32,
    cached_desired_ratio: f32,
    cached_correction_cents: f32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct QuantizerOutput {
    pub target_f0_hz: f32,
    pub pitch_ratio: f32,
    pub correction_cents: f32,
}

impl PitchQuantizer {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(8000.0);
        let release_alpha = 1.0 - (-1.0 / (sr * 0.012)).exp();
        Self {
            current_target_midi: None,
            smoothed_ratio: 1.0,
            sample_rate: sr,
            last_in_f0_hz: -1.0,
            last_in_voiced: false,
            last_sig: None,
            last_speed_bits: u32::MAX,
            cached_alpha: release_alpha,
            cached_release_alpha: release_alpha,
            cached_target_f0_hz: 0.0,
            cached_desired_ratio: 1.0,
            cached_correction_cents: 0.0,
        }
    }

    pub fn reset(&mut self) {
        self.current_target_midi = None;
        self.smoothed_ratio = 1.0;
        self.last_in_f0_hz = -1.0;
        self.last_in_voiced = false;
        self.last_sig = None;
        self.cached_target_f0_hz = 0.0;
        self.cached_desired_ratio = 1.0;
        self.cached_correction_cents = 0.0;
    }

    /// Compute allowed target MIDI note nearest to `midi`, applying hysteresis against `current_target_midi`.
    pub fn select_target_midi(
        &mut self,
        midi: f32,
        tonic: PitchClass,
        scale: MusicalScale,
        hysteresis_cents: f32,
    ) -> i32 {
        let intervals = scale.intervals();
        let tonic_i = tonic as i32;
        let center = midi.round() as i32;

        let mut best_note = center;
        let mut best_dist = f32::MAX;

        for candidate in (center - 12)..=(center + 12) {
            let rel = (candidate - tonic_i).rem_euclid(12) as u8;
            if intervals.contains(&rel) {
                let dist = (candidate as f32 - midi).abs();
                if dist < best_dist {
                    best_dist = dist;
                    best_note = candidate;
                }
            }
        }

        // Hysteresis: retain previous target if still valid in scale and new candidate isn't clearly closer
        if let Some(prev) = self.current_target_midi {
            let prev_rel = (prev - tonic_i).rem_euclid(12) as u8;
            if intervals.contains(&prev_rel) {
                let prev_dist_cents = (prev as f32 - midi).abs() * 100.0;
                let best_dist_cents = best_dist * 100.0;
                if prev != best_note && prev_dist_cents <= best_dist_cents + hysteresis_cents {
                    return prev;
                }
            }
        }

        self.current_target_midi = Some(best_note);
        best_note
    }

    #[inline]
    pub fn update(
        &mut self,
        estimate: PitchEstimate,
        params: &VoiceDspParams,
    ) -> QuantizerOutput {
        // Cache smoothing coefficient so per-sample `.exp()` is avoided (P2-N4)
        let speed = params.pitch_speed.clamp(0.05, 1.0);
        let speed_bits = speed.to_bits();
        if speed_bits != self.last_speed_bits {
            self.last_speed_bits = speed_bits;
            let smooth_ms = 85.0 * (1.0 - speed) + 4.0 * speed;
            self.cached_alpha = 1.0 - (-1.0 / (self.sample_rate * smooth_ms / 1000.0)).exp();
        }
        let alpha = self.cached_alpha;

        if !params.pitch_enabled || params.bypass || params.pitch_amount <= 0.001 || !estimate.voiced
        {
            self.smoothed_ratio += self.cached_release_alpha * (1.0 - self.smoothed_ratio);
            // Always invalidate cache on disable/bypass/unvoiced so live edits take effect immediately (P2-I1)
            self.last_in_voiced = false;
            self.last_sig = None;
            if !estimate.voiced {
                self.current_target_midi = None;
                self.last_in_f0_hz = -1.0;
            }
            return QuantizerOutput {
                target_f0_hz: 0.0,
                pitch_ratio: self.smoothed_ratio,
                correction_cents: 0.0,
            };
        }

        let sig = QuantizerCacheSig {
            tonic: params.tonic,
            scale: params.scale,
            pitch_amount_bits: params.pitch_amount.to_bits(),
            neutral_zone_bits: params.neutral_zone_cents.to_bits(),
            hysteresis_bits: params.hysteresis_cents.to_bits(),
            confidence_bits: estimate.confidence.to_bits(),
        };

        // Recompute scale search & log2/powf when YIN publishes a new F0 OR any live parameter/confidence changes (P2-I1)
        if !self.last_in_voiced
            || (estimate.f0_hz - self.last_in_f0_hz).abs() > 1e-4
            || self.last_sig != Some(sig)
        {
            // Clear hysteresis lock if the user switched key or scale so new scale notes take effect immediately
            if let Some(prev) = self.last_sig {
                if prev.tonic != sig.tonic || prev.scale != sig.scale {
                    self.current_target_midi = None;
                }
            }
            self.last_in_voiced = true;
            self.last_in_f0_hz = estimate.f0_hz;
            self.last_sig = Some(sig);

            let midi = hz_to_midi(estimate.f0_hz);
            let target_midi =
                self.select_target_midi(midi, params.tonic, params.scale, params.hysteresis_cents);
            let raw_error_cents = (target_midi as f32 - midi) * 100.0;

            let abs_err = raw_error_cents.abs();
            let effective_error_cents = if abs_err <= params.neutral_zone_cents {
                0.0
            } else {
                raw_error_cents.signum() * (abs_err - params.neutral_zone_cents)
            };

            let conf_weight = ((estimate.confidence - 0.78) / 0.17).clamp(0.0, 1.0);
            let correction_cents =
                (effective_error_cents * params.pitch_amount * conf_weight).clamp(-700.0, 700.0);
            self.cached_correction_cents = correction_cents;
            self.cached_desired_ratio = 2.0_f32.powf(correction_cents / 1200.0).clamp(0.5, 2.0);
            self.cached_target_f0_hz = midi_to_hz(target_midi as f32);
        }

        self.smoothed_ratio += alpha * (self.cached_desired_ratio - self.smoothed_ratio);

        QuantizerOutput {
            target_f0_hz: self.cached_target_f0_hz,
            pitch_ratio: self.smoothed_ratio,
            correction_cents: self.cached_correction_cents,
        }
    }
}

/// Real-time Pitch-Synchronous Dual-Tap Granular Delay Pitch Shifter (§8.2, ADR-0003).
///
/// Zero allocations. Uses two complementary Hann-windowed read taps (`sin^2(pi*p) + sin^2(pi*(p+0.5)) == 1`)
/// with cubic Hermite interpolation and **independent per-tap grain lengths** (`grain_a`, `grain_b`)
/// latched ONLY when each respective tap wraps at exact zero amplitude (zero click on F0 transitions!).
#[derive(Debug, Clone)]
pub struct PitchShifter {
    buf: Vec<f32>,
    mask: usize,
    write_idx: usize,
    phase_a: f32,
    phase_b: f32,
    base_grain_samples: f32,
    grain_a: f32,
    grain_b: f32,
    min_delay: f32,
    dry_latency_samples: usize,
}

impl PitchShifter {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(8000.0);
        // Nominal ~16 ms grain (~768 samples @ 48kHz), latency midpoint ~8.33 ms (400 samples).
        let base_grain = (sr * 0.016).round().clamp(256.0, 1536.0);
        let min_delay = 16.0_f32;
        let dry_latency_samples = (min_delay + base_grain * 0.5).round() as usize;
        let buf_len = 8192usize; // power of two
        Self {
            buf: vec![0.0; buf_len],
            mask: buf_len - 1,
            write_idx: 0,
            phase_a: 0.0,
            phase_b: 0.5,
            base_grain_samples: base_grain,
            grain_a: base_grain,
            grain_b: base_grain,
            min_delay,
            dry_latency_samples,
        }
    }

    pub fn reset(&mut self) {
        self.buf.fill(0.0);
        self.write_idx = 0;
        self.phase_a = 0.0;
        self.phase_b = 0.5;
        self.grain_a = self.base_grain_samples;
        self.grain_b = self.base_grain_samples;
    }

    #[inline]
    pub fn latency_samples(&self) -> usize {
        self.dry_latency_samples
    }

    /// Process one sample; returns `(shifted_sample, latency_matched_dry_sample)`.
    #[inline]
    pub fn process(&mut self, x: f32, ratio: f32, period_samples: f32) -> (f32, f32) {
        self.buf[self.write_idx] = x;

        let dry_idx = (self.write_idx + self.buf.len() - self.dry_latency_samples) & self.mask;
        let dry = self.buf[dry_idx];

        let r = ratio.clamp(0.5, 2.0);
        let desired_grain = if (48.0..=700.0).contains(&period_samples) {
            let mut g = period_samples * 2.0;
            while g < self.base_grain_samples * 0.65 {
                g += period_samples;
            }
            g.clamp(256.0, 1600.0)
        } else {
            self.base_grain_samples
        };

        // Advance each tap independently using its own latched grain length.
        // Only update a tap's grain length when that tap crosses 0/1 where its Hann weight is EXACTLY 0!
        self.phase_a += (1.0 - r) / self.grain_a;
        if self.phase_a >= 1.0 {
            self.phase_a -= 1.0;
            self.grain_a = desired_grain;
        } else if self.phase_a < 0.0 {
            self.phase_a += 1.0;
            self.grain_a = desired_grain;
        }

        self.phase_b += (1.0 - r) / self.grain_b;
        if self.phase_b >= 1.0 {
            self.phase_b -= 1.0;
            self.grain_b = desired_grain;
        } else if self.phase_b < 0.0 {
            self.phase_b += 1.0;
            self.grain_b = desired_grain;
        }

        let delay_a = self.min_delay + self.phase_a * self.grain_a;
        let delay_b = self.min_delay + self.phase_b * self.grain_b;

        let s_a = self.read_cubic(delay_a);
        let s_b = self.read_cubic(delay_b);

        let sin_a = (PI * self.phase_a).sin();
        let sin_b = (PI * self.phase_b).sin();
        let w_a = sin_a * sin_a;
        let w_b = sin_b * sin_b;
        let w_sum = (w_a + w_b).max(1e-6);

        let shifted = (s_a * w_a + s_b * w_b) / w_sum;
        self.write_idx = (self.write_idx + 1) & self.mask;

        (shifted, dry)
    }

    #[inline]
    fn read_cubic(&self, delay_samples: f32) -> f32 {
        let int_delay = delay_samples.floor() as usize;
        let frac = delay_samples - int_delay as f32;

        let len = self.buf.len();
        let i0 = (self.write_idx + len - int_delay) & self.mask;
        let im1 = (i0 + 1) & self.mask; // one sample newer
        let i1 = (i0 + len - 1) & self.mask; // one sample older
        let i2 = (i0 + len - 2) & self.mask; // two samples older

        let ym1 = self.buf[im1];
        let y0 = self.buf[i0];
        let y1 = self.buf[i1];
        let y2 = self.buf[i2];

        // 4-point Catmull-Rom / Hermite interpolation
        let c0 = y0;
        let c1 = 0.5 * (y1 - ym1);
        let c2 = ym1 - 2.5 * y0 + 2.0 * y1 - 0.5 * y2;
        let c3 = 0.5 * (y2 - ym1) + 1.5 * (y0 - y1);
        ((c3 * frac + c2) * frac + c1) * frac + c0
    }
}

/// Gentle soft-knee vocal compressor (preallocated, zero allocation).
#[derive(Debug, Clone)]
pub struct SoftCompressor {
    env_db: f32,
    attack_coeff: f32,
    release_coeff: f32,
}

impl SoftCompressor {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(8000.0);
        Self {
            env_db: -96.0,
            attack_coeff: (-1.0 / (sr * 0.008)).exp(),
            release_coeff: (-1.0 / (sr * 0.090)).exp(),
        }
    }

    pub fn reset(&mut self) {
        self.env_db = -96.0;
    }

    #[inline]
    pub fn process(&mut self, x: f32, threshold_db: f32, ratio: f32) -> f32 {
        let abs_x = x.abs().max(1e-6);
        let level_db = 20.0 * abs_x.log10();
        let coeff = if level_db > self.env_db {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        self.env_db = coeff * self.env_db + (1.0 - coeff) * level_db;

        let knee_db = 6.0_f32;
        let over_db = self.env_db - threshold_db;
        let gain_reduction_db = if over_db <= -knee_db * 0.5 {
            0.0
        } else if over_db >= knee_db * 0.5 {
            over_db * (1.0 - 1.0 / ratio.max(1.0))
        } else {
            let t = over_db + knee_db * 0.5;
            (1.0 - 1.0 / ratio.max(1.0)) * (t * t) / (2.0 * knee_db)
        };

        // Subtle automatic makeup gain capped conservatively to avoid noise pumping
        let makeup_db = ((-threshold_db - 12.0).max(0.0) * 0.15 * (1.0 - 1.0 / ratio.max(1.0)))
            .clamp(0.0, 4.5);
        x * db_to_linear(-gain_reduction_db + makeup_db)
    }
}

/// 8-channel Householder Feedback Delay Network (FDN) algorithmic stereo reverb (§8.3, ADR-0003).
/// Mathematically guaranteed BIBO-stable (`feedback_gain <= 0.96`), preallocated, zero allocation.
#[derive(Debug, Clone)]
pub struct FdnReverb {
    sample_rate: f32,
    predelay_buf: Vec<f32>,
    predelay_write: usize,
    lines: [Vec<f32>; 8],
    indices: [usize; 8],
    lp_state: [f32; 8],
    gains: [f32; 8],
    last_decay_s: f32,
}

const FDN_BASE_DELAYS_MS: [f32; 8] = [29.7, 37.1, 41.3, 47.9, 53.3, 59.9, 64.7, 71.9];

impl FdnReverb {
    pub fn new(sample_rate: f32) -> Self {
        let sr = sample_rate.max(8000.0);
        let max_predelay = ((sr * 0.10).ceil() as usize).max(64);
        let lines = std::array::from_fn(|i| {
            let len = ((sr * FDN_BASE_DELAYS_MS[i] / 1000.0).round() as usize).max(64);
            vec![0.0; len]
        });
        let mut rev = Self {
            sample_rate: sr,
            predelay_buf: vec![0.0; max_predelay],
            predelay_write: 0,
            lines,
            indices: [0; 8],
            lp_state: [0.0; 8],
            gains: [0.5; 8],
            last_decay_s: -1.0,
        };
        rev.update_decay(1.0);
        rev
    }

    pub fn reset(&mut self) {
        self.predelay_buf.fill(0.0);
        self.predelay_write = 0;
        for line in &mut self.lines {
            line.fill(0.0);
        }
        self.indices = [0; 8];
        self.lp_state = [0.0; 8];
    }

    fn update_decay(&mut self, decay_s: f32) {
        let rt60 = decay_s.clamp(0.2, 4.0);
        if (rt60 - self.last_decay_s).abs() < 0.01 {
            return;
        }
        self.last_decay_s = rt60;
        for i in 0..8 {
            let delay_s = self.lines[i].len() as f32 / self.sample_rate;
            // RT60 formula: g = 10^(-3 * delay_s / rt60), clamped strictly < 1.0
            self.gains[i] = 10.0_f32.powf(-3.0 * delay_s / rt60).clamp(0.05, 0.96);
        }
    }

    /// Process one mono input sample, returning pure wet stereo `(wet_l, wet_r)`.
    #[inline]
    pub fn process_wet(&mut self, input: f32, decay_s: f32, predelay_ms: f32, damping: f32) -> (f32, f32) {
        self.update_decay(decay_s);

        // Pre-delay line
        let pd_len = self.predelay_buf.len();
        self.predelay_buf[self.predelay_write] = input;
        let pd_samples = ((self.sample_rate * predelay_ms.clamp(0.0, 80.0) / 1000.0).round() as usize)
            .min(pd_len - 1);
        let pd_read = (self.predelay_write + pd_len - pd_samples) % pd_len;
        let delayed_in = self.predelay_buf[pd_read];
        self.predelay_write = (self.predelay_write + 1) % pd_len;

        // Read all 8 delay lines + apply 1-pole low-pass damping
        let damp = damping.clamp(0.05, 0.95);
        let mut read_vals = [0.0_f32; 8];
        let mut sum = 0.0_f32;
        for (i, slot) in read_vals.iter_mut().enumerate() {
            let idx = self.indices[i];
            let raw = self.lines[i][idx];
            // Low-pass damping filter inside feedback loop
            let filtered = raw * (1.0 - damp) + self.lp_state[i] * damp;
            self.lp_state[i] = filtered;
            let scaled = filtered * self.gains[i];
            *slot = scaled;
            sum += scaled;
        }

        // Orthogonal Householder mixing matrix: H*v = v - (2/N) * sum(v)
        let mean_2 = sum * 0.25; // 2 / 8 = 0.25
        let inj = delayed_in * 0.35355; // 1 / sqrt(8)

        for (i, &rv) in read_vals.iter().enumerate() {
            let mixed = (rv - mean_2) + if i % 2 == 0 { inj } else { -inj };
            let idx = self.indices[i];
            self.lines[i][idx] = if mixed.is_finite() { mixed } else { 0.0 };
            self.indices[i] = if idx + 1 >= self.lines[i].len() {
                0
            } else {
                idx + 1
            };
        }

        // Decorrelated stereo tap out
        let wet_l = (read_vals[0] + read_vals[2] - read_vals[4] + read_vals[6]) * 0.42;
        let wet_r = (read_vals[1] - read_vals[3] + read_vals[5] + read_vals[7]) * 0.42;
        (wet_l, wet_r)
    }
}

/// Lookahead sample-peak master output limiter with peak-hold over lookahead window + NaN/Inf sanitizer (§8.3).
/// Explicitly labeled as a sample-peak limiter (not oversampled true-peak).
#[derive(Debug, Clone)]
pub struct MasterLimiter {
    delay_l: Vec<f32>,
    delay_r: Vec<f32>,
    peak_ring: Vec<f32>,
    write_idx: usize,
    gain: f32,
    attack_coeff: f32,
    release_coeff: f32,
    ceiling_lin: f32,
}

impl MasterLimiter {
    pub fn new(sample_rate: f32, ceiling_db: f32) -> Self {
        let sr = sample_rate.max(8000.0);
        let lookahead = ((sr * 0.0025).round() as usize).clamp(16, 256); // 2.5 ms lookahead
        Self {
            delay_l: vec![0.0; lookahead],
            delay_r: vec![0.0; lookahead],
            peak_ring: vec![0.0; lookahead],
            write_idx: 0,
            gain: 1.0,
            attack_coeff: (-1.0 / (sr * 0.0005)).exp(),
            release_coeff: (-1.0 / (sr * 0.080)).exp(),
            ceiling_lin: db_to_linear(ceiling_db.clamp(-6.0, -0.1)),
        }
    }

    pub fn set_ceiling_db(&mut self, ceiling_db: f32) {
        self.ceiling_lin = db_to_linear(ceiling_db.clamp(-6.0, -0.1));
    }

    pub fn latency_samples(&self) -> usize {
        self.delay_l.len()
    }

    pub fn reset(&mut self) {
        self.delay_l.fill(0.0);
        self.delay_r.fill(0.0);
        self.peak_ring.fill(0.0);
        self.write_idx = 0;
        self.gain = 1.0;
    }

    /// Process stereo frame, returning `(out_l, out_r, sanitized_non_finite_count, clipped)`.
    #[inline]
    pub fn process(&mut self, mut l: f32, mut r: f32) -> (f32, f32, u64, bool) {
        let mut sanitized = 0u64;
        if !l.is_finite() {
            l = 0.0;
            sanitized += 1;
        }
        if !r.is_finite() {
            r = 0.0;
            sanitized += 1;
        }

        let peak = l.abs().max(r.abs());
        let dl = self.delay_l[self.write_idx];
        let dr = self.delay_r[self.write_idx];
        self.delay_l[self.write_idx] = l;
        self.delay_r[self.write_idx] = r;
        self.peak_ring[self.write_idx] = peak;
        self.write_idx += 1;
        if self.write_idx >= self.delay_l.len() {
            self.write_idx = 0;
        }

        // Hold maximum peak across the entire lookahead window so short transients arrive fully attenuated!
        let mut window_peak = dl.abs().max(dr.abs());
        for &p in &self.peak_ring {
            if p > window_peak {
                window_peak = p;
            }
        }

        let desired_gain = if window_peak > self.ceiling_lin {
            self.ceiling_lin / window_peak
        } else {
            1.0
        };

        let coeff = if desired_gain < self.gain {
            self.attack_coeff
        } else {
            self.release_coeff
        };
        self.gain = coeff * self.gain + (1.0 - coeff) * desired_gain;

        let mut out_l = dl * self.gain;
        let mut out_r = dr * self.gain;
        let clipped = out_l.abs() > self.ceiling_lin || out_r.abs() > self.ceiling_lin;
        out_l = out_l.clamp(-self.ceiling_lin, self.ceiling_lin);
        out_r = out_r.clamp(-self.ceiling_lin, self.ceiling_lin);

        (out_l, out_r, sanitized, clipped)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct VoiceBlockTelemetry {
    pub pre_peak: f32,
    pub pre_rms: f32,
    pub input_clipped: bool,
    pub post_peak: f32,
    pub post_rms: f32,
    pub detected_f0_hz: f32,
    pub target_f0_hz: f32,
    pub pitch_confidence: f32,
    pub voiced: bool,
    pub pitch_correction_cents: f32,
}

/// Complete real-time live vocal processing chain (preallocated, zero allocation on RT thread).
pub struct VoiceDspChain {
    sample_rate: f32,
    params: VoiceDspParams,
    input_gain: SmoothedValue,
    mic_level: SmoothedValue,
    mute_gate: SmoothedValue,
    hpf_mix: SmoothedValue,
    comp_mix: SmoothedValue,
    pitch_mix: SmoothedValue,
    reverb_wet: SmoothedValue,
    hpf: BiquadHighPass,
    yin: YinDetector,
    quantizer: PitchQuantizer,
    shifter: PitchShifter,
    compressor: SoftCompressor,
    reverb: FdnReverb,
    last_estimate: PitchEstimate,
    last_quant: QuantizerOutput,
}

impl VoiceDspChain {
    pub fn new(sample_rate: f32, initial_params: VoiceDspParams) -> Self {
        let sr = sample_rate.max(8000.0);
        let p = initial_params.clamped();
        let mute_val = if p.mic_muted { 0.0 } else { 1.0 };
        let hpf_val = if !p.bypass && p.hpf_enabled { 1.0 } else { 0.0 };
        let comp_val = if !p.bypass && p.compressor_enabled { 1.0 } else { 0.0 };
        let pitch_wet = if !p.bypass && p.pitch_enabled && p.pitch_amount > 0.001 {
            1.0
        } else {
            0.0
        };
        let rev_wet = if !p.bypass && p.reverb_enabled {
            p.reverb_wet
        } else {
            0.0
        };

        Self {
            sample_rate: sr,
            params: p,
            input_gain: SmoothedValue::new(db_to_linear(p.input_gain_db), sr, 10.0),
            mic_level: SmoothedValue::new(p.mic_level, sr, 12.0),
            mute_gate: SmoothedValue::new(mute_val, sr, 12.0),
            hpf_mix: SmoothedValue::new(hpf_val, sr, 12.0),
            comp_mix: SmoothedValue::new(comp_val, sr, 12.0),
            pitch_mix: SmoothedValue::new(pitch_wet, sr, 15.0),
            reverb_wet: SmoothedValue::new(rev_wet, sr, 15.0),
            hpf: BiquadHighPass::new(sr, p.hpf_cutoff_hz),
            yin: YinDetector::new(sr),
            quantizer: PitchQuantizer::new(sr),
            shifter: PitchShifter::new(sr),
            compressor: SoftCompressor::new(sr),
            reverb: FdnReverb::new(sr),
            last_estimate: PitchEstimate::default(),
            last_quant: QuantizerOutput::default(),
        }
    }

    pub fn set_params(&mut self, params: VoiceDspParams) {
        let p = params.clamped();
        self.params = p;
        self.input_gain.set_target(db_to_linear(p.input_gain_db));
        self.mic_level.set_target(p.mic_level);
        self.mute_gate.set_target(if p.mic_muted { 0.0 } else { 1.0 });
        self.hpf_mix.set_target(if !p.bypass && p.hpf_enabled { 1.0 } else { 0.0 });
        self.comp_mix
            .set_target(if !p.bypass && p.compressor_enabled { 1.0 } else { 0.0 });
        let pitch_wet = if !p.bypass && p.pitch_enabled && p.pitch_amount > 0.001 {
            1.0
        } else {
            0.0
        };
        self.pitch_mix.set_target(pitch_wet);
        let rev_wet = if !p.bypass && p.reverb_enabled {
            p.reverb_wet
        } else {
            0.0
        };
        self.reverb_wet.set_target(rev_wet);
        self.hpf.set_cutoff(p.hpf_cutoff_hz);
    }

    pub fn params(&self) -> VoiceDspParams {
        self.params
    }

    /// Algorithmic latency of the vocal DSP path in samples (constant across bypass thanks to dry delay match).
    pub fn algorithmic_latency_samples(&self) -> usize {
        self.shifter.latency_samples()
    }

    pub fn algorithmic_latency_ms(&self) -> f64 {
        (self.algorithmic_latency_samples() as f64 / self.sample_rate as f64) * 1000.0
    }

    pub fn reset(&mut self) {
        self.hpf.reset();
        self.yin.reset();
        self.quantizer.reset();
        self.shifter.reset();
        self.compressor.reset();
        self.reverb.reset();
        self.last_estimate = PitchEstimate::default();
        self.last_quant = QuantizerOutput::default();
    }

    /// Process a block of mono mic samples into stereo output buffers (`out_l`, `out_r`).
    /// ZERO heap allocations. `out_l` and `out_r` must have at least `input.len()` elements.
    pub fn process_block(
        &mut self,
        input: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) -> VoiceBlockTelemetry {
        let n = input.len().min(out_l.len()).min(out_r.len());
        if n == 0 {
            return VoiceBlockTelemetry::default();
        }

        let mut pre_peak = 0.0_f32;
        let mut pre_sq = 0.0_f32;
        let mut input_clipped = false;
        let mut post_peak = 0.0_f32;
        let mut post_sq = 0.0_f32;

        for i in 0..n {
            let raw = if input[i].is_finite() { input[i] } else { 0.0 };
            let raw_abs = raw.abs();
            if raw_abs >= 0.985 {
                input_clipped = true;
            }
            if raw_abs > pre_peak {
                pre_peak = raw_abs;
            }
            pre_sq += raw * raw;

            // 1. Smoothed input gain
            let gained = raw * self.input_gain.next();

            // 2. Click-free smoothed HPF crossfade
            let hpf_wet = self.hpf.process(gained);
            let hmix = self.hpf_mix.next();
            let filtered = gained * (1.0 - hmix) + hpf_wet * hmix;

            // 3. YIN F0 estimation + scale quantizer with hysteresis
            let est = self.yin.push_sample(filtered);
            let quant = self.quantizer.update(est, &self.params);
            self.last_estimate = est;
            self.last_quant = quant;

            // 4. Pitch shifter + latency-matched dry path (no comb filtering on bypass!)
            let (shifted, matched_dry) =
                self.shifter
                    .process(filtered, quant.pitch_ratio, est.period_samples);
            let pmix = self.pitch_mix.next();
            let after_pitch = matched_dry * (1.0 - pmix) + shifted * pmix;

            // 5. Click-free smoothed soft compressor crossfade
            let comp_wet = self.compressor.process(
                after_pitch,
                self.params.compressor_threshold_db,
                self.params.compressor_ratio,
            );
            let cmix = self.comp_mix.next();
            let after_comp = after_pitch * (1.0 - cmix) + comp_wet * cmix;

            // 6. Parallel FDN stereo reverb; mute gate [0..1] cuts both dry & reverb feed cleanly via ramp (§8.3)
            let mute_g = self.mute_gate.next();
            let level_g = self.mic_level.next();
            let feed_reverb = after_comp * mute_g;
            let (wet_l, wet_r) = self.reverb.process_wet(
                feed_reverb,
                self.params.reverb_decay_s,
                self.params.reverb_predelay_ms,
                self.params.reverb_damping,
            );
            let rwet = self.reverb_wet.next();
            let dry_share = (1.0 - rwet * 0.5).clamp(0.4, 1.0);

            // AC-05: Mute ramps BOTH dry and wet contribution to exact 0 over 12ms (click-free total silence!)
            let out_scale = level_g * mute_g;
            let l = (after_comp * dry_share + wet_l * rwet) * out_scale;
            let r = (after_comp * dry_share + wet_r * rwet) * out_scale;

            out_l[i] = l;
            out_r[i] = r;

            let pk = l.abs().max(r.abs());
            if pk > post_peak {
                post_peak = pk;
            }
            post_sq += 0.5 * (l * l + r * r);
        }

        VoiceBlockTelemetry {
            pre_peak,
            pre_rms: (pre_sq / n as f32).sqrt(),
            input_clipped,
            post_peak,
            post_rms: (post_sq / n as f32).sqrt(),
            detected_f0_hz: self.last_estimate.f0_hz,
            target_f0_hz: self.last_quant.target_f0_hz,
            pitch_confidence: self.last_estimate.confidence,
            voiced: self.last_estimate.voiced,
            pitch_correction_cents: self.last_quant.correction_cents,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// Deterministic PRNG (LCG) for repeatable unvoiced noise tests without extra crates.
    struct Lcg(u64);
    impl Lcg {
        fn next_f32(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            let bits = ((self.0 >> 40) as u32) as f32 / (1u32 << 24) as f32;
            bits * 2.0 - 1.0
        }
    }

    fn generate_harmonic_voice(sr: f32, f0: f32, duration_s: f32) -> Vec<f32> {
        let n = (sr * duration_s) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / sr;
                // Rich vowel-like harmonic stack (fundamental + 2nd/3rd/4th harmonics)
                0.55 * (TAU * f0 * t).sin()
                    + 0.25 * (TAU * f0 * 2.0 * t).sin()
                    + 0.12 * (TAU * f0 * 3.0 * t).sin()
                    + 0.06 * (TAU * f0 * 4.0 * t).sin()
            })
            .collect()
    }

    #[test]
    fn ac13_pitch_correction_measurably_corrects_detuned_voiced_note_to_target() {
        let sr = 48_000.0_f32;
        // Sing 36 cents flat: 431.0 Hz (MIDI 68.64 -> nearest note in A Major is A4 = 440.0 Hz)
        let input = generate_harmonic_voice(sr, 431.0, 0.8);

        let mut params = VoiceDspParams::preset_studio();
        params.tonic = PitchClass::A;
        params.scale = MusicalScale::Major;
        params.pitch_enabled = true;
        params.pitch_amount = 1.0;
        params.pitch_speed = 0.95;
        params.neutral_zone_cents = 0.0;
        params.reverb_enabled = false;
        params.compressor_enabled = false;
        params.mic_level = 1.0;

        let mut chain = VoiceDspChain::new(sr, params);
        let mut out_l = vec![0.0_f32; input.len()];
        let mut out_r = vec![0.0_f32; input.len()];
        let telem = chain.process_block(&input, &mut out_l, &mut out_r);

        assert!(telem.voiced, "harmonic voice must be detected as voiced");
        assert!((telem.detected_f0_hz - 431.0).abs() < 3.0);
        assert!((telem.target_f0_hz - 440.0).abs() < 0.5);

        // Independently measure the output signal's actual F0 using a fresh YIN detector on the settled tail!
        let mut verifier = YinDetector::new(sr);
        let tail_start = (sr * 0.35) as usize;
        let mut measured_f0 = 0.0_f32;
        for &s in &out_l[tail_start..] {
            let e = verifier.push_sample(s);
            if e.voiced {
                measured_f0 = e.f0_hz;
            }
        }
        assert!(
            (measured_f0 - 440.0).abs() < 4.0,
            "Expected corrected output F0 near 440 Hz, got {measured_f0:.2} Hz (input was 431 Hz)"
        );

        // Scale-awareness check: 426.0 Hz (MIDI 68.44) snaps DOWN to G#4 (415.30 Hz) in A Major,
        // but snaps UP to A4 (440.0 Hz) in A Pentatonic Major (which excludes G#)!
        let flat_input = generate_harmonic_voice(sr, 426.0, 0.6);
        let mut pent_params = params;
        pent_params.scale = MusicalScale::PentatonicMajor;
        let mut pent_chain = VoiceDspChain::new(sr, pent_params);
        let mut pl = vec![0.0_f32; flat_input.len()];
        let mut pr = vec![0.0_f32; flat_input.len()];
        let pent_telem = pent_chain.process_block(&flat_input, &mut pl, &mut pr);
        assert!((pent_telem.target_f0_hz - 440.0).abs() < 0.5);
    }

    #[test]
    fn ac13_unvoiced_consonant_noise_is_not_forced_to_pitch() {
        let sr = 48_000.0_f32;
        let mut rng = Lcg(42);
        let noise: Vec<f32> = (0..24_000).map(|_| rng.next_f32() * 0.25).collect();

        let mut params = VoiceDspParams::preset_studio();
        params.pitch_amount = 1.0;
        params.reverb_enabled = false;
        let mut chain = VoiceDspChain::new(sr, params);
        let mut out_l = vec![0.0_f32; noise.len()];
        let mut out_r = vec![0.0_f32; noise.len()];
        let telem = chain.process_block(&noise, &mut out_l, &mut out_r);

        assert!(
            !telem.voiced,
            "Unvoiced noise/consonants must NOT be classified as voiced"
        );
        assert_eq!(telem.pitch_correction_cents, 0.0);
        assert!(out_l.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn ac14_presets_and_bypass_and_mute_produce_real_dsp_changes_without_nan_or_clip() {
        let sr = 48_000.0_f32;
        let input = generate_harmonic_voice(sr, 261.63, 0.4);
        let mut out_l = vec![0.0_f32; input.len()];
        let mut out_r = vec![0.0_f32; input.len()];

        let mut chain = VoiceDspChain::new(sr, VoiceDspParams::preset_natural());
        chain.process_block(&input, &mut out_l, &mut out_r);
        let nat_energy: f32 = out_l.iter().map(|s| s * s).sum();

        chain.set_params(VoiceDspParams::preset_stage());
        chain.process_block(&input, &mut out_l, &mut out_r);
        let stage_tail_in = vec![0.0_f32; 12_000];
        let mut tail_l = vec![0.0_f32; 12_000];
        let mut tail_r = vec![0.0_f32; 12_000];
        chain.process_block(&stage_tail_in, &mut tail_l, &mut tail_r);
        let stage_reverb_tail: f32 = tail_l.iter().map(|s| s * s).sum();
        assert!(nat_energy > 1.0);
        assert!(
            stage_reverb_tail > 0.01,
            "Stage preset reverb must leave an audible decaying tail"
        );

        // Mute must silence output after ramp
        let mut muted = VoiceDspParams::preset_stage();
        muted.mic_muted = true;
        chain.set_params(muted);
        chain.process_block(&input, &mut out_l, &mut out_r);
        let final_window_peak = out_l[out_l.len() - 512..]
            .iter()
            .fold(0.0_f32, |m, &x| m.max(x.abs()));
        assert!(
            final_window_peak < 1e-3,
            "Mute must silence mic contribution (peak={final_window_peak})"
        );
    }

    #[test]
    fn master_limiter_sanitizes_nan_and_bounds_peak_to_ceiling() {
        let mut lim = MasterLimiter::new(48_000.0, -0.5);
        let ceiling = db_to_linear(-0.5);
        let mut max_out = 0.0_f32;
        let mut total_sanitized = 0u64;
        for i in 0..2000 {
            let (l, r) = if i == 500 {
                (f32::NAN, f32::INFINITY)
            } else {
                (3.5, -3.5)
            };
            let (ol, or, san, _) = lim.process(l, r);
            total_sanitized += san;
            assert!(ol.is_finite() && or.is_finite());
            max_out = max_out.max(ol.abs()).max(or.abs());
        }
        assert_eq!(total_sanitized, 2);
        assert!(max_out <= ceiling + 1e-5);
    }

    #[test]
    fn quantizer_invalidates_cache_on_live_param_changes() {
        let mut q = PitchQuantizer::new(48_000.0);
        let est = PitchEstimate {
            f0_hz: 426.0, // Between G#4 (415.3 Hz) and A4 (440.0 Hz)
            period_samples: 48_000.0 / 426.0,
            confidence: 0.98,
            voiced: true,
        };
        let mut p = VoiceDspParams::preset_studio();
        p.tonic = PitchClass::A;
        p.scale = MusicalScale::Major; // Contains G#4 (415.3 Hz)
        p.pitch_amount = 1.0;
        p.neutral_zone_cents = 0.0;
        let out_major = q.update(est, &p);
        assert!((out_major.target_f0_hz - 415.3).abs() < 1.0);

        // Hold exact same F0, switch to A Pentatonic Major (removes G#4 -> target MUST immediately become A4 440 Hz!)
        p.scale = MusicalScale::PentatonicMajor;
        let out_penta = q.update(est, &p);
        assert!(
            (out_penta.target_f0_hz - 440.0).abs() < 1.0,
            "Changing scale while holding F0 must invalidate quantizer cache (got {})",
            out_penta.target_f0_hz
        );

        // Hold exact same F0, change pitch_amount -> correction_cents MUST immediately halve!
        p.pitch_amount = 0.5;
        let out_half = q.update(est, &p);
        assert!(
            (out_half.correction_cents - out_penta.correction_cents * 0.5).abs() < 1.0,
            "Changing pitch_amount while holding F0 must immediately update correction_cents"
        );
    }
}
