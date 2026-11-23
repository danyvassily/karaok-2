import SwiftUI

/// Collapsible live voice DSP panel: Presets (Naturel/Studio/Scène), Bypass, real Pitch Correction, Reverb, Dynamics, Latency (§8, I11, Nit 6).
struct VoiceEffectsPanel: View {
    @EnvironmentObject var store: AppStore
    @EnvironmentObject var telemetry: LiveTelemetryStore

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                HStack {
                    Text("Effets voix live")
                        .font(.headline)
                    Spacer()
                    Toggle("Bypass", isOn: routingBinding(\.bypass))
                        .toggleStyle(.switch)
                        .controlSize(.small)
                }

                // Presets Naturel / Studio / Scène — active distinguished by checkmark + weight + tint (never color alone - I11!)
                VStack(alignment: .leading, spacing: 6) {
                    Text("Préréglage : \(presetLabel(store.voiceParams.preset))")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                    HStack(spacing: 6) {
                        presetButton("Naturel", id: "natural")
                        presetButton("Studio", id: "studio")
                        presetButton("Scène", id: "stage")
                    }
                }

                GroupBox("Correction de justesse (Rust DSP)") {
                    VStack(alignment: .leading, spacing: 10) {
                        HStack {
                            VStack(alignment: .leading, spacing: 2) {
                                Text("Détectée : \(telemetry.meters.voiced ? frequencyToNoteLabel(telemetry.meters.detectedF0Hz) : "Non voisé")")
                                    .font(.caption.weight(.semibold))
                                Text(telemetry.meters.voiced ? String(format: "%.1f Hz (conf %.0f%%)", telemetry.meters.detectedF0Hz, telemetry.meters.pitchConfidence * 100) : "Consonne / silence")
                                    .font(.caption2.monospacedDigit())
                                    .foregroundStyle(.secondary)
                            }
                            Spacer()
                            VStack(alignment: .trailing, spacing: 2) {
                                Text("Cible : \(telemetry.meters.targetF0Hz > 0 ? frequencyToNoteLabel(telemetry.meters.targetF0Hz) : "—")")
                                    .font(.caption.weight(.semibold))
                                Text(String(format: "%+.0f cents", telemetry.meters.pitchCorrectionCents))
                                    .font(.caption2.monospacedDigit())
                                    .foregroundStyle(.indigo)
                            }
                        }

                        Toggle("Activer la correction", isOn: toneBinding(\.pitchEnabled))

                        paramSlider("Intensité", value: toneDouble(\.pitchAmount), range: 0...1, suffix: "%", unitSpoken: "pour cent", mult: 100)
                        paramSlider("Vitesse", value: toneDouble(\.pitchSpeed), range: 0.05...1, suffix: "%", unitSpoken: "pour cent", mult: 100)
                        paramSlider("Zone vibrato", value: toneDouble(\.neutralZoneCents), range: 0...45, suffix: " c", unitSpoken: "cents", mult: 1)

                        // Key/scale are song-specific musical context, not preset tone overrides (Nit 6, P2-I7)
                        Picker("Tonique", selection: manualKeyBinding(\.tonic)) {
                            ForEach(allPitchClasses, id: \.key) { item in
                                Text(item.label).tag(item.key)
                            }
                        }
                        .pickerStyle(.menu)

                        Picker("Gamme", selection: manualKeyBinding(\.scale)) {
                            ForEach(allMusicalScales, id: \.key) { item in
                                Text(item.label).tag(item.key)
                            }
                        }
                        .pickerStyle(.menu)

                        if let det = store.activeStageTrackDetail,
                           let estTonic = det.track.estimatedTonic,
                           let estScale = det.track.estimatedScale {
                            Button {
                                store.applyEstimatedTrackKey(det)
                            } label: {
                                Label(
                                    "Appliquer tonalité du morceau (\(pitchClassLabelFr(estTonic)) \(scaleLabelFr(estScale)))",
                                    systemImage: "music.note"
                                )
                                .font(.caption)
                            }
                            .buttonStyle(.bordered)
                            .controlSize(.small)
                        }
                    }
                    .padding(.vertical, 4)
                }

                GroupBox("Réverbération (FDN 8 canaux)") {
                    VStack(alignment: .leading, spacing: 8) {
                        Toggle("Activer la réverbération", isOn: toneBinding(\.reverbEnabled))
                        paramSlider("Quantité (wet)", value: toneDouble(\.reverbWet), range: 0...0.75, suffix: "%", unitSpoken: "pour cent", mult: 100)
                        paramSlider("Décroissance", value: toneDouble(\.reverbDecayS), range: 0.2...4.0, suffix: " s", unitSpoken: "secondes", mult: 1)
                        paramSlider("Prédélai", value: toneDouble(\.reverbPredelayMs), range: 0...80, suffix: " ms", unitSpoken: "millisecondes", mult: 1)
                        paramSlider("Amortissement", value: toneDouble(\.reverbDamping), range: 0.05...0.95, suffix: "%", unitSpoken: "pour cent", mult: 100)
                    }
                    .padding(.vertical, 4)
                }

                GroupBox("Dynamique & Retour voix") {
                    VStack(alignment: .leading, spacing: 8) {
                        Toggle("Filtre passe-haut (80 Hz+)", isOn: toneBinding(\.hpfEnabled))
                        Toggle("Compresseur doux", isOn: toneBinding(\.compressorEnabled))
                        Divider()
                        // Monitoring does NOT mark preset as "custom" (Nit 6)
                        Toggle("Retour de ma voix (casque)", isOn: routingBinding(\.monitoringEnabled))
                        if store.voiceParams.monitoringEnabled {
                            paramSlider("Niveau retour", value: routingDouble(\.monitoringLevel), range: 0...1, suffix: "%", unitSpoken: "pour cent", mult: 100)
                        }
                    }
                    .padding(.vertical, 4)
                }

                GroupBox("Latence (estimation pilote + DSP)") {
                    VStack(alignment: .leading, spacing: 4) {
                        Text(String(format: "Latence signal DSP + limiteur : %.1f ms", store.audioState?.voiceDspLatencyMs ?? 0))
                            .font(.caption.monospacedDigit())
                        Text(String(format: "Latence présentation sortie estimée : %.1f ms", telemetry.transport.presentationLatencyMs))
                            .font(.caption.monospacedDigit())
                        Text("Note : estimation pilote/DSP uniquement, distincte d'une mesure acoustique de bout en bout.")
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                    }
                    .padding(.vertical, 4)
                }
            }
            .padding(16)
        }
        .background(.windowBackground)
    }

    private func presetButton(_ label: String, id: String) -> some View {
        let active = store.voiceParams.preset == id
        return Button {
            store.applyVoicePreset(id)
        } label: {
            HStack(spacing: 4) {
                if active {
                    Image(systemName: "checkmark")
                        .font(.caption2.bold())
                }
                Text(label)
                    .fontWeight(active ? .bold : .regular)
            }
        }
        .buttonStyle(.borderedProminent)
        .tint(active ? .indigo : .secondary.opacity(0.45))
        .controlSize(.small)
        .accessibilityAddTraits(active ? .isSelected : [])
    }

    private func presetLabel(_ id: String) -> String {
        switch id {
        case "natural": return "Naturel"
        case "studio": return "Studio"
        case "stage": return "Scène"
        default: return "Personnalisé"
        }
    }

    private func toneBinding<T>(_ keyPath: WritableKeyPath<VoiceDspParamsDTO, T>) -> Binding<T> {
        Binding(
            get: { store.voiceParams[keyPath: keyPath] },
            set: { newVal in
                var p = store.voiceParams
                p[keyPath: keyPath] = newVal
                p.preset = "custom"
                store.updateVoiceParams(p)
            }
        )
    }

    private func routingBinding<T>(_ keyPath: WritableKeyPath<VoiceDspParamsDTO, T>) -> Binding<T> {
        Binding(
            get: { store.voiceParams[keyPath: keyPath] },
            set: { newVal in
                var p = store.voiceParams
                p[keyPath: keyPath] = newVal
                store.updateVoiceParams(p)
            }
        )
    }

    private func manualKeyBinding<T>(_ keyPath: WritableKeyPath<VoiceDspParamsDTO, T>) -> Binding<T> {
        Binding(
            get: { store.voiceParams[keyPath: keyPath] },
            set: { newVal in
                var p = store.voiceParams
                p[keyPath: keyPath] = newVal
                store.updateVoiceParams(p, manualKeyOverride: true)
            }
        )
    }

    private func toneDouble(_ keyPath: WritableKeyPath<VoiceDspParamsDTO, Float>) -> Binding<Double> {
        Binding(
            get: { Double(store.voiceParams[keyPath: keyPath]) },
            set: { newVal in
                var p = store.voiceParams
                p[keyPath: keyPath] = Float(newVal)
                p.preset = "custom"
                store.updateVoiceParams(p)
            }
        )
    }

    private func routingDouble(_ keyPath: WritableKeyPath<VoiceDspParamsDTO, Float>) -> Binding<Double> {
        Binding(
            get: { Double(store.voiceParams[keyPath: keyPath]) },
            set: { newVal in
                var p = store.voiceParams
                p[keyPath: keyPath] = Float(newVal)
                store.updateVoiceParams(p)
            }
        )
    }

    private func paramSlider(_ title: String, value: Binding<Double>, range: ClosedRange<Double>, suffix: String, unitSpoken: String, mult: Double) -> some View {
        let formatted = String(format: mult == 1 && suffix == " s" ? "%.1f%@" : "%.0f%@", value.wrappedValue * mult, suffix)
        let spoken = String(format: mult == 1 && suffix == " s" ? "%.1f %@" : "%.0f %@", value.wrappedValue * mult, unitSpoken)
        return VStack(alignment: .leading, spacing: 2) {
            HStack {
                Text(title).font(.caption)
                Spacer()
                Text(formatted)
                    .font(.caption2.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
            Slider(value: value, in: range)
                .controlSize(.small)
                .accessibilityLabel(title)
                .accessibilityValue(spoken)
        }
    }
}
