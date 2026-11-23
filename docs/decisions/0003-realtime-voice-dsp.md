# ADR-0003 : Chaîne DSP vocale temps réel en Rust

- **Date** : 2026-11-23
- **Statut** : Accepté
- **Référence** : `SPECIFICATIONS_TECHNIQUES.md` §8

## Contexte

REQ-014 et REQ-015 exigent une vraie correction de justesse temps réel (estimation F0/confiance, décision voisé/non voisé, notes cibles/gamme avec hystérésis, transition lissée, gestion vibrato, latence connue), une réverbération locale, une dynamique douce, des préréglages réels (**Naturel**, **Studio**, **Scène**, **Personnalisé**), un bypass propre et un limiteur de protection contre l'écrêtage numérique — le tout sans aucune allocation mémoire ni blocage dans le callback audio.

## Décision

Implémenter `karaok-voice-dsp` en Rust pur préalloué :
1. **Gain d'entrée lissé + filtre passe-haut biquad Butterworth 80 Hz** (désactivable).
2. **Détecteur F0 YIN/MPM** sur tampon circulaire fixe préalloué (1024 échantillons à 48 kHz = 21,33 ms, plage 75–1000 Hz) avec interpolation parabolique, confiance CMNDF, taux de passage par zéro (ZCR) et porte d'énergie RMS pour isoler sons voisés vs consonnes/souffle/silence.
3. **Quantificateur musical avec hystérésis** : modes Chromatique explicite et Tonique + Gamme (Majeure, Mineure naturelle, Mineure harmonique, Pentatonique majeure/mineure, Dorien, Mixolydien), zone neutre pour préserver le vibrato, hystérésis anti-battement entre demi-tons, vitesse de correction lissée, borne de ratio (`[0.5, 2.0]`).
4. **Correcteur de hauteur temporel synchrone à la période (Dual-Tap Pitch-Synchronous Hann Windowed Delay Shifter)** : ajuste la taille de grain sur `2 × T0` (bornée) pour limiter les artefacts de phase et préserver la continuité périodique des harmoniques, avec compensation de délai sur le chemin sec lors du bypass/mix pour éviter le filtrage en peigne. Latence algorithmique exactement exposée via `latency_samples()`.
5. **Compresseur doux** (suiveur d'enveloppe attaque/relâchement, genou doux).
6. **Réverbération algorithmique FDN à 8 canaux** (matrice de Householder orthogonale, lignes à retard premières, pré-délai, filtres d'amortissement passe-bas dans la boucle, gains strictement < 1 pour garantir la stabilité BIBO, décorélation stéréo).
7. **Limiteur de sortie maître** (lookahead court préalloué + rampe d'atténuation sur pic échantillon, sanitisation stricte NaN/Inf). Annoncé comme limiteur de pic échantillon (non « true peak » suréchantillonné).

## Conséquences

- Zéro dépendance C++/FFI supplémentaire pour le DSP live, zéro allocation sur le thread RT, licences 100 % permissives.
- Comportement vérifiable objectivement par tests signal (sinusoïdes, voyelles synthétiques harmoniques, glissandi, vibrato, bruit blanc non voisé, absence NaN/Inf/clic).
- Limite connue et documentée : le pitch-shifting temporel sans ré-synthèse d'enveloppe spectrale dédiée peut déplacer partiellement les formants sur de grands intervalles (> 3–4 demi-tons) ; la correction vocale karaoké typique (±0,5 à ±2 demi-tons) limite cet effet, et un module formant/PSOLA ou Signalsmith Stretch (MIT) reste évaluable si des essais d'écoute réels révèlent un besoin.
