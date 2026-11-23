//! Strongly-typed domain contracts, invariants, and bounded parameter models for Karaok Party.
//!
//! Design invariants enforced at the type level (SPECIFICATIONS_TECHNIQUES.md §9):
//! - Unknown hardware properties remain `Option::None`, never coerced to `0` or empty string.
//! - Audio preferences persist either `SystemDefault` or `Fixed { uid }`, never ephemeral HAL IDs.
//! - Monitoring defaults to `false` to protect against accidental acoustic feedback.
//! - Lyrics distinguish `first_vocal_activity_s`, `first_sung_activity_s`, and `first_lyric_cue_s`.
//! - Finite floating-point bounds are strictly enforced before entering DSP or persistence.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

pub const SCHEMA_VERSION: u32 = 1;
pub const FFI_ABI_VERSION: u32 = 1;

/// Stable machine-readable error codes crossing module and FFI boundaries (§9.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    PermissionDenied,
    DeviceUnavailable,
    FormatUnsupported,
    RouteOpenFailed,
    ModelMissing,
    ModelUnverified,
    InvalidMediaFile,
    InsufficientDiskSpace,
    JobCancelled,
    StaleRevision,
    DatabaseNewerSchema,
    DatabaseError,
    InvalidParameter,
    QueueFull,
    NotFound,
    IoError,
    InternalError,
}

#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[error("[{code:?}] {message_fr}")]
pub struct KaraokError {
    pub code: ErrorCode,
    pub message_fr: String,
    pub actionable_hint_fr: Option<String>,
}

impl KaraokError {
    pub fn new(code: ErrorCode, message_fr: impl Into<String>) -> Self {
        Self {
            code,
            message_fr: message_fr.into(),
            actionable_hint_fr: None,
        }
    }

    pub fn with_hint(
        code: ErrorCode,
        message_fr: impl Into<String>,
        hint_fr: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message_fr: message_fr.into(),
            actionable_hint_fr: Some(hint_fr.into()),
        }
    }
}

pub type KaraokResult<T> = Result<T, KaraokError>;

/// Produce a non-reversible local redacted identifier for a hardware UID (§11.1).
pub fn redact_device_uid(uid: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(uid.as_bytes());
    let digest = hasher.finalize();
    format!(
        "dev-{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportType {
    Usb,
    Bluetooth,
    BuiltIn,
    Hdmi,
    DisplayPort,
    Virtual,
    Aggregate,
    Unknown,
}

impl TransportType {
    pub fn label_fr(self) -> &'static str {
        match self {
            Self::Usb => "USB",
            Self::Bluetooth => "Bluetooth",
            Self::BuiltIn => "Intégré",
            Self::Hdmi => "HDMI",
            Self::DisplayPort => "DisplayPort",
            Self::Virtual => "Virtuel",
            Self::Aggregate => "Agrégé",
            Self::Unknown => "Inconnu",
        }
    }

    /// True if output route carries elevated acoustic feedback risk when paired with an open mic.
    pub fn is_feedback_risky_output(self) -> bool {
        matches!(self, Self::BuiltIn | Self::Hdmi | Self::DisplayPort)
    }
}

/// Core Audio hardware device snapshot (§5.1). Unknown fields are `None`, never `0`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioDeviceInfo {
    /// Persistent Core Audio UID — internal only, never written to public logs/exports.
    pub uid: String,
    /// Safe redacted local fingerprint for diagnostics/UI reference.
    pub redacted_uid: String,
    pub name: String,
    pub manufacturer: Option<String>,
    pub transport: TransportType,
    pub input_channels: u32,
    pub output_channels: u32,
    pub nominal_sample_rate_hz: Option<f64>,
    pub available_sample_rate_ranges_hz: Vec<(f64, f64)>,
    pub buffer_frame_size: Option<u32>,
    pub buffer_frame_size_range: Option<(u32, u32)>,
    pub input_latency_frames: Option<u32>,
    pub output_latency_frames: Option<u32>,
    pub safety_offset_frames: Option<u32>,
    pub is_alive: bool,
    pub is_default_input: bool,
    pub is_default_output: bool,
    /// Suggestion heuristic only (name/manufacturer match); NEVER proof of identity without user confirmation.
    pub nothing_headphone_a_candidate: bool,
}

impl AudioDeviceInfo {
    pub fn has_input(&self) -> bool {
        self.input_channels > 0
    }

    pub fn has_output(&self) -> bool {
        self.output_channels > 0
    }

    /// Heuristic to suggest a Nothing Headphone (a) USB-C device to the user for explicit confirmation.
    /// Does NOT conflate Headphone (1) or CMF models (§1.1).
    pub fn detect_nothing_headphone_a_candidate(
        name: &str,
        manufacturer: Option<&str>,
        transport: TransportType,
    ) -> bool {
        let lower_name = name.to_lowercase();
        let lower_mfr = manufacturer.unwrap_or("").to_lowercase();
        let is_nothing = lower_name.contains("nothing") || lower_mfr.contains("nothing");
        let is_headphone_a = lower_name.contains("headphone (a)")
            || lower_name.contains("headphone(a)")
            || (lower_name.contains("nothing") && lower_name.contains("(a)"));
        let excluded_other_model =
            lower_name.contains("headphone (1)") || lower_name.contains("cmf");
        is_nothing && is_headphone_a && !excluded_other_model && transport == TransportType::Usb
    }
}

/// Persistent device selection rule (§5.1). Never stores an ephemeral HAL `AudioObjectID`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum DeviceSelector {
    SystemDefault,
    Fixed {
        uid: String,
        last_known_name: String,
        confirmed_by_user: bool,
    },
}

/// Negotiated live audio format read back from Core Audio after stream open (§5.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveStreamFormat {
    pub device_name: String,
    pub redacted_uid: String,
    pub transport: TransportType,
    pub sample_rate_hz: f64,
    pub channels: u32,
    pub buffer_frames: u32,
    pub driver_latency_frames: Option<u32>,
    pub safety_offset_frames: Option<u32>,
    pub is_simulated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteStatus {
    Unconfigured,
    Ready,
    UnavailablePreferred,
    PermissionRequired,
    AwaitingReconnectConfirmation,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioPreference {
    pub input: DeviceSelector,
    pub output: DeviceSelector,
    pub input_channel_index: u32,
    pub sample_rate_preference_hz: Option<f64>,
    pub buffer_frames_preference: Option<u32>,
    pub voice_dsp: VoiceDspParams,
    pub mix: MixParams,
}

impl Default for AudioPreference {
    fn default() -> Self {
        Self {
            input: DeviceSelector::SystemDefault,
            output: DeviceSelector::SystemDefault,
            input_channel_index: 0,
            sample_rate_preference_hz: None,
            buffer_frames_preference: None,
            voice_dsp: VoiceDspParams::preset_natural(),
            mix: MixParams::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoicePreset {
    Natural,
    Studio,
    Stage,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PitchClass {
    C = 0,
    CSharp = 1,
    D = 2,
    DSharp = 3,
    E = 4,
    F = 5,
    FSharp = 6,
    G = 7,
    GSharp = 8,
    A = 9,
    ASharp = 10,
    B = 11,
}

impl PitchClass {
    pub fn from_semitone(semitone: i32) -> Self {
        match semitone.rem_euclid(12) {
            0 => Self::C,
            1 => Self::CSharp,
            2 => Self::D,
            3 => Self::DSharp,
            4 => Self::E,
            5 => Self::F,
            6 => Self::FSharp,
            7 => Self::G,
            8 => Self::GSharp,
            9 => Self::A,
            10 => Self::ASharp,
            _ => Self::B,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::C => "Do (C)",
            Self::CSharp => "Do# (C#)",
            Self::D => "Ré (D)",
            Self::DSharp => "Mib (D#)",
            Self::E => "Mi (E)",
            Self::F => "Fa (F)",
            Self::FSharp => "Fa# (F#)",
            Self::G => "Sol (G)",
            Self::GSharp => "Sol# (G#)",
            Self::A => "La (A)",
            Self::ASharp => "Sib (A#)",
            Self::B => "Si (B)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MusicalScale {
    Chromatic,
    Major,
    NaturalMinor,
    HarmonicMinor,
    PentatonicMajor,
    PentatonicMinor,
    Dorian,
    Mixolydian,
}

impl MusicalScale {
    /// Allowed pitch-class offsets relative to tonic (0..12).
    pub fn intervals(self) -> &'static [u8] {
        match self {
            Self::Chromatic => &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
            Self::Major => &[0, 2, 4, 5, 7, 9, 11],
            Self::NaturalMinor => &[0, 2, 3, 5, 7, 8, 10],
            Self::HarmonicMinor => &[0, 2, 3, 5, 7, 8, 11],
            Self::PentatonicMajor => &[0, 2, 4, 7, 9],
            Self::PentatonicMinor => &[0, 3, 5, 7, 10],
            Self::Dorian => &[0, 2, 3, 5, 7, 9, 10],
            Self::Mixolydian => &[0, 2, 4, 5, 7, 9, 10],
        }
    }
}

/// Bounded DSP parameters for the live vocal chain (§8.1–§8.3).
/// Monitoring starts OFF (`false`) by safety invariant (§3.1, §8.3).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct VoiceDspParams {
    pub preset: VoicePreset,
    pub bypass: bool,
    pub input_gain_db: f32,
    pub hpf_enabled: bool,
    pub hpf_cutoff_hz: f32,
    pub pitch_enabled: bool,
    /// Correction strength `0.0` (none) to `1.0` (full snap toward target note).
    pub pitch_amount: f32,
    /// Correction speed `0.05` (slow/natural) to `1.0` (fast).
    pub pitch_speed: f32,
    /// Neutral zone in cents around target note to preserve intentional vibrato (`0..=45`).
    pub neutral_zone_cents: f32,
    /// Hysteresis in cents preventing rapid semitone oscillation (`5..=35`).
    pub hysteresis_cents: f32,
    pub tonic: PitchClass,
    pub scale: MusicalScale,
    pub compressor_enabled: bool,
    pub compressor_threshold_db: f32,
    pub compressor_ratio: f32,
    pub reverb_enabled: bool,
    pub reverb_wet: f32,
    pub reverb_decay_s: f32,
    pub reverb_predelay_ms: f32,
    pub reverb_damping: f32,
    pub mic_level: f32,
    pub mic_muted: bool,
    pub monitoring_enabled: bool,
    pub monitoring_level: f32,
}

impl VoiceDspParams {
    /// Preset Naturel: gentle correction, subtle reverb, soft dynamics (§8.3).
    pub fn preset_natural() -> Self {
        Self {
            preset: VoicePreset::Natural,
            bypass: false,
            input_gain_db: 0.0,
            hpf_enabled: true,
            hpf_cutoff_hz: 80.0,
            pitch_enabled: true,
            pitch_amount: 0.35,
            pitch_speed: 0.25,
            neutral_zone_cents: 18.0,
            hysteresis_cents: 16.0,
            tonic: PitchClass::C,
            scale: MusicalScale::Chromatic,
            compressor_enabled: true,
            compressor_threshold_db: -18.0,
            compressor_ratio: 2.2,
            reverb_enabled: true,
            reverb_wet: 0.14,
            reverb_decay_s: 1.0,
            reverb_predelay_ms: 14.0,
            reverb_damping: 0.55,
            mic_level: 0.85,
            mic_muted: false,
            monitoring_enabled: false,
            monitoring_level: 0.5,
        }
    }

    /// Preset Studio: moderate correction, short tight reverb, controlled dynamics (§8.3).
    pub fn preset_studio() -> Self {
        Self {
            preset: VoicePreset::Studio,
            bypass: false,
            input_gain_db: 0.0,
            hpf_enabled: true,
            hpf_cutoff_hz: 90.0,
            pitch_enabled: true,
            pitch_amount: 0.68,
            pitch_speed: 0.55,
            neutral_zone_cents: 10.0,
            hysteresis_cents: 14.0,
            tonic: PitchClass::C,
            scale: MusicalScale::Chromatic,
            compressor_enabled: true,
            compressor_threshold_db: -20.0,
            compressor_ratio: 3.2,
            reverb_enabled: true,
            reverb_wet: 0.20,
            reverb_decay_s: 0.85,
            reverb_predelay_ms: 10.0,
            reverb_damping: 0.48,
            mic_level: 0.85,
            mic_muted: false,
            monitoring_enabled: false,
            monitoring_level: 0.5,
        }
    }

    /// Preset Scène: fuller reverb, active dynamic control, expressive correction (§8.3).
    pub fn preset_stage() -> Self {
        Self {
            preset: VoicePreset::Stage,
            bypass: false,
            input_gain_db: 0.0,
            hpf_enabled: true,
            hpf_cutoff_hz: 95.0,
            pitch_enabled: true,
            pitch_amount: 0.55,
            pitch_speed: 0.42,
            neutral_zone_cents: 12.0,
            hysteresis_cents: 15.0,
            tonic: PitchClass::C,
            scale: MusicalScale::Chromatic,
            compressor_enabled: true,
            compressor_threshold_db: -22.0,
            compressor_ratio: 4.0,
            reverb_enabled: true,
            reverb_wet: 0.34,
            reverb_decay_s: 2.1,
            reverb_predelay_ms: 24.0,
            reverb_damping: 0.42,
            mic_level: 0.90,
            mic_muted: false,
            monitoring_enabled: false,
            monitoring_level: 0.5,
        }
    }

    pub fn from_preset_keeping_routing(preset: VoicePreset, current: Self) -> Self {
        let mut base = match preset {
            VoicePreset::Natural => Self::preset_natural(),
            VoicePreset::Studio => Self::preset_studio(),
            VoicePreset::Stage => Self::preset_stage(),
            VoicePreset::Custom => current,
        };
        base.input_gain_db = current.input_gain_db;
        base.tonic = current.tonic;
        base.scale = current.scale;
        base.mic_level = current.mic_level;
        base.mic_muted = current.mic_muted;
        base.monitoring_enabled = current.monitoring_enabled;
        base.monitoring_level = current.monitoring_level;
        base.clamped()
    }

    /// Sanitize any non-finite input and clamp every field into its safe domain (§8.1).
    pub fn clamped(mut self) -> Self {
        self.input_gain_db = finite_clamp(self.input_gain_db, -24.0, 24.0, 0.0);
        self.hpf_cutoff_hz = finite_clamp(self.hpf_cutoff_hz, 40.0, 200.0, 80.0);
        self.pitch_amount = finite_clamp(self.pitch_amount, 0.0, 1.0, 0.35);
        self.pitch_speed = finite_clamp(self.pitch_speed, 0.05, 1.0, 0.25);
        self.neutral_zone_cents = finite_clamp(self.neutral_zone_cents, 0.0, 45.0, 15.0);
        self.hysteresis_cents = finite_clamp(self.hysteresis_cents, 5.0, 35.0, 15.0);
        self.compressor_threshold_db =
            finite_clamp(self.compressor_threshold_db, -36.0, -3.0, -18.0);
        self.compressor_ratio = finite_clamp(self.compressor_ratio, 1.0, 8.0, 2.5);
        self.reverb_wet = finite_clamp(self.reverb_wet, 0.0, 0.75, 0.14);
        self.reverb_decay_s = finite_clamp(self.reverb_decay_s, 0.2, 4.0, 1.0);
        self.reverb_predelay_ms = finite_clamp(self.reverb_predelay_ms, 0.0, 80.0, 14.0);
        self.reverb_damping = finite_clamp(self.reverb_damping, 0.05, 0.95, 0.5);
        self.mic_level = finite_clamp(self.mic_level, 0.0, 1.5, 0.85);
        self.monitoring_level = finite_clamp(self.monitoring_level, 0.0, 1.0, 0.5);
        self
    }
}

/// Separate gains for accompaniment, original vocal stem, and original path (§6.1, REQ-009).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MixParams {
    pub accompaniment_gain: f32,
    pub original_vocal_gain: f32,
    pub use_stems: bool,
    pub master_limiter_ceiling_db: f32,
}

impl Default for MixParams {
    fn default() -> Self {
        Self {
            accompaniment_gain: 0.90,
            original_vocal_gain: 0.25,
            use_stems: true,
            master_limiter_ceiling_db: -0.5,
        }
    }
}

impl MixParams {
    pub fn clamped(mut self) -> Self {
        self.accompaniment_gain = finite_clamp(self.accompaniment_gain, 0.0, 1.5, 0.9);
        self.original_vocal_gain = finite_clamp(self.original_vocal_gain, 0.0, 1.5, 0.25);
        self.master_limiter_ceiling_db =
            finite_clamp(self.master_limiter_ceiling_db, -6.0, -0.1, -0.5);
        self
    }
}

#[inline]
pub fn finite_clamp(value: f32, min: f32, max: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(min, max)
    } else {
        fallback
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackAvailability {
    Available,
    MissingOriginal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: Uuid,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_s: f64,
    pub original_sha256: String,
    pub managed_original_path: String,
    pub format: String,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub encoder_delay_samples: u32,
    pub is_favorite: bool,
    pub availability: TrackAvailability,
    pub estimated_tonic: Option<PitchClass>,
    pub estimated_scale: Option<MusicalScale>,
    pub key_confidence: Option<f32>,
    pub key_ambiguous: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaylistItem {
    pub id: Uuid,
    pub playlist_id: Uuid,
    pub track_id: Uuid,
    pub position: i64,
    pub added_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Original,
    CanonicalPcm,
    VocalStem,
    AccompanimentStem,
    Waveform,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaArtifact {
    pub id: Uuid,
    pub track_id: Uuid,
    pub kind: ArtifactKind,
    pub cache_key: String,
    pub file_path: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub duration_s: f64,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub method: String,
    pub method_version: String,
    pub model_hash: Option<String>,
    pub delay_compensation_samples: i64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisJobState {
    Queued,
    Running,
    Completed,
    NeedsReview,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisStage {
    Decode,
    Separation,
    VocalActivity,
    KeyEstimation,
    Alignment,
    Finalized,
}

impl AnalysisStage {
    pub fn label_fr(self) -> &'static str {
        match self {
            Self::Decode => "Décodage canonique",
            Self::Separation => "Séparation voix / accompagnement",
            Self::VocalActivity => "Repérage du chant",
            Self::KeyEstimation => "Estimation de tonalité",
            Self::Alignment => "Synchronisation des paroles",
            Self::Finalized => "Vérification",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalysisJob {
    pub id: Uuid,
    pub track_id: Uuid,
    pub state: AnalysisJobState,
    pub stage: AnalysisStage,
    /// Observable progress 0.0..=1.0 within pipeline (never animated fake percent).
    pub progress: f32,
    pub status_detail_fr: String,
    pub pipeline_version: String,
    pub last_completed_stage: Option<AnalysisStage>,
    pub error: Option<KaraokError>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VocalRegionKind {
    Singing,
    Speech,
    Uncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    NeedsReview,
    UserValidated,
    NoSingingDetected,
    LyricsMissing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VocalRegion {
    pub id: Uuid,
    pub start_s: f64,
    pub end_s: f64,
    pub kind: VocalRegionKind,
    /// Native uncalibrated score from detector (NOT presented as calibrated ground-truth probability).
    pub raw_score: f32,
    pub uncertainty_s: f64,
    pub method: String,
    pub method_version: String,
    pub human_override: Option<VocalRegionKind>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VocalActivitySummary {
    pub track_id: Uuid,
    pub first_vocal_activity_s: Option<f64>,
    pub first_sung_activity_s: Option<f64>,
    pub no_singing_detected: bool,
    pub method: String,
    pub method_version: String,
    pub review_status: ReviewStatus,
    pub regions: Vec<VocalRegion>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimingSource {
    LrcExplicit,
    LrcInferredEnd,
    AlignedLineAuto,
    AlignedWordValidated,
    Manual,
    Untimed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LyricsGranularity {
    Untimed,
    Line,
    WordValidated,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WordCue {
    pub id: String,
    pub text: String,
    /// Character range (Unicode scalar index, NEVER raw byte index — §9.3).
    pub char_start: usize,
    pub char_end: usize,
    pub start_s: Option<f64>,
    pub end_s: Option<f64>,
    pub timing_source: TimingSource,
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LyricCue {
    pub id: String,
    pub text: String,
    pub start_s: Option<f64>,
    pub end_s: Option<f64>,
    pub timing_source: TimingSource,
    /// Always `None` for human manual placement (§9.3).
    pub confidence: Option<f32>,
    pub is_instrumental_marker: bool,
    pub human_validated: bool,
    pub words: Vec<WordCue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LyricsDocument {
    pub schema_version: u32,
    pub track_id: Uuid,
    pub timebase: String,
    pub text_revision: u64,
    pub text_sha256: String,
    pub raw_text: String,
    pub language: Option<String>,
    pub provenance: String,
    pub granularity: LyricsGranularity,
    /// Positive `global_offset_ms` means display LATER: active interval `[s + d, e + d)` (§6.2).
    pub global_offset_ms: i64,
    pub status: ReviewStatus,
    pub first_vocal_activity_s: Option<f64>,
    pub first_sung_activity_s: Option<f64>,
    pub first_lyric_cue_s: Option<f64>,
    pub warnings: Vec<String>,
    pub cues: Vec<LyricCue>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LatencyOrigin {
    DriverEstimateOnly,
    AcousticLoopbackMeasured,
    Simulated,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatencyProfile {
    pub id: Uuid,
    pub route_fingerprint: String,
    pub input_name: String,
    pub output_name: String,
    pub sample_rate_hz: f64,
    pub buffer_frames: u32,
    pub pitch_correction_enabled: bool,
    pub reverb_enabled: bool,
    pub origin: LatencyOrigin,
    pub input_driver_ms: Option<f64>,
    pub output_driver_ms: Option<f64>,
    pub dsp_algorithmic_ms: f64,
    pub total_estimated_ms: Option<f64>,
    pub measured_p50_ms: Option<f64>,
    pub measured_p95_ms: Option<f64>,
    pub measured_p99_ms: Option<f64>,
    pub os_version: String,
    pub protocol_notes: String,
    pub measured_at: DateTime<Utc>,
    pub valid: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    StemSeparation,
    SingingDetection,
    LyricsAlignment,
    SpeechTranscription,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelInstallState {
    NotInstalled,
    InstalledVerified,
    HashMismatch,
    LicenseUnverified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelManifest {
    pub id: String,
    pub display_name: String,
    pub capability: ModelCapability,
    pub version: String,
    pub official_source_url: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub author: String,
    pub code_license: String,
    pub weights_license: String,
    pub languages: Vec<String>,
    pub runtime: String,
    pub verified_date: String,
    pub local_path: Option<String>,
    pub state: ModelInstallState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportState {
    Stopped = 0,
    Playing = 1,
    Paused = 2,
    Buffering = 3,
    RouteLost = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresentationQuality {
    BackendTimestamp = 0,
    OutputCursorCompensated = 1,
    Uncertain = 2,
}

/// Real-time transport snapshot published by the audio engine's master output clock (§6.2).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TransportSnapshot {
    pub generation: u64,
    pub state: TransportState,
    pub source_position_s: f64,
    pub duration_s: f64,
    pub monotonic_host_ns: u64,
    pub playback_ratio: f64,
    pub presentation_latency_ms: f64,
    pub quality: PresentationQuality,
    pub using_stems: bool,
    pub stems_available: bool,
}

impl Default for TransportSnapshot {
    fn default() -> Self {
        Self {
            generation: 0,
            state: TransportState::Stopped,
            source_position_s: 0.0,
            duration_s: 0.0,
            monotonic_host_ns: 0,
            playback_ratio: 1.0,
            presentation_latency_ms: 0.0,
            quality: PresentationQuality::Uncertain,
            using_stems: false,
            stems_available: false,
        }
    }
}

/// Real-time metering and pitch-correction telemetry read off-RT (§3.1, §8.2, §11.4).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MeterSnapshot {
    pub input_pre_peak: f32,
    pub input_pre_rms: f32,
    pub input_clipped: bool,
    pub voice_post_peak: f32,
    pub voice_post_rms: f32,
    pub output_peak: f32,
    pub output_clipped: bool,
    pub detected_f0_hz: f32,
    pub target_f0_hz: f32,
    pub pitch_confidence: f32,
    pub voiced: bool,
    pub pitch_correction_cents: f32,
    pub active_mic_capture: bool,
}

impl Default for MeterSnapshot {
    fn default() -> Self {
        Self {
            input_pre_peak: 0.0,
            input_pre_rms: 0.0,
            input_clipped: false,
            voice_post_peak: 0.0,
            voice_post_rms: 0.0,
            output_peak: 0.0,
            output_clipped: false,
            detected_f0_hz: 0.0,
            target_f0_hz: 0.0,
            pitch_confidence: 0.0,
            voiced: false,
            pitch_correction_cents: 0.0,
            active_mic_capture: false,
        }
    }
}

/// Lock-free RT health counters aggregated off-RT (§11.4).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct RtCountersSnapshot {
    pub output_callbacks: u64,
    pub input_callbacks: u64,
    pub music_underruns: u64,
    pub mic_underruns: u64,
    pub mic_overruns: u64,
    pub callback_deadline_overruns: u64,
    pub non_finite_sanitizations: u64,
    pub route_transitions: u64,
    pub mic_ring_occupancy_frames: u32,
    pub mic_resample_ratio: f64,
    pub last_render_duration_us: u32,
    pub max_render_duration_us: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AppEvent {
    DevicesChanged {
        generation: u64,
        inputs: Vec<AudioDeviceInfo>,
        outputs: Vec<AudioDeviceInfo>,
    },
    RouteOpening {
        role: String,
        device_name: String,
    },
    RouteReady {
        role: String,
        format: ActiveStreamFormat,
    },
    RouteFailed {
        role: String,
        error: KaraokError,
    },
    RouteLost {
        role: String,
        device_name: String,
        message_fr: String,
        requires_user_confirmation_to_resume: bool,
    },
    PreparationProgress {
        job: AnalysisJob,
    },
    ArtifactReady {
        track_id: Uuid,
        kind: ArtifactKind,
    },
    LyricsNeedsReview {
        track_id: Uuid,
        reason_fr: String,
    },
    AudioUnderrun {
        stream: String,
        count: u64,
    },
    Error {
        error: KaraokError,
    },
}

/// Explicit source-to-presentation timeline conversion (§6.2).
/// At normal speed (`playback_ratio == 1.0`), converts output frames and subtracts presentation delay once.
pub fn compute_presentation_source_position_s(
    rendered_source_s: f64,
    presentation_delay_s: f64,
    playback_ratio: f64,
    duration_s: f64,
    already_presentation_timestamped: bool,
) -> f64 {
    if !rendered_source_s.is_finite() || !duration_s.is_finite() || duration_s <= 0.0 {
        return 0.0;
    }
    let ratio = if playback_ratio.is_finite() && playback_ratio > 0.0 {
        playback_ratio
    } else {
        1.0
    };
    let delay = if already_presentation_timestamped
        || !presentation_delay_s.is_finite()
        || presentation_delay_s < 0.0
    {
        0.0
    } else {
        presentation_delay_s * ratio
    };
    (rendered_source_s - delay).clamp(0.0, duration_s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitoring_starts_disabled_in_all_presets() {
        assert!(!VoiceDspParams::preset_natural().monitoring_enabled);
        assert!(!VoiceDspParams::preset_studio().monitoring_enabled);
        assert!(!VoiceDspParams::preset_stage().monitoring_enabled);
        assert!(!AudioPreference::default().voice_dsp.monitoring_enabled);
    }

    #[test]
    fn non_finite_dsp_params_are_sanitized_and_clamped() {
        let mut p = VoiceDspParams::preset_natural();
        p.input_gain_db = f32::NAN;
        p.pitch_amount = f32::INFINITY;
        p.reverb_wet = -5.0;
        p.reverb_decay_s = 99.0;
        let clean = p.clamped();
        assert_eq!(clean.input_gain_db, 0.0);
        assert_eq!(clean.pitch_amount, 0.35);
        assert_eq!(clean.reverb_wet, 0.0);
        assert_eq!(clean.reverb_decay_s, 4.0);
    }

    #[test]
    fn nothing_headphone_a_heuristic_distinguishes_models_and_transport() {
        assert!(AudioDeviceInfo::detect_nothing_headphone_a_candidate(
            "Nothing Headphone (a)",
            Some("Nothing Tech"),
            TransportType::Usb
        ));
        assert!(!AudioDeviceInfo::detect_nothing_headphone_a_candidate(
            "Nothing Headphone (1)",
            Some("Nothing Tech"),
            TransportType::Usb
        ));
        assert!(!AudioDeviceInfo::detect_nothing_headphone_a_candidate(
            "CMF Headphone Pro",
            Some("Nothing Tech"),
            TransportType::Usb
        ));
        assert!(!AudioDeviceInfo::detect_nothing_headphone_a_candidate(
            "Nothing Headphone (a)",
            Some("Nothing Tech"),
            TransportType::Bluetooth
        ));
    }

    #[test]
    fn presentation_position_does_not_double_subtract_latency() {
        let pos_compensated = compute_presentation_source_position_s(10.0, 0.025, 1.0, 180.0, false);
        assert!((pos_compensated - 9.975).abs() < 1e-9);
        let pos_already_ts = compute_presentation_source_position_s(10.0, 0.025, 1.0, 180.0, true);
        assert!((pos_already_ts - 10.0).abs() < 1e-9);
    }

    #[test]
    fn redact_uid_is_deterministic_and_does_not_expose_raw_uid() {
        let raw = "AppleUSBAudioEngine:NothingTech:SecretSerial12345";
        let red = redact_device_uid(raw);
        assert!(red.starts_with("dev-"));
        assert!(!red.contains("SecretSerial"));
        assert_eq!(red, redact_device_uid(raw));
    }
}
