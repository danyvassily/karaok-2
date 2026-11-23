# ADR-0004 : Analyse offline, séparation locale, détection chant et ModelStore vérifié

- **Date** : 2026-11-23
- **Statut** : Accepté
- **Référence** : `SPECIFICATIONS_TECHNIQUES.md` §7, §11.3, `MASTER_PROMPT.md`

## Contexte

Le pipeline de préparation avant lecture doit :
- garder l'original immuable et ancrer toute la timeline dans son décodage canonique (intro comprise) ;
- produire des stems voix originale et accompagnement dans un cache cléé et atomique ;
- détecter l'activité chantée (`first_vocal_activity_s`, `first_sung_activity_s`, intervalles `singing`/`speech`/`uncertain`) sans se limiter à un seuil d'énergie ou VAD parole ;
- aligner les paroles fournies ou importées (priorité LRC validé > texte aligné par ligne > saisie/transcription brouillon) sans jamais inventer de timing mot par mot ;
- respecter l'interdiction stricte de télécharger ou charger des poids de modèles ML non vérifiés ou sans consentement explicite.

## Décision

1. **Décodage canonique Symphonia (`karaok-media`)** : décodage MP3/WAV vers PCM `f32` stéréo canonique en cache avec hash SHA-256, métadonnées, durée exacte et résumé de forme d'onde.
2. **Séparation enfichable (`karaok-analysis::separation`)** :
   - **Méthode locale immédiate (`dsp_center_spectral_v1`)** : séparation STFT (fenêtre de Hann 2048, recouvrement 75 %, zéro clic aux jonctions) par cohérence de phase inter-canal et masque fréquentiel de la voix centrale. Propriété mesurable : `vocal + accompaniment = original` (reconstruction exacte), longueur strictement identique au PCM canonique, 100 % locale et hors ligne. Ses limites réelles (voix centrée stéréo requise, résidus possibles sur chœurs/réverbération/mix mono) sont affichées explicitement.
   - **Point d'extension neuronal vérifié (`onnx_verified_model`)** : intégré via `karaok-model-store` qui exige un `ModelManifest` complet (URL officielle, SHA-256 vérifié, taille, licence code et licence poids distinctes). En l'absence de poids installés par l'utilisateur, le système signale explicitement `ModelMissing` sans téléchargement silencieux ni simulation.
3. **Détecteur d'activité chantée multi-critères (`multi_feature_harmonic_v1`)** : combine ratio stem vocal / mix original, périodicité harmonique F0 (YIN/MPM), stabilité de hauteur soutenue, platitude spectrale et rapport d'énergie bande formantique (300–3400 Hz) avec hystérésis. Retourne explicitement `NoSingingDetected` sur un instrumental et `Uncertain` / `NeedsReview` sur passages ambigus.
4. **Alignement contraint monotone par ligne (`karaok-analysis::alignment`)** : respecte l'ordre de priorité de §7.5, utilise les régions vocales et ancres validées humainement, ne force jamais de ligne dans un silence instrumental et ne fabrique jamais de temps mot par mot.
5. **Estimation de tonalité (`krumhansl_chromagram_v1`)** : chromagramme STFT + corrélation Krumhansl-Schmuckler, score et drapeau d'ambiguïté, toujours révisable manuellement.

## Conséquences

- Le pipeline complet fonctionne immédiatement hors ligne sur des fichiers MP3/WAV réels dès le premier lancement, avec transparence complète sur la méthode et les limites.
- L'intégration future d'un modèle neuronal (ex. Demucs ONNX ou SOFA) passe par le `ModelStore` vérifié sans refonte d'architecture ni téléchargement caché.
