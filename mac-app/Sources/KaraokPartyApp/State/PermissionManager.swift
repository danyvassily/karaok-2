import AVFoundation
import AppKit
import Foundation

/// Manages macOS microphone permission at the moment of need (§3.1, §5.5, §11.2, AC-21).
/// Never blocks file playback or library browsing if denied; never suggests bypassing macOS security.
@MainActor
final class PermissionManager: ObservableObject {
    enum MicPermissionStatus: String {
        case notDetermined
        case authorized
        case denied
        case restricted

        var labelFr: String {
            switch self {
            case .notDetermined: return "Non demandée"
            case .authorized: return "Autorisée"
            case .denied: return "Refusée"
            case .restricted: return "Restreinte par le système"
            }
        }
    }

    @Published private(set) var status: MicPermissionStatus = .notDetermined

    init() {
        refreshStatus()
    }

    func refreshStatus() {
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .notDetermined: status = .notDetermined
        case .authorized: status = .authorized
        case .denied: status = .denied
        case .restricted: status = .restricted
        @unknown default: status = .denied
        }
    }

    func requestAccessIfNeeded() async -> Bool {
        refreshStatus()
        switch status {
        case .authorized:
            return true
        case .notDetermined:
            let granted = await AVCaptureDevice.requestAccess(for: .audio)
            refreshStatus()
            return granted
        case .denied, .restricted:
            return false
        }
    }

    func openSystemSettingsMicrophonePrivacy() {
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone") {
            NSWorkspace.shared.open(url)
        }
    }
}
