# ADR-0005 : Proposition de licence open source et inventaire des dépendances

- **Date** : 2026-11-23
- **Statut** : Proposé — en attente de validation explicite du propriétaire du dépôt avant la première release (§12.2)
- **Référence** : `SPECIFICATIONS_TECHNIQUES.md` §12.1–§12.2

## Contexte

`SPECIFICATIONS_TECHNIQUES.md` §12.2 précise que la licence open source de Karaok Party doit être choisie avant la première release et n'est pas tranchée automatiquement par les spécifications.

## Décision proposée

- Recommander **MIT OR Apache-2.0** (double licence idiomatique de l'écosystème Rust) pour le code original de Karaok Party, ou **Apache-2.0** seule, en conservant une compatibilité propre avec l'ensemble des dépendances retenues (MIT, Apache-2.0 et MPL-2.0 pour Symphonia, dont la copyleft faible s'applique au niveau fichier).
- Aucune dépendance GPL/AGPL (telle que OpenKara ou applemusic-like-lyrics) n'est introduite dans le code.
- Aucun fichier `LICENSE` définitif n'est imposé sans décision explicite du propriétaire ; l'inventaire complet des dépendances tierces et de leurs licences vérifiées est maintenu dans `THIRD_PARTY_NOTICES.md`.

## Conséquences

- Compatibilité complète avec une distribution macOS open source gratuite.
- Avant publication d'une release, le propriétaire du dépôt choisit et ajoute le fichier `LICENSE` définitif et met à jour le statut de cet ADR en « Accepté ».
