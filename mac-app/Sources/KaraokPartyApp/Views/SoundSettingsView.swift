import SwiftUI

/// Sound, Core Audio device inventory, Nothing Headphone (a) explicit selection, channel selection, and safe mic test (§3.1, §5).
struct SoundSettingsView: View {
    @EnvironmentObject var store: AppStore
    @EnvironmentObject var telemetry: LiveTelemetryStore

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                headerBar
                permissionCard

                if let candidate = store.audioState?.inputs.first(where: { $0.nothingHeadphoneACandidate }),
                   store.audioState?.preference.input.uid != candidate.uid {
                    nothingSuggestionCard(candidate)
                }

                if let state = store.audioState,
                   state.inputStatus == "unavailable_preferred" || state.inputStatus == "awaiting_reconnect_confirmation" {
                    inputRouteAlertCard(state)
                }

                inputDevicesSection
                outputDevicesSection
                monitoringAndTestSection
            }
            .padding(24)
        }
    }

    private var headerBar: some View {
        HStack {
            VStack(alignment: .leading, spacing: 4) {
                Text("Son & Périphériques Core Audio")
                    .font(.title2.bold())
                Text("Sélection explicite par UID persistant — ne modifie jamais les réglages système globaux de macOS.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Button {
                store.refreshDevicesManually()
                store.permissions.refreshStatus()
            } label: {
                Label("Rafraîchir", systemImage: "arrow.clockwise")
            }
            .buttonStyle(.bordered)
        }
    }

    private var permissionCard: some View {
        GroupBox("Autorisation microphone macOS") {
            HStack {
                VStack(alignment: .leading, spacing: 4) {
                    Text("Statut : \(store.permissions.status.labelFr)")
                        .font(.subheadline.weight(.semibold))
                    Text("Demandée uniquement lors d'un test ou de l'activation du chant. La lecture musicale et la bibliothèque restent utilisables en cas de refus.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                if store.permissions.status == .denied || store.permissions.status == .restricted {
                    Button("Ouvrir Réglages Système") {
                        store.permissions.openSystemSettingsMicrophonePrivacy()
                    }
                    .buttonStyle(.borderedProminent)
                } else if store.permissions.status == .notDetermined {
                    Button("Autoriser le microphone") {
                        Task { _ = await store.permissions.requestAccessIfNeeded() }
                    }
                    .buttonStyle(.bordered)
                }
            }
            .padding(.vertical, 4)
        }
    }

    private func nothingSuggestionCard(_ candidate: AudioDeviceDTO) -> some View {
        HStack(spacing: 12) {
            Image(systemName: "headphones.circle.fill")
                .font(.title)
                .foregroundStyle(.indigo)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 3) {
                Text("Nothing Headphone (a) USB-C détecté (suggestion)")
                    .font(.headline)
                Text("Canaux d'entrée : \(candidate.inputChannels) • \(candidate.sampleRateLabel) • UID local : \(candidate.redactedUid). Confirmez pour le mémoriser comme micro principal fixe.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Button("Confirmer comme micro principal") {
                store.selectInput(selector: .fixed(uid: candidate.uid, name: candidate.name, confirmed: true))
            }
            .buttonStyle(.borderedProminent)
        }
        .padding(14)
        .background(Color.indigo.opacity(0.12), in: RoundedRectangle(cornerRadius: 10))
    }

    private func inputRouteAlertCard(_ state: AudioRouteStateDTO) -> some View {
        HStack(spacing: 12) {
            Image(systemName: "mic.slash.circle.fill")
                .font(.title2)
                .foregroundStyle(.orange)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 3) {
                Text(state.inputStatus == "awaiting_reconnect_confirmation" ? "Micro préféré reconnecté" : "Micro préféré indisponible")
                    .font(.headline)
                Text(state.inputStatusMessageFr ?? "Sélectionnez explicitement une entrée disponible.")
                    .font(.callout)
            }
            Spacer()
            if state.inputStatus == "awaiting_reconnect_confirmation",
               let uid = state.preference.input.uid,
               let name = state.preference.input.lastKnownName {
                Button("Confirmer la reprise") {
                    store.selectInput(selector: .fixed(uid: uid, name: name, confirmed: true), confirmReconnect: true)
                }
                .buttonStyle(.borderedProminent)
            }
        }
        .padding(14)
        .background(Color.orange.opacity(0.14), in: RoundedRectangle(cornerRadius: 10))
    }

    private var selectedInputChannelCount: UInt32 {
        if let active = store.audioState?.activeInputFormat {
            return active.channels
        }
        if let uid = store.audioState?.preference.input.uid,
           let dev = store.audioState?.inputs.first(where: { $0.uid == uid }) {
            return dev.inputChannels
        }
        return store.audioState?.inputs.first(where: { $0.isDefaultInput })?.inputChannels ?? 1
    }

    private var inputDevicesSection: some View {
        GroupBox("1. Entrée microphone") {
            VStack(alignment: .leading, spacing: 10) {
                if let activeIn = store.audioState?.activeInputFormat {
                    activeFormatBadge("Format actif ouvert", format: activeIn)
                }

                // Multi-channel selector when interface exposes >1 input channel (I13)
                if selectedInputChannelCount > 1 {
                    Picker("Canal micro utilisé", selection: Binding(
                        get: { store.audioState?.preference.inputChannelIndex ?? 0 },
                        set: { store.setInputChannel($0) }
                    )) {
                        ForEach(0..<selectedInputChannelCount, id: \.self) { ch in
                            Text("Canal \(ch + 1)").tag(ch)
                        }
                    }
                    .pickerStyle(.segmented)
                    .accessibilityLabel("Sélection du canal microphone")
                }

                deviceRow(
                    title: "Rôle système par défaut (suit les réglages macOS)",
                    subtitle: "Suit dynamiquement l'entrée par défaut macOS sans la modifier",
                    selected: store.audioState?.preference.input.mode == "system_default",
                    badges: ["Dynamique"]
                ) {
                    store.selectInput(selector: .systemDefault)
                }

                Divider()

                ForEach(store.audioState?.inputs ?? []) { dev in
                    let isSelected = store.audioState?.preference.input.mode == "fixed"
                        && store.audioState?.preference.input.uid == dev.uid
                    deviceRow(
                        title: dev.name,
                        subtitle: "\(dev.inputChannels) ch entrée • \(dev.sampleRateLabel) • buf \(dev.bufferFrameSize.map { "\($0)" } ?? "inconnu") • \(dev.redactedUid)",
                        selected: isSelected,
                        badges: [
                            dev.transportLabelFr,
                            dev.isDefaultInput ? "Défaut macOS" : nil,
                            dev.nothingHeadphoneACandidate ? "Référence USB-C" : nil
                        ].compactMap { $0 }
                    ) {
                        store.selectInput(selector: .fixed(uid: dev.uid, name: dev.name, confirmed: true))
                    }
                }
            }
            .padding(.vertical, 6)
        }
    }

    private var outputDevicesSection: some View {
        GroupBox("2. Sortie musique et mix (1 sortie active en V1)") {
            VStack(alignment: .leading, spacing: 10) {
                if let activeOut = store.audioState?.activeOutputFormat {
                    activeFormatBadge("Sortie active négociée", format: activeOut)
                    if activeOut.transport == "bluetooth" && telemetry.meters.activeMicCapture {
                        Label(
                            "Bluetooth + micro actif : macOS peut réduire la bande passante audio. Préférez la connexion USB-C ou une sortie filaire distincte.",
                            systemImage: "exclamationmark.triangle"
                        )
                        .font(.caption)
                        .foregroundStyle(.orange)
                    }
                }

                deviceRow(
                    title: "Rôle système par défaut (suit les réglages macOS)",
                    subtitle: "Bascule automatiquement selon la sortie par défaut macOS ; le retour voix reste coupé par sécurité",
                    selected: store.audioState?.preference.output.mode == "system_default",
                    badges: ["Dynamique"]
                ) {
                    store.selectOutput(selector: .systemDefault)
                }

                Divider()

                ForEach(store.audioState?.outputs ?? []) { dev in
                    let isSelected = store.audioState?.preference.output.mode == "fixed"
                        && store.audioState?.preference.output.uid == dev.uid
                    deviceRow(
                        title: dev.name,
                        subtitle: "\(dev.outputChannels) ch sortie • \(dev.sampleRateLabel) • buf \(dev.bufferFrameSize.map { "\($0)" } ?? "inconnu") • \(dev.redactedUid)",
                        selected: isSelected,
                        badges: [
                            dev.transportLabelFr,
                            dev.isDefaultOutput ? "Défaut macOS" : nil
                        ].compactMap { $0 }
                    ) {
                        store.selectOutput(selector: .fixed(uid: dev.uid, name: dev.name, confirmed: true))
                    }
                }
            }
            .padding(.vertical, 6)
        }
    }

    private var monitoringAndTestSection: some View {
        GroupBox("3. Retour de ma voix & Test du microphone") {
            VStack(alignment: .leading, spacing: 14) {
                Text("En V1, le retour logiciel dirige votre voix traitée vers la sortie musique/mix sélectionnée. Il démarre toujours désactivé pour éviter tout larsen acoustique sur haut-parleurs.")
                    .font(.caption)
                    .foregroundStyle(.secondary)

                HStack(spacing: 12) {
                    if store.isRunningMicTest {
                        Button(role: .destructive) {
                            store.stopMicTest()
                        } label: {
                            Label("Arrêter le test (\(store.micTestSecondsRemaining)s)", systemImage: "stop.circle.fill")
                        }
                        .buttonStyle(.borderedProminent)
                    } else {
                        Button {
                            Task { await store.startSafeMicTest(withMonitoring: false) }
                        } label: {
                            Label("Tester mon micro (sans retour)", systemImage: "mic.badge.plus")
                        }
                        .buttonStyle(.borderedProminent)

                        Button {
                            Task { await store.startSafeMicTest(withMonitoring: true) }
                        } label: {
                            Label("Test avec retour modéré (10 s)", systemImage: "waveform")
                        }
                        .buttonStyle(.bordered)
                    }

                    if store.audioState?.micCaptureActive == true {
                        Button("Couper la capture micro") {
                            store.stopMicCapture()
                        }
                        .buttonStyle(.bordered)
                    }
                }

                VStack(alignment: .leading, spacing: 8) {
                    meterBar(
                        label: "Niveau entrée pré-effets",
                        value: Double(telemetry.meters.inputPrePeak),
                        clipped: telemetry.meters.inputClipped
                    )
                    if telemetry.meters.inputClipped {
                        Text("Saturation d'entrée détectée : baissez le niveau matériel du casque ou éloignez le micro (le gain logiciel ne corrige pas un écrêtage matériel).")
                            .font(.caption)
                            .foregroundStyle(.red)
                    }
                    meterBar(
                        label: "Niveau voix post-effets",
                        value: Double(telemetry.meters.voicePostPeak),
                        clipped: telemetry.meters.outputClipped
                    )
                }

                Divider()

                Toggle("Activer le retour logiciel de ma voix vers la sortie active", isOn: Binding(
                    get: { store.voiceParams.monitoringEnabled },
                    set: { enabled in
                        var p = store.voiceParams
                        p.monitoringEnabled = enabled
                        store.updateVoiceParams(p)
                    }
                ))

                HStack {
                    Text("Volume retour voix")
                    Slider(
                        value: Binding(
                            get: { Double(store.voiceParams.monitoringLevel) },
                            set: { v in
                                var p = store.voiceParams
                                p.monitoringLevel = Float(v)
                                store.updateVoiceParams(p)
                            }
                        ),
                        in: 0...1
                    )
                    .disabled(!store.voiceParams.monitoringEnabled)
                    .accessibilityLabel("Volume du retour de voix")
                    .accessibilityValue("\(Int(store.voiceParams.monitoringLevel * 100)) pour cent")
                    Text("\(Int(store.voiceParams.monitoringLevel * 100)) %")
                        .font(.caption.monospacedDigit())
                        .frame(width: 44, alignment: .trailing)
                }

                HStack {
                    Text("Gain d'entrée numérique")
                    Slider(
                        value: Binding(
                            get: { Double(store.voiceParams.inputGainDb) },
                            set: { v in
                                var p = store.voiceParams
                                p.inputGainDb = Float(v)
                                store.updateVoiceParams(p)
                            }
                        ),
                        in: -24...24
                    )
                    .accessibilityLabel("Gain d'entrée numérique")
                    .accessibilityValue(String(format: "%+.1f décibels", store.voiceParams.inputGainDb))
                    Text(String(format: "%+.1f dB", store.voiceParams.inputGainDb))
                        .font(.caption.monospacedDigit())
                        .frame(width: 58, alignment: .trailing)
                }
            }
            .padding(.vertical, 6)
        }
    }

    private func activeFormatBadge(_ title: String, format: ActiveStreamFormatDTO) -> some View {
        HStack(spacing: 8) {
            Image(systemName: "checkmark.seal.fill").foregroundStyle(.green)
            Text("\(title) : \(format.deviceName) • \(Int(format.sampleRateHz)) Hz • \(format.channels) ch • buffer \(format.bufferFrames) frames")
                .font(.caption.weight(.medium))
        }
        .padding(8)
        .background(Color.green.opacity(0.10), in: RoundedRectangle(cornerRadius: 8))
    }

    private func deviceRow(
        title: String,
        subtitle: String,
        selected: Bool,
        badges: [String],
        action: @escaping () -> Void
    ) -> some View {
        Button(action: action) {
            HStack(spacing: 10) {
                Image(systemName: selected ? "checkmark.circle.fill" : "circle")
                    .foregroundStyle(selected ? Color.accentColor : Color.secondary)
                VStack(alignment: .leading, spacing: 2) {
                    Text(title)
                        .font(.subheadline.weight(selected ? .semibold : .regular))
                        .foregroundStyle(.primary)
                    Text(subtitle)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                ForEach(badges, id: \.self) { b in
                    Text(b)
                        .font(.caption2)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 2)
                        .background(Color.secondary.opacity(0.15), in: Capsule())
                }
            }
            .padding(8)
            .background(selected ? Color.accentColor.opacity(0.10) : Color.clear, in: RoundedRectangle(cornerRadius: 8))
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }

    private func meterBar(label: String, value: Double, clipped: Bool) -> some View {
        HStack(spacing: 10) {
            Text(label)
                .font(.caption)
                .frame(width: 150, alignment: .leading)
            GeometryReader { geo in
                ZStack(alignment: .leading) {
                    RoundedRectangle(cornerRadius: 4)
                        .fill(Color.secondary.opacity(0.2))
                    RoundedRectangle(cornerRadius: 4)
                        .fill(clipped ? Color.red : (value > 0.8 ? Color.orange : Color.green))
                        .frame(width: max(0, min(1, value)) * geo.size.width)
                }
            }
            .frame(height: 10)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel(label)
            .accessibilityValue(clipped ? "Saturation détectée" : "\(Int(min(100, value * 100))) pour cent")

            Text(clipped ? "CLIP" : String(format: "%.0f %%", min(100, value * 100)))
                .font(.caption2.monospacedDigit().weight(clipped ? .bold : .regular))
                .foregroundStyle(clipped ? .red : .secondary)
                .frame(width: 42, alignment: .trailing)
        }
    }
}
