import SwiftUI

@main
struct KaraokPartyApp: App {
    @StateObject private var store = AppStore()

    var body: some Scene {
        WindowGroup("Karaok Party") {
            RootView()
                .environmentObject(store)
                .environmentObject(store.telemetry)
                .frame(minWidth: 960, minHeight: 640)
        }
        .commands {
            CommandMenu("Lecture") {
                // Note (B1): Unmodified Spacebar is handled strictly outside text inputs via NSEvent monitor in RootView
                // so TextField / TextEditor always receive spaces normally (§3.5).
                Button("Lecture / Pause (Espace hors saisie)") {
                    store.togglePlayPause()
                }
                .keyboardShortcut("p", modifiers: [.command, .shift])

                Button(store.isFullScreenLyrics ? "Quitter le plein écran paroles (Échap)" : "Plein écran paroles") {
                    store.isFullScreenLyrics.toggle()
                }
                .keyboardShortcut("f", modifiers: [.command, .shift])

                Divider()

                Button("Agrandir les paroles") {
                    store.lyricsScale = min(2.0, store.lyricsScale + 0.15)
                }
                .keyboardShortcut("+", modifiers: .command)

                Button("Réduire les paroles") {
                    store.lyricsScale = max(1.0, store.lyricsScale - 0.15)
                }
                .keyboardShortcut("-", modifiers: .command)
            }
            CommandMenu("Son") {
                Button("Rafraîchir les périphériques Core Audio") {
                    store.refreshDevicesManually()
                }
                .keyboardShortcut("r", modifiers: [.command, .shift])

                Button(store.voiceParams.micMuted ? "Réactiver le micro" : "Couper le micro (Mute)") {
                    var p = store.voiceParams
                    p.micMuted.toggle()
                    store.updateVoiceParams(p)
                }
                .keyboardShortcut("m", modifiers: [.command, .shift])
            }
        }
    }
}
