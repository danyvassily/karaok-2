import Foundation
import CKaraokFFI

/// Safe Swift RAII wrapper around the narrow C ABI (`libkaraok_ffi.a`, ADR-0001).
/// Uses a fresh `JSONDecoder()` per call so concurrent calls from background tasks are thread-safe (I7).
final class RustBridge: @unchecked Sendable {
    private let handle: OpaquePointer

    init(appSupportDir: String? = nil, cacheDir: String? = nil) throws {
        let abi = karaok_abi_version()
        guard abi == 1 else {
            throw KaraokErrorDTO(
                code: "internal_error",
                messageFr: "Version ABI FFI incompatible (attendu 1, reçu \(abi)).",
                actionableHintFr: "Recompilez la bibliothèque Rust et l'application Swift ensemble."
            )
        }

        var errPtr: UnsafeMutablePointer<CChar>? = nil
        let rawHandle: OpaquePointer? = {
            if let appDir = appSupportDir, let cDir = cacheDir {
                return appDir.withCString { aPtr in
                    cDir.withCString { cPtr in
                        karaok_controller_create(aPtr, cPtr, &errPtr)
                    }
                }
            } else {
                return karaok_controller_create(nil, nil, &errPtr)
            }
        }()

        if let h = rawHandle {
            self.handle = h
        } else {
            let errText: String
            if let ep = errPtr {
                errText = String(cString: ep)
                karaok_string_free(ep)
            } else {
                errText = "Échec d'initialisation du contrôleur Rust."
            }
            if let data = errText.data(using: .utf8),
               let dto = try? JSONDecoder().decode(KaraokErrorDTO.self, from: data) {
                throw dto
            }
            throw KaraokErrorDTO(code: "internal_error", messageFr: errText, actionableHintFr: nil)
        }
    }

    deinit {
        karaok_controller_destroy(handle)
    }

    func dispatch<T: Decodable>(_ payload: [String: Any], as type: T.Type) throws -> T {
        let reqData = try JSONSerialization.data(withJSONObject: payload, options: [])
        guard let reqString = String(data: reqData, encoding: .utf8) else {
            throw KaraokErrorDTO(code: "invalid_parameter", messageFr: "Encodage UTF-8 impossible.", actionableHintFr: nil)
        }
        let respPtr = reqString.withCString { cStr in
            karaok_controller_dispatch_json(handle, cStr)
        }
        guard let ptr = respPtr else {
            throw KaraokErrorDTO(code: "internal_error", messageFr: "Réponse FFI nulle.", actionableHintFr: nil)
        }
        defer { karaok_string_free(ptr) }
        let jsonStr = String(cString: ptr)
        guard let respData = jsonStr.data(using: .utf8) else {
            throw KaraokErrorDTO(code: "internal_error", messageFr: "Réponse JSON invalide.", actionableHintFr: nil)
        }
        let envelope = try JSONDecoder().decode(ControllerResponseEnvelope<T>.self, from: respData)
        if envelope.ok == "success", let data = envelope.data {
            return data
        }
        if let err = envelope.error {
            throw err
        }
        throw KaraokErrorDTO(code: "internal_error", messageFr: "Erreur inconnue du contrôleur.", actionableHintFr: nil)
    }

    func pollEventsRaw() -> [[String: Any]] {
        guard let ptr = karaok_controller_poll_events_json(handle) else { return [] }
        defer { karaok_string_free(ptr) }
        let jsonStr = String(cString: ptr)
        guard let data = jsonStr.data(using: .utf8),
              let arr = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]] else {
            return []
        }
        return arr
    }

    func readTransport() -> TransportStateSnapshot {
        var raw = KaraokTransportSnapshotC()
        let ok = karaok_controller_read_transport(handle, &raw)
        guard ok == 1 else { return .empty }
        return TransportStateSnapshot(
            generation: raw.generation,
            stateCode: raw.state,
            presentationQuality: raw.presentation_quality,
            usingStems: raw.using_stems != 0,
            stemsAvailable: raw.stems_available != 0,
            sourcePositionS: raw.source_position_s,
            durationS: raw.duration_s,
            monotonicHostNs: raw.monotonic_host_ns,
            playbackRatio: raw.playback_ratio,
            presentationLatencyMs: raw.presentation_latency_ms
        )
    }

    func readMeters() -> MeterStateSnapshot {
        var raw = KaraokMeterSnapshotC()
        let ok = karaok_controller_read_meters(handle, &raw)
        guard ok == 1 else { return .empty }
        return MeterStateSnapshot(
            inputPrePeak: raw.input_pre_peak,
            inputPreRms: raw.input_pre_rms,
            inputClipped: raw.input_clipped != 0,
            outputClipped: raw.output_clipped != 0,
            voiced: raw.voiced != 0,
            activeMicCapture: raw.active_mic_capture != 0,
            voicePostPeak: raw.voice_post_peak,
            voicePostRms: raw.voice_post_rms,
            outputPeak: raw.output_peak,
            detectedF0Hz: raw.detected_f0_hz,
            targetF0Hz: raw.target_f0_hz,
            pitchConfidence: raw.pitch_confidence,
            pitchCorrectionCents: raw.pitch_correction_cents
        )
    }
}
