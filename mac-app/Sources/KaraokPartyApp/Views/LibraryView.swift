import AppKit
import SwiftUI
import UniformTypeIdentifiers

private struct LibraryRowItem: Identifiable, Hashable {
    let id: String // Unique per playlist entry (supports intentional duplicate tracks in a playlist §9.2, I13!)
    let playlistItemId: String?
    let track: TrackDTO
}

struct LibraryView: View {
    @EnvironmentObject var store: AppStore
    @State private var newPlaylistName: String = ""
    @State private var showNewPlaylistPopover = false
    @State private var isDropTargeted = false

    @State private var editTitle: String = ""
    @State private var editArtist: String = ""
    @State private var editAlbum: String = ""
    @State private var editTonic: String = "c"
    @State private var editScale: String = "chromatic"
    @State private var showEditSheet = false

    private var displayedRows: [LibraryRowItem] {
        if store.selectedPlaylistId != nil {
            let map = Dictionary(uniqueKeysWithValues: store.tracks.map { ($0.id, $0) })
            return store.playlistItems.compactMap { item in
                guard let tr = map[item.trackId] else { return nil }
                return LibraryRowItem(id: item.id, playlistItemId: item.id, track: tr)
            }
        }
        return store.tracks.map { LibraryRowItem(id: $0.id, playlistItemId: nil, track: $0) }
    }

    var body: some View {
        HSplitView {
            leftColumn
                .frame(minWidth: 340, idealWidth: 420)
            rightInspector
                .frame(minWidth: 360)
        }
        .onDrop(of: [.fileURL], isTargeted: $isDropTargeted) { providers in
            handleFileDrop(providers)
        }
        .overlay {
            if isDropTargeted {
                RoundedRectangle(cornerRadius: 12)
                    .strokeBorder(Color.accentColor, style: StrokeStyle(lineWidth: 3, dash: [8]))
                    .background(Color.accentColor.opacity(0.08))
                    .padding(12)
                    .allowsHitTesting(false)
            }
        }
        .sheet(isPresented: $showEditSheet) {
            metadataEditSheet
        }
    }

    private var leftColumn: some View {
        VStack(spacing: 0) {
            VStack(spacing: 10) {
                HStack {
                    Text("Bibliothèque locale")
                        .font(.title2.bold())
                    if store.isImporting {
                        ProgressView().controlSize(.small)
                    }
                    Spacer()
                    Button {
                        openAudioImportPanel()
                    } label: {
                        Label("Importer un morceau", systemImage: "plus.circle.fill")
                    }
                    .buttonStyle(.borderedProminent)
                    .disabled(store.isImporting)
                }

                HStack(spacing: 8) {
                    TextField("Rechercher titre, artiste, album…", text: $store.searchQuery)
                        .textFieldStyle(.roundedBorder)
                        .onChange(of: store.searchQuery) { _, _ in store.refreshTracks() }

                    Toggle(isOn: $store.favoritesOnly) {
                        Image(systemName: store.favoritesOnly ? "star.fill" : "star")
                    }
                    .toggleStyle(.button)
                    .help("Afficher uniquement les favoris")
                    .accessibilityLabel("Filtrer uniquement les favoris")
                    .onChange(of: store.favoritesOnly) { _, _ in store.refreshTracks() }
                }

                // Playlists selector
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 6) {
                        Button("Tous (\(store.tracks.count))") {
                            store.selectedPlaylistId = nil
                        }
                        .buttonStyle(.bordered)
                        .tint(store.selectedPlaylistId == nil ? .accentColor : .secondary)

                        ForEach(store.playlists) { pl in
                            Button(pl.name) {
                                store.selectedPlaylistId = pl.id
                                store.loadPlaylistItems(pl.id)
                            }
                            .buttonStyle(.bordered)
                            .tint(store.selectedPlaylistId == pl.id ? .accentColor : .secondary)
                        }

                        Button {
                            showNewPlaylistPopover = true
                        } label: {
                            Image(systemName: "folder.badge.plus")
                        }
                        .buttonStyle(.bordered)
                        .accessibilityLabel("Créer une nouvelle playlist")
                        .popover(isPresented: $showNewPlaylistPopover) {
                            VStack(alignment: .leading, spacing: 10) {
                                Text("Nouvelle playlist")
                                    .font(.headline)
                                TextField("Nom de la playlist", text: $newPlaylistName)
                                    .textFieldStyle(.roundedBorder)
                                    .frame(width: 220)
                                HStack {
                                    Spacer()
                                    Button("Créer") {
                                        let trimmed = newPlaylistName.trimmingCharacters(in: .whitespaces)
                                        if !trimmed.isEmpty {
                                            store.createPlaylist(name: trimmed)
                                            newPlaylistName = ""
                                            showNewPlaylistPopover = false
                                        }
                                    }
                                    .buttonStyle(.borderedProminent)
                                }
                            }
                            .padding()
                        }
                    }
                }
            }
            .padding()

            Divider()

            if displayedRows.isEmpty {
                emptyLibraryView
            } else {
                List {
                    ForEach(Array(displayedRows.enumerated()), id: \.element.id) { idx, row in
                        let track = row.track
                        HStack(spacing: 10) {
                            Button {
                                store.toggleFavorite(track)
                            } label: {
                                Image(systemName: track.isFavorite ? "star.fill" : "star")
                                    .foregroundStyle(track.isFavorite ? .yellow : .secondary)
                            }
                            .buttonStyle(.plain)
                            .accessibilityLabel(track.isFavorite ? "Retirer des favoris" : "Ajouter aux favoris")

                            VStack(alignment: .leading, spacing: 2) {
                                Text(track.title)
                                    .font(.headline)
                                    .lineLimit(1)
                                Text("\(track.artist) • \(track.format.uppercased()) • \(formatTimeMMSS(track.durationS))")
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                            .contentShape(Rectangle())
                            .onTapGesture { store.selectTrack(track.id) }

                            Spacer()

                            // Playlist reorder & remove item controls (REQ-007, I13, P2-I6)
                            if let itemId = row.playlistItemId {
                                let isFiltered = !store.searchQuery.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || store.favoritesOnly
                                HStack(spacing: 4) {
                                    Button {
                                        store.movePlaylistItem(itemId: itemId, direction: -1)
                                    } label: {
                                        Image(systemName: "chevron.up")
                                    }
                                    .buttonStyle(.borderless)
                                    .disabled(isFiltered || idx == 0)
                                    .help(isFiltered ? "Effacez les filtres pour réordonner la playlist" : "Monter dans la playlist")
                                    .accessibilityLabel("Monter dans la playlist")

                                    Button {
                                        store.movePlaylistItem(itemId: itemId, direction: 1)
                                    } label: {
                                        Image(systemName: "chevron.down")
                                    }
                                    .buttonStyle(.borderless)
                                    .disabled(isFiltered || idx + 1 >= displayedRows.count)
                                    .help(isFiltered ? "Effacez les filtres pour réordonner la playlist" : "Descendre dans la playlist")
                                    .accessibilityLabel("Descendre dans la playlist")

                                    Button {
                                        store.removePlaylistItem(itemId)
                                    } label: {
                                        Image(systemName: "minus.circle")
                                    }
                                    .buttonStyle(.borderless)
                                    .accessibilityLabel("Retirer de la playlist (conserve le morceau)")
                                }
                            }

                            Menu {
                                Button("Écouter l'original (sans stems)") {
                                    store.loadAndPlay(trackId: track.id, openSingTab: false, useStems: false)
                                }
                                Button("Ouvrir dans Chanter") {
                                    store.loadAndPlay(trackId: track.id, openSingTab: true, useStems: true)
                                }
                                if !store.playlists.isEmpty {
                                    Menu("Ajouter à une playlist") {
                                        ForEach(store.playlists) { pl in
                                            Button(pl.name) {
                                                store.addTrack(track.id, toPlaylist: pl.id)
                                            }
                                        }
                                    }
                                }
                                Divider()
                                Button("Retirer de la bibliothèque (fichier externe intact)", role: .destructive) {
                                    store.deleteTrack(track.id, deleteManagedCopy: false)
                                }
                            } label: {
                                Image(systemName: "ellipsis.circle")
                            }
                            .menuStyle(.borderlessButton)
                            .frame(width: 24)
                            .accessibilityLabel("Actions pour \(track.title)")
                        }
                        .padding(.vertical, 4)
                        .listRowBackground(store.selectedTrackId == track.id ? Color.accentColor.opacity(0.14) : Color.clear)
                    }
                }
            }
        }
    }

    private var emptyLibraryView: some View {
        VStack(spacing: 16) {
            Spacer()
            Image(systemName: "music.note.house")
                .font(.system(size: 44))
                .foregroundStyle(.secondary)
                .accessibilityHidden(true)
            Text("Votre bibliothèque Karaok Party est vide")
                .font(.title3.bold())
            Text("Importez un fichier local MP3 ou WAV (glisser-déposer ou sélecteur). L'original reste toujours conservé localement sur votre Mac.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 340)
            HStack(spacing: 12) {
                Button {
                    openAudioImportPanel()
                } label: {
                    Label("Importer un morceau", systemImage: "square.and.arrow.down")
                }
                .buttonStyle(.borderedProminent)

                Button {
                    store.selectedTab = .sound
                } label: {
                    Label("Régler le son / micro", systemImage: "slider.horizontal.3")
                }
                .buttonStyle(.bordered)
            }
            Spacer()
        }
        .padding()
    }

    @ViewBuilder
    private var rightInspector: some View {
        if let detail = store.selectedTrackDetail {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    HStack(alignment: .top) {
                        VStack(alignment: .leading, spacing: 4) {
                            Text(detail.track.title)
                                .font(.title2.bold())
                            Text(detail.track.artist)
                                .font(.headline)
                                .foregroundStyle(.secondary)
                            HStack(spacing: 8) {
                                Label("Original prêt", systemImage: "checkmark.circle.fill")
                                    .font(.caption)
                                    .foregroundStyle(.green)
                                Text("• \(detail.track.format.uppercased()) \(detail.track.sampleRateHz) Hz • \(formatTimeMMSS(detail.track.durationS))")
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                        }
                        Spacer()
                        Button("Modifier") {
                            editTitle = detail.track.title
                            editArtist = detail.track.artist
                            editAlbum = detail.track.album ?? ""
                            editTonic = detail.track.estimatedTonic ?? "c"
                            editScale = detail.track.estimatedScale ?? "chromatic"
                            showEditSheet = true
                        }
                        .buttonStyle(.bordered)
                    }

                    HStack(spacing: 10) {
                        Button {
                            store.loadAndPlay(
                                trackId: detail.track.id,
                                openSingTab: true,
                                useStems: detail.stemsReady
                            )
                        } label: {
                            Label(
                                detail.stemsReady && detail.lyrics?.status == "user_validated"
                                    ? "Chanter (accompagnement + paroles validées)"
                                    : (detail.stemsReady ? "Chanter (stems prêts, paroles à vérifier)" : "Écouter / chanter sur l'original"),
                                systemImage: "play.fill"
                            )
                        }
                        .buttonStyle(.borderedProminent)

                        if detail.stemsReady {
                            Button("Écouter l'original") {
                                store.loadAndPlay(trackId: detail.track.id, openSingTab: false, useStems: false)
                            }
                            .buttonStyle(.bordered)
                        }

                        if store.activePrepJob?.trackId == detail.track.id && store.activePrepJob?.state == "running" {
                            Button("Annuler la préparation", role: .destructive) {
                                store.cancelPreparation(trackId: detail.track.id)
                            }
                            .buttonStyle(.bordered)
                        } else {
                            Button {
                                store.startPreparation(trackId: detail.track.id)
                            } label: {
                                Label(detail.stemsReady ? "Relancer l'analyse" : "Préparer avant lecture", systemImage: "wand.and.stars")
                            }
                            .buttonStyle(.bordered)
                        }
                    }

                    GroupBox("Préparation locale avant lecture") {
                        VStack(alignment: .leading, spacing: 10) {
                            if let job = store.activePrepJob, job.trackId == detail.track.id {
                                HStack {
                                    Text(job.statusDetailFr)
                                        .font(.callout)
                                    Spacer()
                                    Text("\(Int((job.progress * 100).rounded())) %")
                                        .font(.caption.monospacedDigit())
                                }
                                ProgressView(value: Double(job.progress), total: 1.0)
                            }

                            stepRow("1. Original & décodage canonique", ready: true, detail: "SHA-256 vérifié, intro préservée")
                            stepRow(
                                "2. Séparation voix / accompagnement",
                                ready: detail.stemsReady,
                                detail: detail.separationMethod ?? "Non préparée — séparation DSP spectrale locale disponible hors ligne"
                            )
                            stepRow(
                                "3. Repérage du chant",
                                ready: detail.vocalActivity != nil,
                                detail: vocalSummaryLabel(detail.vocalActivity)
                            )
                            stepRow(
                                "4. Paroles & vérification",
                                ready: detail.lyrics?.status == "user_validated",
                                detail: lyricsSummaryLabel(detail.lyrics)
                            )

                            if let tonic = detail.track.estimatedTonic, let scale = detail.track.estimatedScale {
                                HStack {
                                    Text("Tonalité estimée (révisable) :")
                                        .font(.caption)
                                        .foregroundStyle(.secondary)
                                    Text("\(pitchClassLabelFr(tonic)) • \(scaleLabelFr(scale))\(detail.track.keyAmbiguous ? " (ambiguë)" : "")")
                                        .font(.caption.weight(.medium))
                                }
                            }
                        }
                        .padding(.vertical, 4)
                    }

                    HStack {
                        Button {
                            store.selectedTab = .lyricsReview
                        } label: {
                            Label("Vérifier ou importer les paroles", systemImage: "text.alignleft")
                        }
                        .buttonStyle(.bordered)
                    }
                }
                .padding(20)
            }
        } else {
            VStack(spacing: 10) {
                Text("Sélectionnez un morceau")
                    .font(.title3)
                    .foregroundStyle(.secondary)
                Text("Importez un fichier MP3 ou WAV pour préparer ses pistes et ses paroles.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private func stepRow(_ title: String, ready: Bool, detail: String) -> some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: ready ? "checkmark.circle.fill" : "circle.dashed")
                .foregroundStyle(ready ? .green : .secondary)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.subheadline.weight(.medium))
                Text(detail).font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    private func vocalSummaryLabel(_ va: VocalActivitySummaryDTO?) -> String {
        guard let va else { return "En attente de préparation" }
        if va.noSingingDetected {
            return "Aucun chant détecté (instrumental ou vérification requise)"
        }
        let first = va.firstSungActivityS.map { String(format: "%.1fs", $0) } ?? "inconnu"
        return "\(va.regions.count) segment(s) • premier chant estimé à \(first) (\(va.method))"
    }

    private func lyricsSummaryLabel(_ doc: LyricsDocumentDTO?) -> String {
        guard let doc else { return "Aucune parole fournie (collez un texte ou importez un .lrc)" }
        switch doc.status {
        case "user_validated": return "Validées (révision \(doc.textRevision), \(doc.cues.count) lignes)"
        case "no_singing_detected": return "Aucun chant détecté — minutage manuel possible"
        default: return "À vérifier (révision \(doc.textRevision), \(doc.cues.count) lignes, granularité : \(doc.granularity))"
        }
    }

    private var metadataEditSheet: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Modifier les informations du morceau")
                .font(.headline)
            Form {
                TextField("Titre", text: $editTitle)
                TextField("Artiste", text: $editArtist)
                TextField("Album", text: $editAlbum)
                Picker("Tonique", selection: $editTonic) {
                    ForEach(allPitchClasses, id: \.key) { item in
                        Text(item.label).tag(item.key)
                    }
                }
                Picker("Gamme", selection: $editScale) {
                    ForEach(allMusicalScales, id: \.key) { item in
                        Text(item.label).tag(item.key)
                    }
                }
            }
            HStack {
                Spacer()
                Button("Annuler") { showEditSheet = false }
                Button("Enregistrer") {
                    if let det = store.selectedTrackDetail {
                        store.updateTrackMetadata(
                            trackId: det.track.id,
                            title: editTitle,
                            artist: editArtist,
                            album: editAlbum,
                            isFavorite: det.track.isFavorite,
                            tonic: editTonic,
                            scale: editScale
                        )
                    }
                    showEditSheet = false
                }
                .buttonStyle(.borderedProminent)
            }
        }
        .padding(20)
        .frame(width: 420)
    }

    private func openAudioImportPanel() {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [UTType.mp3, UTType.wav]
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = false
        panel.message = "Choisissez des fichiers locaux MP3 ou WAV (l'original reste conservé)."
        if panel.runModal() == .OK {
            for url in panel.urls {
                store.importTrack(url: url)
            }
        }
    }

    private func handleFileDrop(_ providers: [NSItemProvider]) -> Bool {
        var handled = false
        for p in providers {
            if p.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier) {
                handled = true
                p.loadItem(forTypeIdentifier: UTType.fileURL.identifier, options: nil) { item, _ in
                    if let data = item as? Data, let url = URL(dataRepresentation: data, relativeTo: nil) {
                        Task { @MainActor in
                            store.importTrack(url: url)
                        }
                    }
                }
            }
        }
        return handled
    }
}
