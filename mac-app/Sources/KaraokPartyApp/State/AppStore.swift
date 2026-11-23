import Foundation
import SwiftUI

enum SidebarTab: String, CaseIterable, Identifiable {
    case sing = "Chanter"
    case library = "Bibliothèque"
    case lyricsReview = "Vérifier les paroles"
    case sound = "Son"
    case diagnostics = "Diagnostics"

    var id: String { rawValue }

    var icon: String {
        switch self {
        case .sing: return "music.mic"
        case .library: return "music.note.list"
        case .lyricsReview: return "text.alignleft"
        case .sound: return "slider.horizontal.3"
        case .diagnostics: return "waveform.path.ecg"
        }
    }
}

/// Isolated high-frequency RT telemetry store (I12) so only views observing playhead/meters
/// redraw at 40 Hz, preventing idle full-window SwiftUI invalidations!
@MainActor
final class LiveTelemetryStore: ObservableObject {
    @Published var transport: TransportStateSnapshot = .empty
    @Published var meters: MeterStateSnapshot = .empty

    func update(transport newT: TransportStateSnapshot, meters newM: MeterStateSnapshot) {
        // Ignore monotonicHostNs-only churn when transport is idle/stopped (I12)
        let transportChanged = newT.stateCode != transport.stateCode
            || newT.generation != transport.generation
            || abs(newT.sourcePositionS - transport.sourcePositionS) > 0.005
            || abs(newT.durationS - transport.durationS) > 0.005
            || newT.usingStems != transport.usingStems
            || newT.stemsAvailable != transport.stemsAvailable
            || newT.presentationQuality != transport.presentationQuality
        if transportChanged {
            transport = newT
        }

        let metersChanged = newM.activeMicCapture != meters.activeMicCapture
            || newM.voiced != meters.voiced
            || newM.inputClipped != meters.inputClipped
            || newM.outputClipped != meters.outputClipped
            || abs(newM.inputPrePeak - meters.inputPrePeak) > 0.004
            || abs(newM.voicePostPeak - meters.voicePostPeak) > 0.004
            || abs(newM.outputPeak - meters.outputPeak) > 0.004
            || abs(newM.detectedF0Hz - meters.detectedF0Hz) > 0.5
        if metersChanged {
            meters = newM
        }
    }
}

/// Main-actor observable store separating durable state, async jobs, and isolated RT telemetry (§4.3, I7, I12).
@MainActor
final class AppStore: ObservableObject {
    private var bridge: RustBridge?
    private var pollTimer: Timer?
    private var micTestTimer: Timer?

    // Isolated 40Hz RT telemetry store (I12)
    let telemetry = LiveTelemetryStore()

    // Navigation & UI preferences
    @Published var selectedTab: SidebarTab = .library
    @Published var isFullScreenLyrics: Bool = false
    @Published var showEffectsPanel: Bool = true
    @Published var lyricsScale: Double = 1.0 // 1.0 .. 2.0 (200% accessibility test §3.5)

    // Startup / Banner / Loading alerts
    @Published var fatalInitError: KaraokErrorDTO?
    @Published var bannerError: KaraokErrorDTO?
    @Published var routeAlertMessageFr: String?
    @Published var isImporting: Bool = false
    @Published var isLoadingPlayback: Bool = false

    // Durable Audio & Route State
    @Published var audioState: AudioRouteStateDTO?
    @Published var voiceParams: VoiceDspParamsDTO = .defaultNatural
    @Published var mixParams: MixParamsDTO = .defaultMix
    @Published var latencyProfiles: [LatencyProfileDTO] = []
    @Published var loadedTrackId: String? = nil
    @Published var loadedTrackDetail: TrackDetailDTO? = nil

    // Concurrency & key-tracking guards (P2-I7)
    private var activeImportsCount: Int = 0
    private var loadGeneration: UInt64 = 0
    private var keyWasAutoApplied: Bool = false

    /// Authoritative detail for the karaoke stage & transport bar (prevents Library inspection bleed - P2-I5).
    var activeStageTrackDetail: TrackDetailDTO? {
        loadedTrackDetail ?? selectedTrackDetail
    }

    // Safe timed microphone test state (§3.1)
    @Published var isRunningMicTest: Bool = false
    @Published var micTestSecondsRemaining: Int = 0

    // Library & Playlists
    @Published var tracks: [TrackDTO] = []
    @Published var playlists: [PlaylistDTO] = []
    @Published var selectedPlaylistId: String? = nil
    @Published var playlistItems: [PlaylistItemDTO] = []
    @Published var searchQuery: String = ""
    @Published var favoritesOnly: Bool = false

    // Active Track & Preparation
    @Published var selectedTrackId: String? = nil
    @Published var selectedTrackDetail: TrackDetailDTO? = nil
    @Published var activePrepJob: AnalysisJobDTO? = nil

    // Models & Diagnostics
    @Published var models: [ModelManifestDTO] = []
    @Published var diagnostics: DiagnosticsExportDTO? = nil

    let permissions = PermissionManager()

    init() {
        do {
            self.bridge = try RustBridge()
            refreshAll()
            startPollingLoop()
        } catch let err as KaraokErrorDTO {
            self.fatalInitError = err
        } catch {
            self.fatalInitError = KaraokErrorDTO(
                code: "internal_error",
                messageFr: "Impossible de démarrer le moteur Rust : \(error.localizedDescription)",
                actionableHintFr: nil
            )
        }
    }

    deinit {
        pollTimer?.invalidate()
        micTestTimer?.invalidate()
    }

    private func startPollingLoop() {
        pollTimer = Timer.scheduledTimer(withTimeInterval: 1.0 / 40.0, repeats: true) { [weak self] _ in
            Task { @MainActor in
                self?.pollTick()
            }
        }
    }

    private var eventPollDivider = 0
    private func pollTick() {
        guard let bridge else { return }
        telemetry.update(
            transport: bridge.readTransport(),
            meters: bridge.readMeters()
        )

        eventPollDivider += 1
        if eventPollDivider >= 4 {
            eventPollDivider = 0
            let events = bridge.pollEventsRaw()
            for ev in events {
                handleEvent(ev)
            }
        }
    }

    private func handleEvent(_ ev: [String: Any]) {
        guard let type = ev["type"] as? String else { return }
        switch type {
        case "devices_changed":
            refreshAudioState()
        case "route_lost":
            if let msg = ev["message_fr"] as? String {
                routeAlertMessageFr = msg
            }
            stopMicTest()
            refreshAudioState()
        case "route_failed":
            if let errDict = ev["error"] as? [String: Any],
               let msg = errDict["message_fr"] as? String {
                bannerError = KaraokErrorDTO(
                    code: errDict["code"] as? String ?? "route_open_failed",
                    messageFr: msg,
                    actionableHintFr: errDict["actionable_hint_fr"] as? String
                )
            }
            refreshAudioState()
        case "route_ready":
            refreshAudioState()
            refreshLatencyProfiles()
        case "preparation_progress":
            if let jobObj = ev["job"],
               let data = try? JSONSerialization.data(withJSONObject: jobObj),
               let job = try? JSONDecoder().decode(AnalysisJobDTO.self, from: data) {
                activePrepJob = job
                if job.state == "completed" || job.state == "needs_review" || job.state == "failed" || job.state == "cancelled" {
                    if let tid = selectedTrackId, tid == job.trackId {
                        refreshSelectedTrackDetail()
                    }
                    if let lid = loadedTrackId, lid == job.trackId, lid != selectedTrackId {
                        refreshLoadedTrackDetail()
                    }
                    refreshTracks()
                }
            }
        case "artifact_ready":
            if let tid = ev["track_id"] as? String {
                if tid == selectedTrackId {
                    refreshSelectedTrackDetail()
                } else if tid == loadedTrackId {
                    refreshLoadedTrackDetail()
                }
            }
        case "error":
            if let errDict = ev["error"] as? [String: Any],
               let msg = errDict["message_fr"] as? String {
                bannerError = KaraokErrorDTO(
                    code: errDict["code"] as? String ?? "internal_error",
                    messageFr: msg,
                    actionableHintFr: errDict["actionable_hint_fr"] as? String
                )
            }
        default:
            break
        }
    }

    // MARK: - Refresh Helpers

    func refreshAll() {
        refreshAudioState()
        refreshTracks()
        refreshPlaylists()
        refreshModels()
        refreshLatencyProfiles()
        refreshDiagnostics()
    }

    func refreshAudioState() {
        guard let bridge else { return }
        do {
            let state = try bridge.dispatch(["cmd": "get_audio_state"], as: AudioRouteStateDTO.self)
            self.audioState = state
            self.voiceParams = state.preference.voiceDsp
            self.mixParams = state.preference.mix
            self.loadedTrackId = state.loadedTrackId
        } catch let err as KaraokErrorDTO {
            bannerError = err
        } catch {}
    }

    func refreshDevicesManually() {
        guard let bridge else { return }
        Task.detached { [bridge] in
            do {
                let state = try bridge.dispatch(["cmd": "refresh_devices"], as: AudioRouteStateDTO.self)
                await MainActor.run {
                    self.audioState = state
                    self.loadedTrackId = state.loadedTrackId
                }
            } catch let err as KaraokErrorDTO {
                await MainActor.run { self.bannerError = err }
            } catch {}
        }
    }

    func refreshTracks() {
        guard let bridge else { return }
        var payload: [String: Any] = ["cmd": "list_tracks", "favorites_only": favoritesOnly]
        let q = searchQuery.trimmingCharacters(in: .whitespacesAndNewlines)
        if !q.isEmpty { payload["search"] = q }
        do {
            let list = try bridge.dispatch(payload, as: [TrackDTO].self)
            self.tracks = list
            if selectedTrackId == nil, let first = list.first {
                selectTrack(first.id)
            }
        } catch let err as KaraokErrorDTO {
            bannerError = err
        } catch {}
    }

    func selectTrack(_ trackId: String) {
        selectedTrackId = trackId
        refreshSelectedTrackDetail()
    }

    func refreshSelectedTrackDetail() {
        guard let bridge, let tid = selectedTrackId else {
            selectedTrackDetail = nil
            return
        }
        do {
            let det = try bridge.dispatch(["cmd": "get_track_detail", "track_id": tid], as: TrackDetailDTO.self)
            self.selectedTrackDetail = det
            if loadedTrackId == tid {
                self.loadedTrackDetail = det
            }
            if let job = det.latestJob {
                self.activePrepJob = job
            }
        } catch let err as KaraokErrorDTO {
            bannerError = err
        } catch {}
    }

    func refreshLoadedTrackDetail() {
        guard let bridge, let tid = loadedTrackId else {
            loadedTrackDetail = nil
            return
        }
        if let det = try? bridge.dispatch(["cmd": "get_track_detail", "track_id": tid], as: TrackDetailDTO.self) {
            self.loadedTrackDetail = det
            if selectedTrackId == tid {
                self.selectedTrackDetail = det
            }
        }
    }

    func refreshPlaylists() {
        guard let bridge else { return }
        if let list = try? bridge.dispatch(["cmd": "list_playlists"], as: [PlaylistDTO].self) {
            self.playlists = list
        }
        if let pid = selectedPlaylistId {
            loadPlaylistItems(pid)
        }
    }

    func loadPlaylistItems(_ playlistId: String) {
        guard let bridge else { return }
        if let items = try? bridge.dispatch(["cmd": "list_playlist_items", "playlist_id": playlistId], as: [PlaylistItemDTO].self) {
            self.playlistItems = items
        }
    }

    func removePlaylistItem(_ itemId: String) {
        guard let bridge else { return }
        let _ = try? bridge.dispatch(["cmd": "remove_playlist_item", "item_id": itemId], as: Bool.self)
        if let pid = selectedPlaylistId {
            loadPlaylistItems(pid)
        }
    }

    func movePlaylistItem(itemId: String, direction: Int) {
        guard let bridge, let pid = selectedPlaylistId else { return }
        guard let fromIndex = playlistItems.firstIndex(where: { $0.id == itemId }) else { return }
        let toIndex = fromIndex + direction
        guard toIndex >= 0, toIndex < playlistItems.count else { return }
        var copy = playlistItems
        let moved = copy.remove(at: fromIndex)
        copy.insert(moved, at: toIndex)
        playlistItems = copy
        let orderedIds = copy.map { $0.id }
        let _ = try? bridge.dispatch([
            "cmd": "reorder_playlist_items",
            "playlist_id": pid,
            "ordered_item_ids": orderedIds
        ], as: Bool.self)
    }

    func refreshModels() {
        guard let bridge else { return }
        if let list = try? bridge.dispatch(["cmd": "list_models"], as: [ModelManifestDTO].self) {
            self.models = list
        }
    }

    func importVerifiedModel(fileURL: URL, manifest: ModelManifestDTO) {
        guard let bridge else { return }
        guard let data = try? JSONEncoder().encode(manifest),
              let manifestDict = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return }
        let path = fileURL.path
        Task.detached { [bridge] in
            do {
                let _ = try bridge.dispatch([
                    "cmd": "import_verified_model",
                    "file_path": path,
                    "manifest": manifestDict
                ], as: ModelManifestDTO.self)
                await MainActor.run { self.refreshModels() }
            } catch let err as KaraokErrorDTO {
                await MainActor.run { self.bannerError = err }
            } catch {}
        }
    }

    func refreshLatencyProfiles() {
        guard let bridge else { return }
        if let list = try? bridge.dispatch(["cmd": "list_latency_profiles"], as: [LatencyProfileDTO].self) {
            self.latencyProfiles = list
        }
    }

    func refreshDiagnostics() {
        guard let bridge else { return }
        if let diag = try? bridge.dispatch(["cmd": "get_diagnostics"], as: DiagnosticsExportDTO.self) {
            self.diagnostics = diag
        }
    }

    // MARK: - Audio Routes & Safe Mic Test

    func selectInput(selector: DeviceSelectorDTO, confirmReconnect: Bool = false) {
        guard let bridge else { return }
        var selDict: [String: Any] = ["mode": selector.mode]
        if let uid = selector.uid { selDict["uid"] = uid }
        if let name = selector.lastKnownName { selDict["last_known_name"] = name }
        if let conf = selector.confirmedByUser { selDict["confirmed_by_user"] = conf }
        do {
            let state = try bridge.dispatch([
                "cmd": "select_input",
                "selector": selDict,
                "confirm_reconnect": confirmReconnect
            ], as: AudioRouteStateDTO.self)
            self.audioState = state
            self.routeAlertMessageFr = nil
        } catch let err as KaraokErrorDTO {
            bannerError = err
        } catch {}
    }

    func selectOutput(selector: DeviceSelectorDTO) {
        guard let bridge else { return }
        var selDict: [String: Any] = ["mode": selector.mode]
        if let uid = selector.uid { selDict["uid"] = uid }
        if let name = selector.lastKnownName { selDict["last_known_name"] = name }
        if let conf = selector.confirmedByUser { selDict["confirmed_by_user"] = conf }
        do {
            let state = try bridge.dispatch([
                "cmd": "select_output",
                "selector": selDict
            ], as: AudioRouteStateDTO.self)
            self.audioState = state
            self.voiceParams = state.preference.voiceDsp
            self.routeAlertMessageFr = nil
            refreshLatencyProfiles()
        } catch let err as KaraokErrorDTO {
            bannerError = err
        } catch {}
    }

    func setInputChannel(_ channelIndex: UInt32) {
        guard let bridge else { return }
        if let state = try? bridge.dispatch([
            "cmd": "set_input_channel",
            "channel_index": channelIndex
        ], as: AudioRouteStateDTO.self) {
            self.audioState = state
        }
    }

    func startMicCaptureWithPermission() async {
        let granted = await permissions.requestAccessIfNeeded()
        guard granted else {
            bannerError = KaraokErrorDTO(
                code: "permission_denied",
                messageFr: "L'accès au microphone a été refusé par macOS.",
                actionableHintFr: "Ouvrez Réglages Système > Confidentialité et sécurité > Microphone pour autoriser Karaok Party. La lecture musicale reste utilisable."
            )
            return
        }
        guard let bridge else { return }
        do {
            let _ = try bridge.dispatch(["cmd": "start_mic_capture"], as: ActiveStreamFormatDTO.self)
            refreshAudioState()
        } catch let err as KaraokErrorDTO {
            bannerError = err
        } catch {}
    }

    func stopMicCapture() {
        guard let bridge else { return }
        stopMicTest()
        if let state = try? bridge.dispatch(["cmd": "stop_mic_capture"], as: AudioRouteStateDTO.self) {
            self.audioState = state
        }
    }

    func startSafeMicTest(withMonitoring: Bool) async {
        await startMicCaptureWithPermission()
        guard audioState?.micCaptureActive == true else { return }

        if withMonitoring {
            var p = voiceParams
            p.monitoringEnabled = true
            p.monitoringLevel = min(p.monitoringLevel, 0.35)
            p.micMuted = false
            updateVoiceParams(p)
        }
        isRunningMicTest = true
        micTestSecondsRemaining = 10
        micTestTimer?.invalidate()
        micTestTimer = Timer.scheduledTimer(withTimeInterval: 1.0, repeats: true) { [weak self] _ in
            Task { @MainActor in
                guard let self else { return }
                self.micTestSecondsRemaining -= 1
                if self.micTestSecondsRemaining <= 0 {
                    self.stopMicTest()
                }
            }
        }
    }

    func stopMicTest() {
        micTestTimer?.invalidate()
        micTestTimer = nil
        if isRunningMicTest {
            isRunningMicTest = false
            micTestSecondsRemaining = 0
            var p = voiceParams
            p.monitoringEnabled = false
            updateVoiceParams(p)
        }
    }

    // MARK: - Voice DSP & Mix Controls

    func applyVoicePreset(_ preset: String) {
        guard let bridge else { return }
        do {
            let updated = try bridge.dispatch(["cmd": "apply_voice_preset", "preset": preset], as: VoiceDspParamsDTO.self)
            self.voiceParams = updated
        } catch let err as KaraokErrorDTO {
            bannerError = err
        } catch {}
    }

    func updateVoiceParams(_ params: VoiceDspParamsDTO, manualKeyOverride: Bool = false) {
        if manualKeyOverride {
            keyWasAutoApplied = false
        }
        self.voiceParams = params
        guard let bridge else { return }
        guard let data = try? JSONEncoder().encode(params),
              let dict = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return }
        if let updated = try? bridge.dispatch(["cmd": "set_voice_params", "params": dict], as: VoiceDspParamsDTO.self) {
            self.voiceParams = updated
        }
    }

    func applyEstimatedTrackKey(_ detail: TrackDetailDTO) {
        guard let tonic = detail.track.estimatedTonic,
              let scale = detail.track.estimatedScale else { return }
        var vp = voiceParams
        vp.tonic = tonic
        vp.scale = scale
        keyWasAutoApplied = true
        updateVoiceParams(vp, manualKeyOverride: false)
    }

    func updateMixParams(_ params: MixParamsDTO) {
        self.mixParams = params
        guard let bridge else { return }
        guard let data = try? JSONEncoder().encode(params),
              let dict = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return }
        if let updated = try? bridge.dispatch(["cmd": "set_mix_params", "params": dict], as: MixParamsDTO.self) {
            self.mixParams = updated
        }
    }

    // MARK: - Async Library Import, Preparation, Lyrics & Transport (I7, B7, I13, P2-I7)

    func importTrack(url: URL) {
        guard let bridge else { return }
        activeImportsCount += 1
        isImporting = activeImportsCount > 0
        let path = url.path
        Task.detached { [bridge] in
            do {
                let track = try bridge.dispatch(["cmd": "import_track", "file_path": path], as: TrackDTO.self)
                await MainActor.run {
                    self.activeImportsCount = max(0, self.activeImportsCount - 1)
                    self.isImporting = self.activeImportsCount > 0
                    self.refreshTracks()
                    self.selectTrack(track.id)
                }
            } catch let err as KaraokErrorDTO {
                await MainActor.run {
                    self.activeImportsCount = max(0, self.activeImportsCount - 1)
                    self.isImporting = self.activeImportsCount > 0
                    self.bannerError = err
                }
            } catch {
                await MainActor.run {
                    self.activeImportsCount = max(0, self.activeImportsCount - 1)
                    self.isImporting = self.activeImportsCount > 0
                }
            }
        }
    }

    func toggleFavorite(_ track: TrackDTO) {
        guard let bridge else { return }
        var payload: [String: Any] = [
            "cmd": "update_track_metadata",
            "track_id": track.id,
            "title": track.title,
            "artist": track.artist,
            "is_favorite": !track.isFavorite
        ]
        if let alb = track.album { payload["album"] = alb }
        if let _ = try? bridge.dispatch(payload, as: TrackDTO.self) {
            refreshTracks()
            if selectedTrackId == track.id { refreshSelectedTrackDetail() }
        }
    }

    func updateTrackMetadata(trackId: String, title: String, artist: String, album: String?, isFavorite: Bool, tonic: String?, scale: String?) {
        guard let bridge else { return }
        var payload: [String: Any] = [
            "cmd": "update_track_metadata",
            "track_id": trackId,
            "title": title,
            "artist": artist,
            "is_favorite": isFavorite
        ]
        if let album, !album.isEmpty { payload["album"] = album }
        if let tonic { payload["tonic"] = tonic }
        if let scale { payload["scale"] = scale }
        do {
            let _ = try bridge.dispatch(payload, as: TrackDTO.self)
            refreshTracks()
            refreshSelectedTrackDetail()
        } catch let err as KaraokErrorDTO {
            bannerError = err
        } catch {}
    }

    func deleteTrack(_ trackId: String, deleteManagedCopy: Bool) {
        guard let bridge else { return }
        if let _ = try? bridge.dispatch([
            "cmd": "delete_track",
            "track_id": trackId,
            "delete_managed_copy": deleteManagedCopy
        ], as: Bool.self) {
            if selectedTrackId == trackId {
                selectedTrackId = nil
                selectedTrackDetail = nil
            }
            if loadedTrackId == trackId {
                loadedTrackId = nil
                loadedTrackDetail = nil
            }
            refreshTracks()
            if let pid = selectedPlaylistId { loadPlaylistItems(pid) }
        }
    }

    func createPlaylist(name: String) {
        guard let bridge else { return }
        do {
            let pl = try bridge.dispatch(["cmd": "create_playlist", "name": name], as: PlaylistDTO.self)
            refreshPlaylists()
            selectedPlaylistId = pl.id
            loadPlaylistItems(pl.id)
        } catch let err as KaraokErrorDTO {
            bannerError = err
        } catch {}
    }

    func addTrack(_ trackId: String, toPlaylist playlistId: String) {
        guard let bridge else { return }
        let _ = try? bridge.dispatch([
            "cmd": "add_track_to_playlist",
            "playlist_id": playlistId,
            "track_id": trackId
        ], as: PlaylistItemDTO.self)
        if selectedPlaylistId == playlistId {
            loadPlaylistItems(playlistId)
        }
    }

    func startPreparation(trackId: String) {
        guard let bridge else { return }
        do {
            let _ = try bridge.dispatch(["cmd": "start_preparation", "track_id": trackId], as: String.self)
        } catch let err as KaraokErrorDTO {
            bannerError = err
        } catch {}
    }

    func cancelPreparation(trackId: String) {
        guard let bridge else { return }
        let _ = try? bridge.dispatch(["cmd": "cancel_preparation", "track_id": trackId], as: Bool.self)
    }

    func setLyricsText(trackId: String, rawText: String, provenance: String, language: String?) {
        guard let bridge else { return }
        var payload: [String: Any] = [
            "cmd": "set_lyrics_text",
            "track_id": trackId,
            "raw_text": rawText,
            "provenance": provenance
        ]
        if let language, !language.isEmpty { payload["language"] = language }
        do {
            let _ = try bridge.dispatch(payload, as: AnyCodableResponse.self)
            refreshSelectedTrackDetail()
        } catch let err as KaraokErrorDTO {
            bannerError = err
        } catch {}
    }

    func placeLyricCue(trackId: String, cueId: String, startS: Double, endS: Double? = nil) {
        guard let bridge else { return }
        var payload: [String: Any] = [
            "cmd": "place_lyric_cue",
            "track_id": trackId,
            "cue_id": cueId,
            "start_s": startS
        ]
        if let endS { payload["end_s"] = endS }
        do {
            let _ = try bridge.dispatch(payload, as: LyricsDocumentDTO.self)
            refreshSelectedTrackDetail()
        } catch let err as KaraokErrorDTO {
            bannerError = err
        } catch {}
    }

    func markCueInstrumental(trackId: String, cueId: String, isInstrumental: Bool) {
        guard let bridge else { return }
        do {
            let _ = try bridge.dispatch([
                "cmd": "mark_cue_instrumental",
                "track_id": trackId,
                "cue_id": cueId,
                "is_instrumental": isInstrumental
            ], as: LyricsDocumentDTO.self)
            refreshSelectedTrackDetail()
        } catch let err as KaraokErrorDTO {
            bannerError = err
        } catch {}
    }

    func setLyricsOffset(trackId: String, offsetMs: Int64) {
        guard let bridge else { return }
        if let _ = try? bridge.dispatch([
            "cmd": "set_lyrics_offset",
            "track_id": trackId,
            "global_offset_ms": offsetMs
        ], as: LyricsDocumentDTO.self) {
            refreshSelectedTrackDetail()
        }
    }

    func validateLyrics(trackId: String) {
        guard let bridge else { return }
        if let _ = try? bridge.dispatch(["cmd": "validate_lyrics", "track_id": trackId], as: LyricsDocumentDTO.self) {
            refreshSelectedTrackDetail()
        }
    }

    /// Load track off-MainActor with generation guard so rapid switches never race (I7, B7, I13, P2-I5, P2-I7).
    func loadAndPlay(trackId: String, openSingTab: Bool = true, useStems: Bool? = nil, startAt: Double? = nil) {
        guard let bridge else { return }
        selectTrack(trackId)
        loadGeneration &+= 1
        let expectedGen = loadGeneration
        isLoadingPlayback = true

        var payload: [String: Any] = [
            "cmd": "load_track_for_playback",
            "track_id": trackId,
            "request_generation": expectedGen
        ]
        if let useStems { payload["use_stems"] = useStems }

        Task.detached { [bridge] in
            do {
                let _ = try bridge.dispatch(payload, as: AnyCodableResponse.self)
                if let startAt, startAt > 0 {
                    let _ = try bridge.dispatch([
                        "cmd": "seek",
                        "position_s": startAt,
                        "request_generation": expectedGen
                    ], as: AnyCodableResponse.self)
                }
                let _ = try bridge.dispatch([
                    "cmd": "play",
                    "request_generation": expectedGen
                ], as: AnyCodableResponse.self)
                let loadedDetail = try? bridge.dispatch(["cmd": "get_track_detail", "track_id": trackId], as: TrackDetailDTO.self)
                await MainActor.run {
                    // Discard stale async completion if another track load started afterward (P2-I7)
                    guard self.loadGeneration == expectedGen else { return }
                    self.isLoadingPlayback = false
                    self.loadedTrackId = trackId
                    if let loadedDetail {
                        self.loadedTrackDetail = loadedDetail
                        if self.selectedTrackId == trackId {
                            self.selectedTrackDetail = loadedDetail
                        }
                    } else {
                        self.refreshLoadedTrackDetail()
                    }
                    if let useStems {
                        self.mixParams.useStems = useStems
                    }
                    if openSingTab {
                        self.selectedTab = .sing
                    }
                    // Apply estimated track key when on default Chromatic OR previously auto-applied, and reset if new track is ambiguous (P2-I7)
                    if self.voiceParams.scale == "chromatic" || self.keyWasAutoApplied {
                        if let det = self.loadedTrackDetail,
                           !det.track.keyAmbiguous,
                           det.track.estimatedTonic != nil,
                           det.track.estimatedScale != nil {
                            self.applyEstimatedTrackKey(det)
                        } else if self.keyWasAutoApplied {
                            var vp = self.voiceParams
                            vp.tonic = "c"
                            vp.scale = "chromatic"
                            self.keyWasAutoApplied = false
                            self.updateVoiceParams(vp, manualKeyOverride: false)
                        }
                    }
                }
            } catch let err as KaraokErrorDTO {
                await MainActor.run {
                    guard self.loadGeneration == expectedGen else { return }
                    self.isLoadingPlayback = false
                    self.bannerError = err
                }
            } catch {
                await MainActor.run {
                    guard self.loadGeneration == expectedGen else { return }
                    self.isLoadingPlayback = false
                }
            }
        }
    }

    func togglePlayPause() {
        guard let bridge else { return }
        if telemetry.transport.isPlaying {
            let _ = try? bridge.dispatch(["cmd": "pause"], as: AnyCodableResponse.self)
        } else {
            if loadedTrackId == nil, let tid = selectedTrackId {
                loadAndPlay(trackId: tid, openSingTab: false)
            } else {
                let _ = try? bridge.dispatch(["cmd": "play"], as: AnyCodableResponse.self)
            }
        }
    }

    func seek(to seconds: Double) {
        guard let bridge else { return }
        let _ = try? bridge.dispatch(["cmd": "seek", "position_s": max(0, seconds)], as: AnyCodableResponse.self)
    }

    /// Seek and ensure playback is running (used by "Écouter" in LyricsReviewView - P2-I5).
    func seekAndPlay(to seconds: Double) {
        guard let bridge else { return }
        let _ = try? bridge.dispatch(["cmd": "seek", "position_s": max(0, seconds)], as: AnyCodableResponse.self)
        if !telemetry.transport.isPlaying {
            let _ = try? bridge.dispatch(["cmd": "play"], as: AnyCodableResponse.self)
        }
    }

    func purgeReconstructibleCache() {
        guard let bridge else { return }
        if let _ = try? bridge.dispatch(["cmd": "purge_cache"], as: UInt64.self) {
            refreshSelectedTrackDetail()
            if loadedTrackId != selectedTrackId {
                refreshLoadedTrackDetail()
            }
            refreshDiagnostics()
        }
    }
}

private struct AnyCodableResponse: Decodable {
    init(from decoder: Decoder) throws {
        let _ = try? decoder.singleValueContainer()
    }
}
