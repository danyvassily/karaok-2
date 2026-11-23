import AppKit
import SwiftUI
import UniformTypeIdentifiers

/// Lyrics import (with explicit non-UTF-8 encoding preview - §9.1/I10), mini-timeline, manual cue placement,
/// end-time adjustment, instrumental marking, offset & revision validation (§3.3, AC-09, B7, I11, I13).
struct LyricsReviewView: View {
    @EnvironmentObject var store: AppStore
    @EnvironmentObject var telemetry: LiveTelemetryStore
    @State private var rawEditorText: String = ""
    @State private var selectedLanguage: String = "fr"
    @State private var loadedTrackIdForEditor: String? = nil

    // Non-UTF-8 encoding preview confirmation sheet (§9.1, I10)
    @State private var pendingConvertedText: String? = nil
    @State private var pendingEncodingName: String = ""
    @State private var pendingEncodingTrackId: String = ""

    private let supportedLanguages = [
        ("fr", "Français"), ("en", "Anglais"), ("ja", "Japonais"),
        ("ar", "Arabe"), ("es", "Espagnol"), ("de", "Allemand"), ("", "Autre / non précisé")
    ]

    var body: some View {
        Group {
            if let detail = store.selectedTrackDetail {
                HSplitView {
                    leftTextImportPane(detail)
                        .frame(minWidth: 320, idealWidth: 380)
                    rightTimelineAndCuesPane(detail)
                        .frame(minWidth: 460)
                }
                .onAppear { syncEditorFromDetail(detail) }
                .onChange(of: detail.track.id) { _, _ in syncEditorFromDetail(detail) }
                .sheet(isPresented: Binding(
                    get: { pendingConvertedText != nil },
                    set: { if !$0 { pendingConvertedText = nil } }
                )) {
                    encodingPreviewSheet
                }
            } else {
                VStack(spacing: 10) {
                    Image(systemName: "text.alignleft")
                        .font(.system(size: 38))
                        .foregroundStyle(.secondary)
                    Text("Sélectionnez d'abord un morceau dans la Bibliothèque")
                        .font(.title3)
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
    }

    private func syncEditorFromDetail(_ detail: TrackDetailDTO) {
        if loadedTrackIdForEditor != detail.track.id {
            rawEditorText = detail.lyrics?.rawText ?? ""
            selectedLanguage = detail.lyrics?.language ?? "fr"
            loadedTrackIdForEditor = detail.track.id
        }
    }

    private func leftTextImportPane(_ detail: TrackDetailDTO) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Texte & import LRC")
                    .font(.headline)
                Spacer()
                Button {
                    importLrcFile(for: detail.track.id)
                } label: {
                    Label("Importer .lrc / .txt", systemImage: "doc.text")
                }
                .buttonStyle(.bordered)
            }

            HStack {
                Text("Langue :")
                    .font(.caption)
                Picker("Langue des paroles", selection: $selectedLanguage) {
                    ForEach(supportedLanguages, id: \.0) { code, label in
                        Text(label).tag(code)
                    }
                }
                .labelsHidden()
                .frame(maxWidth: 180)
            }

            Text("Collez les paroles brutes UTF-8 ou un fichier LRC minuté. Une nouvelle révision invalide uniquement l'alignement dépendant, jamais les stems audio.")
                .font(.caption)
                .foregroundStyle(.secondary)

            TextEditor(text: $rawEditorText)
                .font(.body.monospaced())
                .border(Color.secondary.opacity(0.3))
                .accessibilityLabel("Éditeur de paroles UTF-8 ou LRC")

            HStack {
                Button("Enregistrer & analyser les lignes") {
                    store.setLyricsText(
                        trackId: detail.track.id,
                        rawText: rawEditorText,
                        provenance: "user_editor",
                        language: selectedLanguage.isEmpty ? nil : selectedLanguage
                    )
                }
                .buttonStyle(.borderedProminent)
                .disabled(rawEditorText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)

                Spacer()
            }
        }
        .padding(16)
    }

    private func rightTimelineAndCuesPane(_ detail: TrackDetailDTO) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Vérification & mini-timeline : \(detail.track.title)")
                        .font(.title3.bold())
                    HStack(spacing: 12) {
                        markerLabel("1re voix", detail.vocalActivity?.firstVocalActivityS)
                        markerLabel("1er chant", detail.vocalActivity?.firstSungActivityS)
                        markerLabel("1re phrase", detail.lyrics?.firstLyricCueS)
                    }
                    .font(.caption)
                }
                Spacer()
                if detail.lyrics != nil {
                    Button {
                        store.validateLyrics(trackId: detail.track.id)
                    } label: {
                        Label(
                            detail.lyrics?.status == "user_validated" ? "Validé" : "Valider cette révision",
                            systemImage: "checkmark.seal.fill"
                        )
                    }
                    .buttonStyle(.borderedProminent)
                    .tint(detail.lyrics?.status == "user_validated" ? .green : .accentColor)
                }
            }

            miniTimelineBar(detail)

            if let doc = detail.lyrics {
                HStack(spacing: 10) {
                    Text("Décalage global :")
                        .font(.callout.weight(.medium))
                    Stepper(
                        value: Binding(
                            get: { Int(doc.globalOffsetMs) },
                            set: { store.setLyricsOffset(trackId: detail.track.id, offsetMs: Int64($0)) }
                        ),
                        in: -15000...15000,
                        step: 100
                    ) {
                        Text("\(doc.globalOffsetMs >= 0 ? "+" : "")\(doc.globalOffsetMs) ms")
                            .font(.callout.monospacedDigit())
                    }
                    .accessibilityLabel("Décalage global des paroles en millisecondes")
                    Text("(positif = afficher plus tard)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Spacer()
                }

                if !doc.warnings.isEmpty {
                    VStack(alignment: .leading, spacing: 4) {
                        ForEach(doc.warnings, id: \.self) { w in
                            Label(w, systemImage: "info.circle")
                                .font(.caption)
                                .foregroundStyle(.orange)
                        }
                    }
                    .padding(8)
                    .background(Color.orange.opacity(0.10), in: RoundedRectangle(cornerRadius: 8))
                }

                let isThisTrackLoaded = (store.loadedTrackId == detail.track.id)
                let offsetS = Double(doc.globalOffsetMs) / 1000.0
                List(doc.cues) { cue in
                    HStack(spacing: 8) {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(cue.isInstrumentalMarker ? "[Passage instrumental]" : cue.text)
                                .font(.body.weight(cue.humanValidated ? .semibold : .regular))
                                .foregroundStyle(cue.isInstrumentalMarker ? .secondary : .primary)
                            HStack(spacing: 8) {
                                Text(cueTimingRange(cue))
                                    .font(.caption.monospacedDigit())
                                    .foregroundStyle(.secondary)
                                Text(timingSourceBadge(cue.timingSource))
                                    .font(.caption2)
                                    .padding(.horizontal, 5)
                                    .padding(.vertical, 1)
                                    .background(Color.secondary.opacity(0.15), in: Capsule())
                            }
                        }
                        Spacer()

                        if let start = cue.startS {
                            Button("Écouter") {
                                // Respect globalOffsetMs and ensure playback resumes even when paused (B7, P2-I5)
                                let seekTarget = max(0, start + offsetS - 0.5)
                                if !isThisTrackLoaded {
                                    store.loadAndPlay(trackId: detail.track.id, openSingTab: false, startAt: seekTarget)
                                } else {
                                    store.seekAndPlay(to: seekTarget)
                                }
                            }
                            .buttonStyle(.bordered)
                            .controlSize(.small)

                            // End-time adjustment (+/- 0.5s - I13)
                            let currentEnd = cue.endS ?? (start + 4.0)
                            Button("-0.5s fin") {
                                let newEnd = max(start + 0.3, currentEnd - 0.5)
                                store.placeLyricCue(trackId: detail.track.id, cueId: cue.id, startS: start, endS: newEnd)
                            }
                            .buttonStyle(.bordered)
                            .controlSize(.mini)
                            .accessibilityLabel("Raccourcir la fin de la ligne d'une demi-seconde")

                            Button("+0.5s fin") {
                                let newEnd = min(detail.track.durationS, currentEnd + 0.5)
                                store.placeLyricCue(trackId: detail.track.id, cueId: cue.id, startS: start, endS: newEnd)
                            }
                            .buttonStyle(.bordered)
                            .controlSize(.mini)
                            .accessibilityLabel("Allonger la fin de la ligne d'une demi-seconde")
                        }

                        Button(isThisTrackLoaded ? "Placer ici (\(formatTimeMMSS(telemetry.transport.sourcePositionS)))" : "Placer ici") {
                            // Compensate globalOffsetMs so effective rendered cue time lands exactly at the current playhead (P2-I5)
                            let rawCueStart = max(0, telemetry.transport.sourcePositionS - offsetS)
                            store.placeLyricCue(
                                trackId: detail.track.id,
                                cueId: cue.id,
                                startS: rawCueStart
                            )
                        }
                        .buttonStyle(.bordered)
                        .controlSize(.small)
                        .disabled(!isThisTrackLoaded)
                        .help(isThisTrackLoaded
                              ? "Annotation humaine explicite : marque cette ligne à la position courante (compensée du décalage global)"
                              : "Lancez la lecture de ce morceau (ex. via la mini-timeline ou Écouter) avant d'utiliser Placer ici")

                        Button(cue.isInstrumentalMarker ? "Parole" : "Inst.") {
                            store.markCueInstrumental(
                                trackId: detail.track.id,
                                cueId: cue.id,
                                isInstrumental: !cue.isInstrumentalMarker
                            )
                        }
                        .buttonStyle(.bordered)
                        .controlSize(.mini)
                        .accessibilityLabel(cue.isInstrumentalMarker ? "Reclasser comme phrase chantée" : "Marquer comme passage instrumental")
                    }
                    .padding(.vertical, 3)
                }
            } else {
                Text("Aucune parole enregistrée pour ce morceau. Collez un texte ou importez un fichier .lrc à gauche.")
                    .foregroundStyle(.secondary)
                    .padding(.top, 16)
                Spacer()
            }
        }
        .padding(16)
    }

    private func miniTimelineBar(_ detail: TrackDetailDTO) -> some View {
        let dur = max(detail.track.durationS, 1.0)
        let isThisTrackLoaded = (store.loadedTrackId == detail.track.id)
        let localPlayheadS = isThisTrackLoaded ? telemetry.transport.sourcePositionS : 0.0
        let offsetS = Double(detail.lyrics?.globalOffsetMs ?? 0) / 1000.0
        return VStack(alignment: .leading, spacing: 4) {
            GeometryReader { geo in
                ZStack(alignment: .leading) {
                    RoundedRectangle(cornerRadius: 6)
                        .fill(Color.secondary.opacity(0.15))

                    ForEach(detail.vocalActivity?.regions ?? []) { reg in
                        let x = CGFloat(reg.startS / dur) * geo.size.width
                        let w = max(2, CGFloat((reg.endS - reg.startS) / dur) * geo.size.width)
                        RoundedRectangle(cornerRadius: 3)
                            .fill(reg.kind == "singing" ? Color.indigo.opacity(0.45) : Color.orange.opacity(0.4))
                            .frame(width: w, height: 22)
                            .offset(x: x)
                    }

                    ForEach(detail.lyrics?.cues ?? []) { cue in
                        if let s = cue.startS {
                            let effectiveS = min(max(0, s + offsetS), dur)
                            Rectangle()
                                .fill(cue.humanValidated ? Color.green : Color.primary.opacity(0.6))
                                .frame(width: 2, height: 30)
                                .offset(x: CGFloat(effectiveS / dur) * geo.size.width)
                        }
                    }

                    if isThisTrackLoaded {
                        let playX = CGFloat(min(localPlayheadS, dur) / dur) * geo.size.width
                        Rectangle()
                            .fill(Color.red)
                            .frame(width: 2, height: 34)
                            .offset(x: playX)
                    }
                }
                .contentShape(Rectangle())
                .onTapGesture { loc in
                    let target = Double(loc.x / max(geo.size.width, 1)) * dur
                    if !isThisTrackLoaded {
                        store.loadAndPlay(trackId: detail.track.id, openSingTab: false, startAt: target)
                    } else {
                        store.seek(to: target)
                    }
                }
            }
            .frame(height: 34)
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("Mini-timeline vocale et paroles")
            .accessibilityValue(isThisTrackLoaded
                                ? "Position \(formatTimeMMSS(localPlayheadS)) sur \(formatTimeMMSS(dur))"
                                : "Morceau non chargé en lecture (durée \(formatTimeMMSS(dur)))")
            .accessibilityHint("Cliquez pour charger ou déplacer la tête de lecture sur un repère")

            Text("Indigo : chant détecté • Orange : incertain/parole • Vert : repère validé • Rouge : tête de lecture (cliquer pour se déplacer)")
                .font(.caption2)
                .foregroundStyle(.secondary)
        }
    }

    private var encodingPreviewSheet: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Encodage non UTF-8 détecté (\(pendingEncodingName))")
                .font(.headline)
            Text("Vérifiez l'aperçu des caractères accentués ci-dessous avant d'importer ce fichier (§9.1) :")
                .font(.callout)
                .foregroundStyle(.secondary)
            ScrollView {
                Text(pendingConvertedText ?? "")
                    .font(.caption.monospaced())
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(8)
            }
            .frame(height: 220)
            .border(Color.secondary.opacity(0.3))
            HStack {
                Spacer()
                Button("Annuler") { pendingConvertedText = nil }
                Button("Confirmer l'import") {
                    if let txt = pendingConvertedText {
                        rawEditorText = txt
                        store.setLyricsText(
                            trackId: pendingEncodingTrackId,
                            rawText: txt,
                            provenance: "lrc_file_converted_\(pendingEncodingName)",
                            language: selectedLanguage.isEmpty ? nil : selectedLanguage
                        )
                    }
                    pendingConvertedText = nil
                }
                .buttonStyle(.borderedProminent)
            }
        }
        .padding(20)
        .frame(width: 500)
    }

    private func markerLabel(_ name: String, _ val: Double?) -> some View {
        Text("\(name) : \(val.map { String(format: "%.1fs", $0) } ?? "inconnu")")
            .foregroundStyle(.secondary)
    }

    private func cueTimingRange(_ cue: LyricCueDTO) -> String {
        switch (cue.startS, cue.endS) {
        case (let s?, let e?): return "\(formatTimeMMSS(s)) → \(formatTimeMMSS(e))"
        case (let s?, nil): return "\(formatTimeMMSS(s)) → ?"
        default: return "Non minutée"
        }
    }

    private func timingSourceBadge(_ src: String) -> String {
        switch src {
        case "manual": return "Manuel"
        case "lrc_explicit": return "LRC"
        case "lrc_inferred_end": return "LRC (fin inférée)"
        case "aligned_line_auto": return "Alignement auto (ligne)"
        default: return "Non minutée"
        }
    }

    private func importLrcFile(for trackId: String) {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [UTType.plainText, UTType(filenameExtension: "lrc") ?? .plainText]
        panel.allowsMultipleSelection = false
        guard panel.runModal() == .OK, let url = panel.url else { return }

        guard let data = try? Data(contentsOf: url) else {
            store.bannerError = KaraokErrorDTO(
                code: "io_error",
                messageFr: "Impossible de lire le fichier de paroles sélectionné.",
                actionableHintFr: nil
            )
            return
        }
        if let utf8 = String(data: data, encoding: .utf8) {
            rawEditorText = utf8
            store.setLyricsText(
                trackId: trackId,
                rawText: utf8,
                provenance: "lrc_file_import",
                language: selectedLanguage.isEmpty ? nil : selectedLanguage
            )
        } else if let cp1252 = String(data: data, encoding: .windowsCP1252) {
            // Explicit preview before accepting non-UTF-8 encoding (§9.1, I10)
            pendingEncodingName = "Windows-1252 / Latin-1"
            pendingEncodingTrackId = trackId
            pendingConvertedText = cp1252
        } else {
            store.bannerError = KaraokErrorDTO(
                code: "format_unsupported",
                messageFr: "Encodage de fichier de paroles non pris en charge (UTF-8 requis).",
                actionableHintFr: "Réenregistrez le fichier .lrc/.txt en UTF-8 avant de l'importer."
            )
        }
    }
}
