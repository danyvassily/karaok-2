# ADR-0005 : Licence open source MIT et inventaire des dépendances

- **Date** : 2026-11-23
- **Statut** : Accepté
- **Référence** : `SPECIFICATIONS_TECHNIQUES.md` §12.1–§12.2

## Contexte

`SPECIFICATIONS_TECHNIQUES.md` §12.2 précise que la licence open source de Karaok Party doit être choisie explicitement avant publication et rester compatible avec toutes les dépendances retenues.

## Décision

- Adopter la licence **MIT** pour le code original de Karaok Party, formalisée par le fichier `LICENSE` à la racine du dépôt, la métadonnée `license = "MIT"` dans `rust/Cargo.toml` (héritée par les 13 crates membres) et `NSHumanReadableCopyright` dans `mac-app/Resources/Info.plist`.
- Conserver une compatibilité propre avec l'ensemble des dépendances retenues (MIT, Apache-2.0, SQLite Domaine Public et MPL-2.0 pour Symphonia dont le copyleft faible s'applique uniquement au niveau fichier).
- Aucune dépendance GPL/AGPL (telle que OpenKara ou applemusic-like-lyrics) n'est introduite dans le code.
- L'inventaire complet des dépendances tierces et de leurs licences vérifiées est maintenu dans `THIRD_PARTY_NOTICES.md`.

## Conséquences

- Compatibilité complète avec une distribution macOS open source gratuite et réutilisable.
- Toute modification future de fichiers source de Symphonia (MPL-2.0) devra être publiée sous MPL-2.0 conformément à ses termes (cf. `THIRD_PARTY_NOTICES.md`).
