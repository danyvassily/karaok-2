# Rapport de validation technique — Karaok Party

- **Date** : 2026-11-23
- **Environnement de mesure** : Apple M3 (`arm64`), macOS 27.3 (`26C5035r`), `rustc 1.94.1`, `Swift 6.4.2`, `Xcode 27.2`
- **Branche** : `feat/karaok-party-v1` (licence MIT validée — [ADR-0005](decisions/0005-license-proposal-and-dependencies.md))

Conformément au [MASTER_PROMPT.md](../MASTER_PROMPT.md), chaque affirmation ci-dessous est qualifiée (**Fait vérifié**, **Mesuré**, **Décision**, **Hypothèse**, **À valider**).

---

## 1. Vérifications exécutées (Fait vérifié)

| Vérification | Commande exécutée | Résultat |
|---|---|---|
| Lint strict Rust | `cd rust && cargo clippy --workspace --all-targets -- -D warnings` | **PASS** (0 erreur, 0 avertissement sur les 13 crates) |
| Suite de tests Rust | `cd rust && cargo test --workspace` | **PASS** (34/34 tests unitaires & intégration réussis, 0 échec, 0 ignoré) |
| Compilation & bundle macOS | `./scripts/build-app.sh` (`MACOSX_DEPLOYMENT_TARGET=14.0`, Rust `--release` + Swift `release`) | **PASS** (0 erreur, 0 avertissement Swift/linker, bundle `build/KaraokParty.app` généré) |
| Validation `Info.plist` & entitlements | `plutil -lint mac-app/Resources/Info.plist mac-app/Resources/KaraokParty.entitlements` | **PASS** (`OK` sur les 2 fichiers) |
| Signature & intégrité bundle | `codesign --verify --deep --strict --verbose=2 build/KaraokParty.app` | **PASS** (`valid on disk`, Hardened Runtime local, seul entitlement `com.apple.security.device.audio-input`) |
| Dépendances dynamiques Mach-O | `otool -L build/KaraokParty.app/Contents/MacOS/KaraokParty` | **PASS** (frameworks système Apple uniquement ; moteur Rust lié statiquement) |
| Inventaire Core Audio HAL réel | `./rust/target/release/karaok-cli devices` | **PASS** (3 entrées et 4 sorties énumérées sur ce Mac avec fréquences 48 kHz, buffers 512, latences pilote et UID expurgés localement) |
| Contrôle périmètre & confidentialité | `grep` sur `rust/crates`, `mac-app/Sources`, `docs`, `scripts` | **PASS** (0 mention WebRTC/QR/Spotify extraction/YouTube extraction, 0 donnée personnelle ou chemin `/Users/...` en dur) |

---

## 2. Mesures DSP et moteur temps réel (Mesuré)

Protocole : exécuté en mode `--release` via `./rust/target/release/karaok-cli bench-dsp` sur Apple M3 (`arm64`, macOS 27.3), fréquence `48 000 Hz`, taille de bloc `256 frames` (budget temps réel par bloc : **5 333,33 µs**), voix harmonique synthétique désaccordée à `431,0 Hz` (-36 cents sous `La4 = 440,0 Hz`), gamme `La Majeur`, préréglage `Studio` (`pitch_amount = 1.0`, `pitch_speed = 0.90`, `neutral_zone = 0 c`).

| Mesure | Valeur mesurée | Distinction / Note de protocole |
|---|---|---|
| Latence du chemin signal `VoiceDspChain` | **8,33 ms** (400 échantillons) | Constante en mode actif et bypass (chemin sec compensé en délai). Distincte de la fenêtre d'analyse F0 YIN (~22 ms, pas de mise à jour ~5,3 ms) qui régit le temps de réaction du suivi de hauteur. |
| Latence algorithmique totale (DSP + Limiteur lookahead 2,5 ms) | **10,83 ms** | N'inclut pas le convertisseur/pilote matériel ni le délai acoustique. |
| Précision de correction de justesse (entrée `431,0 Hz` → cible `440,0 Hz`) | **440,07 Hz** (médiane mesurée par un second détecteur YIN indépendant) | Écart résiduel : +0,27 cents |
| Temps de calcul `VoiceDspChain` (2 000 blocs) | `p50 = 158,1 µs` \| `p95 = 172,5 µs` \| **`p99 = 208,9 µs` (3,92 % du budget)** \| `max = 309,6 µs` | Cible interne : `p99 < 50 %` du bloc |
| Temps de calcul `RtOutputProcessor` (30 000 blocs, dérive horloge simulée `+50 ppm`) | `p50 = 174,1 µs` \| `p95 = 189,7 µs` \| **`p99 = 228,0 µs` (4,28 % du budget)** \| `max = 414,2 µs` | `callbacks = 30000`, `deadline_overruns = 0`, `mic_underruns = 0`, `mic_overruns = 0`, `non_finite = 0`, `occ = 384 frames`, `ratio = 1.000050` |

> **Transparence protocole** : ce benchmark mesure la charge CPU et le contrôleur de dérive sur un thread CLI simulé (hors périphérique physique). La latence acoustique en boucle et l'endurance 60 minutes sur thread Core Audio temps réel restent **À valider** sur matériel réel.

---

## 3. Couverture des critères d'acceptation (AC-01 à AC-24)

| Critères | Statut honnête | Preuve / Limite explicite |
|---|---|---|
| AC-01, AC-19 (build local sans compte payant, hors ligne, entitlements) | **Fait vérifié (build) / À valider (dialogue permission OS)** | `scripts/build-app.sh`, `codesign --verify`, `otool -L`, zéro appel réseau. Reste à valider le dialogue macOS de permission micro en lancement interactif. |
| AC-02, AC-03, AC-04 (HAL inventaire, Fixed UID vs SystemDefault, Nothing sans fallback silencieux) | **Fait vérifié (HAL réel + simulation hotplug) / À valider (matériel Nothing)** | HAL réel vérifié via `karaok-cli devices` + listeners système et par périphérique ; retrait/reconnexion Nothing et bascule micro sans coupure lecture testés en simulation (`karaok-audio-device`, `karaok-controller`). **À valider sur matériel physique** : casque **Nothing Headphone (a) USB-C** non branché lors de ce build (spikes P-01/P-02). |
| AC-05 (test micro, niveaux pré/post, clip, retour OFF par défaut, mute) | **Partiellement vérifié (DSP/état) / À valider (auditif GUI)** | Monitoring OFF vérifié dans tous les presets et après rechargement SQLite ; mute total (sec + mouillé sur rampe 12 ms) vérifié par test `karaok-voice-dsp`. Reste à valider à l'oreille dans l'interface macOS. |
| AC-06, AC-18 (import MP3/WAV, SHA-256, playlists, SQLite migrations sûres) | **Fait vérifié (import/refus schéma) / Partiellement vérifié (migration interrompue)** | Tests `karaok-media` (WAV + vrai MP3 VBR via `ffmpeg` + rejet DRM/corrompu) et `karaok-library` (playlists, révisions, `busy_timeout`, sauvegarde pré-migration WAL, refus schéma v99 sans effacement). Une coupure brutale en pleine migration de schéma > v1 reste **À valider** lors d'une future migration v1→v2. |
| AC-07, AC-08, AC-12, AC-17 (stems distincts, détection chant/instrumental, cache, annulation) | **Fait vérifié (DSP spectral local + annulation) / À valider (disque plein & corpus étendu)** | Tests `karaok-analysis` (STFT, détection vocale et estimation tonalité tous en streaming borné en RAM, reconstruction `vocal + acc == orig`, intro 2 s préservée, instrumental explicite, réutilisation cache stems, annulation propre) et `karaok-audio-engine`. `InsufficientDiskSpace` mappé sur `ENOSPC` (testé unitairement), mais non simulé sur volume plein réel ; corpus étendu (rap, chœurs, live) et éventuel modèle neuronal ONNX restent **À valider**. |
| AC-09, AC-10, AC-11 (LRC/UTF-8, offset, placement manuel, 4–5 blocs, horloge musique) | **Fait vérifié (moteur/parser) / À valider (interaction GUI)** | Tests `karaok-lyrics` et `karaok-audio-engine` (silence micro ne bloque jamais la timeline, seek immédiat après `load_track`, reprise après fin de morceau, ordre chronologique garanti). Rendu visuel plein écran et défilement à 200 % dans le GUI macOS restent **À valider** visuellement. |
| AC-13, AC-14 (vraie correction de justesse, non-voisé préservé, presets, bypass, limiteur) | **Mesuré (signaux synthétiques) / À valider (écoute voix réelles)** | Correction `431,0 Hz → 440,07 Hz` mesurée, bruit non voisé préservé, grains indépendants A/B sans clic, limiteur peak-hold lookahead vérifié. Écoute humaine AB sur voix réelles (graves/aiguës/vibrato/formants) reste **À valider**. |
| AC-15, AC-16 (dérive horloge, resampling multi-fréquences, profil latence) | **Mesuré (simulé) / À valider (matériel)** | Resampling 16 kHz→48 kHz + dérive `+100 ppm` (120 s) et `+50 ppm` (30 000 blocs) stabilisés avec 0 underrun. Mesure acoustique en boucle et endurance 60 min Core Audio RT restent **À valider**. |
| AC-20, AC-21, AC-22 (accessibilité, erreurs actionnables, licences/notices) | **Fait vérifié (licence MIT & notices) / À valider (VoiceOver réel)** | Raccourci `Espace` isolé hors champs texte, `Échap` plein écran, libellés/unités VoiceOver sur curseurs, indicateurs non basés sur la couleur seule, licence **MIT** (`LICENSE` + `Cargo.toml` + `ADR-0005` Accepté) et `THIRD_PARTY_NOTICES.md` vérifiés. Test manuel VoiceOver/contraste reste **À valider**. |
| AC-23, AC-24 (P1 transposition morceau, Spotify/YouTube hors cœur) | **Décision respectée** | Transposition P1 non livrée (aucun contrôle factice) ; aucune intégration Spotify/YouTube introduite. |

---

## 4. Actions restantes avant validation matérielle complète (À valider)

1. **Essai matériel physique P-01 / P-02** : brancher le **Nothing Headphone (a) en USB-C**, confirmer sa détection dans **Son**, tester la capture mono 48 kHz + sortie stéréo 48 kHz, débrancher/rebrancher à chaud et relever la latence acoustique mesurée (p50/p95/p99) ainsi qu'une session d'endurance 60 minutes.
2. **Test interactif GUI & accessibilité macOS** : ouvrir `build/KaraokParty.app`, valider le dialogue système de permission microphone, tester VoiceOver, texte 200 %, Réduire les animations, saisie d'espaces dans les champs texte vs `Espace` lecture/pause, `Échap` plein écran et l'écoute humaine des préréglages **Naturel / Studio / Scène**.
3. **Modèles neuronaux optionnels** : évaluer et approuver explicitement un manifeste ONNX (poids + licence + SHA-256) si une séparation neuronale supérieure à la séparation spectrale DSP locale est souhaitée.
