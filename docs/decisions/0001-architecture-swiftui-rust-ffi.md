# ADR-0001 : Architecture SwiftUI macOS + moteur temps réel Rust via ABI C

- **Date** : 2026-11-23
- **Statut** : Accepté (`Décision` D-001 des spécifications techniques)
- **Référence** : `SPECIFICATIONS_TECHNIQUES.md` §4.1–§4.3

## Contexte

Karaok Party cible exclusivement macOS avec une exigence ferme de moteur audio temps réel en Rust et une interface macOS native sobre, française et accessible (VoiceOver, Réduire les animations, texte agrandi jusqu'à 200 %).

## Décision

Séparer strictement :
1. **`rust/`** (workspace Cargo modulaire) : tout le domaine métier, stockage SQLite, décodage média, analyse offline, inventaire Core Audio, moteur audio temps réel, DSP voix live et télémétrie RT sans allocation.
2. **`rust/crates/karaok-ffi`** : bibliothèque statique `libkaraok_ffi.a` exposant une ABI C étroite et versionnée (`include/karaok_ffi.h`), protégeant chaque point d'entrée par `std::panic::catch_unwind` et utilisant des handles opaques.
3. **`mac-app/`** (SwiftPM + SwiftUI macOS natif) : interface utilisateur, intégration système (fenêtres, plein écran, sélection de fichiers, demande de permission microphone `AVCaptureDevice`), sans calcul DSP ni travail lourd synchrone.

Les snapshots temps réel (`KaraokTransportSnapshotC`, `KaraokMeterSnapshotC`) traversent la frontière FFI sous forme de structs C `#[repr(C)]` lues sans allocation ni sérialisation JSON ; les commandes de contrôle utilisent des enveloppes JSON versionnées hors thread audio.

## Alternatives considérées

- **Tauri + WebView React** : écartée pour la V1 car elle ajoute un runtime WebView et réduit l'intégration native macOS/VoiceOver par rapport à SwiftUI (D-001).
- **SwiftUI + AVAudioEngine pour le DSP** : rejetée car elle viole l'invariant de cœur temps réel Rust (REQ-001).

## Conséquences

- Frontière explicite entre Swift (UI/système) et Rust (temps réel/domaine) : aucune dépendance à ARC Swift ou `MainActor` sur le thread audio.
- Build local reproductible sans abonnement Apple Developer payant (via `cargo build` + `swift build` + assemblage de bundle `.app`).
- Maintenance d'un en-tête C (`karaok_ffi.h`) synchronisé avec `karaok-ffi`.
