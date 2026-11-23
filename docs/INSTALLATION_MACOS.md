# Installation et exécution locale sur macOS — Karaok Party

Ce guide documente le chemin légitime de compilation, d'exécution locale et de test des permissions sur macOS **sans abonnement payant Apple Developer** (`SPECIFICATIONS_TECHNIQUES.md` §11.2, AC-01, AC-19).

## Prérequis vérifiés

- **macOS 14.0+** (validé sur Apple Silicon `arm64`, macOS 27.3 / Apple M3)
- **Rust & Cargo** (`rustc 1.80+`, testé avec `rustc 1.94.1`)
- **Xcode / Command Line Tools** (`swift 6.0+`, testé avec Swift 6.4.2 / Xcode 27.2)

Aucun compte Apple Developer payant n'est requis pour compiler et exécuter localement. Aucune désactivation de Gatekeeper ou de SIP n'est demandée ni recommandée.

## Compiler et assembler le bundle `KaraokParty.app`

```bash
./scripts/build-app.sh
```

Le script :
1. synchronise l'en-tête C FFI (`karaok_ffi.h`) entre `rust/crates/karaok-ffi` et `mac-app/Sources/CKaraokFFI` ;
2. compile le moteur temps réel Rust et la bibliothèque statique `libkaraok_ffi.a` en mode `--release` ainsi que l'outil CLI `karaok-cli` ;
3. compile l'application native SwiftUI `KaraokParty` en mode `release` liée statiquement au moteur Rust ;
4. assemble `build/KaraokParty.app` avec `Info.plist` (justification microphone française `NSMicrophoneUsageDescription`) et applique une signature ad-hoc locale portant uniquement l'entitlement `com.apple.security.device.audio-input`.

## Lancer l'application

```bash
open build/KaraokParty.app
```

## Permissions et comportement hors ligne

- **Microphone** : macOS affiche la boîte de dialogue d'autorisation uniquement lorsque vous cliquez sur **Tester mon micro** ou **Activer micro**. En cas de refus ou de révocation, l'importation, la bibliothèque et la lecture de fichiers locaux restent utilisables ; un bouton renvoie directement vers *Réglages Système → Confidentialité et sécurité → Microphone*.
- **Périphérique Nothing Headphone (a) USB-C** : connectez le casque en USB-C puis ouvrez l'onglet **Son** pour le confirmer explicitement. Karaok Party mémorise son UID Core Audio persistant sans modifier les périphériques système par défaut et sans jamais basculer silencieusement vers le micro intégré du Mac s'il est débranché.
- **Accès réseau** : aucun appel réseau implicite. Bibliothèque, décodage, séparation spectrale DSP locale, détection de chant, alignement par ligne et effets voix live fonctionnent 100 % hors ligne.
- **Stockage local** :
  - Données durables (base SQLite, copies originales, modèles vérifiés, sauvegardes de migration) : `~/Library/Application Support/KaraokParty/`
  - Cache reconstructible (PCM canonique, stems) : `~/Library/Caches/KaraokParty/`

## Limites de distribution (honnêteté)

- La signature **ad-hoc locale** (`codesign --sign -`) permet l'exécution locale sur le Mac qui a compilé l'application.
- La distribution publique de binaires téléchargeables sur d'autres Mac (notarisation Apple, Developer ID, Gatekeeper sans avertissement) relève d'une adhésion Apple Developer distincte et d'une recette de release dédiée (§11.2, AC-01). Elle n'est pas revendiquée par ce build local.
