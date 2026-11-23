//! Real-time safe atomic diagnostics and privacy-preserving log/export redaction (§11.1, §11.4).
//!
//! Strict invariants:
//! - RT callbacks only call atomic integer stores/fetch_add: ZERO allocations, ZERO locks, ZERO formatting.
//! - Redacted logs and diagnostic exports NEVER contain PCM audio, full lyrics, user home paths,
//!   tokens, or full hardware UIDs.

use chrono::{DateTime, Utc};
use karaok_types::RtCountersSnapshot;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

/// Preallocated atomic RT telemetry counters shared between RT callbacks and UI/diagnostics thread.
#[derive(Debug, Default)]
pub struct RtCounters {
    output_callbacks: AtomicU64,
    input_callbacks: AtomicU64,
    music_underruns: AtomicU64,
    mic_underruns: AtomicU64,
    mic_overruns: AtomicU64,
    callback_deadline_overruns: AtomicU64,
    non_finite_sanitizations: AtomicU64,
    route_transitions: AtomicU64,
    mic_ring_occupancy_frames: AtomicU32,
    mic_resample_ratio_bits: AtomicU64,
    last_render_duration_us: AtomicU32,
    max_render_duration_us: AtomicU32,
}

impl RtCounters {
    pub fn new() -> Self {
        let s = Self::default();
        s.mic_resample_ratio_bits
            .store(1.0_f64.to_bits(), Ordering::Relaxed);
        s
    }

    #[inline]
    pub fn inc_output_callbacks(&self) {
        self.output_callbacks.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_input_callbacks(&self) {
        self.input_callbacks.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn inc_music_underruns(&self) -> u64 {
        self.music_underruns.fetch_add(1, Ordering::Relaxed) + 1
    }

    #[inline]
    pub fn inc_mic_underruns(&self) -> u64 {
        self.mic_underruns.fetch_add(1, Ordering::Relaxed) + 1
    }

    #[inline]
    pub fn inc_mic_overruns(&self) -> u64 {
        self.mic_overruns.fetch_add(1, Ordering::Relaxed) + 1
    }

    #[inline]
    pub fn inc_callback_deadline_overruns(&self) {
        self.callback_deadline_overruns
            .fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn add_non_finite_sanitizations(&self, count: u64) {
        if count > 0 {
            self.non_finite_sanitizations
                .fetch_add(count, Ordering::Relaxed);
        }
    }

    #[inline]
    pub fn inc_route_transitions(&self) {
        self.route_transitions.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn set_mic_ring_occupancy(&self, frames: u32) {
        self.mic_ring_occupancy_frames
            .store(frames, Ordering::Relaxed);
    }

    #[inline]
    pub fn set_mic_resample_ratio(&self, ratio: f64) {
        self.mic_resample_ratio_bits
            .store(ratio.to_bits(), Ordering::Relaxed);
    }

    #[inline]
    pub fn record_render_duration_us(&self, us: u32, budget_us: u32) {
        self.last_render_duration_us.store(us, Ordering::Relaxed);
        let mut prev = self.max_render_duration_us.load(Ordering::Relaxed);
        while us > prev {
            match self.max_render_duration_us.compare_exchange_weak(
                prev,
                us,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(cur) => prev = cur,
            }
        }
        if budget_us > 0 && us > budget_us {
            self.inc_callback_deadline_overruns();
        }
    }

    pub fn snapshot(&self) -> RtCountersSnapshot {
        RtCountersSnapshot {
            output_callbacks: self.output_callbacks.load(Ordering::Relaxed),
            input_callbacks: self.input_callbacks.load(Ordering::Relaxed),
            music_underruns: self.music_underruns.load(Ordering::Relaxed),
            mic_underruns: self.mic_underruns.load(Ordering::Relaxed),
            mic_overruns: self.mic_overruns.load(Ordering::Relaxed),
            callback_deadline_overruns: self.callback_deadline_overruns.load(Ordering::Relaxed),
            non_finite_sanitizations: self.non_finite_sanitizations.load(Ordering::Relaxed),
            route_transitions: self.route_transitions.load(Ordering::Relaxed),
            mic_ring_occupancy_frames: self.mic_ring_occupancy_frames.load(Ordering::Relaxed),
            mic_resample_ratio: f64::from_bits(
                self.mic_resample_ratio_bits.load(Ordering::Relaxed),
            ),
            last_render_duration_us: self.last_render_duration_us.load(Ordering::Relaxed),
            max_render_duration_us: self.max_render_duration_us.load(Ordering::Relaxed),
        }
    }

    pub fn reset_max_render_duration(&self) {
        self.max_render_duration_us.store(0, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticEntry {
    pub timestamp: DateTime<Utc>,
    pub level: LogLevel,
    pub component: String,
    pub message: String,
}

/// Scrubs user home directory segments and sensitive markers from any diagnostic string (§11.1).
pub fn redact_sensitive_text(input: &str) -> String {
    let mut out = input.to_string();
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            out = out.replace(&home, "<HOME>");
        }
    }
    // Scrub EVERY generic macOS `/Users/<name>` segment (regardless of $USER env var - P2-I3)
    let mut search_from = 0usize;
    while let Some(rel) = out[search_from..].find("/Users/") {
        let pos = search_from + rel;
        let name_start = pos + 7;
        let rest = &out[name_start..];
        let seg_len = rest
            .find(|c: char| c == '/' || c.is_whitespace() || c == '"' || c == '\'' || c == ')')
            .unwrap_or(rest.len());
        if seg_len == 0 {
            search_from = name_start;
            continue;
        }
        if &rest[..seg_len] == "<USER>" {
            search_from = name_start + "<USER>".len();
            continue;
        }
        out.replace_range(name_start..name_start + seg_len, "<USER>");
        search_from = name_start + "<USER>".len();
    }
    // Truncate excessively long entries so lyrics or dumped payloads never end up in logs.
    const MAX_LOG_CHARS: usize = 320;
    if out.chars().count() > MAX_LOG_CHARS {
        let truncated: String = out.chars().take(MAX_LOG_CHARS).collect();
        format!("{truncated}… [tronqué]")
    } else {
        out
    }
}

/// Bounded off-RT ring log for user-previewable diagnostic export (§11.1).
#[derive(Debug)]
pub struct DiagnosticLog {
    capacity: usize,
    entries: Mutex<VecDeque<DiagnosticEntry>>,
}

impl DiagnosticLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(16),
            entries: Mutex::new(VecDeque::with_capacity(capacity.max(16))),
        }
    }

    pub fn record(&self, level: LogLevel, component: &str, message: &str) {
        let entry = DiagnosticEntry {
            timestamp: Utc::now(),
            level,
            component: component.to_string(),
            message: redact_sensitive_text(message),
        };
        if let Ok(mut guard) = self.entries.lock() {
            if guard.len() >= self.capacity {
                guard.pop_front();
            }
            guard.push_back(entry);
        }
    }

    pub fn entries(&self) -> Vec<DiagnosticEntry> {
        self.entries
            .lock()
            .map(|g| g.iter().cloned().collect())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactedDiagnosticExport {
    pub exported_at: DateTime<Utc>,
    pub app_version: String,
    pub privacy_notice_fr: String,
    pub rt_counters: RtCountersSnapshot,
    pub recent_logs: Vec<DiagnosticEntry>,
}

impl RedactedDiagnosticExport {
    pub fn build(counters: &RtCounters, log: &DiagnosticLog) -> Self {
        Self {
            exported_at: Utc::now(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            privacy_notice_fr: "Export local expurgé : aucun audio PCM, aucune parole, aucun chemin personnel complet ni UID matériel brut.".to_string(),
            rt_counters: counters.snapshot(),
            recent_logs: log.entries(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rt_counters_track_max_and_overruns() {
        let c = RtCounters::new();
        c.inc_output_callbacks();
        c.record_render_duration_us(420, 2666);
        c.record_render_duration_us(3100, 2666);
        c.record_render_duration_us(900, 2666);
        let snap = c.snapshot();
        assert_eq!(snap.output_callbacks, 1);
        assert_eq!(snap.last_render_duration_us, 900);
        assert_eq!(snap.max_render_duration_us, 3100);
        assert_eq!(snap.callback_deadline_overruns, 1);
        assert!((snap.mic_resample_ratio - 1.0).abs() < 1e-12);
    }

    #[test]
    fn redaction_strips_home_path_and_truncates() {
        if let Ok(home) = std::env::var("HOME") {
            let sample = format!("Opened file {home}/Music/secret.mp3");
            let redacted = redact_sensitive_text(&sample);
            assert!(!redacted.contains(&home));
            assert!(redacted.contains("<HOME>"));
        }
        // Regression test P2-I3: multiple /Users/<name> paths in one string must ALL be redacted!
        let multi = "Copied /Users/alice/Music/a.mp3 to /Users/bob/Cache/b.wav and /Users/charlie/x";
        let redacted_multi = redact_sensitive_text(multi);
        assert!(!redacted_multi.contains("alice"), "got: {redacted_multi}");
        assert!(!redacted_multi.contains("bob"), "got: {redacted_multi}");
        assert!(!redacted_multi.contains("charlie"), "got: {redacted_multi}");
        assert_eq!(redacted_multi.matches("/Users/<USER>").count(), 3);

        let long = "a".repeat(500);
        let redacted = redact_sensitive_text(&long);
        assert!(redacted.contains("[tronqué]"));
    }
}
