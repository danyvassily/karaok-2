import SwiftUI

/// Lyrics-dominant karaoke stage driven strictly by the Rust master output clock (§3.4, §3.5, AC-10, AC-11, AC-20).
/// Adapts between 5 and 4 visible logical blocks according to window height & accessibility text zoom (100%–200%),
/// scrollable when enlarged so long/200% lines never clip off-screen (I11).
struct SingView: View {
    @EnvironmentObject var store: AppStore
    @EnvironmentObject var telemetry: LiveTelemetryStore
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        HStack(spacing: 0) {
            VStack(spacing: 0) {
                stageHeader
                lyricsStageBody
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                stageMixerBar
            }
            .background(
                LinearGradient(
                    colors: [Color(red: 0.08, green: 0.09, blue: 0.14), Color(red: 0.05, green: 0.06, blue: 0.10)],
                    startPoint: .top,
                    endPoint: .bottom
                )
            )
            .foregroundStyle(.white)

            if store.showEffectsPanel && !store.isFullScreenLyrics {
                Divider()
                VoiceEffectsPanel()
                    .frame(width: 330)
            }
        }
    }

    private var stageHeader: some View {
        let stageDetail = store.activeStageTrackDetail
        return HStack(spacing: 12) {
            VStack(alignment: .leading, spacing: 2) {
                Text(stageDetail?.track.title ?? "Prêt à chanter")
                    .font(.title3.bold())
                if let artist = stageDetail?.track.artist {
                    Text(artist)
                        .font(.callout)
                        .foregroundStyle(.white.opacity(0.75))
                }
            }
            Spacer()

            HStack(spacing: 6) {
                Image(systemName: "textformat.size")
                    .foregroundStyle(.white.opacity(0.7))
                    .accessibilityHidden(true)
                Slider(value: $store.lyricsScale, in: 1.0...2.0, step: 0.1)
                    .frame(width: 90)
                    .accessibilityLabel("Taille des paroles")
                    .accessibilityValue("\(Int(store.lyricsScale * 100)) pour cent")
                Text("\(Int(store.lyricsScale * 100)) %")
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.white.opacity(0.75))
            }

            Button {
                if store.audioState?.micCaptureActive == true {
                    store.stopMicCapture()
                } else {
                    Task { await store.startMicCaptureWithPermission() }
                }
            } label: {
                Label(
                    store.audioState?.micCaptureActive == true ? "Micro actif" : "Activer micro",
                    systemImage: store.audioState?.micCaptureActive == true ? "mic.fill" : "mic"
                )
            }
            .buttonStyle(.borderedProminent)
            .tint(store.audioState?.micCaptureActive == true ? .green : .indigo)

            if !store.isFullScreenLyrics {
                Button {
                    store.showEffectsPanel.toggle()
                } label: {
                    Label("Effets", systemImage: "slider.vertical.3")
                }
                .buttonStyle(.bordered)
            }

            Button {
                store.isFullScreenLyrics.toggle()
            } label: {
                Image(systemName: store.isFullScreenLyrics ? "arrow.down.right.and.arrow.up.left" : "arrow.up.left.and.arrow.down.right")
            }
            .buttonStyle(.bordered)
            .accessibilityLabel(store.isFullScreenLyrics ? "Quitter le plein écran" : "Plein écran")
        }
        .padding(.horizontal, 24)
        .padding(.vertical, 14)
    }

    private var lyricsStageBody: some View {
        let stageDetail = store.activeStageTrackDetail
        let stagePositionS = (store.loadedTrackId != nil && store.loadedTrackId == stageDetail?.track.id)
            ? telemetry.transport.sourcePositionS
            : 0.0
        return GeometryReader { geo in
            let maxBlocks = (geo.size.height < 520 || store.lyricsScale >= 1.4) ? 4 : 5
            let computed = computeVisibleLyrics(
                doc: stageDetail?.lyrics,
                sourcePositionS: stagePositionS,
                maxBlocks: maxBlocks
            )

            ScrollView(.vertical, showsIndicators: false) {
                VStack(alignment: .leading, spacing: 18) {
                    // Explicit uncertainty/route-loss notice (§6.2, I11)
                    if telemetry.transport.isRouteLost {
                        Label("Sortie audio perdue — lecture suspendue (choisissez une sortie dans Son)", systemImage: "exclamationmark.triangle.fill")
                            .font(.callout.weight(.semibold))
                            .padding(.horizontal, 12)
                            .padding(.vertical, 6)
                            .background(Color.orange.opacity(0.28), in: Capsule())
                    } else if telemetry.transport.isPlaying && telemetry.transport.presentationQuality == 2 {
                        Label("Position temporelle incertaine", systemImage: "clock.badge.exclamationmark")
                            .font(.caption.weight(.medium))
                            .padding(.horizontal, 10)
                            .padding(.vertical, 4)
                            .background(Color.orange.opacity(0.22), in: Capsule())
                    }

                    if let phaseText = computed.phaseBanner {
                        HStack(spacing: 10) {
                            Image(systemName: computed.phaseIcon)
                                .accessibilityHidden(true)
                            Text(phaseText)
                                .font(.headline)
                            if let countdown = computed.introCountdownS {
                                Text(String(format: "Départ dans %.0f…", ceil(countdown)))
                                    .font(.headline.monospacedDigit())
                                    .padding(.horizontal, 10)
                                    .padding(.vertical, 4)
                                    .background(Color.indigo, in: Capsule())
                            }
                        }
                        .padding(.horizontal, 14)
                        .padding(.vertical, 8)
                        .background(Color.white.opacity(0.12), in: Capsule())
                    }

                    if computed.blocks.isEmpty {
                        VStack(alignment: .leading, spacing: 10) {
                            Text("Aucune parole chargée pour ce morceau")
                                .font(.title2.bold())
                            Text("Ouvrez l'onglet « Vérifier les paroles » pour coller les paroles ou importer un fichier LRC. La musique reste jouable immédiatement.")
                                .font(.body)
                                .foregroundStyle(.white.opacity(0.75))
                        }
                    } else {
                        ForEach(computed.blocks) { block in
                            HStack(alignment: .firstTextBaseline, spacing: 14) {
                                RoundedRectangle(cornerRadius: 3)
                                    .fill(block.isActive ? Color.indigo : Color.clear)
                                    .frame(width: 6, height: block.isActive ? 28 : 16)
                                    .overlay {
                                        if block.isActive {
                                            RoundedRectangle(cornerRadius: 3)
                                                .stroke(Color.white, lineWidth: 1.5)
                                        }
                                    }

                                Text(block.text)
                                    .font(.system(
                                        size: (block.isActive ? 30 : 21) * store.lyricsScale,
                                        weight: block.isActive ? .bold : .medium,
                                        design: .rounded
                                    ))
                                    .foregroundStyle(
                                        block.isActive
                                            ? Color.white
                                            : (block.isPast ? Color.white.opacity(0.45) : Color.white.opacity(0.84))
                                    )
                                    .fixedSize(horizontal: false, vertical: true)
                                    .accessibilityAddTraits(block.isActive ? .isSelected : [])
                            }
                            .padding(.vertical, block.isActive ? 6 : 2)
                            .animation(reduceMotion ? nil : .easeInOut(duration: 0.18), value: block.isActive)
                        }
                    }
                }
                .frame(maxWidth: 820, alignment: .leading)
                .padding(.horizontal, 36)
                .padding(.vertical, 24)
                .frame(maxWidth: .infinity, minHeight: geo.size.height, alignment: .center)
            }
        }
    }

    private var stageMixerBar: some View {
        VStack(spacing: 10) {
            Divider().overlay(Color.white.opacity(0.18))
            HStack(spacing: 24) {
                mixerSlider(
                    title: "Accompagnement",
                    value: Binding(
                        get: { Double(store.mixParams.accompanimentGain) },
                        set: { v in
                            var m = store.mixParams
                            m.accompanimentGain = Float(v)
                            store.updateMixParams(m)
                        }
                    ),
                    range: 0...1.5,
                    enabled: true
                )

                mixerSlider(
                    title: telemetry.transport.stemsAvailable ? "Voix originale (stem)" : "Voix originale (préparer stems)",
                    value: Binding(
                        get: { Double(store.mixParams.originalVocalGain) },
                        set: { v in
                            var m = store.mixParams
                            m.originalVocalGain = Float(v)
                            store.updateMixParams(m)
                        }
                    ),
                    range: 0...1.5,
                    enabled: telemetry.transport.stemsAvailable && store.mixParams.useStems
                )

                mixerSlider(
                    title: store.voiceParams.micMuted ? "Mon micro (MUET)" : "Mon micro",
                    value: Binding(
                        get: { Double(store.voiceParams.micLevel) },
                        set: { v in
                            var p = store.voiceParams
                            p.micLevel = Float(v)
                            store.updateVoiceParams(p)
                        }
                    ),
                    range: 0...1.5,
                    enabled: !store.voiceParams.micMuted
                )

                Toggle("Mode stems", isOn: Binding(
                    get: { store.mixParams.useStems },
                    set: { u in
                        var m = store.mixParams
                        m.useStems = u
                        store.updateMixParams(m)
                    }
                ))
                .disabled(!telemetry.transport.stemsAvailable)
                .toggleStyle(.switch)
                .controlSize(.small)
            }
            .padding(.horizontal, 24)
            .padding(.bottom, 14)
        }
    }

    private func mixerSlider(title: String, value: Binding<Double>, range: ClosedRange<Double>, enabled: Bool) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text(title).font(.caption.weight(.medium))
                Spacer()
                Text("\(Int(value.wrappedValue * 100)) %")
                    .font(.caption2.monospacedDigit())
                    .foregroundStyle(.white.opacity(0.75))
            }
            Slider(value: value, in: range)
                .disabled(!enabled)
                .accessibilityLabel(title)
                .accessibilityValue("\(Int(value.wrappedValue * 100)) pour cent")
        }
        .frame(maxWidth: 210)
    }

    private struct ComputedViewport {
        let phaseBanner: String?
        let phaseIcon: String
        let introCountdownS: Double?
        let blocks: [DisplayBlock]
    }

    private struct DisplayBlock: Identifiable {
        let id: String
        let text: String
        let isActive: Bool
        let isPast: Bool
    }

    private func computeVisibleLyrics(
        doc: LyricsDocumentDTO?,
        sourcePositionS: Double,
        maxBlocks: Int
    ) -> ComputedViewport {
        guard let doc else {
            return ComputedViewport(phaseBanner: nil, phaseIcon: "music.note", introCountdownS: nil, blocks: [])
        }
        let offsetS = Double(doc.globalOffsetMs) / 1000.0
        let lyricCues = doc.cues.filter { !$0.isInstrumentalMarker && !$0.text.trimmingCharacters(in: .whitespaces).isEmpty }
        // Sort chronologically so out-of-order manual placements stay consistent with Rust evaluate_viewport (I10)
        let timed = lyricCues
            .filter { $0.startS != nil }
            .sorted { ($0.startS ?? 0) < ($1.startS ?? 0) }

        if timed.isEmpty {
            let preview = lyricCues.prefix(maxBlocks).map {
                DisplayBlock(id: $0.id, text: $0.text, isActive: false, isPast: false)
            }
            return ComputedViewport(
                phaseBanner: "Paroles non minutées (défilement manuel)",
                phaseIcon: "text.alignleft",
                introCountdownS: nil,
                blocks: Array(preview)
            )
        }

        let firstStart = (timed[0].startS ?? 0) + offsetS
        var activePos: Int? = nil
        var firstUpcomingPos: Int? = nil

        for (i, cue) in timed.enumerated() {
            let s = (cue.startS ?? 0) + offsetS
            let e = (cue.endS.map { $0 + offsetS }) ?? (s + 4.0)
            if sourcePositionS >= s && sourcePositionS < e {
                if activePos == nil { activePos = i }
            } else if sourcePositionS < s && firstUpcomingPos == nil {
                firstUpcomingPos = i
            }
        }

        let phaseBanner: String?
        let phaseIcon: String
        var countdown: Double? = nil

        if activePos != nil {
            phaseBanner = nil
            phaseIcon = "music.mic"
        } else if sourcePositionS < firstStart {
            phaseBanner = "Introduction"
            phaseIcon = "hourglass"
            if doc.status == "user_validated" || timed[0].humanValidated {
                let rem = firstStart - sourcePositionS
                if rem > 0 && rem <= 5.0 { countdown = rem }
            }
        } else if firstUpcomingPos != nil {
            phaseBanner = "Passage instrumental"
            phaseIcon = "guitars"
        } else {
            phaseBanner = "Fin du chant"
            phaseIcon = "flag.checkered"
        }

        let anchor = activePos ?? firstUpcomingPos ?? max(0, timed.count - 1)
        let rawStart = max(0, anchor - 1)
        let end = min(timed.count, rawStart + maxBlocks)
        let start = max(0, end - maxBlocks)

        let blocks = timed[start..<end].map { cue -> DisplayBlock in
            let s = (cue.startS ?? 0) + offsetS
            let e = (cue.endS.map { $0 + offsetS }) ?? (s + 4.0)
            let isActive = sourcePositionS >= s && sourcePositionS < e
            let isPast = sourcePositionS >= e
            return DisplayBlock(id: cue.id, text: cue.text, isActive: isActive, isPast: isPast)
        }

        return ComputedViewport(
            phaseBanner: phaseBanner,
            phaseIcon: phaseIcon,
            introCountdownS: countdown,
            blocks: blocks
        )
    }
}
