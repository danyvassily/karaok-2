---
title: Karaok Party — Prompt master de développement
version: 1.0
date_created: 2026-11-23
last_updated: 2026-11-23
audience: Codex, Antigravity, Hermes Agent et agents compatibles
---

# Prompt master — Karaok Party

Ce document est l’instruction de travail durable du projet **Karaok Party**. Il s’adresse à un agent chargé d’explorer, concevoir, implémenter, tester ou revoir l’application. Il ne remplace jamais les instructions système, politiques de sécurité, autorisations de l’utilisateur, règles de l’outil ou fichiers d’instructions plus prioritaires.

Avant toute modification, lire :

1. `AGENTS.md` à la racine ;
2. [`SPECIFICATIONS_TECHNIQUES.md`](SPECIFICATIONS_TECHNIQUES.md), en entier pour toute tâche audio, média, paroles, modèle, UX ou distribution ;
3. l’état réel du dépôt, ses instructions locales, les changements non commités et la documentation du composant touché.

Ce prompt **ne garantit pas l’absence d’hallucination**. Il impose des pratiques qui la rendent détectable : séparer fait/hypothèse/mesure, consulter les sources primaires, exécuter les validations annoncées et nommer les inconnues. Si une preuve manque, dire « non vérifié » et créer ou demander la validation adaptée ; ne pas combler le vide par une API, une mesure, un résultat ou une compatibilité inventés.

## Mission et invariants non négociables

Construire progressivement une application karaoké **macOS**, **gratuite et open source**, à l’interface soignée, simple, française et accessible. Le moteur audio temps réel est écrit en **Rust**. Toute couche SwiftUI/Swift, WebView, C++ ou autre est une frontière explicite : elle ne transforme pas ces composants en Rust et ne doit pas absorber le moteur temps réel.

La configuration de référence est le **Nothing Headphone (a) connecté en USB-C**. Le constat matériel daté du 23 novembre 2026 est : macOS expose une entrée mono et une sortie stéréo à 48 kHz. Ce constat établit une détection ; il ne prouve ni capture, ni qualité, ni latence, ni permission. L’application doit sélectionner explicitement son entrée par UID/capacités réels, sans dépendre du périphérique macOS par défaut et sans modifier les réglages système globaux.

Gérer toutes les entrées/sorties que Core Audio expose à l’application. Fournir listes séparées entrée microphone, sortie musique/mix et retour de voix ; formats actifs et disponibles, canaux, changements à chaud, disparitions et permissions. Ne jamais remplacer silencieusement le Nothing absent par le micro du Mac. Une sortie à la fois est la règle V1. Multisortie et deux entrées sont des extensions qui exigent leur traitement d’horloge et leurs recettes.

Lire les fichiers locaux MP3/WAV, constituer une bibliothèque, favoris et playlists, et conserver l’original. Préparer le fichier avant lecture : décodage, séparation de la voix originale et de l’accompagnement, activité de chant, premier départ du chant, et minutage des paroles. La musique pilote les paroles : le silence ou la justesse du chanteur ne doit jamais arrêter ou faire avancer l’affichage.

Le rendu paroles affiche quatre ou cinq blocs lisibles, avec bloc actif et anticipation, défilement fluide et plein écran. Les paroles minutées correspondant exactement au morceau ont priorité. Sinon aligner offline un texte fourni sur la voix séparée ; sinon proposer une transcription révisable. Ne jamais feindre un minutage mot par mot en répartissant uniformément des mots sans l’annoncer. Le premier chant détecté, le premier cue de paroles et le texte aligné sont trois résultats différents.

Appliquer sur la voix live du casque : gain/mute/mètres, correction de justesse réelle, réverbération, dynamique raisonnable et protection contre l’écrêtage. Proposer Naturel/Studio/Scène, bypass et intensité. Une transposition constante n’est pas une correction de justesse. Un produit ne revendique pas le moteur ni la marque propriétaires Auto-Tune.

La réduction de voix concerne les fichiers importés uniquement : elle atténue la voix originale au moyen de stems préparés, sans promettre de supprimer parfaitement chœurs/réverbération. Conserver original, accompagnement, voix originale et voix live comme chemins et gains distincts.

Sont hors périmètre courant : iPhone comme micro, app iOS, QR, Safari microphone, WebRTC, serveur de signalisation, duo via téléphones. Ne les réintroduis pas comme solution de repli. Spotify et YouTube ne sont pas le cœur : ne télécharge, n’extrais, n’analyse, ne sépare, ne mixes et ne synchronise aucun flux interdit. Leur éventuelle intégration future est isolée et soumise à revue des conditions actuelles.

## Mode de vérité

Dans toute proposition, commentaire de code, document de décision, résultat de test ou réponse finale, qualifier les affirmations :

| Marqueur | Usage |
|---|---|
| **Fait vérifié** | Soutenu par code lu, sortie de commande, documentation primaire datée ou test reproductible. Citer la source ou l’artefact. |
| **Mesuré** | Produit par un protocole avec environnement, versions, paramètres, données, mesures brutes et agrégats. |
| **Hypothèse** | Plausible mais non contrôlée ; indiquer comment l’infirmer. |
| **Décision** | Choix du projet, lié à son ADR ou à la section des spécifications. |
| **À valider** | Condition bloquante avant une promesse, une dépendance ou une release. |

Ne pas :

- inventer noms de fonctions, options de CLI, entitlements, APIs de bibliothèques, versions, compatibilités, mesures, fichiers ou tests ;
- affirmer qu’un modèle, le microphone Nothing, la correction de pitch, l’alignement chant ou une intégration est opérationnel avant preuve ;
- présenter une cible de latence comme une performance mesurée ;
- remplacer une exigence difficile par un bouton factice, un mock silencieux ou une interface statique ;
- supprimer une exigence, un test défaillant, une limite ou un changement tiers pour rendre l’état vert ;
- modifier la configuration globale de l’utilisateur, installer/activer un service externe, téléverser audio ou envoyer un message sans autorisation explicite ;
- contourner une permission, TLS, signature, Gatekeeper, DRM, des conditions d’utilisation ou une restriction de plateforme.

Une donnée inconnue vaut `unknown`, un champ nullable ou une erreur explicite. Ne pas la convertir en zéro, chaîne vide, succès ou valeur plausible. Lorsque deux sources se contredisent, privilégier la documentation primaire actuelle, expliquer le conflit et reporter la décision si nécessaire.

## Règles de développement audio temps réel

Le callback audio doit être borné et prévisible. Il ne peut pas effectuer d’allocation, accès fichier, SQLite, réseau, téléchargement, chargement de modèle, inférence ML, verrou potentiellement bloquant, formatage de chaîne, logs synchrones, recherche de périphérique, construction/destruction lourde ni attente d’un autre thread.

Préallouer buffers, resamplers et structures DSP avant le démarrage du flux. Utiliser des files bornées et une politique explicite si elles sont pleines. Les paramètres simples se transmettent par atomiques ou messages non bloquants. Les changements structurels préparent un nouveau graphe hors RT et le remplacent à la frontière d’un bloc ; la libération est différée hors RT.

Le moteur possède une horloge de sortie maître. Les paroles consomment une position de présentation publiée par le moteur. Elles ne suivent pas l’horloge UI, le timer de lecture naïf, le F0 ou l’activité du microphone. Pause, seek, changement de route et tempo, s’il existe, invalident les snapshots périmés par génération et imposent un recalage déterministe.

Entrée et sortie peuvent avoir deux horloges. Même à 48 kHz, mesurer dérive et buffer occupancy puis corriger par resampling asynchrone/adaptation explicitement testée. Une valeur de fréquence, buffer ou latence déclarée par le pilote n’est pas la latence de monitoring totale. Ne promets jamais « zéro latence », « compensation Bluetooth magique » ou synchronisation de sorties multiples sans mesures.

Évalue séparément : Nothing USB en entrée et autre sortie ; Nothing USB en entrée et sortie ; Bluetooth seulement s’il est réellement utilisé. Le casque Bluetooth peut changer de profil lorsqu’un micro est actif. Ne devine ni codec, ni nombre de canaux, ni qualité : lire les formats actifs après ouverture et signaler tout changement.

Le limiter protège le domaine numérique selon sa conception ; il ne garantit pas l’absence de larsen acoustique. Démarrer le retour micro désactivé, ramper les changements de gain et fournir un mute efficace. Une route perdue coupe la contribution micro ; ne bascule jamais sans consentement vers une autre entrée ou vers les haut-parleurs.

## Règles média, IA et paroles

L’original est immuable. Tous les artefacts et temps sont ancrés dans sa timeline décodée, intro comprise. Conserver hash, format, durée, convention de padding/délai et mapping de tout resampling. Ne rogne pas une intro pour que le premier chant semble commencer à zéro.

Séparer les responsabilités et caches : décodage ; stems ; détection activité chantée ; texte ; alignement ; rendu UI. Un changement de texte invalide l’alignement dépendant, pas les stems. Le cache est cléé par hash fichier, version de pipeline/runtime, hash/version de modèle/poids, hash/révision du texte, langue et paramètres significatifs. Publication atomique d’artefacts validés ; les résultats d’un job ancien ne remplacent jamais une révision nouvelle.

Un seuil d’énergie ou VAD parole seul ne répond pas au besoin de chant. Le pipeline de détection doit traiter fuites instrumentales, intro, chœurs, adlibs, rap, parole, voix faibles, instrumental, version live et chevauchements. Renvoyer intervalles, méthode/version, score non surinterprété, incertitude et statut de revue. « Pas de chant détecté » et « incertain » sont des résultats valides.

Pour l’alignement, utiliser un texte connu lorsqu’il existe ; conserver provenance, langue, correspondance de version/durée et corrections humaines. Tester les répétitions, omissions et refrains. N’activer un affichage mot par mot que si les temps de mots sont importés ou évalués et validés. Sinon afficher par ligne/phrase. Une transcription est un brouillon, pas des paroles certaines.

Les modèles sont des dépendances distinctes du code. Avant ajout : licence code **et** poids, termes de distribution, source officielle, hash, taille, langues, matériel/runtime, évaluation locale et sortie de sécurité. Ne charge pas de poids non vérifiés, pickle ou script arbitraire. Les tâches ML sont workers annulables/reprenables et ne tournent jamais sur le chemin RT.

Pour la correction de justesse, implémente et teste : estimation F0/confiance, décision voisé/non voisé, tonique/gamme/notes cibles ou mode chromatique explicite, hystérésis, ratio lissé, gestion de vibrato/formants et latence. Une tonalité inférée reste révisable. Les réglages Naturel/Studio/Scène doivent modifier de vrais paramètres DSP, avec bypass fiable. Les valeurs finales sont issues d’écoute et mesures, pas choisies pour remplir une UI.

## Périphériques, permissions et confidentialité

Identifie un périphérique par UID persistant lorsqu’il est fourni par Core Audio, et présente son nom lisible. Ne persiste pas seulement un handle numérique de session ; ne considère pas deux noms identiques comme une même identité. Une préférence est soit rôle système par défaut, soit UID fixe. Le Nothing est une préférence fixe après confirmation ; s’il est absent, il reste indisponible et l’utilisateur choisit une route de secours explicite.

Énumère les entrées et sorties réellement publiées, y compris USB, Bluetooth, intégré, HDMI, virtuel, agrégé lorsqu’ils sont exposés. Ne scanne pas de matériel absent et ne révèle pas de noms personnels, UID complets ou inventaires de l’utilisateur dans le dépôt, logs ou captures publiques. Écoute les changements de liste, défaut, format, buffer, nom et état vivant via mécanisme documenté du backend ; le handler se contente de signaler un travail de contrôle.

Demande le microphone au moment utile avec une justification claire. Traite non demandé/autorisé/refusé/révoqué. Vérifie les entitlements nécessaires au bundle, selon sandbox/hardened runtime, sur une app empaquetée. Ne déduis pas qu’un accès audio Core Audio nécessite un accès USB direct. Ne demandes pas à l’utilisateur de désactiver des protections du système.

Audio, paroles, fichiers et diagnostics restent locaux. Aucun appel réseau implicite. Les recherches de paroles et téléchargements de modèles sont opt-in, identifiés et vérifiés. Les logs ne contiennent ni PCM, ni paroles complètes, ni chemin/identifiant personnel, ni secret. Toute télémétrie externe, synchronisation cloud ou rapport de crash exige décision séparée, texte de confidentialité et consentement approprié.

## Méthode de travail obligatoire

### 1. Avant d’agir

1. Lire l’état Git (`status`, branche, diff) et respecter tout travail non lié. Ne jamais utiliser reset/checkout destructif, force push, réécriture d’historique ou suppression large sans demande explicite.
2. Lire les instructions de dossier et les parties pertinentes des spécifications. Rechercher d’abord avant de créer une abstraction, un doublon ou un fichier.
3. Énoncer l’objectif, le périmètre, les invariants touchés, les hypothèses et les critères de fin. Pour une décision dépendante d’une version ou d’une API, consulter la documentation primaire actuelle, puis noter URL et date.
4. Choisir le plus petit jalon qui réduit le risque. Ne pas démolir/recréer architecture, build ou configuration à l’aveugle.
5. Pour un changement entrant en conflit avec une exigence, une licence ou un travail existant, s’arrêter avec faits, alternatives et besoin de décision ; ne pas « résoudre » en supprimant la contrainte.

### 2. Pendant l’implémentation

- Faire un changement focalisé et cohérent, conserver style/propriété existants, éviter le refactoring opportuniste.
- Écrire des types et contrats qui rendent les états impossibles difficiles à représenter : opérations versionnées, résultats distincts, erreurs exploitables, paramètres bornés, buffers bornés.
- Préserver les changements d’autrui. Isoler tout travail de migration, génération ou mise à jour de lockfile et expliquer son impact.
- Pour une couche UI, séparer état durable, état de transport, commandes async et snapshots temps réel. Ne jamais simuler la préparation/latence/permission parce que le backend n’existe pas.
- Pour une dépendance, épingler après revue de licence et maintenance ; ne pas coller du code ou poids depuis un exemple sans attribution et décision.
- Pour une recherche/IA, rendre le résultat annulable, persistant, invalidable et explicitement révisable. L’échec doit laisser les données utilisateur intègres.
- Pour une fonctionnalité externe, vérifier les conditions à la date de travail ; isoler le code derrière une interface et désactiver par défaut tant que l’autorisation n’est pas établie.

### 3. Vérification

Exécuter les vérifications pertinentes au changement et annoncer celles réellement exécutées. Préférer des tests qui peuvent échouer pour une raison métier/technique réelle à des tests miroirs d’implémentation.

| Changement | Vérification minimale attendue |
|---|---|
| Parsers, timeline, cache, migration | Unitaires + propriété/fuzz ciblé, fixtures avec droits. |
| Graphe/DSP | Tests signal, absence NaN/clip/click documentée, profil des allocations et charge. |
| I/O matériel | Réel matériel ou simulation explicitement étiquetée ; formats actifs, hotplug et permission. |
| Correction pitch | Notes/voix voisées et non voisées, erreurs d’octave, artefacts, délai mesuré. |
| Stems/alignement | Corpus annoté, précision/rappel/erreurs de frontières, répétitions et versions live. |
| UI paroles | Pause/seek/route change, clavier, VoiceOver, contrastes, textes agrandis. |
| Dépendance/modèle | Licence code/poids, hash, version, SBOM/notices, test du chemin effectivement expédié. |
| Release | Build propre, tests, permissions/entitlements, installation et limitations documentées. |

Ne gomme pas un échec. Reproduis-le, minimise-le, rattache-le à une cause si possible et décide : corriger, contourner temporairement avec comportement honnête, déprioriser avec accord ou bloquer. Les benchmarks précisent matériel, OS, route, fréquence, buffers, paramètres, nombre de répétitions, p50/p95/p99, protocole et artefact brut. Pas de chiffres décoratifs.

### 4. Décisions et continuité

Maintenir, ou créer lorsque la structure le prévoit, un journal concis dans `docs/decisions/` avec : contexte, décision, alternatives, conséquences, preuve, date et statut. Utiliser une entrée de progression/handoff à la fin de travaux longs : dernier commit/état, changements faits, validations, limites, prochaines étapes ordonnées, fichiers et commandes utiles. Ne pas enfouir une décision majeure dans un message de commit seul.

Lorsque plusieurs agents/outils travaillent : lire et actualiser avant d’écrire, réserver ou découper les zones plutôt que se chevaucher, ne pas écraser un diff existant, et rapporter clairement les fichiers touchés. Les sous-agents sont une aide d’exploration, pas une autorité ; vérifier leurs conclusions avant intégration.

## Protocole de prototypes indispensables

Ne pas construire l’UI finale avant les résultats des spikes qui conditionnent ses promesses. Chaque prototype doit préciser hypothèse, matériel/versions, protocole, données sous droits, résultats bruts, mesures, limites, coût et décision.

1. **Nothing USB** : ouvrir entrée puis sortie, permission, format effectivement actif, déconnexion/reconnexion. Le nom du périphérique seul ne valide pas le microphone.
2. **Latence/routes** : Nothing entrée avec sortie distincte puis Nothing entrée/sortie ; délai brut/effets, jitter, dérive et endurances. Bluetooth seulement selon usage réel.
3. **Moteur RT** : lecture, capture, mix, route changes, profiling de callback et 60 minutes. Interdire disque/réseau/ML/allocations dans callback.
4. **Pitch** : F0, correction, notes cibles, formants, voix/plages/effets et latency. Un prototype qui ne corrige pas réellement une note ne passe pas.
5. **Séparation** : modèle, runtime, qualité, stems alignés, ressources, licences code/poids et cache.
6. **Premier chant** : intro/chœurs/rap/instrumental/live, faux positifs, confiance et statut incertain.
7. **Alignement** : LRC correct, texte brut, langues, répétitions, conflits de version, mot à mot seulement après évaluation.
8. **Transport/UI** : écart paroles/audio à pause/seek/reprise et changement de route.

Un prototype négatif est une information utile. Ne le cache pas et ne remplace pas son résultat par l’expression « à optimiser » si l’exigence reste non démontrée.

## Règles pour les agents et outils

### Codex

Codex reconnaît `AGENTS.md` dans le projet selon sa chaîne d’instructions. Le fichier racine du dépôt pointe vers les deux documents de fond ; vérifier qu’ils sont lus dans le contexte de la tâche. Si l’outil ne charge pas `MASTER_PROMPT.md` automatiquement, l’agent doit l’ouvrir explicitement avant une tâche substantielle. Ne modifie pas `~/.codex` ni la configuration globale pour rendre cela automatique.

### Antigravity

Les règles d’espace de travail Antigravity peuvent reconnaître `AGENTS.md`, `GEMINI.md` et `.agents/rules/*.md` selon la surface. Le dépôt ne crée pas de règles globales ni de duplication cachée. Démarrer Antigravity à la racine, vérifier les règles détectées dans son interface/CLI et joindre ou référencer manuellement `MASTER_PROMPT.md` si nécessaire. Les mécanismes et limites évoluent : consulter la [documentation officielle Antigravity](https://antigravity.google/docs/rules/) avant de déclarer une règle chargée.

### Hermes Agent

Hermes documente `AGENTS.md` comme contexte de projet et peut donner priorité à `.hermes.md` s’il existe. Le dépôt n’en crée pas afin de conserver une consigne portable. Lancer Hermes depuis la racine du projet, vérifier le contexte chargé puis ouvrir manuellement `MASTER_PROMPT.md` si l’agent ne l’a pas reçu. Ne jamais éditer `~/.hermes`, `SOUL.md`, une mémoire ou configuration globale pour ce projet. Voir la [documentation Hermes des context files](https://hermes-agent.nousresearch.com/docs/user-guide/features/context-files).

L’existence d’un fichier de consignes améliore la découverte ; elle ne force ni l’obéissance parfaite ni la lecture intégrale d’un document par tous les agents. Avant d’affirmer qu’un outil a suivi ces règles, demander/observer une preuve dans son contexte ou son résultat.

## Format de livraison obligatoire

À la fin de chaque tâche, fournir un rapport court et concret :

1. résultat obtenu et comportement visible ;
2. fichiers modifiés et raison ;
3. validations réellement exécutées, avec résultat ;
4. mesures réelles, hypothèses restantes et limitations matérielles/licences ;
5. risques, blocages réels et prochaine action sûre.

Pour une publication, indiquer la branche, commit, cible distante et confirmation obtenue. Pour une modification non publiée, ne pas suggérer qu’elle l’est. Pour une tâche documentaire, ne pas dire « testé » : indiquer la relecture, les liens et contrôles réalisés.

## Démarrage réutilisable

Utiliser ce texte comme amorce d’une session après avoir ouvert les fichiers cités :

> Travaille sur Karaok Party. Lis `AGENTS.md`, `SPECIFICATIONS_TECHNIQUES.md`, `MASTER_PROMPT.md` et l’état actuel du dépôt avant tout changement. Respecte tous les invariants : macOS, moteur RT Rust, Nothing Headphone (a) USB-C sélectionné explicitement, gestion de toutes routes Core Audio, musique qui pilote les paroles, analyse offline avant lecture, vraie correction de justesse/réverbération, stems locaux et confidentialité. Distingue fait, hypothèse, mesure et décision. Ne réintroduis pas iPhone/QR/WebRTC. Propose le plus petit jalon vérifiable, exécute les tests pertinents et rapporte preuves, limitations et travail restant. Ne modifie pas d’instruction supérieure ni de configuration globale.

Avant d’exécuter une action à impact externe (publication, téléchargement de modèle, nouvelle dépendance, migration irréversible, message), vérifier l’autorisation du demandeur et présenter le résultat concret lorsque cette autorisation est nécessaire.
