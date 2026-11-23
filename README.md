# Karaok Party

Application de karaoké **macOS**, gratuite et open source : moteur audio temps réel **Rust** (`rust/`), interface native **SwiftUI** (`mac-app/`) reliée par une ABI C étroite (`karaok-ffi`), support explicite du microphone **Nothing Headphone (a) en USB-C** par UID Core Audio persistant, bibliothèque locale MP3/WAV, séparation voix/accompagnement hors ligne, paroles pilotées par l'horloge musicale et effets voix temps réel (correction de justesse YIN + pitch-shifter synchrone, réverbération FDN 8 canaux, compresseur doux, limiteur).

## État réel du projet (2026-11-23)

- **Implémenté et vérifié localement (`Fait vérifié` / `Mesuré`)** : workspace Rust (13 crates, 34/34 tests, `cargo clippy -D warnings` propre), application macOS `build/KaraokParty.app` signée localement ad-hoc (`com.apple.security.device.audio-input`), énumération Core Audio HAL vérifiée sur Apple M3, correction de justesse live mesurée (`431,0 Hz → 440,07 Hz`, charge `p99 = 4,28 %` du budget bloc à 48 kHz / 256 frames, `0` overrun sur 30 000 blocs simulés).
- **Reste à valider (`À valider`)** : essai matériel physique avec le casque **Nothing Headphone (a) en USB-C** (non connecté lors de la construction), mesure de latence acoustique en boucle, test interactif VoiceOver/GUI macOS, et validation par le propriétaire du fichier `LICENSE` (voir [ADR-0005](docs/decisions/0005-license-proposal-and-dependencies.md)).

## Démarrage rapide (macOS local)

```bash
# 1. Exécuter les tests Rust
make test

# 2. Compiler le moteur Rust release et assembler build/KaraokParty.app
make build

# 3. Lancer l'application macOS
open build/KaraokParty.app

# Outils de diagnostic et benchmarks locaux
make devices   # Inventaire Core Audio HAL (UID expurgés)
make bench     # Benchmark DSP & moteur RT
```

Voir [docs/INSTALLATION_MACOS.md](docs/INSTALLATION_MACOS.md) pour le détail des permissions, du stockage local et des limites de distribution sans compte Apple Developer payant.

## Documentation de référence

- [Spécifications techniques](SPECIFICATIONS_TECHNIQUES.md)
- [Prompt master pour agents](MASTER_PROMPT.md)
- [Instructions courtes de projet](AGENTS.md)
- [Rapport de validation et mesures réelles](docs/VALIDATION_REPORT.md)
- [Journal des décisions d'architecture (ADR)](docs/decisions/README.md)
- [Notices des dépendances tierces](THIRD_PARTY_NOTICES.md)

Hors périmètre garanti : micro iPhone, app iOS, QR code, WebRTC, serveur de signalisation, extraction de flux Spotify/YouTube.
