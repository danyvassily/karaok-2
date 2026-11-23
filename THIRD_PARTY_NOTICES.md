# Third-Party Notices — Karaok Party

Inventaire vérifié des dépendances directes Rust (versions et licences relevées via `cargo info` le 2026-11-23).
Aucune dépendance GPL/AGPL ni aucun poids de modèle externe non vérifié n'est inclus dans ce dépôt.

| Crate | Version épinglée | Licence vérifiée | Usage | Dépôt |
|---|---|---|---|---|
| `symphonia` | `0.5.4` (features: `mp3`, `wav`, `pcm`) | `MPL-2.0` | Décodage et métadonnées MP3/WAV | https://github.com/pdeljanov/Symphonia |
| `rusqlite` | `0.32.1` (feature: `bundled`, embarque SQLite C — Domaine Public) | `MIT` + SQLite Public Domain | Base SQLite locale (bibliothèque, révisions, jobs) | https://github.com/rusqlite/rusqlite |
| `rtrb` | `0.4.1` | `MIT OR Apache-2.0` | Tampon circulaire SPSC temps réel sans verrou | https://github.com/mgeier/rtrb |
| `rustfft` | `6.4.1` | `MIT OR Apache-2.0` | STFT offline (séparation spectrale, détection chant, chromagramme) | https://github.com/ejmahler/RustFFT |
| `coreaudio-sys` | `0.2.18` (features: `core_audio`, `audio_unit`, `audio_toolbox`) | `MIT` | Bindings Core Audio HAL / AUHAL macOS | https://github.com/RustAudio/coreaudio-sys |
| `core-foundation-sys` | `0.8.7` | `MIT OR Apache-2.0` | Types CoreFoundation (`CFStringRef`, etc.) pour HAL macOS | https://github.com/servo/core-foundation-rs |
| `sha2` | `0.10.9` | `MIT OR Apache-2.0` | Empreintes SHA-256 (fichiers originaux, cache, modèles) | https://github.com/RustCrypto/hashes |
| `serde` | `1.0.229` | `MIT OR Apache-2.0` | Sérialisation typée des contrats internes | https://github.com/serde-rs/serde |
| `serde_json` | `1.0.154` | `MIT OR Apache-2.0` | Enveloppes JSON FFI et manifestes | https://github.com/serde-rs/json |
| `uuid` | `1.29.3` | `Apache-2.0 OR MIT` | Identifiants stables locaux | https://github.com/uuid-rs/uuid |
| `thiserror` | `2.0.21` | `MIT OR Apache-2.0` | Hiérarchie d'erreurs structurées | https://github.com/dtolnay/thiserror |
| `crossbeam-channel` | `0.5.17` | `MIT OR Apache-2.0` | Canaux hors thread RT (contrôle, événements, listeners HAL) | https://github.com/crossbeam-rs/crossbeam |
| `chrono` | `0.4.45` | `MIT OR Apache-2.0` | Horodatages UTC persistés | https://github.com/chronotope/chrono |
| `dirs` | `7.0.0` | `MIT OR Apache-2.0` | Résolution chemins système macOS (Application Support / Caches) | https://codeberg.org/dirs/dirs-rs |
| `tempfile` | `3.27.0` | `MIT OR Apache-2.0` | Répertoires temporaires atomiques et tests | https://github.com/Stebalien/tempfile |
| `hound` | `3.5.1` | `Apache-2.0` | Lecture/écriture WAV PCM f32 canonique (cache stems et fixtures) | https://github.com/ruuda/hound |
| `libc` | `0.2.191` | `MIT OR Apache-2.0` | Types C pour FFI et Core Audio | https://github.com/rust-lang/libc |

## Frameworks système macOS (SDK Apple)

Utilisés uniquement via les API système documentées macOS : `SwiftUI`, `AppKit`, `AVFoundation`, `CoreAudio`, `AudioToolbox`, `CoreFoundation`. Soumis aux conditions du SDK Apple.

## Note sur Symphonia (MPL-2.0)

Symphonia est utilisé en tant que crate non modifiée sous licence MPL-2.0. Toute modification future de fichiers source de Symphonia devrait être publiée sous MPL-2.0 conformément à ses termes.
