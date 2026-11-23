# Karaok Party — consignes de projet

Avant une tâche substantielle, lire :

1. [SPECIFICATIONS_TECHNIQUES.md](SPECIFICATIONS_TECHNIQUES.md) — comportement produit, architecture, critères d’acceptation et limites.
2. [MASTER_PROMPT.md](MASTER_PROMPT.md) — méthode de travail, sécurité, validation et règles agents.

Invariants : application macOS gratuite/open source, moteur audio temps réel Rust, Nothing Headphone (a) USB-C comme configuration de référence sélectionnée explicitement, musique qui pilote les paroles, analyse locale du morceau avant lecture, traitement live réel de la voix (correction de justesse, réverbération, gains/protection), fichiers locaux et confidentialité.

Ne pas réintroduire micro iPhone, application iOS, QR code, WebRTC ou serveur de signalisation. Ne pas inventer de mesure, d’API, de test ou de compatibilité. Préserver les changements existants, vérifier les licences des dépendances/modèles et ne pas effectuer de travail bloquant dans le callback audio.

`AGENTS.md` est reconnu par de nombreux agents, mais ne garantit pas que chacun charge automatiquement les deux documents liés. Les ouvrir explicitement lorsque le contexte de l’outil ne les contient pas.
