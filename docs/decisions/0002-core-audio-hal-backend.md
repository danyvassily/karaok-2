# ADR-0002 : Accès matériel Core Audio HAL / AUHAL unifié en Rust

- **Date** : 2026-11-23
- **Statut** : Accepté (`Décision` D-002 des spécifications techniques)
- **Référence** : `SPECIFICATIONS_TECHNIQUES.md` §4.1, §5, §6.4

## Contexte

L'application doit énumérer séparément toutes les entrées et sorties Core Audio exposées sur macOS, conserver une identité persistante par **UID Core Audio** (`kAudioDevicePropertyDeviceUID`), lire les formats/canaux/latences réels (et marquer `unknown` ce qui est absent), écouter les changements à chaud (liste, défauts, état vivant, format, taille de buffer), et ouvrir explicitement un périphérique d'entrée (notamment **Nothing Headphone (a) USB-C**) sans jamais modifier les réglages globaux macOS ni basculer silencieusement vers le micro intégré.

## Décision

Utiliser un module unique `karaok-audio-device` basé directement sur **Core Audio HAL et AUHAL** via `coreaudio-sys` (`0.2.18`, MIT) et `core-foundation-sys` (`0.8.7`, MIT/Apache-2.0) :
- Énumération HAL (`kAudioHardwarePropertyDevices`), lecture UID, nom, fabricant, transport, configurations de flux entrée/sortie, plages de fréquences, taille de buffer, latence et `safety offset` déclarés.
- Enregistrement des listeners HAL (`AudioObjectAddPropertyListener`) ne faisant qu'envoyer un signal non bloquant vers un canal de contrôle (désabonnement symétrique dans `Drop`).
- Ouverture AUHAL explicite par `AudioDeviceID` résolu depuis l'UID sélectionné (entrée et sortie indépendantes, sans jamais toucher aux périphériques système par défaut).
- Abstraction `AudioHalBackend` permettant l'injection d'un backend simulé déterministe pour les tests unitaires/d'intégration/hotplug (explicitement étiquetés « simulés »).

## Alternatives considérées

- **CPAL seul** : CPAL n'expose pas de façon uniforme sur macOS les propriétés HAL détaillées (transport type, safety offset, stream latency, listeners multi-propriétés, sélection UID persistante garantie). Combiner CPAL et HAL imposerait une double gestion risquée du cycle de vie.

## Conséquences

- Autorité unique sur le cycle de vie matériel Core Audio.
- Aucun appel réseau ni accès USB direct requis (accès Core Audio standard + entitlement `com.apple.security.device.audio-input`).
- **À valider sur matériel physique** : essai réel P-01/P-02 avec le casque **Nothing Headphone (a) en USB-C** non connecté au Mac lors de la construction initiale.
