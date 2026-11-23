import AppKit
import SwiftUI

struct RootView: View {
    @EnvironmentObject var store: AppStore
    @State private var keyMonitor: Any? = nil

    var body: some View {
        Group {
            if let fatal = store.fatalInitError {
                fatalErrorView(fatal)
            } else if store.isFullScreenLyrics {
                VStack(spacing: 0) {
                    // Keep safety/route-loss alerts visible even in fullscreen mode (I11)!
                    alertsBanner
                    SingView()
                }
            } else {
                NavigationSplitView {
                    sidebar
                } detail: {
                    VStack(spacing: 0) {
                        alertsBanner
                        mainContent
                            .frame(maxWidth: .infinity, maxHeight: .infinity)
                        Divider()
                        TransportBarView()
                    }
                }
                .navigationSplitViewStyle(.balanced)
            }
        }
        .onAppear { installKeyboardMonitor() }
        .onDisappear { removeKeyboardMonitor() }
    }

    /// Intercept Space (play/pause STRICTLY outside text/controls/modals/repeats - B1, P2-I4) and Esc (exit fullscreen).
    private func installKeyboardMonitor() {
        guard keyMonitor == nil else { return }
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
            // Never intercept key repeats, modal panels (NSOpenPanel/NSSavePanel), or active sheets (P2-I4)
            guard !event.isARepeat,
                  NSApp.modalWindow == nil,
                  (NSApp.keyWindow?.sheets.isEmpty ?? true),
                  event.window == NSApp.mainWindow else {
                return event
            }
            // Escape exits fullscreen lyrics when no modal/sheet is active (I11, P2-I4)
            if event.keyCode == 53 && store.isFullScreenLyrics {
                Task { @MainActor in store.isFullScreenLyrics = false }
                return nil
            }
            // Unmodified Space toggles Play/Pause ONLY when firstResponder is NOT a text input or interactive NSControl
            // (preserving Full Keyboard Access Tab+Space activation on buttons/toggles/pickers/sliders - B1, P2-I4)
            let relevantMods = event.modifierFlags.intersection([.command, .option, .control, .shift])
            if event.keyCode == 49 && relevantMods.isEmpty {
                let responder = NSApp.keyWindow?.firstResponder
                let isInteractiveControl = (responder is NSText) || (responder is NSTextField) || (responder is NSControl)
                if !isInteractiveControl {
                    Task { @MainActor in store.togglePlayPause() }
                    return nil
                }
            }
            return event
        }
    }

    private func removeKeyboardMonitor() {
        if let monitor = keyMonitor {
            NSEvent.removeMonitor(monitor)
            keyMonitor = nil
        }
    }

    private var sidebar: some View {
        List(SidebarTab.allCases, selection: $store.selectedTab) { tab in
            Label(tab.rawValue, systemImage: tab.icon)
                .tag(tab)
                .accessibilityLabel(tab.rawValue)
        }
        .listStyle(.sidebar)
        .navigationSplitViewColumnWidth(min: 190, ideal: 220, max: 260)
        .safeAreaInset(edge: .bottom) {
            SidebarMicStatusFooter()
        }
    }

    @ViewBuilder
    private var alertsBanner: some View {
        if let routeAlert = store.routeAlertMessageFr {
            HStack(alignment: .top, spacing: 10) {
                Image(systemName: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)
                    .accessibilityHidden(true)
                Text(routeAlert)
                    .font(.callout)
                Spacer()
                Button("Ouvrir Son") {
                    store.isFullScreenLyrics = false
                    store.selectedTab = .sound
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.small)
                Button("Fermer") {
                    store.routeAlertMessageFr = nil
                }
                .buttonStyle(.borderless)
                .controlSize(.small)
            }
            .padding(10)
            .background(Color.orange.opacity(0.16))
        }

        if let err = store.bannerError {
            HStack(alignment: .top, spacing: 10) {
                Image(systemName: "xmark.octagon.fill")
                    .foregroundStyle(.red)
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: 2) {
                    Text(err.messageFr)
                        .font(.callout.weight(.medium))
                    if let hint = err.actionableHintFr {
                        Text(hint)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer()
                if err.code == "permission_denied" {
                    Button("Réglages Système") {
                        store.permissions.openSystemSettingsMicrophonePrivacy()
                    }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
                }
                Button("Fermer") {
                    store.bannerError = nil
                }
                .buttonStyle(.borderless)
                .controlSize(.small)
            }
            .padding(10)
            .background(Color.red.opacity(0.14))
        }
    }

    @ViewBuilder
    private var mainContent: some View {
        switch store.selectedTab {
        case .sing:
            SingView()
        case .library:
            LibraryView()
        case .lyricsReview:
            LyricsReviewView()
        case .sound:
            SoundSettingsView()
        case .diagnostics:
            DiagnosticsView()
        }
    }

    private func fatalErrorView(_ err: KaraokErrorDTO) -> some View {
        VStack(spacing: 14) {
            Image(systemName: "exclamationmark.shield.fill")
                .font(.system(size: 44))
                .foregroundStyle(.red)
            Text("Ouverture de Karaok Party impossible")
                .font(.title2.bold())
            Text(err.messageFr)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 520)
            if let hint = err.actionableHintFr {
                Text(hint)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .frame(maxWidth: 520)
            }
        }
        .padding(40)
    }
}

private struct SidebarMicStatusFooter: View {
    @EnvironmentObject var store: AppStore
    @EnvironmentObject var telemetry: LiveTelemetryStore

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Divider()
            HStack(spacing: 6) {
                Circle()
                    .fill(telemetry.meters.activeMicCapture ? Color.green : Color.secondary.opacity(0.5))
                    .frame(width: 8, height: 8)
                Text(telemetry.meters.activeMicCapture ? "Micro actif" : "Micro inactif")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
                Text("100 % local")
                    .font(.caption2)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(Color.secondary.opacity(0.15), in: Capsule())
            }
            if let activeIn = store.audioState?.activeInputFormat {
                Text("\(activeIn.deviceName) • \(Int(activeIn.sampleRateHz)) Hz")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
        .padding(.horizontal, 12)
        .padding(.bottom, 10)
    }
}

struct TransportBarView: View {
    @EnvironmentObject var store: AppStore
    @EnvironmentObject var telemetry: LiveTelemetryStore
    @State private var isScrubbing = false
    @State private var scrubPosition: Double = 0

    var body: some View {
        HStack(spacing: 16) {
            let barDetail = store.activeStageTrackDetail
            VStack(alignment: .leading, spacing: 2) {
                Text(barDetail?.track.title ?? "Aucun morceau chargé")
                    .font(.subheadline.weight(.semibold))
                    .lineLimit(1)
                HStack(spacing: 6) {
                    Text(barDetail?.track.artist ?? "Importez un MP3 ou WAV")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                    if telemetry.transport.usingStems {
                        Text("Stems")
                            .font(.caption2.weight(.semibold))
                            .padding(.horizontal, 5)
                            .padding(.vertical, 1)
                            .background(Color.indigo.opacity(0.2), in: Capsule())
                    }
                    if store.isLoadingPlayback {
                        ProgressView().controlSize(.mini)
                    }
                }
            }
            .frame(width: 210, alignment: .leading)

            Button {
                store.togglePlayPause()
            } label: {
                Image(systemName: telemetry.transport.isPlaying ? "pause.circle.fill" : "play.circle.fill")
                    .font(.system(size: 28))
            }
            .buttonStyle(.plain)
            .disabled(store.selectedTrackId == nil && store.loadedTrackId == nil)
            .accessibilityLabel(telemetry.transport.isPlaying ? "Pause" : "Lecture")

            HStack(spacing: 8) {
                Text(formatTimeMMSS(isScrubbing ? scrubPosition : telemetry.transport.sourcePositionS))
                    .font(.caption.monospacedDigit())
                    .frame(width: 44, alignment: .trailing)

                Slider(
                    value: Binding(
                        get: { isScrubbing ? scrubPosition : telemetry.transport.sourcePositionS },
                        set: { scrubPosition = $0 }
                    ),
                    in: 0...max(telemetry.transport.durationS, 1.0),
                    onEditingChanged: { editing in
                        isScrubbing = editing
                        if !editing {
                            store.seek(to: scrubPosition)
                        }
                    }
                )
                .disabled(telemetry.transport.durationS <= 0)
                .accessibilityLabel("Position de lecture")
                .accessibilityValue(formatTimeMMSS(telemetry.transport.sourcePositionS))

                Text(formatTimeMMSS(telemetry.transport.durationS))
                    .font(.caption.monospacedDigit())
                    .frame(width: 44, alignment: .leading)
            }

            Button {
                var p = store.voiceParams
                p.micMuted.toggle()
                store.updateVoiceParams(p)
            } label: {
                Label(store.voiceParams.micMuted ? "Micro coupé" : "Micro", systemImage: store.voiceParams.micMuted ? "mic.slash.fill" : "mic.fill")
                    .font(.caption)
            }
            .buttonStyle(.bordered)
            .tint(store.voiceParams.micMuted ? .red : .accentColor)
            .accessibilityLabel(store.voiceParams.micMuted ? "Réactiver le micro" : "Couper le micro")

            Button {
                store.selectedTab = .sing
                store.isFullScreenLyrics = true
            } label: {
                Image(systemName: "arrow.up.left.and.arrow.down.right")
            }
            .buttonStyle(.bordered)
            .help("Plein écran paroles (Échap pour quitter)")
            .accessibilityLabel("Afficher les paroles en plein écran")
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
        .background(.bar)
    }
}
