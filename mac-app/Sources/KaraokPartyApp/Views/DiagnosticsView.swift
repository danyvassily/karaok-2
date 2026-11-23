import AppKit
import SwiftUI
import UniformTypeIdentifiers

/// Real-time telemetry counters, verified local ModelStore (with local ONNX import UI - I13),
/// latency profiles, privacy & redacted export (§11, Nit 5).
struct DiagnosticsView: View {
    @EnvironmentObject var store: AppStore
    @State private var exportedNotice: String? = nil

    // Local verified ONNX model import sheet (I13)
    @State private var showImportModelSheet = false
    @State private var importModelFileURL: URL? = nil
    @State private var importModelId: String = "custom-onnx-stem-v1"
    @State private var importModelName: String = "Modèle ONNX vérifié local"
    @State private var importSha256: String = ""
    @State private var importCodeLicense: String = "MIT"
    @State private var importWeightsLicense: String = ""

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                HStack {
                    VStack(alignment: .leading, spacing: 4) {
                        Text("Diagnostics, Modèles & Confidentialité")
                            .font(.title2.bold())
                        Text("Mode hors ligne par défaut — aucun appel réseau caché, aucun PCM ni parole dans les journaux.")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button("Actualiser") {
                        store.refreshDiagnostics()
                        store.refreshModels()
                        store.refreshLatencyProfiles()
                    }
                    .buttonStyle(.bordered)
                }

                GroupBox("Compteurs temps réel Rust (lus hors callback)") {
                    if let c = store.diagnostics?.rtCounters {
                        LazyVGrid(columns: [GridItem(.flexible()), GridItem(.flexible()), GridItem(.flexible())], spacing: 10) {
                            metricTile("Callbacks sortie", "\(c.outputCallbacks)")
                            metricTile("Callbacks micro", "\(c.inputCallbacks)")
                            metricTile("Dépassements échéance", "\(c.callbackDeadlineOverruns)", alert: c.callbackDeadlineOverruns > 0)
                            metricTile("Sous-alim musique", "\(c.musicUnderruns)", alert: c.musicUnderruns > 0)
                            metricTile("Sous-alim / sur-alim micro", "\(c.micUnderruns) / \(c.micOverruns)")
                            metricTile("NaN/Inf bloqués", "\(c.nonFiniteSanitizations)")
                            metricTile("Dernier / Max rendu", "\(c.lastRenderDurationUs) / \(c.maxRenderDurationUs) µs")
                            metricTile("Occupation ring micro", "\(c.micRingOccupancyFrames) frames")
                            metricTile("Ratio resampling dérive", String(format: "%.6f", c.micResampleRatio))
                        }
                        .padding(.vertical, 6)
                    } else {
                        Text("Chargement des compteurs…").foregroundStyle(.secondary)
                    }
                }

                GroupBox("Registre des modèles locaux (opt-in vérifié)") {
                    VStack(alignment: .leading, spacing: 10) {
                        HStack {
                            Text("La séparation spectrale DSP locale fonctionne immédiatement hors ligne. Les modèles neuronaux ONNX sont optionnels, jamais téléchargés silencieusement et vérifiés par empreinte SHA-256 + licence des poids avant enregistrement dans le registre vérifié (inférence ONNX non activée en V1).")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            Spacer()
                            Button("Importer un modèle .onnx…") {
                                showImportModelSheet = true
                            }
                            .buttonStyle(.bordered)
                            .controlSize(.small)
                        }

                        ForEach(store.models) { model in
                            VStack(alignment: .leading, spacing: 3) {
                                HStack {
                                    Text(model.displayName).font(.subheadline.weight(.semibold))
                                    Spacer()
                                    Text(modelStateLabelFr(model.state))
                                        .font(.caption2)
                                        .padding(.horizontal, 6)
                                        .padding(.vertical, 2)
                                        .background(Color.secondary.opacity(0.15), in: Capsule())
                                }
                                Text("Code : \(model.codeLicense) • Poids : \(model.weightsLicense) • Runtime : \(model.runtime)")
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                            .padding(8)
                            .background(Color.secondary.opacity(0.06), in: RoundedRectangle(cornerRadius: 8))
                        }
                    }
                    .padding(.vertical, 4)
                }

                GroupBox("Profils de latence enregistrés") {
                    if store.latencyProfiles.isEmpty {
                        Text("Aucun profil enregistré.").font(.caption).foregroundStyle(.secondary)
                    } else {
                        ForEach(store.latencyProfiles) { prof in
                            VStack(alignment: .leading, spacing: 2) {
                                Text("\(prof.outputName) • \(Int(prof.sampleRateHz)) Hz • buf \(prof.bufferFrames) • \(latencyOriginLabelFr(prof.origin))")
                                    .font(.caption.weight(.semibold))
                                Text(String(format: "DSP algo : %.1f ms • Sortie pilote : %.1f ms • Total estimé : %.1f ms", prof.dspAlgorithmicMs, prof.outputDriverMs ?? 0, prof.totalEstimatedMs ?? 0))
                                    .font(.caption2.monospacedDigit())
                                Text(prof.protocolNotes)
                                    .font(.caption2)
                                    .foregroundStyle(.secondary)
                            }
                            .padding(.vertical, 4)
                        }
                    }
                }

                HStack(spacing: 12) {
                    Button("Purger le cache reconstructible (stems/PCM)") {
                        store.purgeReconstructibleCache()
                        exportedNotice = "Cache purgé. Vos fichiers originaux et paroles ont été conservés intacts."
                    }
                    .buttonStyle(.bordered)

                    Button("Exporter diagnostic expurgé (.json)") {
                        exportRedactedDiagnostics()
                    }
                    .buttonStyle(.borderedProminent)
                }

                if let note = exportedNotice {
                    Text(note).font(.caption).foregroundStyle(.green)
                }

                GroupBox("Journal local expurgé") {
                    if let logs = store.diagnostics?.recentLogs, !logs.isEmpty {
                        VStack(alignment: .leading, spacing: 4) {
                            ForEach(logs.suffix(20)) { entry in
                                Text("[\(entry.level.uppercased())] (\(entry.component)) \(entry.message)")
                                    .font(.caption.monospaced())
                                    .foregroundStyle(entry.level == "error" ? .red : .secondary)
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                    } else {
                        Text("Aucune entrée.").font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
            .padding(24)
        }
        .sheet(isPresented: $showImportModelSheet) {
            importModelSheet
        }
    }

    private var importModelSheet: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Importer un modèle ONNX vérifié")
                .font(.headline)
            Text("Seuls les fichiers .onnx avec empreinte SHA-256 (64 hex) et licences code + poids explicites sont acceptés (aucun pickle ni script).")
                .font(.caption)
                .foregroundStyle(.secondary)
            Form {
                HStack {
                    Text(importModelFileURL?.lastPathComponent ?? "Aucun fichier sélectionné")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Spacer()
                    Button("Choisir .onnx…") {
                        let p = NSOpenPanel()
                        p.allowedContentTypes = [UTType(filenameExtension: "onnx") ?? .data]
                        if p.runModal() == .OK { importModelFileURL = p.url }
                    }
                }
                TextField("Identifiant", text: $importModelId)
                TextField("Nom affiché", text: $importModelName)
                TextField("SHA-256 attendu (64 hex)", text: $importSha256)
                TextField("Licence du code", text: $importCodeLicense)
                TextField("Licence des poids (obligatoire)", text: $importWeightsLicense)
            }
            HStack {
                Spacer()
                Button("Annuler") { showImportModelSheet = false }
                Button("Vérifier et installer") {
                    if let url = importModelFileURL {
                        let dateFormatter = ISO8601DateFormatter()
                        dateFormatter.formatOptions = [.withFullDate]
                        let todayIso = dateFormatter.string(from: Date())
                        let trimmedSha = importSha256.trimmingCharacters(in: .whitespacesAndNewlines)
                        let manifest = ModelManifestDTO(
                            id: importModelId,
                            displayName: importModelName,
                            capability: "stem_separation",
                            version: "local-import",
                            officialSourceUrl: "local",
                            sha256: trimmedSha,
                            sizeBytes: 0,
                            author: "Import local utilisateur",
                            codeLicense: importCodeLicense,
                            weightsLicense: importWeightsLicense,
                            languages: ["multi"],
                            runtime: "onnx",
                            verifiedDate: todayIso,
                            localPath: nil,
                            state: "not_installed"
                        )
                        store.importVerifiedModel(fileURL: url, manifest: manifest)
                    }
                    showImportModelSheet = false
                }
                .buttonStyle(.borderedProminent)
                .disabled(
                    importModelFileURL == nil
                        || importSha256.trimmingCharacters(in: .whitespacesAndNewlines).count != 64
                        || importWeightsLicense.trimmingCharacters(in: .whitespaces).isEmpty
                )
            }
        }
        .padding(20)
        .frame(width: 480)
    }

    private func metricTile(_ title: String, _ value: String, alert: Bool = false) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title)
                .font(.caption2)
                .foregroundStyle(.secondary)
            Text(value)
                .font(.callout.monospacedDigit().weight(.semibold))
                .foregroundStyle(alert ? .red : .primary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(8)
        .background(Color.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
    }

    private func exportRedactedDiagnostics() {
        store.refreshDiagnostics()
        guard let diag = store.diagnostics,
              let data = try? JSONEncoder().encode(diag) else { return }
        let panel = NSSavePanel()
        panel.nameFieldStringValue = "karaok-party-diagnostics-expurge.json"
        if panel.runModal() == .OK, let url = panel.url {
            do {
                try data.write(to: url, options: .atomic)
                exportedNotice = "Diagnostic expurgé enregistré : \(url.lastPathComponent)"
            } catch {
                exportedNotice = "Erreur export : \(error.localizedDescription)"
            }
        }
    }
}
