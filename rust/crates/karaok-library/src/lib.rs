//! Local SQLite persistence, safe versioned migrations, library, playlists, artifacts, and revisions (§9.2, §10, AC-06, AC-18).
//!
//! Safety invariants:
//! - Uses macOS Application Support for durable data & Caches for reconstructible stems/PCM (never hardcoded developer paths).
//! - WAL mode + `PRAGMA foreign_keys = ON` + short off-RT transactions.
//! - Creates a pre-migration backup file before applying schema migrations.
//! - Refuses cleanly (`ErrorCode::DatabaseNewerSchema`) if an existing DB has a schema version newer than supported;
//!   NEVER silently wipes or resets the user's library!

use chrono::{DateTime, Utc};
use karaok_types::{
    AnalysisJob, ArtifactKind, AudioPreference, ErrorCode, KaraokError, KaraokResult,
    LatencyProfile, LyricsDocument, MediaArtifact, MusicalScale, PitchClass, Playlist,
    PlaylistItem, Track, TrackAvailability, VocalActivitySummary,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const CURRENT_DB_SCHEMA_VERSION: u32 = 1;

/// Standard macOS storage directories resolved via OS APIs (§10).
#[derive(Debug, Clone)]
pub struct StoragePaths {
    pub app_support_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub db_path: PathBuf,
    pub originals_dir: PathBuf,
    pub models_dir: PathBuf,
    pub stems_cache_dir: PathBuf,
    pub pcm_cache_dir: PathBuf,
    pub backups_dir: PathBuf,
}

impl StoragePaths {
    /// Resolve default macOS `~/Library/Application Support/KaraokParty` and `~/Library/Caches/KaraokParty`.
    pub fn macos_default() -> KaraokResult<Self> {
        let data_root = dirs::data_dir().ok_or_else(|| {
            KaraokError::new(
                ErrorCode::IoError,
                "Impossible de localiser le dossier Application Support de macOS.",
            )
        })?;
        let cache_root = dirs::cache_dir().unwrap_or_else(|| data_root.clone());
        Self::from_roots(
            data_root.join("KaraokParty"),
            cache_root.join("KaraokParty"),
        )
    }

    /// Custom root constructor (used by isolated tests and portable CLI runs).
    pub fn from_roots(app_support_dir: PathBuf, cache_dir: PathBuf) -> KaraokResult<Self> {
        let paths = Self {
            db_path: app_support_dir.join("library.sqlite"),
            originals_dir: app_support_dir.join("originals"),
            models_dir: app_support_dir.join("models"),
            backups_dir: app_support_dir.join("backups"),
            stems_cache_dir: cache_dir.join("stems"),
            pcm_cache_dir: cache_dir.join("pcm"),
            app_support_dir,
            cache_dir,
        };
        for dir in [
            &paths.app_support_dir,
            &paths.originals_dir,
            &paths.models_dir,
            &paths.backups_dir,
            &paths.cache_dir,
            &paths.stems_cache_dir,
            &paths.pcm_cache_dir,
        ] {
            fs::create_dir_all(dir).map_err(|e| {
                KaraokError::new(
                    ErrorCode::IoError,
                    format!("Impossible de créer le répertoire local : {e}"),
                )
            })?;
        }
        Ok(paths)
    }
}

pub struct LibraryStore {
    conn: Connection,
    db_path: PathBuf,
}

impl LibraryStore {
    pub fn open(paths: &StoragePaths) -> KaraokResult<Self> {
        let mut conn = Connection::open(&paths.db_path).map_err(map_sql_err)?;
        // Prevent SQLITE_BUSY during concurrent analysis worker + UI writes (B4)
        conn.busy_timeout(std::time::Duration::from_millis(5_000))
            .map_err(map_sql_err)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(map_sql_err)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(map_sql_err)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(map_sql_err)?;

        let existing_version: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(map_sql_err)?;

        if existing_version > CURRENT_DB_SCHEMA_VERSION {
            return Err(KaraokError::with_hint(
                ErrorCode::DatabaseNewerSchema,
                format!(
                    "La bibliothèque utilise un schéma plus récent (v{existing_version}) que cette version de Karaok Party (v{CURRENT_DB_SCHEMA_VERSION}). Ouverture refusée pour protéger vos données."
                ),
                "Mettez à jour Karaok Party ou restaurez une sauvegarde depuis le dossier backups.",
            ));
        }

        if existing_version < CURRENT_DB_SCHEMA_VERSION {
            // Checkpoint WAL and require clean backup before migrating an existing DB (§10, AC-18, I8).
            if existing_version > 0 && paths.db_path.exists() {
                let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
                let ts = Utc::now().format("%Y%m%dT%H%M%SZ");
                let backup = paths
                    .backups_dir
                    .join(format!("library-pre-v{CURRENT_DB_SCHEMA_VERSION}-{ts}.sqlite"));
                fs::copy(&paths.db_path, &backup).map_err(|e| {
                    KaraokError::new(
                        ErrorCode::IoError,
                        format!("Sauvegarde pré-migration échouée (migration annulée pour protéger vos données) : {e}"),
                    )
                })?;
            }
            apply_migrations(&mut conn, existing_version)?;
        }

        Ok(Self {
            conn,
            db_path: paths.db_path.clone(),
        })
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn schema_version(&self) -> KaraokResult<u32> {
        self.conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(map_sql_err)
    }

    // ---- Tracks ----

    pub fn find_track_by_hash(&self, sha256: &str) -> KaraokResult<Option<Track>> {
        self.conn
            .query_row(
                "SELECT id, title, artist, album, duration_s, original_sha256, managed_original_path,
                        format, sample_rate_hz, channels, encoder_delay_samples, is_favorite,
                        availability, estimated_tonic, estimated_scale, key_confidence, key_ambiguous,
                        created_at, updated_at
                 FROM tracks WHERE original_sha256 = ?1 LIMIT 1",
                params![sha256],
                row_to_track,
            )
            .optional()
            .map_err(map_sql_err)
    }

    pub fn upsert_track(&self, track: &Track) -> KaraokResult<()> {
        let tonic_json = track
            .estimated_tonic
            .map(|t| serde_json::to_string(&t).unwrap_or_default());
        let scale_json = track
            .estimated_scale
            .map(|s| serde_json::to_string(&s).unwrap_or_default());
        let avail_str = match track.availability {
            TrackAvailability::Available => "available",
            TrackAvailability::MissingOriginal => "missing_original",
        };
        self.conn
            .execute(
                "INSERT INTO tracks (
                    id, title, artist, album, duration_s, original_sha256, managed_original_path,
                    format, sample_rate_hz, channels, encoder_delay_samples, is_favorite,
                    availability, estimated_tonic, estimated_scale, key_confidence, key_ambiguous,
                    created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)
                 ON CONFLICT(id) DO UPDATE SET
                    title = excluded.title,
                    artist = excluded.artist,
                    album = excluded.album,
                    duration_s = excluded.duration_s,
                    managed_original_path = excluded.managed_original_path,
                    is_favorite = excluded.is_favorite,
                    availability = excluded.availability,
                    estimated_tonic = excluded.estimated_tonic,
                    estimated_scale = excluded.estimated_scale,
                    key_confidence = excluded.key_confidence,
                    key_ambiguous = excluded.key_ambiguous,
                    updated_at = excluded.updated_at",
                params![
                    track.id.to_string(),
                    track.title,
                    track.artist,
                    track.album,
                    track.duration_s,
                    track.original_sha256,
                    track.managed_original_path,
                    track.format,
                    track.sample_rate_hz,
                    track.channels,
                    track.encoder_delay_samples,
                    track.is_favorite as i64,
                    avail_str,
                    tonic_json,
                    scale_json,
                    track.key_confidence,
                    track.key_ambiguous as i64,
                    track.created_at.to_rfc3339(),
                    track.updated_at.to_rfc3339(),
                ],
            )
            .map_err(map_sql_err)?;
        Ok(())
    }

    pub fn get_track(&self, id: Uuid) -> KaraokResult<Option<Track>> {
        self.conn
            .query_row(
                "SELECT id, title, artist, album, duration_s, original_sha256, managed_original_path,
                        format, sample_rate_hz, channels, encoder_delay_samples, is_favorite,
                        availability, estimated_tonic, estimated_scale, key_confidence, key_ambiguous,
                        created_at, updated_at
                 FROM tracks WHERE id = ?1",
                params![id.to_string()],
                row_to_track,
            )
            .optional()
            .map_err(map_sql_err)
    }

    pub fn list_tracks(&self, search_query: Option<&str>, favorites_only: bool) -> KaraokResult<Vec<Track>> {
        let mut tracks = Vec::new();
        let query = search_query.map(|q| {
            let escaped = q
                .trim()
                .to_lowercase()
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            format!("%{escaped}%")
        });
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, title, artist, album, duration_s, original_sha256, managed_original_path,
                        format, sample_rate_hz, channels, encoder_delay_samples, is_favorite,
                        availability, estimated_tonic, estimated_scale, key_confidence, key_ambiguous,
                        created_at, updated_at
                 FROM tracks
                 WHERE (?1 = 0 OR is_favorite = 1)
                   AND (?2 IS NULL
                        OR lower(title) LIKE ?2 ESCAPE '\\'
                        OR lower(artist) LIKE ?2 ESCAPE '\\'
                        OR lower(coalesce(album, '')) LIKE ?2 ESCAPE '\\')
                 ORDER BY updated_at DESC",
            )
            .map_err(map_sql_err)?;
        let rows = stmt
            .query_map(params![favorites_only as i64, query], row_to_track)
            .map_err(map_sql_err)?;
        for r in rows {
            tracks.push(r.map_err(map_sql_err)?);
        }
        Ok(tracks)
    }

    pub fn delete_track(&self, id: Uuid) -> KaraokResult<bool> {
        let rows = self
            .conn
            .execute("DELETE FROM tracks WHERE id = ?1", params![id.to_string()])
            .map_err(map_sql_err)?;
        Ok(rows > 0)
    }

    // ---- Playlists ----

    pub fn create_playlist(&self, name: &str) -> KaraokResult<Playlist> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(KaraokError::new(
                ErrorCode::InvalidParameter,
                "Le nom de la playlist ne peut pas être vide.",
            ));
        }
        let now = Utc::now();
        let pl = Playlist {
            id: Uuid::new_v4(),
            name: trimmed.to_string(),
            created_at: now,
            updated_at: now,
        };
        self.conn
            .execute(
                "INSERT INTO playlists (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?4)",
                params![
                    pl.id.to_string(),
                    pl.name,
                    pl.created_at.to_rfc3339(),
                    pl.updated_at.to_rfc3339()
                ],
            )
            .map_err(map_sql_err)?;
        Ok(pl)
    }

    pub fn list_playlists(&self) -> KaraokResult<Vec<Playlist>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, created_at, updated_at FROM playlists ORDER BY name ASC")
            .map_err(map_sql_err)?;
        let rows = stmt
            .query_map([], |row| {
                let id_str: String = row.get(0)?;
                let created_str: String = row.get(2)?;
                let updated_str: String = row.get(3)?;
                Ok(Playlist {
                    id: Uuid::parse_str(&id_str).unwrap_or_else(|_| Uuid::nil()),
                    name: row.get(1)?,
                    created_at: parse_dt(&created_str),
                    updated_at: parse_dt(&updated_str),
                })
            })
            .map_err(map_sql_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sql_err)
    }

    pub fn add_track_to_playlist(&self, playlist_id: Uuid, track_id: Uuid) -> KaraokResult<PlaylistItem> {
        let next_pos: i64 = self
            .conn
            .query_row(
                "SELECT coalesce(max(position), -1) + 1 FROM playlist_items WHERE playlist_id = ?1",
                params![playlist_id.to_string()],
                |row| row.get(0),
            )
            .map_err(map_sql_err)?;
        let item = PlaylistItem {
            id: Uuid::new_v4(),
            playlist_id,
            track_id,
            position: next_pos,
            added_at: Utc::now(),
        };
        self.conn
            .execute(
                "INSERT INTO playlist_items (id, playlist_id, track_id, position, added_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    item.id.to_string(),
                    item.playlist_id.to_string(),
                    item.track_id.to_string(),
                    item.position,
                    item.added_at.to_rfc3339()
                ],
            )
            .map_err(map_sql_err)?;
        Ok(item)
    }

    pub fn list_playlist_items(&self, playlist_id: Uuid) -> KaraokResult<Vec<PlaylistItem>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, playlist_id, track_id, position, added_at
                 FROM playlist_items WHERE playlist_id = ?1 ORDER BY position ASC",
            )
            .map_err(map_sql_err)?;
        let rows = stmt
            .query_map(params![playlist_id.to_string()], |row| {
                let id_s: String = row.get(0)?;
                let pl_s: String = row.get(1)?;
                let tr_s: String = row.get(2)?;
                let at_s: String = row.get(4)?;
                Ok(PlaylistItem {
                    id: Uuid::parse_str(&id_s).unwrap_or_else(|_| Uuid::nil()),
                    playlist_id: Uuid::parse_str(&pl_s).unwrap_or_else(|_| Uuid::nil()),
                    track_id: Uuid::parse_str(&tr_s).unwrap_or_else(|_| Uuid::nil()),
                    position: row.get(3)?,
                    added_at: parse_dt(&at_s),
                })
            })
            .map_err(map_sql_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sql_err)
    }

    /// Remove item from playlist without deleting the underlying track (§9.2).
    pub fn remove_playlist_item(&self, item_id: Uuid) -> KaraokResult<bool> {
        let n = self
            .conn
            .execute(
                "DELETE FROM playlist_items WHERE id = ?1",
                params![item_id.to_string()],
            )
            .map_err(map_sql_err)?;
        Ok(n > 0)
    }

    pub fn reorder_playlist_items(&mut self, playlist_id: Uuid, ordered_item_ids: &[Uuid]) -> KaraokResult<()> {
        let tx = self.conn.transaction().map_err(map_sql_err)?;
        for (idx, item_id) in ordered_item_ids.iter().enumerate() {
            tx.execute(
                "UPDATE playlist_items SET position = ?1 WHERE id = ?2 AND playlist_id = ?3",
                params![idx as i64, item_id.to_string(), playlist_id.to_string()],
            )
            .map_err(map_sql_err)?;
        }
        tx.commit().map_err(map_sql_err)?;
        Ok(())
    }

    // ---- Media Artifacts ----

    pub fn save_artifact(&self, artifact: &MediaArtifact) -> KaraokResult<()> {
        let kind_str = serde_json::to_string(&artifact.kind).unwrap_or_default();
        self.conn
            .execute(
                "INSERT INTO media_artifacts (
                    id, track_id, kind, cache_key, file_path, sha256, size_bytes, duration_s,
                    sample_rate_hz, channels, method, method_version, model_hash,
                    delay_compensation_samples, created_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                 ON CONFLICT(track_id, kind, cache_key) DO UPDATE SET
                    file_path = excluded.file_path,
                    sha256 = excluded.sha256,
                    size_bytes = excluded.size_bytes,
                    duration_s = excluded.duration_s,
                    created_at = excluded.created_at",
                params![
                    artifact.id.to_string(),
                    artifact.track_id.to_string(),
                    kind_str,
                    artifact.cache_key,
                    artifact.file_path,
                    artifact.sha256,
                    artifact.size_bytes as i64,
                    artifact.duration_s,
                    artifact.sample_rate_hz,
                    artifact.channels,
                    artifact.method,
                    artifact.method_version,
                    artifact.model_hash,
                    artifact.delay_compensation_samples,
                    artifact.created_at.to_rfc3339(),
                ],
            )
            .map_err(map_sql_err)?;
        Ok(())
    }

    pub fn find_artifact(
        &self,
        track_id: Uuid,
        kind: ArtifactKind,
        cache_key: Option<&str>,
    ) -> KaraokResult<Option<MediaArtifact>> {
        let kind_str = serde_json::to_string(&kind).unwrap_or_default();
        let res = if let Some(key) = cache_key {
            self.conn
                .query_row(
                    "SELECT id, track_id, kind, cache_key, file_path, sha256, size_bytes, duration_s,
                            sample_rate_hz, channels, method, method_version, model_hash,
                            delay_compensation_samples, created_at
                     FROM media_artifacts
                     WHERE track_id = ?1 AND kind = ?2 AND cache_key = ?3
                     ORDER BY created_at DESC LIMIT 1",
                    params![track_id.to_string(), kind_str, key],
                    row_to_artifact,
                )
                .optional()
                .map_err(map_sql_err)?
        } else {
            self.conn
                .query_row(
                    "SELECT id, track_id, kind, cache_key, file_path, sha256, size_bytes, duration_s,
                            sample_rate_hz, channels, method, method_version, model_hash,
                            delay_compensation_samples, created_at
                     FROM media_artifacts
                     WHERE track_id = ?1 AND kind = ?2
                     ORDER BY created_at DESC LIMIT 1",
                    params![track_id.to_string(), kind_str],
                    row_to_artifact,
                )
                .optional()
                .map_err(map_sql_err)?
        };
        Ok(res)
    }

    pub fn list_artifacts_for_track(&self, track_id: Uuid) -> KaraokResult<Vec<MediaArtifact>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, track_id, kind, cache_key, file_path, sha256, size_bytes, duration_s,
                        sample_rate_hz, channels, method, method_version, model_hash,
                        delay_compensation_samples, created_at
                 FROM media_artifacts WHERE track_id = ?1 ORDER BY created_at DESC",
            )
            .map_err(map_sql_err)?;
        let rows = stmt
            .query_map(params![track_id.to_string()], row_to_artifact)
            .map_err(map_sql_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sql_err)
    }

    // ---- Analysis Jobs, Vocal Activity, Lyrics Revisions ----

    pub fn save_analysis_job(&self, job: &AnalysisJob) -> KaraokResult<()> {
        let payload = serde_json::to_string(job).map_err(map_serde_err)?;
        self.conn
            .execute(
                "INSERT INTO analysis_jobs (id, track_id, state, payload_json, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(id) DO UPDATE SET
                    state = excluded.state,
                    payload_json = excluded.payload_json,
                    updated_at = excluded.updated_at",
                params![
                    job.id.to_string(),
                    job.track_id.to_string(),
                    format!("{:?}", job.state),
                    payload,
                    job.updated_at.to_rfc3339()
                ],
            )
            .map_err(map_sql_err)?;
        Ok(())
    }

    pub fn latest_analysis_job(&self, track_id: Uuid) -> KaraokResult<Option<AnalysisJob>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT payload_json FROM analysis_jobs WHERE track_id = ?1 ORDER BY updated_at DESC LIMIT 1",
                params![track_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sql_err)?;
        raw.map(|s| serde_json::from_str(&s).map_err(map_serde_err))
            .transpose()
    }

    pub fn save_vocal_activity(&self, summary: &VocalActivitySummary) -> KaraokResult<()> {
        let payload = serde_json::to_string(summary).map_err(map_serde_err)?;
        self.conn
            .execute(
                "INSERT INTO vocal_activity (track_id, payload_json, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(track_id) DO UPDATE SET
                    payload_json = excluded.payload_json,
                    updated_at = excluded.updated_at",
                params![summary.track_id.to_string(), payload, Utc::now().to_rfc3339()],
            )
            .map_err(map_sql_err)?;
        Ok(())
    }

    pub fn get_vocal_activity(&self, track_id: Uuid) -> KaraokResult<Option<VocalActivitySummary>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT payload_json FROM vocal_activity WHERE track_id = ?1",
                params![track_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sql_err)?;
        raw.map(|s| serde_json::from_str(&s).map_err(map_serde_err))
            .transpose()
    }

    /// Save lyrics revision with strict monotonic revision check (prevents stale job overwriting newer human edit §7.6).
    pub fn save_lyrics_document(&self, doc: &LyricsDocument) -> KaraokResult<()> {
        let existing_rev: Option<u64> = self
            .conn
            .query_row(
                "SELECT max(text_revision) FROM lyrics_documents WHERE track_id = ?1",
                params![doc.track_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sql_err)?
            .flatten();

        if let Some(cur) = existing_rev {
            if doc.text_revision < cur {
                return Err(KaraokError::new(
                    ErrorCode::StaleRevision,
                    format!(
                        "Révision de paroles obsolète ({} < révision courante {cur}) ignorée.",
                        doc.text_revision
                    ),
                ));
            }
        }

        let payload = serde_json::to_string(doc).map_err(map_serde_err)?;
        self.conn
            .execute(
                "INSERT INTO lyrics_documents (track_id, text_revision, text_sha256, payload_json, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(track_id, text_revision) DO UPDATE SET
                    text_sha256 = excluded.text_sha256,
                    payload_json = excluded.payload_json,
                    updated_at = excluded.updated_at",
                params![
                    doc.track_id.to_string(),
                    doc.text_revision as i64,
                    doc.text_sha256,
                    payload,
                    doc.updated_at.to_rfc3339()
                ],
            )
            .map_err(map_sql_err)?;
        Ok(())
    }

    pub fn get_latest_lyrics(&self, track_id: Uuid) -> KaraokResult<Option<LyricsDocument>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT payload_json FROM lyrics_documents WHERE track_id = ?1 ORDER BY text_revision DESC LIMIT 1",
                params![track_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sql_err)?;
        raw.map(|s| serde_json::from_str(&s).map_err(map_serde_err))
            .transpose()
    }

    // ---- Audio Preferences & Latency Profiles ----

    pub fn save_audio_preference(&self, pref: &AudioPreference) -> KaraokResult<()> {
        let payload = serde_json::to_string(pref).map_err(map_serde_err)?;
        self.conn
            .execute(
                "INSERT INTO app_kv (key, value_json, updated_at) VALUES ('audio_preference', ?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json, updated_at = excluded.updated_at",
                params![payload, Utc::now().to_rfc3339()],
            )
            .map_err(map_sql_err)?;
        Ok(())
    }

    pub fn load_audio_preference(&self) -> KaraokResult<AudioPreference> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value_json FROM app_kv WHERE key = 'audio_preference'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_sql_err)?;
        match raw {
            Some(s) => {
                let mut pref: AudioPreference = serde_json::from_str(&s).map_err(map_serde_err)?;
                // Safety invariant: NEVER auto-enable live monitoring across restarts (§3.1, §8.3).
                pref.voice_dsp.monitoring_enabled = false;
                pref.voice_dsp = pref.voice_dsp.clamped();
                pref.mix = pref.mix.clamped();
                Ok(pref)
            }
            None => Ok(AudioPreference::default()),
        }
    }

    pub fn save_latency_profile(&self, profile: &LatencyProfile) -> KaraokResult<()> {
        let payload = serde_json::to_string(profile).map_err(map_serde_err)?;
        self.conn
            .execute(
                "INSERT INTO latency_profiles (id, route_fingerprint, payload_json, valid, measured_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(route_fingerprint) DO UPDATE SET
                    id = excluded.id,
                    payload_json = excluded.payload_json,
                    valid = excluded.valid,
                    measured_at = excluded.measured_at",
                params![
                    profile.id.to_string(),
                    profile.route_fingerprint,
                    payload,
                    profile.valid as i64,
                    profile.measured_at.to_rfc3339()
                ],
            )
            .map_err(map_sql_err)?;
        Ok(())
    }

    pub fn list_latency_profiles(&self) -> KaraokResult<Vec<LatencyProfile>> {
        let mut stmt = self
            .conn
            .prepare("SELECT payload_json FROM latency_profiles ORDER BY measured_at DESC")
            .map_err(map_sql_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(map_sql_err)?;
        let mut out = Vec::new();
        for r in rows {
            let raw = r.map_err(map_sql_err)?;
            if let Ok(p) = serde_json::from_str(&raw) {
                out.push(p);
            }
        }
        Ok(out)
    }

    /// Purge reconstructible cache rows (PCM/stems) ONLY — NEVER touches originals, lyrics, or annotations (§7.6).
    pub fn purge_reconstructible_cache(&self, paths: &StoragePaths) -> KaraokResult<u64> {
        let deleted = self
            .conn
            .execute(
                "DELETE FROM media_artifacts WHERE kind IN ('\"canonical_pcm\"', '\"vocal_stem\"', '\"accompaniment_stem\"')",
                [],
            )
            .map_err(map_sql_err)? as u64;
        for dir in [&paths.stems_cache_dir, &paths.pcm_cache_dir] {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        Ok(deleted)
    }
}

fn apply_migrations(conn: &mut Connection, from_version: u32) -> KaraokResult<()> {
    if from_version == 0 {
        let tx = conn.transaction().map_err(map_sql_err)?;
        tx.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS tracks (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                artist TEXT NOT NULL,
                album TEXT,
                duration_s REAL NOT NULL,
                original_sha256 TEXT NOT NULL,
                managed_original_path TEXT NOT NULL,
                format TEXT NOT NULL,
                sample_rate_hz INTEGER NOT NULL,
                channels INTEGER NOT NULL,
                encoder_delay_samples INTEGER NOT NULL DEFAULT 0,
                is_favorite INTEGER NOT NULL DEFAULT 0,
                availability TEXT NOT NULL DEFAULT 'available',
                estimated_tonic TEXT,
                estimated_scale TEXT,
                key_confidence REAL,
                key_ambiguous INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_tracks_sha256 ON tracks(original_sha256);
            CREATE INDEX IF NOT EXISTS idx_tracks_updated ON tracks(updated_at);

            CREATE TABLE IF NOT EXISTS playlists (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS playlist_items (
                id TEXT PRIMARY KEY,
                playlist_id TEXT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
                track_id TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
                position INTEGER NOT NULL,
                added_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_playlist_items_order ON playlist_items(playlist_id, position);

            CREATE TABLE IF NOT EXISTS media_artifacts (
                id TEXT PRIMARY KEY,
                track_id TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
                kind TEXT NOT NULL,
                cache_key TEXT NOT NULL,
                file_path TEXT NOT NULL,
                sha256 TEXT NOT NULL,
                size_bytes INTEGER NOT NULL,
                duration_s REAL NOT NULL,
                sample_rate_hz INTEGER NOT NULL,
                channels INTEGER NOT NULL,
                method TEXT NOT NULL,
                method_version TEXT NOT NULL,
                model_hash TEXT,
                delay_compensation_samples INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                UNIQUE(track_id, kind, cache_key)
            );

            CREATE TABLE IF NOT EXISTS analysis_jobs (
                id TEXT PRIMARY KEY,
                track_id TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
                state TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS vocal_activity (
                track_id TEXT PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
                payload_json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS lyrics_documents (
                track_id TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
                text_revision INTEGER NOT NULL,
                text_sha256 TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY(track_id, text_revision)
            );

            CREATE TABLE IF NOT EXISTS latency_profiles (
                id TEXT PRIMARY KEY,
                route_fingerprint TEXT NOT NULL UNIQUE,
                payload_json TEXT NOT NULL,
                valid INTEGER NOT NULL DEFAULT 1,
                measured_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS app_kv (
                key TEXT PRIMARY KEY,
                value_json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            ",
        )
        .map_err(map_sql_err)?;
        tx.pragma_update(None, "user_version", CURRENT_DB_SCHEMA_VERSION)
            .map_err(map_sql_err)?;
        tx.commit().map_err(map_sql_err)?;
    }
    Ok(())
}

fn row_to_track(row: &rusqlite::Row<'_>) -> rusqlite::Result<Track> {
    let id_s: String = row.get(0)?;
    let avail_s: String = row.get(12)?;
    let tonic_s: Option<String> = row.get(13)?;
    let scale_s: Option<String> = row.get(14)?;
    let created_s: String = row.get(17)?;
    let updated_s: String = row.get(18)?;

    Ok(Track {
        id: Uuid::parse_str(&id_s).unwrap_or_else(|_| Uuid::nil()),
        title: row.get(1)?,
        artist: row.get(2)?,
        album: row.get(3)?,
        duration_s: row.get(4)?,
        original_sha256: row.get(5)?,
        managed_original_path: row.get(6)?,
        format: row.get(7)?,
        sample_rate_hz: row.get(8)?,
        channels: row.get(9)?,
        encoder_delay_samples: row.get(10)?,
        is_favorite: row.get::<_, i64>(11)? != 0,
        availability: if avail_s == "missing_original" {
            TrackAvailability::MissingOriginal
        } else {
            TrackAvailability::Available
        },
        estimated_tonic: tonic_s.and_then(|s| serde_json::from_str::<PitchClass>(&s).ok()),
        estimated_scale: scale_s.and_then(|s| serde_json::from_str::<MusicalScale>(&s).ok()),
        key_confidence: row.get(15)?,
        key_ambiguous: row.get::<_, i64>(16)? != 0,
        created_at: parse_dt(&created_s),
        updated_at: parse_dt(&updated_s),
    })
}

fn row_to_artifact(row: &rusqlite::Row<'_>) -> rusqlite::Result<MediaArtifact> {
    let id_s: String = row.get(0)?;
    let track_s: String = row.get(1)?;
    let kind_s: String = row.get(2)?;
    let created_s: String = row.get(14)?;
    Ok(MediaArtifact {
        id: Uuid::parse_str(&id_s).unwrap_or_else(|_| Uuid::nil()),
        track_id: Uuid::parse_str(&track_s).unwrap_or_else(|_| Uuid::nil()),
        kind: serde_json::from_str(&kind_s).unwrap_or(ArtifactKind::Original),
        cache_key: row.get(3)?,
        file_path: row.get(4)?,
        sha256: row.get(5)?,
        size_bytes: row.get::<_, i64>(6)?.max(0) as u64,
        duration_s: row.get(7)?,
        sample_rate_hz: row.get(8)?,
        channels: row.get(9)?,
        method: row.get(10)?,
        method_version: row.get(11)?,
        model_hash: row.get(12)?,
        delay_compensation_samples: row.get(13)?,
        created_at: parse_dt(&created_s),
    })
}

fn parse_dt(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn map_sql_err(e: rusqlite::Error) -> KaraokError {
    KaraokError::new(
        ErrorCode::DatabaseError,
        format!("Erreur base de données SQLite : {e}"),
    )
}

fn map_serde_err(e: serde_json::Error) -> KaraokError {
    KaraokError::new(
        ErrorCode::InternalError,
        format!("Erreur de sérialisation interne : {e}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use karaok_types::{LyricsGranularity, ReviewStatus, SCHEMA_VERSION};
    use tempfile::tempdir;

    fn sample_track() -> Track {
        let now = Utc::now();
        Track {
            id: Uuid::new_v4(),
            title: "Lumière".into(),
            artist: "Test Artist".into(),
            album: Some("Fixture".into()),
            duration_s: 120.0,
            original_sha256: "abc123".into(),
            managed_original_path: "/tmp/original.wav".into(),
            format: "wav".into(),
            sample_rate_hz: 48_000,
            channels: 2,
            encoder_delay_samples: 0,
            is_favorite: false,
            availability: TrackAvailability::Available,
            estimated_tonic: Some(PitchClass::A),
            estimated_scale: Some(MusicalScale::NaturalMinor),
            key_confidence: Some(0.81),
            key_ambiguous: false,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn tracks_playlists_revisions_and_newer_schema_refusal() {
        let dir = tempdir().unwrap();
        let paths = StoragePaths::from_roots(dir.path().join("app"), dir.path().join("cache")).unwrap();
        let store = LibraryStore::open(&paths).unwrap();
        assert_eq!(store.schema_version().unwrap(), CURRENT_DB_SCHEMA_VERSION);

        let track = sample_track();
        store.upsert_track(&track).unwrap();
        assert_eq!(store.list_tracks(Some("lumi"), false).unwrap().len(), 1);

        let pl = store.create_playlist("Soirée").unwrap();
        let item = store.add_track_to_playlist(pl.id, track.id).unwrap();
        assert_eq!(store.list_playlist_items(pl.id).unwrap().len(), 1);
        store.remove_playlist_item(item.id).unwrap();
        // Removing from playlist must NOT delete track!
        assert!(store.get_track(track.id).unwrap().is_some());

        // Stale lyrics revision is rejected cleanly
        let doc2 = LyricsDocument {
            schema_version: SCHEMA_VERSION,
            track_id: track.id,
            timebase: "original_decoded_audio_seconds".into(),
            text_revision: 2,
            text_sha256: "h2".into(),
            raw_text: "v2".into(),
            language: Some("fr".into()),
            provenance: "user".into(),
            granularity: LyricsGranularity::Line,
            global_offset_ms: 0,
            status: ReviewStatus::UserValidated,
            first_vocal_activity_s: None,
            first_sung_activity_s: None,
            first_lyric_cue_s: None,
            warnings: vec![],
            cues: vec![],
            updated_at: Utc::now(),
        };
        store.save_lyrics_document(&doc2).unwrap();
        let mut stale = doc2.clone();
        stale.text_revision = 1;
        assert_eq!(
            store.save_lyrics_document(&stale).unwrap_err().code,
            ErrorCode::StaleRevision
        );

        // Simulate future DB schema v99: must refuse cleanly without wiping!
        store
            .conn
            .pragma_update(None, "user_version", 99u32)
            .unwrap();
        drop(store);
        let reopen_err = LibraryStore::open(&paths).err().unwrap();
        assert_eq!(reopen_err.code, ErrorCode::DatabaseNewerSchema);
    }

    #[test]
    fn monitoring_always_resets_to_disabled_on_load() {
        let dir = tempdir().unwrap();
        let paths = StoragePaths::from_roots(dir.path().join("app"), dir.path().join("cache")).unwrap();
        let store = LibraryStore::open(&paths).unwrap();
        let mut pref = AudioPreference::default();
        pref.voice_dsp.monitoring_enabled = true;
        store.save_audio_preference(&pref).unwrap();
        let loaded = store.load_audio_preference().unwrap();
        assert!(!loaded.voice_dsp.monitoring_enabled);
    }
}
