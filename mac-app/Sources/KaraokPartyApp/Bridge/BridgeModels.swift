import Foundation

// MARK: - Error & Envelope Contracts

struct KaraokErrorDTO: Codable, Error, Identifiable, Equatable {
    var id: String { "\(code):\(messageFr)" }
    let code: String
    let messageFr: String
    let actionableHintFr: String?

    enum CodingKeys: String, CodingKey {
        case code
        case messageFr = "message_fr"
        case actionableHintFr = "actionable_hint_fr"
    }
}

struct ControllerResponseEnvelope<T: Decodable>: Decodable {
    let ok: String
    let data: T?
    let error: KaraokErrorDTO?
}

// MARK: - Audio Devices & Routing

struct AudioDeviceDTO: Codable, Identifiable, Equatable, Hashable {
    var id: String { uid }
    let uid: String
    let redactedUid: String
    let name: String
    let manufacturer: String?
    let transport: String
    let inputChannels: UInt32
    let outputChannels: UInt32
    let nominalSampleRateHz: Double?
    let availableSampleRateRangesHz: [[Double]]
    let bufferFrameSize: UInt32?
    let bufferFrameSizeRange: [UInt32]?
    let inputLatencyFrames: UInt32?
    let outputLatencyFrames: UInt32?
    let safetyOffsetFrames: UInt32?
    let isAlive: Bool
    let isDefaultInput: Bool
    let isDefaultOutput: Bool
    let nothingHeadphoneACandidate: Bool

    var transportLabelFr: String {
        switch transport {
        case "usb": return "USB"
        case "bluetooth": return "Bluetooth"
        case "built_in": return "Intégré"
        case "hdmi": return "HDMI"
        case "display_port": return "DisplayPort"
        case "virtual": return "Virtuel"
        case "aggregate": return "Agrégé"
        default: return "Inconnu"
        }
    }

    var sampleRateLabel: String {
        if let sr = nominalSampleRateHz {
            return String(format: "%.1f kHz", sr / 1000.0)
        }
        return "Inconnu"
    }

    enum CodingKeys: String, CodingKey {
        case uid
        case redactedUid = "redacted_uid"
        case name, manufacturer, transport
        case inputChannels = "input_channels"
        case outputChannels = "output_channels"
        case nominalSampleRateHz = "nominal_sample_rate_hz"
        case availableSampleRateRangesHz = "available_sample_rate_ranges_hz"
        case bufferFrameSize = "buffer_frame_size"
        case bufferFrameSizeRange = "buffer_frame_size_range"
        case inputLatencyFrames = "input_latency_frames"
        case outputLatencyFrames = "output_latency_frames"
        case safetyOffsetFrames = "safety_offset_frames"
        case isAlive = "is_alive"
        case isDefaultInput = "is_default_input"
        case isDefaultOutput = "is_default_output"
        case nothingHeadphoneACandidate = "nothing_headphone_a_candidate"
    }
}

struct DeviceSelectorDTO: Codable, Equatable {
    let mode: String
    let uid: String?
    let lastKnownName: String?
    let confirmedByUser: Bool?

    static let systemDefault = DeviceSelectorDTO(mode: "system_default", uid: nil, lastKnownName: nil, confirmedByUser: nil)

    static func fixed(uid: String, name: String, confirmed: Bool = true) -> DeviceSelectorDTO {
        DeviceSelectorDTO(mode: "fixed", uid: uid, lastKnownName: name, confirmedByUser: confirmed)
    }

    enum CodingKeys: String, CodingKey {
        case mode, uid
        case lastKnownName = "last_known_name"
        case confirmedByUser = "confirmed_by_user"
    }
}

struct ActiveStreamFormatDTO: Codable, Equatable {
    let deviceName: String
    let redactedUid: String
    let transport: String
    let sampleRateHz: Double
    let channels: UInt32
    let bufferFrames: UInt32
    let driverLatencyFrames: UInt32?
    let safetyOffsetFrames: UInt32?
    let isSimulated: Bool

    enum CodingKeys: String, CodingKey {
        case deviceName = "device_name"
        case redactedUid = "redacted_uid"
        case transport
        case sampleRateHz = "sample_rate_hz"
        case channels
        case bufferFrames = "buffer_frames"
        case driverLatencyFrames = "driver_latency_frames"
        case safetyOffsetFrames = "safety_offset_frames"
        case isSimulated = "is_simulated"
    }
}

struct VoiceDspParamsDTO: Codable, Equatable {
    var preset: String
    var bypass: Bool
    var inputGainDb: Float
    var hpfEnabled: Bool
    var hpfCutoffHz: Float
    var pitchEnabled: Bool
    var pitchAmount: Float
    var pitchSpeed: Float
    var neutralZoneCents: Float
    var hysteresisCents: Float
    var tonic: String
    var scale: String
    var compressorEnabled: Bool
    var compressorThresholdDb: Float
    var compressorRatio: Float
    var reverbEnabled: Bool
    var reverbWet: Float
    var reverbDecayS: Float
    var reverbPredelayMs: Float
    var reverbDamping: Float
    var micLevel: Float
    var micMuted: Bool
    var monitoringEnabled: Bool
    var monitoringLevel: Float

    static let defaultNatural = VoiceDspParamsDTO(
        preset: "natural", bypass: false, inputGainDb: 0, hpfEnabled: true, hpfCutoffHz: 80,
        pitchEnabled: true, pitchAmount: 0.35, pitchSpeed: 0.25, neutralZoneCents: 18,
        hysteresisCents: 16, tonic: "c", scale: "chromatic", compressorEnabled: true,
        compressorThresholdDb: -18, compressorRatio: 2.2, reverbEnabled: true, reverbWet: 0.14,
        reverbDecayS: 1.0, reverbPredelayMs: 14, reverbDamping: 0.55, micLevel: 0.85,
        micMuted: false, monitoringEnabled: false, monitoringLevel: 0.5
    )

    enum CodingKeys: String, CodingKey {
        case preset, bypass
        case inputGainDb = "input_gain_db"
        case hpfEnabled = "hpf_enabled"
        case hpfCutoffHz = "hpf_cutoff_hz"
        case pitchEnabled = "pitch_enabled"
        case pitchAmount = "pitch_amount"
        case pitchSpeed = "pitch_speed"
        case neutralZoneCents = "neutral_zone_cents"
        case hysteresisCents = "hysteresis_cents"
        case tonic, scale
        case compressorEnabled = "compressor_enabled"
        case compressorThresholdDb = "compressor_threshold_db"
        case compressorRatio = "compressor_ratio"
        case reverbEnabled = "reverb_enabled"
        case reverbWet = "reverb_wet"
        case reverbDecayS = "reverb_decay_s"
        case reverbPredelayMs = "reverb_predelay_ms"
        case reverbDamping = "reverb_damping"
        case micLevel = "mic_level"
        case micMuted = "mic_muted"
        case monitoringEnabled = "monitoring_enabled"
        case monitoringLevel = "monitoring_level"
    }
}

struct MixParamsDTO: Codable, Equatable {
    var accompanimentGain: Float
    var originalVocalGain: Float
    var useStems: Bool
    var masterLimiterCeilingDb: Float

    static let defaultMix = MixParamsDTO(
        accompanimentGain: 0.90,
        originalVocalGain: 0.25,
        useStems: true,
        masterLimiterCeilingDb: -0.5
    )

    enum CodingKeys: String, CodingKey {
        case accompanimentGain = "accompaniment_gain"
        case originalVocalGain = "original_vocal_gain"
        case useStems = "use_stems"
        case masterLimiterCeilingDb = "master_limiter_ceiling_db"
    }
}

struct AudioPreferenceDTO: Codable, Equatable {
    var input: DeviceSelectorDTO
    var output: DeviceSelectorDTO
    var inputChannelIndex: UInt32
    var sampleRatePreferenceHz: Double?
    var bufferFramesPreference: UInt32?
    var voiceDsp: VoiceDspParamsDTO
    var mix: MixParamsDTO

    enum CodingKeys: String, CodingKey {
        case input, output
        case inputChannelIndex = "input_channel_index"
        case sampleRatePreferenceHz = "sample_rate_preference_hz"
        case bufferFramesPreference = "buffer_frames_preference"
        case voiceDsp = "voice_dsp"
        case mix
    }
}

struct AudioRouteStateDTO: Codable, Equatable {
    let inputs: [AudioDeviceDTO]
    let outputs: [AudioDeviceDTO]
    let preference: AudioPreferenceDTO
    let inputStatus: String
    let outputStatus: String
    let inputStatusMessageFr: String?
    let outputStatusMessageFr: String?
    let activeInputFormat: ActiveStreamFormatDTO?
    let activeOutputFormat: ActiveStreamFormatDTO?
    let micCaptureActive: Bool
    let voiceDspLatencyMs: Double
    let loadedTrackId: String?

    enum CodingKeys: String, CodingKey {
        case inputs, outputs, preference
        case inputStatus = "input_status"
        case outputStatus = "output_status"
        case inputStatusMessageFr = "input_status_message_fr"
        case outputStatusMessageFr = "output_status_message_fr"
        case activeInputFormat = "active_input_format"
        case activeOutputFormat = "active_output_format"
        case micCaptureActive = "mic_capture_active"
        case voiceDspLatencyMs = "voice_dsp_latency_ms"
        case loadedTrackId = "loaded_track_id"
    }
}

// MARK: - Library, Tracks, Playlists, Analysis & Lyrics

struct TrackDTO: Codable, Identifiable, Equatable, Hashable {
    let id: String
    let title: String
    let artist: String
    let album: String?
    let durationS: Double
    let originalSha256: String
    let managedOriginalPath: String
    let format: String
    let sampleRateHz: UInt32
    let channels: UInt16
    let encoderDelaySamples: UInt32
    let isFavorite: Bool
    let availability: String
    let estimatedTonic: String?
    let estimatedScale: String?
    let keyConfidence: Float?
    let keyAmbiguous: Bool

    enum CodingKeys: String, CodingKey {
        case id, title, artist, album, format, channels, availability
        case durationS = "duration_s"
        case originalSha256 = "original_sha256"
        case managedOriginalPath = "managed_original_path"
        case sampleRateHz = "sample_rate_hz"
        case encoderDelaySamples = "encoder_delay_samples"
        case isFavorite = "is_favorite"
        case estimatedTonic = "estimated_tonic"
        case estimatedScale = "estimated_scale"
        case keyConfidence = "key_confidence"
        case keyAmbiguous = "key_ambiguous"
    }
}

struct PlaylistDTO: Codable, Identifiable, Equatable, Hashable {
    let id: String
    let name: String
}

struct PlaylistItemDTO: Codable, Identifiable, Equatable {
    let id: String
    let playlistId: String
    let trackId: String
    let position: Int64

    enum CodingKeys: String, CodingKey {
        case id, position
        case playlistId = "playlist_id"
        case trackId = "track_id"
    }
}

struct AnalysisJobDTO: Codable, Identifiable, Equatable {
    let id: String
    let trackId: String
    let state: String
    let stage: String
    let progress: Float
    let statusDetailFr: String
    let pipelineVersion: String
    let lastCompletedStage: String?
    let error: KaraokErrorDTO?

    enum CodingKeys: String, CodingKey {
        case id, state, stage, progress, error
        case trackId = "track_id"
        case statusDetailFr = "status_detail_fr"
        case pipelineVersion = "pipeline_version"
        case lastCompletedStage = "last_completed_stage"
    }
}

struct VocalRegionDTO: Codable, Identifiable, Equatable {
    let id: String
    let startS: Double
    let endS: Double
    let kind: String
    let rawScore: Float
    let uncertaintyS: Double
    let method: String
    let methodVersion: String

    enum CodingKeys: String, CodingKey {
        case id, kind, method
        case startS = "start_s"
        case endS = "end_s"
        case rawScore = "raw_score"
        case uncertaintyS = "uncertainty_s"
        case methodVersion = "method_version"
    }
}

struct VocalActivitySummaryDTO: Codable, Equatable {
    let trackId: String
    let firstVocalActivityS: Double?
    let firstSungActivityS: Double?
    let noSingingDetected: Bool
    let method: String
    let methodVersion: String
    let reviewStatus: String
    let regions: [VocalRegionDTO]

    enum CodingKeys: String, CodingKey {
        case method, regions
        case trackId = "track_id"
        case firstVocalActivityS = "first_vocal_activity_s"
        case firstSungActivityS = "first_sung_activity_s"
        case noSingingDetected = "no_singing_detected"
        case methodVersion = "method_version"
        case reviewStatus = "review_status"
    }
}

struct LyricCueDTO: Codable, Identifiable, Equatable {
    let id: String
    let text: String
    let startS: Double?
    let endS: Double?
    let timingSource: String
    let confidence: Float?
    let isInstrumentalMarker: Bool
    let humanValidated: Bool

    enum CodingKeys: String, CodingKey {
        case id, text, confidence
        case startS = "start_s"
        case endS = "end_s"
        case timingSource = "timing_source"
        case isInstrumentalMarker = "is_instrumental_marker"
        case humanValidated = "human_validated"
    }
}

struct LyricsDocumentDTO: Codable, Equatable {
    let schemaVersion: UInt32
    let trackId: String
    let timebase: String
    let textRevision: UInt64
    let textSha256: String
    let rawText: String
    let language: String?
    let provenance: String
    let granularity: String
    let globalOffsetMs: Int64
    let status: String
    let firstVocalActivityS: Double?
    let firstSungActivityS: Double?
    let firstLyricCueS: Double?
    let warnings: [String]
    let cues: [LyricCueDTO]

    enum CodingKeys: String, CodingKey {
        case timebase, language, provenance, granularity, status, warnings, cues
        case schemaVersion = "schema_version"
        case trackId = "track_id"
        case textRevision = "text_revision"
        case textSha256 = "text_sha256"
        case rawText = "raw_text"
        case globalOffsetMs = "global_offset_ms"
        case firstVocalActivityS = "first_vocal_activity_s"
        case firstSungActivityS = "first_sung_activity_s"
        case firstLyricCueS = "first_lyric_cue_s"
    }
}

struct TrackDetailDTO: Codable, Equatable {
    let track: TrackDTO
    let stemsReady: Bool
    let separationMethod: String?
    let latestJob: AnalysisJobDTO?
    let vocalActivity: VocalActivitySummaryDTO?
    let lyrics: LyricsDocumentDTO?

    enum CodingKeys: String, CodingKey {
        case track, lyrics
        case stemsReady = "stems_ready"
        case separationMethod = "separation_method"
        case latestJob = "latest_job"
        case vocalActivity = "vocal_activity"
    }
}

// MARK: - Models, Latency Profiles & Diagnostics

struct ModelManifestDTO: Codable, Identifiable, Equatable {
    let id: String
    let displayName: String
    let capability: String
    let version: String
    let officialSourceUrl: String
    let sha256: String
    let sizeBytes: UInt64
    let author: String
    let codeLicense: String
    let weightsLicense: String
    let languages: [String]
    let runtime: String
    let verifiedDate: String
    let localPath: String?
    let state: String

    enum CodingKeys: String, CodingKey {
        case id, capability, version, sha256, author, languages, runtime, state
        case displayName = "display_name"
        case officialSourceUrl = "official_source_url"
        case sizeBytes = "size_bytes"
        case codeLicense = "code_license"
        case weightsLicense = "weights_license"
        case verifiedDate = "verified_date"
        case localPath = "local_path"
    }
}

struct LatencyProfileDTO: Codable, Identifiable, Equatable {
    let id: String
    let routeFingerprint: String
    let inputName: String
    let outputName: String
    let sampleRateHz: Double
    let bufferFrames: UInt32
    let pitchCorrectionEnabled: Bool
    let reverbEnabled: Bool
    let origin: String
    let inputDriverMs: Double?
    let outputDriverMs: Double?
    let dspAlgorithmicMs: Double
    let totalEstimatedMs: Double?
    let measuredP50Ms: Double?
    let measuredP95Ms: Double?
    let measuredP99Ms: Double?
    let protocolNotes: String

    enum CodingKeys: String, CodingKey {
        case id, origin
        case routeFingerprint = "route_fingerprint"
        case inputName = "input_name"
        case outputName = "output_name"
        case sampleRateHz = "sample_rate_hz"
        case bufferFrames = "buffer_frames"
        case pitchCorrectionEnabled = "pitch_correction_enabled"
        case reverbEnabled = "reverb_enabled"
        case inputDriverMs = "input_driver_ms"
        case outputDriverMs = "output_driver_ms"
        case dspAlgorithmicMs = "dsp_algorithmic_ms"
        case totalEstimatedMs = "total_estimated_ms"
        case measuredP50Ms = "measured_p50_ms"
        case measuredP95Ms = "measured_p95_ms"
        case measuredP99Ms = "measured_p99_ms"
        case protocolNotes = "protocol_notes"
    }
}

struct RtCountersDTO: Codable, Equatable {
    let outputCallbacks: UInt64
    let inputCallbacks: UInt64
    let musicUnderruns: UInt64
    let micUnderruns: UInt64
    let micOverruns: UInt64
    let callbackDeadlineOverruns: UInt64
    let nonFiniteSanitizations: UInt64
    let routeTransitions: UInt64
    let micRingOccupancyFrames: UInt32
    let micResampleRatio: Double
    let lastRenderDurationUs: UInt32
    let maxRenderDurationUs: UInt32

    enum CodingKeys: String, CodingKey {
        case outputCallbacks = "output_callbacks"
        case inputCallbacks = "input_callbacks"
        case musicUnderruns = "music_underruns"
        case micUnderruns = "mic_underruns"
        case micOverruns = "mic_overruns"
        case callbackDeadlineOverruns = "callback_deadline_overruns"
        case nonFiniteSanitizations = "non_finite_sanitizations"
        case routeTransitions = "route_transitions"
        case micRingOccupancyFrames = "mic_ring_occupancy_frames"
        case micResampleRatio = "mic_resample_ratio"
        case lastRenderDurationUs = "last_render_duration_us"
        case maxRenderDurationUs = "max_render_duration_us"
    }
}

struct DiagnosticEntryDTO: Codable, Identifiable, Equatable {
    let id: UUID = UUID()
    let timestamp: String
    let level: String
    let component: String
    let message: String

    enum CodingKeys: String, CodingKey {
        case timestamp, level, component, message
    }
}

struct DiagnosticsExportDTO: Codable, Equatable {
    let exportedAt: String
    let appVersion: String
    let privacyNoticeFr: String
    let rtCounters: RtCountersDTO
    let recentLogs: [DiagnosticEntryDTO]

    enum CodingKeys: String, CodingKey {
        case exportedAt = "exported_at"
        case appVersion = "app_version"
        case privacyNoticeFr = "privacy_notice_fr"
        case rtCounters = "rt_counters"
        case recentLogs = "recent_logs"
    }
}

// MARK: - Real-time Lock-Free Snapshots

struct TransportStateSnapshot: Equatable {
    let generation: UInt64
    let stateCode: UInt8 // 0=Stopped, 1=Playing, 2=Paused, 3=Buffering, 4=RouteLost
    let presentationQuality: UInt8
    let usingStems: Bool
    let stemsAvailable: Bool
    let sourcePositionS: Double
    let durationS: Double
    let monotonicHostNs: UInt64
    let playbackRatio: Double
    let presentationLatencyMs: Double

    var isPlaying: Bool { stateCode == 1 }
    var isRouteLost: Bool { stateCode == 4 }

    static let empty = TransportStateSnapshot(
        generation: 0, stateCode: 0, presentationQuality: 2,
        usingStems: false, stemsAvailable: false, sourcePositionS: 0,
        durationS: 0, monotonicHostNs: 0, playbackRatio: 1.0, presentationLatencyMs: 0
    )
}

struct MeterStateSnapshot: Equatable {
    let inputPrePeak: Float
    let inputPreRms: Float
    let inputClipped: Bool
    let outputClipped: Bool
    let voiced: Bool
    let activeMicCapture: Bool
    let voicePostPeak: Float
    let voicePostRms: Float
    let outputPeak: Float
    let detectedF0Hz: Float
    let targetF0Hz: Float
    let pitchConfidence: Float
    let pitchCorrectionCents: Float

    static let empty = MeterStateSnapshot(
        inputPrePeak: 0, inputPreRms: 0, inputClipped: false, outputClipped: false,
        voiced: false, activeMicCapture: false, voicePostPeak: 0, voicePostRms: 0,
        outputPeak: 0, detectedF0Hz: 0, targetF0Hz: 0, pitchConfidence: 0, pitchCorrectionCents: 0
    )
}

// MARK: - Musical Lookup Helpers

let allPitchClasses: [(key: String, label: String)] = [
    ("c", "Do (C)"), ("c_sharp", "Do# (C#)"), ("d", "Ré (D)"), ("d_sharp", "Mib (D#)"),
    ("e", "Mi (E)"), ("f", "Fa (F)"), ("f_sharp", "Fa# (F#)"), ("g", "Sol (G)"),
    ("g_sharp", "Sol# (G#)"), ("a", "La (A)"), ("a_sharp", "Sib (A#)"), ("b", "Si (B)")
]

let allMusicalScales: [(key: String, label: String)] = [
    ("chromatic", "Chromatique (toutes les notes)"),
    ("major", "Majeure"),
    ("natural_minor", "Mineure naturelle"),
    ("harmonic_minor", "Mineure harmonique"),
    ("pentatonic_major", "Pentatonique majeure"),
    ("pentatonic_minor", "Pentatonique mineure"),
    ("dorian", "Dorien"),
    ("mixolydian", "Mixolydien")
]

func formatTimeMMSS(_ seconds: Double) -> String {
    guard seconds.isFinite && seconds >= 0 else { return "00:00" }
    let total = Int(seconds.rounded(.down))
    let mins = total / 60
    let secs = total % 60
    return String(format: "%02d:%02d", mins, secs)
}

func frequencyToNoteLabel(_ hz: Float) -> String {
    guard hz.isFinite && hz >= 50 && hz <= 2000 else { return "—" }
    let midi = Int((69.0 + 12.0 * log2(Double(hz) / 440.0)).rounded())
    let names = ["Do", "Do#", "Ré", "Mib", "Mi", "Fa", "Fa#", "Sol", "Sol#", "La", "Sib", "Si"]
    let note = names[((midi % 12) + 12) % 12]
    let octave = (midi / 12) - 1
    return "\(note)\(octave)"
}

func pitchClassLabelFr(_ key: String) -> String {
    allPitchClasses.first(where: { $0.key == key })?.label ?? key.uppercased()
}

func scaleLabelFr(_ key: String) -> String {
    allMusicalScales.first(where: { $0.key == key })?.label ?? key
}

func modelStateLabelFr(_ state: String) -> String {
    switch state {
    case "installed_verified": return "Installé et vérifié (SHA-256)"
    case "not_installed": return "Non installé (optionnel)"
    case "hash_mismatch": return "Empreinte SHA-256 invalide"
    case "license_unverified": return "Licence poids non vérifiée"
    default: return state
    }
}

func latencyOriginLabelFr(_ origin: String) -> String {
    switch origin {
    case "driver_estimate_only": return "Estimation pilote + DSP"
    case "acoustic_loopback_measured": return "Mesure acoustique réelle"
    case "simulated": return "Simulation de test"
    default: return origin
    }
}
