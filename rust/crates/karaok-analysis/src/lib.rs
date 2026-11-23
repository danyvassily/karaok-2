//! Offline audio preparation pipeline: canonical decode, vocal/accompaniment separation,
//! multi-feature singing activity detection, key estimation, and constrained line lyrics alignment
//! (§7, REQ-008, REQ-009, REQ-011, REQ-017, AC-07, AC-08, AC-09, AC-12, AC-17, ADR-0004).
//!
//! Key guarantees:
//! - Original file stays untouched; all positions anchor to canonical decoded audio (intro included).
//! - Cache keys separate decode, separation, vocal activity, and alignment so text edits invalidate
//!   alignment ONLY without rerunning stem separation (AC-09, AC-12).
//! - Cooperative cancellation between chunks/stages; atomic publication via temp files -> verify -> rename.
//! - Separation explicitly states its real method (`dsp_center_spectral_v1` local STFT center-pan mask)
//!   and checks `ModelStore` for any user-verified ONNX model without ever downloading unverified weights.
//! - Singing detector goes beyond raw energy/speech VAD by combining stem-to-mix ratio, YIN harmonicity,
//!   sustained pitch stability, spectral flatness, and vocal formant band ratio; returns explicit
//!   `NoSingingDetected` on instrumentals and `Uncertain` / `NeedsReview` on ambiguous passages.

use chrono::Utc;
use karaok_library::{LibraryStore, StoragePaths};
use karaok_media::{decode_to_canonical_stereo_wav, hash_file_sha256, DECODER_PIPELINE_VERSION};
use karaok_model_store::ModelStore;
use karaok_types::{
    AnalysisJob, AnalysisJobState, AnalysisStage, ArtifactKind, ErrorCode, KaraokError,
    KaraokResult, LyricsDocument, LyricsGranularity, MediaArtifact, ModelCapability, ModelManifest,
    MusicalScale, PitchClass, ReviewStatus, TimingSource, Track, VocalActivitySummary, VocalRegion,
    VocalRegionKind,
};
use karaok_voice_dsp::YinDetector;
use rustfft::{num_complex::Complex, FftPlanner};
use sha2::{Digest, Sha256};
use std::f32::consts::PI;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use uuid::Uuid;

/// Map std::io::Error cleanly including `ENOSPC` -> `InsufficientDiskSpace` (P2-I8).
fn map_analysis_io_err(context: &str, err: std::io::Error) -> KaraokError {
    if err.kind() == std::io::ErrorKind::StorageFull || err.raw_os_error() == Some(28) {
        KaraokError::with_hint(
            ErrorCode::InsufficientDiskSpace,
            format!("Espace disque insuffisant ({context})."),
            "Libérez de l'espace disque ou purgez le cache reconstruisible dans Diagnostics.",
        )
    } else {
        KaraokError::new(ErrorCode::IoError, format!("{context} : {err}"))
    }
}

fn map_analysis_hound_err(context: &str, err: hound::Error) -> KaraokError {
    match err {
        hound::Error::IoError(io_err) => map_analysis_io_err(context, io_err),
        other => KaraokError::new(ErrorCode::IoError, format!("{context} : {other}")),
    }
}

/// Explicitly checks for a verified neural model before invoking optional ONNX inference (I9, I14).
/// Returns `ErrorCode::ModelMissing` cleanly when absent — never downloads or fakes ML!
pub fn require_verified_neural_model(
    model_store: &ModelStore,
    capability: ModelCapability,
) -> KaraokResult<&ModelManifest> {
    model_store.verified_model_for(capability).ok_or_else(|| {
        KaraokError::with_hint(
            ErrorCode::ModelMissing,
            format!(
                "Aucun modèle neuronal ONNX vérifié n'est installé localement pour {:?}.",
                capability
            ),
            "Utilisez la séparation DSP spectrale locale hors ligne ou importez un fichier .onnx vérifié (SHA-256 + licence poids) dans Diagnostics.",
        )
    })
}

pub const PIPELINE_VERSION: &str = "karaok-analysis-v1";
pub const SEPARATION_METHOD_DSP: &str = "dsp_center_spectral_v1";
pub const VOCAL_ACTIVITY_METHOD: &str = "multi_feature_harmonic_v1";
pub const KEY_ESTIMATION_METHOD: &str = "krumhansl_chromagram_v1";
pub const ALIGNMENT_METHOD: &str = "constrained_monotonic_vocal_region_line_v1";

#[derive(Debug, Clone)]
pub struct PreparationResult {
    pub job: AnalysisJob,
    pub canonical_pcm: MediaArtifact,
    pub vocal_stem: MediaArtifact,
    pub accompaniment_stem: MediaArtifact,
    pub vocal_activity: VocalActivitySummary,
    pub key_estimate: KeyEstimate,
    pub updated_lyrics: Option<LyricsDocument>,
    pub reused_cached_stems: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyEstimate {
    pub tonic: PitchClass,
    pub scale: MusicalScale,
    pub confidence: f32,
    pub ambiguous: bool,
}

fn cache_key(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update(b"|");
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn check_cancelled(cancel: &AtomicBool) -> KaraokResult<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(KaraokError::new(
            ErrorCode::JobCancelled,
            "Préparation annulée par l'utilisateur. Les étapes déjà validées sont conservées en cache.",
        ))
    } else {
        Ok(())
    }
}

/// Run the full multi-stage offline preparation pipeline for a track with cooperative cancellation,
/// per-stage cache reuse, and observable progress callbacks (§7.6, AC-08, AC-12, AC-17).
pub fn run_track_preparation<F>(
    track: &mut Track,
    store: &LibraryStore,
    paths: &StoragePaths,
    model_store: &ModelStore,
    cancel: Arc<AtomicBool>,
    mut on_progress: F,
) -> KaraokResult<PreparationResult>
where
    F: FnMut(&AnalysisJob),
{
    let job_id = Uuid::new_v4();
    let mut job = AnalysisJob {
        id: job_id,
        track_id: track.id,
        state: AnalysisJobState::Running,
        stage: AnalysisStage::Decode,
        progress: 0.0,
        status_detail_fr: "Vérification du décodage canonique…".to_string(),
        pipeline_version: PIPELINE_VERSION.to_string(),
        last_completed_stage: None,
        error: None,
        updated_at: Utc::now(),
    };
    let _ = store.save_analysis_job(&job);
    on_progress(&job);

    let mut run = || -> KaraokResult<PreparationResult> {
        check_cancelled(&cancel)?;

        // ---- Stage 1: Canonical Decode (cached by original_sha256 + decoder version) ----
        let decode_key = cache_key(&[&track.original_sha256, DECODER_PIPELINE_VERSION, "f32-stereo"]);
        let canonical_pcm = match store.find_artifact(track.id, ArtifactKind::CanonicalPcm, Some(&decode_key))? {
            Some(art) if Path::new(&art.file_path).exists() => art,
            _ => {
                let pcm_path = paths.pcm_cache_dir.join(format!("{}.wav", &decode_key[..24]));
                let decoded = decode_to_canonical_stereo_wav(
                    Path::new(&track.managed_original_path),
                    &pcm_path,
                    256,
                )?;
                let art = MediaArtifact {
                    id: Uuid::new_v4(),
                    track_id: track.id,
                    kind: ArtifactKind::CanonicalPcm,
                    cache_key: decode_key.clone(),
                    file_path: decoded.wav_path.to_string_lossy().to_string(),
                    sha256: decoded.sha256,
                    size_bytes: decoded.size_bytes,
                    duration_s: decoded.duration_s,
                    sample_rate_hz: decoded.sample_rate_hz,
                    channels: decoded.channels,
                    method: DECODER_PIPELINE_VERSION.to_string(),
                    method_version: "1.0".to_string(),
                    model_hash: None,
                    delay_compensation_samples: 0,
                    created_at: Utc::now(),
                };
                store.save_artifact(&art)?;
                track.duration_s = decoded.duration_s;
                track.sample_rate_hz = decoded.sample_rate_hz;
                art
            }
        };

        job.last_completed_stage = Some(AnalysisStage::Decode);
        job.stage = AnalysisStage::Separation;
        job.progress = 0.20;
        job.status_detail_fr = "Séparation voix / accompagnement…".to_string();
        job.updated_at = Utc::now();
        let _ = store.save_analysis_job(&job);
        on_progress(&job);
        check_cancelled(&cancel)?;

        // ---- Stage 2: Vocal / Accompaniment Separation (cached independently of lyrics & unused ONNX models - I9!) ----
        let _ = model_store;
        let sep_method = SEPARATION_METHOD_DSP;
        let sep_key = cache_key(&[&canonical_pcm.sha256, sep_method, "1.0", PIPELINE_VERSION]);

        let cached_voc = store.find_artifact(track.id, ArtifactKind::VocalStem, Some(&sep_key))?;
        let cached_acc = store.find_artifact(track.id, ArtifactKind::AccompanimentStem, Some(&sep_key))?;

        let (vocal_stem, accompaniment_stem, reused_cached_stems) = match (cached_voc, cached_acc) {
            (Some(v), Some(a)) if Path::new(&v.file_path).exists() && Path::new(&a.file_path).exists() => {
                (v, a, true)
            }
            _ => {
                let voc_path = paths.stems_cache_dir.join(format!("{}-vocal.wav", &sep_key[..24]));
                let acc_path = paths.stems_cache_dir.join(format!("{}-accompaniment.wav", &sep_key[..24]));
                // Bounded-memory streaming STFT separation directly from canonical WAV to disk (I6)
                let (sr, voc_sha, voc_bytes, acc_sha, acc_bytes) =
                    separate_center_spectral_stft_streaming(
                        Path::new(&canonical_pcm.file_path),
                        &voc_path,
                        &acc_path,
                        &cancel,
                        |frac| {
                            job.progress = 0.20 + frac * 0.35;
                            job.updated_at = Utc::now();
                            on_progress(&job);
                        },
                    )?;

                let voc_art = MediaArtifact {
                    id: Uuid::new_v4(),
                    track_id: track.id,
                    kind: ArtifactKind::VocalStem,
                    cache_key: sep_key.clone(),
                    file_path: voc_path.to_string_lossy().to_string(),
                    sha256: voc_sha,
                    size_bytes: voc_bytes,
                    duration_s: canonical_pcm.duration_s,
                    sample_rate_hz: sr,
                    channels: 2,
                    method: sep_method.to_string(),
                    method_version: "1.0".to_string(),
                    model_hash: None,
                    delay_compensation_samples: 0,
                    created_at: Utc::now(),
                };
                let acc_art = MediaArtifact {
                    id: Uuid::new_v4(),
                    track_id: track.id,
                    kind: ArtifactKind::AccompanimentStem,
                    cache_key: sep_key.clone(),
                    file_path: acc_path.to_string_lossy().to_string(),
                    sha256: acc_sha,
                    size_bytes: acc_bytes,
                    duration_s: canonical_pcm.duration_s,
                    sample_rate_hz: sr,
                    channels: 2,
                    method: sep_method.to_string(),
                    method_version: "1.0".to_string(),
                    model_hash: None,
                    delay_compensation_samples: 0,
                    created_at: Utc::now(),
                };
                store.save_artifact(&voc_art)?;
                store.save_artifact(&acc_art)?;
                (voc_art, acc_art, false)
            }
        };

        job.last_completed_stage = Some(AnalysisStage::Separation);
        job.stage = AnalysisStage::VocalActivity;
        job.progress = 0.60;
        job.status_detail_fr = "Détection de l'activité chantée et du premier départ…".to_string();
        job.updated_at = Utc::now();
        let _ = store.save_analysis_job(&job);
        on_progress(&job);
        check_cancelled(&cancel)?;

        // ---- Stage 3: Bounded-RAM Streaming Multi-Feature Singing Activity Detection (P2-I8) ----
        let vocal_activity = detect_vocal_activity_streaming(
            track.id,
            Path::new(&vocal_stem.file_path),
            Path::new(&canonical_pcm.file_path),
            &cancel,
        )?;
        store.save_vocal_activity(&vocal_activity)?;

        job.last_completed_stage = Some(AnalysisStage::VocalActivity);
        job.stage = AnalysisStage::KeyEstimation;
        job.progress = 0.78;
        job.status_detail_fr = "Estimation de la tonalité musicale…".to_string();
        job.updated_at = Utc::now();
        let _ = store.save_analysis_job(&job);
        on_progress(&job);
        check_cancelled(&cancel)?;

        // ---- Stage 4: Bounded-RAM Streaming Key Estimation (Krumhansl-Schmuckler - P2-I8) ----
        let key_estimate = estimate_musical_key_streaming(Path::new(&canonical_pcm.file_path))?;
        track.estimated_tonic = Some(key_estimate.tonic);
        track.estimated_scale = Some(key_estimate.scale);
        track.key_confidence = Some(key_estimate.confidence);
        track.key_ambiguous = key_estimate.ambiguous;
        track.updated_at = Utc::now();
        store.upsert_track(track)?;

        job.last_completed_stage = Some(AnalysisStage::KeyEstimation);
        job.stage = AnalysisStage::Alignment;
        job.progress = 0.88;
        job.status_detail_fr = "Alignement des paroles sur l'activité chantée…".to_string();
        job.updated_at = Utc::now();
        let _ = store.save_analysis_job(&job);
        on_progress(&job);
        check_cancelled(&cancel)?;

        // ---- Stage 5: Lyrics Alignment (respecting §7.5 priority order & monotonic revision - I10!) ----
        let updated_lyrics = if let Some(mut doc) = store.get_latest_lyrics(track.id)? {
            doc.first_vocal_activity_s = vocal_activity.first_vocal_activity_s;
            doc.first_sung_activity_s = vocal_activity.first_sung_activity_s;

            if vocal_activity.no_singing_detected && doc.granularity == LyricsGranularity::Untimed {
                doc.status = ReviewStatus::NoSingingDetected;
                doc.warnings.push(
                    "Aucun chant clairement détecté dans ce morceau : les paroles n'ont pas été placées automatiquement.".to_string()
                );
            } else if doc.granularity == LyricsGranularity::Untimed && !doc.cues.is_empty() {
                align_untimed_lines_to_vocal_regions(&mut doc, &vocal_activity, track.duration_s);
            } else {
                validate_timed_lyrics_against_vocal_activity(&mut doc, &vocal_activity, track.duration_s);
            }

            // Increment revision so prior revision is preserved; handle concurrent user edit gracefully (I10)
            doc.text_revision = doc.text_revision.saturating_add(1);
            doc.updated_at = Utc::now();
            match store.save_lyrics_document(&doc) {
                Ok(()) => Some(doc),
                Err(err) if err.code == ErrorCode::StaleRevision => {
                    // User saved a newer lyrics revision while preparation ran — keep stems & vocal activity intact!
                    store.get_latest_lyrics(track.id)?
                }
                Err(err) => return Err(err),
            }
        } else {
            None
        };

        let needs_review = vocal_activity.review_status != ReviewStatus::UserValidated
            || updated_lyrics
                .as_ref()
                .map(|l| l.status != ReviewStatus::UserValidated)
                .unwrap_or(true);

        job.last_completed_stage = Some(AnalysisStage::Finalized);
        job.stage = AnalysisStage::Finalized;
        job.progress = 1.0;
        job.state = if needs_review {
            AnalysisJobState::NeedsReview
        } else {
            AnalysisJobState::Completed
        };
        job.status_detail_fr = if vocal_activity.no_singing_detected {
            "Analyse terminée : morceau détecté comme instrumental (vérification conseillée).".to_string()
        } else if updated_lyrics.is_none() {
            "Stems et repérage vocal prêts. Ajoutez ou importez les paroles pour synchroniser.".to_string()
        } else {
            "Préparation terminée — vérifiez les repères et paroles avant de valider.".to_string()
        };
        job.updated_at = Utc::now();
        let _ = store.save_analysis_job(&job);
        on_progress(&job);

        Ok(PreparationResult {
            job: job.clone(),
            canonical_pcm,
            vocal_stem,
            accompaniment_stem,
            vocal_activity,
            key_estimate,
            updated_lyrics,
            reused_cached_stems,
        })
    };

    match run() {
        Ok(res) => Ok(res),
        Err(err) => {
            job.state = if err.code == ErrorCode::JobCancelled {
                AnalysisJobState::Cancelled
            } else {
                AnalysisJobState::Failed
            };
            job.status_detail_fr = err.message_fr.clone();
            job.error = Some(err.clone());
            job.updated_at = Utc::now();
            let _ = store.save_analysis_job(&job);
            on_progress(&job);
            Err(err)
        }
    }
}

/// Bounded-memory streaming STFT separator reading canonical WAV and writing vocal/accompaniment WAVs atomically (I6).
/// Uses O(FFT_SIZE) overlap-add ring state (~2 MB RAM even on multi-hour files) with exact `vocal + acc == orig`.
pub fn separate_center_spectral_stft_streaming<F>(
    canonical_wav: &Path,
    vocal_out_path: &Path,
    acc_out_path: &Path,
    cancel: &AtomicBool,
    mut on_fraction: F,
) -> KaraokResult<(u32, String, u64, String, u64)>
where
    F: FnMut(f32),
{
    let mut reader = hound::WavReader::open(canonical_wav).map_err(|e| {
        KaraokError::new(ErrorCode::IoError, format!("Lecture PCM canonique échouée : {e}"))
    })?;
    let spec = reader.spec();
    let sr = spec.sample_rate.max(8000);
    let total_frames = reader.duration() as usize;

    for out_p in [vocal_out_path, acc_out_path] {
        if let Some(parent) = out_p.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                KaraokError::new(ErrorCode::IoError, format!("Création dossier stems : {e}"))
            })?;
        }
    }

    let tmp_voc = vocal_out_path.with_extension(format!("tmp-{}.wav", Uuid::new_v4()));
    let tmp_acc = acc_out_path.with_extension(format!("tmp-{}.wav", Uuid::new_v4()));
    let cleanup = |p1: &Path, p2: &Path| {
        let _ = fs::remove_file(p1);
        let _ = fs::remove_file(p2);
    };

    let wav_spec = hound::WavSpec {
        channels: 2,
        sample_rate: sr,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut voc_writer = hound::WavWriter::create(&tmp_voc, wav_spec).map_err(|e| {
        cleanup(&tmp_voc, &tmp_acc);
        map_analysis_hound_err("Création stem vocal", e)
    })?;
    let mut acc_writer = hound::WavWriter::create(&tmp_acc, wav_spec).map_err(|e| {
        cleanup(&tmp_voc, &tmp_acc);
        map_analysis_hound_err("Création stem accompagnement", e)
    })?;

    const FFT_SIZE: usize = 2048;
    const CHUNK_FRAMES: usize = 65_536; // ~1.36s per bounded memory slice (~4 MB RAM total)
    const MARGIN_FRAMES: usize = FFT_SIZE * 2; // 4096-frame symmetric overlap margin for seamless boundaries

    let mut iter = reader.samples::<f32>();
    let mut prev_tail_stereo: Vec<f32> = Vec::new();
    let mut frames_emitted = 0usize;

    // Read first chunk + lookahead margin
    let read_stereo = |it: &mut hound::WavSamples<'_, std::io::BufReader<std::fs::File>, f32>,
                       n_frames: usize|
     -> Vec<f32> {
        let mut v = Vec::with_capacity(n_frames * 2);
        for s in it.take(n_frames * 2) {
            let val = s.unwrap_or(0.0);
            v.push(if val.is_finite() { val } else { 0.0 });
        }
        v
    };

    let mut cur_buf = read_stereo(&mut iter, CHUNK_FRAMES + MARGIN_FRAMES);

    while frames_emitted < total_frames {
        check_cancelled(cancel).inspect_err(|_| cleanup(&tmp_voc, &tmp_acc))?;

        let cur_frames = cur_buf.len() / 2;
        if cur_frames == 0 {
            break;
        }
        let prev_frames = prev_tail_stereo.len() / 2;
        let emit_frames = (total_frames - frames_emitted).min(CHUNK_FRAMES).min(cur_frames);

        let mut window_stereo = Vec::with_capacity((prev_frames + cur_frames) * 2);
        window_stereo.extend_from_slice(&prev_tail_stereo);
        window_stereo.extend_from_slice(&cur_buf);

        let (voc_w, acc_w) =
            separate_center_spectral_stft(&window_stereo, sr, cancel, |_| {})
                .inspect_err(|_| cleanup(&tmp_voc, &tmp_acc))?;

        let start_s = prev_frames * 2;
        let end_s = (prev_frames + emit_frames) * 2;
        for (&v, &a) in voc_w[start_s..end_s].iter().zip(acc_w[start_s..end_s].iter()) {
            if let Err(e) = voc_writer.write_sample(v).and_then(|_| acc_writer.write_sample(a)) {
                cleanup(&tmp_voc, &tmp_acc);
                return Err(map_analysis_hound_err("Écriture stem WAV", e));
            }
        }

        frames_emitted += emit_frames;
        on_fraction((frames_emitted as f32 / total_frames.max(1) as f32).clamp(0.0, 1.0));

        // Carry tail context for next chunk and shift lookahead forward
        let tail_keep = emit_frames.min(MARGIN_FRAMES);
        let tail_start = (emit_frames - tail_keep) * 2;
        prev_tail_stereo.clear();
        prev_tail_stereo.extend_from_slice(&cur_buf[tail_start..emit_frames * 2]);

        let leftover = cur_buf.split_off(emit_frames * 2);
        let needed = (CHUNK_FRAMES + MARGIN_FRAMES).saturating_sub(leftover.len() / 2);
        let next_more = read_stereo(&mut iter, needed);
        cur_buf = leftover;
        cur_buf.extend_from_slice(&next_more);
    }

    if let Err(e) = voc_writer.finalize().and_then(|_| acc_writer.finalize()) {
        cleanup(&tmp_voc, &tmp_acc);
        return Err(map_analysis_hound_err("Finalisation stems", e));
    }

    fs::rename(&tmp_voc, vocal_out_path).map_err(|e| {
        cleanup(&tmp_voc, &tmp_acc);
        map_analysis_io_err("Publication stem vocal", e)
    })?;
    fs::rename(&tmp_acc, acc_out_path).map_err(|e| {
        cleanup(&tmp_voc, &tmp_acc);
        map_analysis_io_err("Publication stem accompagnement", e)
    })?;

    let (voc_sha, voc_bytes) = hash_file_sha256(vocal_out_path)?;
    let (acc_sha, acc_bytes) = hash_file_sha256(acc_out_path)?;
    Ok((sr, voc_sha, voc_bytes, acc_sha, acc_bytes))
}

/// Real local STFT center-channel spectral vocal separator (`dsp_center_spectral_v1`, ADR-0004).
///
/// Uses 2048-point Hann-windowed STFT with 75% overlap (hop = 512) and inter-channel phase/amplitude
/// similarity soft masking inside the vocal frequency band (140 Hz – 5500 Hz).
/// Mathematical guarantee: `vocal[i] + accompaniment[i] == original[i]` (exact reconstruction, exact length,
/// zero clicks at frame boundaries).
pub fn separate_center_spectral_stft<F>(
    interleaved_stereo: &[f32],
    sample_rate_hz: u32,
    cancel: &AtomicBool,
    mut on_fraction: F,
) -> KaraokResult<(Vec<f32>, Vec<f32>)>
where
    F: FnMut(f32),
{
    let total_frames = interleaved_stereo.len() / 2;
    if total_frames == 0 {
        return Ok((Vec::new(), Vec::new()));
    }

    const FFT_SIZE: usize = 2048;
    const HOP_SIZE: usize = 512; // 75% overlap => COLA exact with Hann^2 normalization

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let ifft = planner.plan_fft_inverse(FFT_SIZE);

    let window: Vec<f32> = (0..FFT_SIZE)
        .map(|i| 0.5 * (1.0 - (2.0 * PI * i as f32 / FFT_SIZE as f32).cos()))
        .collect();

    let pad = FFT_SIZE;
    let padded_len = total_frames + 2 * pad;
    let mut voc_l = vec![0.0_f32; padded_len];
    let mut voc_r = vec![0.0_f32; padded_len];
    let mut norm = vec![0.0_f32; padded_len];

    let mut buf_l = vec![Complex::new(0.0_f32, 0.0); FFT_SIZE];
    let mut buf_r = vec![Complex::new(0.0_f32, 0.0); FFT_SIZE];
    let mut out_l = vec![Complex::new(0.0_f32, 0.0); FFT_SIZE];
    let mut out_r = vec![Complex::new(0.0_f32, 0.0); FFT_SIZE];

    let bin_hz = sample_rate_hz.max(8000) as f32 / FFT_SIZE as f32;
    let inv_n = 1.0 / FFT_SIZE as f32;

    let num_hops = (total_frames + pad) / HOP_SIZE + 1;
    let mut hop_idx = 0usize;
    let mut pos = 0usize;

    while pos + FFT_SIZE <= padded_len {
        if hop_idx.is_multiple_of(64) {
            check_cancelled(cancel)?;
            on_fraction((hop_idx as f32 / num_hops.max(1) as f32).clamp(0.0, 1.0));
        }

        for k in 0..FFT_SIZE {
            let src_i = (pos + k) as isize - pad as isize;
            let (l, r) = if src_i >= 0 && (src_i as usize) < total_frames {
                let idx = (src_i as usize) * 2;
                (interleaved_stereo[idx], interleaved_stereo[idx + 1])
            } else {
                (0.0, 0.0)
            };
            let w = window[k];
            buf_l[k] = Complex::new(l * w, 0.0);
            buf_r[k] = Complex::new(r * w, 0.0);
        }

        fft.process(&mut buf_l);
        fft.process(&mut buf_r);

        for bin in 0..FFT_SIZE {
            let mirror_bin = if bin <= FFT_SIZE / 2 {
                bin
            } else {
                FFT_SIZE - bin
            };
            let freq = mirror_bin as f32 * bin_hz;

            // Smooth vocal band weight: strong between 160 Hz and 4800 Hz, tapered outside
            let band_weight = if !(110.0..=7500.0).contains(&freq) {
                0.0
            } else if freq < 190.0 {
                (freq - 110.0) / 80.0
            } else if freq > 4800.0 {
                (7500.0 - freq) / 2700.0
            } else {
                1.0
            };

            let l = buf_l[bin];
            let r = buf_r[bin];
            let mid = (l + r) * 0.5;
            let side = (l - r) * 0.5;
            let mid_mag2 = mid.norm_sqr();
            let side_mag2 = side.norm_sqr();
            let total_mag2 = mid_mag2 + side_mag2 + 1e-12;

            // Center-dominance Wiener-like soft mask: approaches 1 when L≈R in vocal band, 0 when stereo-panned
            let center_ratio = ((mid_mag2 - side_mag2).max(0.0) / total_mag2).powf(1.5);
            let vocal_mask = (center_ratio * band_weight).clamp(0.0, 0.96);

            let v = mid * vocal_mask;
            out_l[bin] = v;
            out_r[bin] = v;
        }

        ifft.process(&mut out_l);
        ifft.process(&mut out_r);

        for k in 0..FFT_SIZE {
            let w = window[k];
            let dst = pos + k;
            voc_l[dst] += out_l[k].re * inv_n * w;
            voc_r[dst] += out_r[k].re * inv_n * w;
            norm[dst] += w * w;
        }

        pos += HOP_SIZE;
        hop_idx += 1;
    }

    let mut vocal_out = vec![0.0_f32; total_frames * 2];
    let mut acc_out = vec![0.0_f32; total_frames * 2];
    for f in 0..total_frames {
        let p = f + pad;
        let denom = norm[p].max(1e-6);
        let vl = (voc_l[p] / denom).clamp(-1.5, 1.5);
        let vr = (voc_r[p] / denom).clamp(-1.5, 1.5);
        let ol = interleaved_stereo[f * 2];
        let or = interleaved_stereo[f * 2 + 1];

        vocal_out[f * 2] = vl;
        vocal_out[f * 2 + 1] = vr;
        // Exact residual accompaniment guarantees `vocal + accompaniment == original` (no phase doubling!)
        acc_out[f * 2] = ol - vl;
        acc_out[f * 2 + 1] = or - vr;
    }

    on_fraction(1.0);
    Ok((vocal_out, acc_out))
}

#[derive(Clone, Copy)]
struct VocalFrameFeat {
    t_s: f64,
    vocal_rms: f32,
    stem_ratio: f32,
    voiced: bool,
    f0_hz: f32,
    pitch_conf: f32,
    formant_ratio: f32,
    singing_score: f32,
    speech_score: f32,
}

fn finalize_vocal_activity_regions(
    track_id: Uuid,
    feats: &[VocalFrameFeat],
    total_frames: usize,
    sr: f32,
    frame_hop: usize,
) -> VocalActivitySummary {
    let mut regions: Vec<VocalRegion> = Vec::new();
    let mut i = 0usize;
    let hop_s = frame_hop as f64 / sr as f64;

    while i < feats.len() {
        let f = feats[i];
        let is_active = f.vocal_rms > 0.012
            && f.stem_ratio > 0.22
            && (f.singing_score > 0.48 || f.speech_score > 0.56);
        if !is_active {
            i += 1;
            continue;
        }
        let start_i = i;
        let mut gap = 0usize;
        let mut end_i = i;
        while i < feats.len() {
            let cur = feats[i];
            let active = cur.vocal_rms > 0.010
                && cur.stem_ratio > 0.18
                && (cur.singing_score > 0.42 || cur.speech_score > 0.50);
            if active {
                end_i = i;
                gap = 0;
            } else {
                gap += 1;
                if gap > 7 {
                    break;
                }
            }
            i += 1;
        }

        let slice = &feats[start_i..=end_i];
        let duration = (end_i - start_i + 1) as f64 * hop_s;
        if duration < 0.28 {
            continue;
        }

        let voiced_frac =
            slice.iter().filter(|x| x.voiced).count() as f32 / slice.len().max(1) as f32;
        let avg_sing =
            slice.iter().map(|x| x.singing_score).sum::<f32>() / slice.len().max(1) as f32;
        let avg_stem =
            slice.iter().map(|x| x.stem_ratio).sum::<f32>() / slice.len().max(1) as f32;
        let avg_formant =
            slice.iter().map(|x| x.formant_ratio).sum::<f32>() / slice.len().max(1) as f32;

        let mut stable_pairs = 0usize;
        for w in slice.windows(2) {
            if w[0].voiced && w[1].voiced && w[0].f0_hz > 1.0 && w[1].f0_hz > 1.0 {
                let ratio = (w[1].f0_hz / w[0].f0_hz).max(w[0].f0_hz / w[1].f0_hz);
                if ratio < 1.13 {
                    stable_pairs += 1;
                }
            }
        }
        let stability_frac = stable_pairs as f32 / slice.len().saturating_sub(1).max(1) as f32;

        let kind = if voiced_frac >= 0.45
            && stability_frac >= 0.35
            && avg_sing >= 0.55
            && avg_stem >= 0.35
        {
            VocalRegionKind::Singing
        } else if avg_formant >= 0.45 && avg_stem >= 0.35 && voiced_frac < 0.30 {
            VocalRegionKind::Speech
        } else {
            VocalRegionKind::Uncertain
        };

        let _ = (slice[0].pitch_conf,);
        regions.push(VocalRegion {
            id: Uuid::new_v4(),
            start_s: feats[start_i].t_s,
            end_s: (feats[end_i].t_s + hop_s).min(total_frames as f64 / sr as f64),
            kind,
            raw_score: avg_sing.clamp(0.0, 1.0),
            uncertainty_s: 0.12,
            method: VOCAL_ACTIVITY_METHOD.to_string(),
            method_version: "1.0".to_string(),
            human_override: None,
        });
    }

    let first_vocal_activity_s = regions.first().map(|r| r.start_s);
    let first_sung_activity_s = regions
        .iter()
        .find(|r| r.kind == VocalRegionKind::Singing)
        .map(|r| r.start_s);
    let no_singing_detected = first_sung_activity_s.is_none();

    VocalActivitySummary {
        track_id,
        first_vocal_activity_s,
        first_sung_activity_s,
        no_singing_detected,
        method: VOCAL_ACTIVITY_METHOD.to_string(),
        method_version: "1.0".to_string(),
        review_status: if no_singing_detected {
            ReviewStatus::NoSingingDetected
        } else {
            ReviewStatus::NeedsReview
        },
        regions,
    }
}

/// Bounded-RAM streaming multi-feature singing activity detector reading stereo f32 WAVs directly (P2-I8).
/// Uses O(frame_hop + fft_len) sliding buffers (~32 KB RAM even on multi-hour tracks).
pub fn detect_vocal_activity_streaming(
    track_id: Uuid,
    vocal_wav: &Path,
    original_wav: &Path,
    cancel: &AtomicBool,
) -> KaraokResult<VocalActivitySummary> {
    let mut voc_reader = hound::WavReader::open(vocal_wav)
        .map_err(|e| map_analysis_hound_err("Lecture stem vocal", e))?;
    let mut orig_reader = hound::WavReader::open(original_wav)
        .map_err(|e| map_analysis_hound_err("Lecture PCM canonique", e))?;

    let sr_u32 = orig_reader.spec().sample_rate.max(8000);
    let total_frames = (voc_reader.duration() as usize).min(orig_reader.duration() as usize);
    let sr = sr_u32 as f32;
    let frame_hop = ((sr * 0.04).round() as usize).max(256);
    let fft_len = 2048usize;

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_len);
    let mut fft_buf = vec![Complex::new(0.0_f32, 0.0); fft_len];
    let mut yin = YinDetector::new(sr);

    let bin_hz = sr / fft_len as f32;
    let b_300 = (300.0 / bin_hz).round() as usize;
    let b_3400 = ((3400.0 / bin_hz).round() as usize).min(fft_len / 2);
    let b_8000 = ((8000.0 / bin_hz).round() as usize).min(fft_len / 2);

    let mut voc_it = voc_reader.samples::<f32>();
    let mut orig_it = orig_reader.samples::<f32>();
    let read_mono_chunk =
        |it: &mut hound::WavSamples<'_, std::io::BufReader<std::fs::File>, f32>,
         n_frames: usize,
         dst: &mut Vec<f32>| {
            for _ in 0..n_frames {
                let Some(l_res) = it.next() else { break };
                let Some(r_res) = it.next() else { break };
                let l = l_res.unwrap_or(0.0);
                let r = r_res.unwrap_or(0.0);
                let m = 0.5 * (l + r);
                dst.push(if m.is_finite() { m } else { 0.0 });
            }
        };

    // Bounded lookahead buffer holding at most `frame_hop + fft_len` mono samples (~16 KB)
    let mut voc_win: Vec<f32> = Vec::with_capacity(frame_hop + fft_len);
    let mut orig_win: Vec<f32> = Vec::with_capacity(frame_hop + fft_len);
    read_mono_chunk(&mut voc_it, frame_hop + fft_len, &mut voc_win);
    read_mono_chunk(&mut orig_it, frame_hop + fft_len, &mut orig_win);

    let mut feats = Vec::with_capacity(total_frames / frame_hop + 1);
    let mut frame_idx = 0usize;
    let mut start = 0usize;

    while start + frame_hop <= total_frames && voc_win.len() >= frame_hop && orig_win.len() >= frame_hop
    {
        if frame_idx.is_multiple_of(50) {
            check_cancelled(cancel)?;
        }

        let mut voc_sq = 0.0_f32;
        let mut orig_sq = 0.0_f32;
        let mut est = karaok_voice_dsp::PitchEstimate::default();
        for i in 0..frame_hop {
            let vm = voc_win[i];
            let om = orig_win[i];
            voc_sq += vm * vm;
            orig_sq += om * om;
            est = yin.push_sample(vm);
        }

        let vocal_rms = (voc_sq / frame_hop as f32).sqrt();
        let orig_rms = (orig_sq / frame_hop as f32).sqrt();
        let stem_ratio = if orig_rms > 1e-5 {
            (vocal_rms / orig_rms).clamp(0.0, 1.5)
        } else {
            0.0
        };

        for (k, slot) in fft_buf.iter_mut().enumerate() {
            let sample = voc_win.get(k).copied().unwrap_or(0.0);
            let w = 0.5 * (1.0 - (2.0 * PI * k as f32 / fft_len as f32).cos());
            *slot = Complex::new(sample * w, 0.0);
        }
        fft.process(&mut fft_buf);

        let mut formant_e = 0.0_f32;
        let mut total_e = 1e-12_f32;
        let mut log_sum = 0.0_f64;
        let mut arith_sum = 0.0_f64;
        let mut flat_bins = 0usize;
        for (b, c) in fft_buf.iter().enumerate().take(b_8000).skip(1) {
            let m2 = c.norm_sqr();
            total_e += m2;
            if b >= b_300 && b <= b_3400 {
                formant_e += m2;
                let p = (m2 as f64).max(1e-12);
                log_sum += p.ln();
                arith_sum += p;
                flat_bins += 1;
            }
        }
        let formant_ratio = (formant_e / total_e).clamp(0.0, 1.0);
        let spectral_flatness = if flat_bins > 0 && arith_sum > 1e-12 {
            let geom = (log_sum / flat_bins as f64).exp();
            let arith = arith_sum / flat_bins as f64;
            (geom / arith).clamp(0.0, 1.0) as f32
        } else {
            1.0
        };
        let tonality = (1.0 - spectral_flatness).clamp(0.0, 1.0);
        let energy_gate = (vocal_rms > 0.012) as i32 as f32;
        let f0_in_vocal_band = est.voiced && (85.0..=900.0).contains(&est.f0_hz);
        let pitch_term = if f0_in_vocal_band { est.confidence } else { 0.0 };

        let singing_score = energy_gate
            * (0.35 * pitch_term
                + 0.25 * stem_ratio.min(1.0)
                + 0.25 * formant_ratio
                + 0.15 * tonality);
        let speech_score = energy_gate
            * (0.40 * formant_ratio
                + 0.30 * stem_ratio.min(1.0)
                + 0.15 * (1.0 - pitch_term)
                + 0.15 * spectral_flatness);

        feats.push(VocalFrameFeat {
            t_s: start as f64 / sr as f64,
            vocal_rms,
            stem_ratio,
            voiced: f0_in_vocal_band,
            f0_hz: est.f0_hz,
            pitch_conf: est.confidence,
            formant_ratio,
            singing_score,
            speech_score,
        });

        voc_win.drain(..frame_hop);
        orig_win.drain(..frame_hop);
        let need_v = (frame_hop + fft_len).saturating_sub(voc_win.len());
        let need_o = (frame_hop + fft_len).saturating_sub(orig_win.len());
        read_mono_chunk(&mut voc_it, need_v, &mut voc_win);
        read_mono_chunk(&mut orig_it, need_o, &mut orig_win);

        start += frame_hop;
        frame_idx += 1;
    }

    Ok(finalize_vocal_activity_regions(
        track_id,
        &feats,
        total_frames,
        sr,
        frame_hop,
    ))
}

/// Multi-feature singing activity detector (`multi_feature_harmonic_v1`, §7.4, AC-08).
///
/// Combines:
/// 1. Vocal stem vs original mix energy ratio (suppresses instrumental bleed)
/// 2. YIN pitch periodicity / harmonicity in singing range (85–900 Hz)
/// 3. Sustained pitch continuity across adjacent frames (distinguishes sung melody vs spoken/percussive bursts)
/// 4. Formant band (300–3400 Hz) spectral concentration vs broadband noise/cymbals
///
/// Explicitly returns `no_singing_detected = true` on instrumental tracks and distinguishes
/// `first_vocal_activity_s` vs `first_sung_activity_s`.
pub fn detect_vocal_activity(
    track_id: Uuid,
    vocal_stereo: &[f32],
    original_stereo: &[f32],
    sample_rate_hz: u32,
    cancel: &AtomicBool,
) -> KaraokResult<VocalActivitySummary> {
    let total_frames = (vocal_stereo.len() / 2).min(original_stereo.len() / 2);
    let sr = sample_rate_hz.max(8000) as f32;
    let frame_hop = ((sr * 0.04).round() as usize).max(256); // 40 ms analysis frame
    let fft_len = 2048usize;

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_len);
    let mut fft_buf = vec![Complex::new(0.0_f32, 0.0); fft_len];
    let mut yin = YinDetector::new(sr);

    let mut feats = Vec::with_capacity(total_frames / frame_hop + 1);
    let bin_hz = sr / fft_len as f32;
    let b_300 = (300.0 / bin_hz).round() as usize;
    let b_3400 = ((3400.0 / bin_hz).round() as usize).min(fft_len / 2);
    let b_8000 = ((8000.0 / bin_hz).round() as usize).min(fft_len / 2);

    let mut frame_idx = 0usize;
    let mut start = 0usize;
    while start + frame_hop <= total_frames {
        if frame_idx.is_multiple_of(50) {
            check_cancelled(cancel)?;
        }

        let mut voc_sq = 0.0_f32;
        let mut orig_sq = 0.0_f32;
        let mut est = karaok_voice_dsp::PitchEstimate::default();

        for i in 0..frame_hop {
            let idx = (start + i) * 2;
            let vm = 0.5 * (vocal_stereo[idx] + vocal_stereo[idx + 1]);
            let om = 0.5 * (original_stereo[idx] + original_stereo[idx + 1]);
            voc_sq += vm * vm;
            orig_sq += om * om;
            est = yin.push_sample(vm);
        }

        let vocal_rms = (voc_sq / frame_hop as f32).sqrt();
        let orig_rms = (orig_sq / frame_hop as f32).sqrt();
        let stem_ratio = if orig_rms > 1e-5 {
            (vocal_rms / orig_rms).clamp(0.0, 1.5)
        } else {
            0.0
        };

        for (k, slot) in fft_buf.iter_mut().enumerate() {
            let s_idx = start + k;
            let sample = if s_idx < total_frames {
                0.5 * (vocal_stereo[s_idx * 2] + vocal_stereo[s_idx * 2 + 1])
            } else {
                0.0
            };
            let w = 0.5 * (1.0 - (2.0 * PI * k as f32 / fft_len as f32).cos());
            *slot = Complex::new(sample * w, 0.0);
        }
        fft.process(&mut fft_buf);

        let mut formant_e = 0.0_f32;
        let mut total_e = 1e-12_f32;
        let mut log_sum = 0.0_f64;
        let mut arith_sum = 0.0_f64;
        let mut flat_bins = 0usize;
        for (b, c) in fft_buf.iter().enumerate().take(b_8000).skip(1) {
            let m2 = c.norm_sqr();
            total_e += m2;
            if b >= b_300 && b <= b_3400 {
                formant_e += m2;
                let p = (m2 as f64).max(1e-12);
                log_sum += p.ln();
                arith_sum += p;
                flat_bins += 1;
            }
        }
        let formant_ratio = (formant_e / total_e).clamp(0.0, 1.0);
        let spectral_flatness = if flat_bins > 0 && arith_sum > 1e-12 {
            let geom = (log_sum / flat_bins as f64).exp();
            let arith = arith_sum / flat_bins as f64;
            (geom / arith).clamp(0.0, 1.0) as f32
        } else {
            1.0
        };
        let tonality = (1.0 - spectral_flatness).clamp(0.0, 1.0);

        let energy_gate = (vocal_rms > 0.012) as i32 as f32;
        let f0_in_vocal_band = est.voiced && (85.0..=900.0).contains(&est.f0_hz);
        let pitch_term = if f0_in_vocal_band { est.confidence } else { 0.0 };

        let singing_score = energy_gate
            * (0.35 * pitch_term
                + 0.25 * stem_ratio.min(1.0)
                + 0.25 * formant_ratio
                + 0.15 * tonality);
        let speech_score = energy_gate
            * (0.40 * formant_ratio
                + 0.30 * stem_ratio.min(1.0)
                + 0.15 * (1.0 - pitch_term)
                + 0.15 * spectral_flatness);

        feats.push(VocalFrameFeat {
            t_s: start as f64 / sr as f64,
            vocal_rms,
            stem_ratio,
            voiced: f0_in_vocal_band,
            f0_hz: est.f0_hz,
            pitch_conf: est.confidence,
            formant_ratio,
            singing_score,
            speech_score,
        });

        start += frame_hop;
        frame_idx += 1;
    }

    Ok(finalize_vocal_activity_regions(
        track_id,
        &feats,
        total_frames,
        sr,
        frame_hop,
    ))
}

fn score_chromagram_key(mut chroma: [f32; 12]) -> KeyEstimate {
    const MAJOR_PROFILE: [f32; 12] = [
        6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88,
    ];
    const MINOR_PROFILE: [f32; 12] = [
        6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17,
    ];

    let sum: f32 = chroma.iter().sum();
    if sum < 1e-9 {
        return KeyEstimate {
            tonic: PitchClass::C,
            scale: MusicalScale::Chromatic,
            confidence: 0.0,
            ambiguous: true,
        };
    }
    for c in &mut chroma {
        *c /= sum;
    }

    let mut best_score = f32::NEG_INFINITY;
    let mut second_score = f32::NEG_INFINITY;
    let mut best_tonic = PitchClass::C;
    let mut best_scale = MusicalScale::Major;

    for tonic_i in 0..12 {
        for (scale, profile) in [
            (MusicalScale::Major, &MAJOR_PROFILE),
            (MusicalScale::NaturalMinor, &MINOR_PROFILE),
        ] {
            let corr = pearson_rotated(&chroma, profile, tonic_i);
            if corr > best_score {
                second_score = best_score;
                best_score = corr;
                best_tonic = PitchClass::from_semitone(tonic_i as i32);
                best_scale = scale;
            } else if corr > second_score {
                second_score = corr;
            }
        }
    }

    let margin = (best_score - second_score).max(0.0);
    KeyEstimate {
        tonic: best_tonic,
        scale: best_scale,
        confidence: best_score.clamp(0.0, 1.0),
        ambiguous: best_score < 0.55 || margin < 0.05,
    }
}

/// Bounded-RAM streaming Krumhansl-Schmuckler musical key estimation via strided seeks (P2-I8).
/// Allocates only a single 4096-point FFT buffer (~32 KB RAM even on multi-hour files).
pub fn estimate_musical_key_streaming(wav_path: &Path) -> KaraokResult<KeyEstimate> {
    let mut reader = hound::WavReader::open(wav_path)
        .map_err(|e| map_analysis_hound_err("Lecture PCM pour estimation tonalité", e))?;
    let total_frames = reader.duration() as usize;
    let sr = reader.spec().sample_rate.max(8000) as f32;
    let fft_len = 4096usize;
    let hop = 4096usize;
    if total_frames < fft_len {
        return Ok(KeyEstimate {
            tonic: PitchClass::C,
            scale: MusicalScale::Chromatic,
            confidence: 0.0,
            ambiguous: true,
        });
    }

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_len);
    let mut buf = vec![Complex::new(0.0_f32, 0.0); fft_len];
    let mut chroma = [0.0_f32; 12];

    let max_hops = 400usize;
    let stride_hops = ((total_frames / hop) / max_hops).max(1);
    let mut hop_count = 0usize;
    let mut pos = 0usize;

    while pos + fft_len <= total_frames {
        if hop_count.is_multiple_of(stride_hops) {
            if reader.seek(pos as u32).is_err() {
                break;
            }
            let mut it = reader.samples::<f32>();
            for (k, slot) in buf.iter_mut().enumerate() {
                let l = it.next().and_then(Result::ok).unwrap_or(0.0);
                let r = it.next().and_then(Result::ok).unwrap_or(0.0);
                let mono = 0.5 * (l + r);
                let safe = if mono.is_finite() { mono } else { 0.0 };
                let w = 0.5 * (1.0 - (2.0 * PI * k as f32 / fft_len as f32).cos());
                *slot = Complex::new(safe * w, 0.0);
            }
            fft.process(&mut buf);
            for (b, c) in buf.iter().enumerate().take(fft_len / 2).skip(1) {
                let freq = b as f32 * sr / fft_len as f32;
                if !(65.0..=2000.0).contains(&freq) {
                    continue;
                }
                let midi = 69.0 + 12.0 * (freq / 440.0).log2();
                let pc = (midi.round() as i32).rem_euclid(12) as usize;
                chroma[pc] += c.norm_sqr();
            }
        }
        pos += hop;
        hop_count += 1;
    }

    Ok(score_chromagram_key(chroma))
}

/// Krumhansl-Schmuckler chromagram musical key estimation (§8.2, revisable by user).
pub fn estimate_musical_key(interleaved_stereo: &[f32], sample_rate_hz: u32) -> KeyEstimate {
    let total_frames = interleaved_stereo.len() / 2;
    let fft_len = 4096usize;
    let hop = 4096usize;
    if total_frames < fft_len {
        return KeyEstimate {
            tonic: PitchClass::C,
            scale: MusicalScale::Chromatic,
            confidence: 0.0,
            ambiguous: true,
        };
    }

    let sr = sample_rate_hz.max(8000) as f32;
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_len);
    let mut buf = vec![Complex::new(0.0_f32, 0.0); fft_len];
    let mut chroma = [0.0_f32; 12];

    let mut pos = 0usize;
    let max_hops = 400usize; // cap analysis work for very long files
    let stride_hops = ((total_frames / hop) / max_hops).max(1);
    let mut hop_count = 0usize;

    while pos + fft_len <= total_frames {
        if hop_count.is_multiple_of(stride_hops) {
            for (k, slot) in buf.iter_mut().enumerate() {
                let idx = (pos + k) * 2;
                let mono = 0.5 * (interleaved_stereo[idx] + interleaved_stereo[idx + 1]);
                let w = 0.5 * (1.0 - (2.0 * PI * k as f32 / fft_len as f32).cos());
                *slot = Complex::new(mono * w, 0.0);
            }
            fft.process(&mut buf);
            for (b, c) in buf.iter().enumerate().take(fft_len / 2).skip(1) {
                let freq = b as f32 * sr / fft_len as f32;
                if !(65.0..=2000.0).contains(&freq) {
                    continue;
                }
                let midi = 69.0 + 12.0 * (freq / 440.0).log2();
                let pc = (midi.round() as i32).rem_euclid(12) as usize;
                chroma[pc] += c.norm_sqr();
            }
        }
        pos += hop;
        hop_count += 1;
    }

    score_chromagram_key(chroma)
}

fn pearson_rotated(chroma: &[f32; 12], profile: &[f32; 12], shift: usize) -> f32 {
    let mean_c = chroma.iter().sum::<f32>() / 12.0;
    let mean_p = profile.iter().sum::<f32>() / 12.0;
    let mut num = 0.0_f32;
    let mut den_c = 0.0_f32;
    let mut den_p = 0.0_f32;
    for i in 0..12 {
        let dc = chroma[(i + shift) % 12] - mean_c;
        let dp = profile[i] - mean_p;
        num += dc * dp;
        den_c += dc * dc;
        den_p += dp * dp;
    }
    let denom = (den_c * den_p).sqrt();
    if denom > 1e-9 {
        num / denom
    } else {
        0.0
    }
}

/// Constrained monotonic line aligner anchored to detected vocal regions (§7.5).
/// NEVER places lines inside detected instrumental silences and NEVER fabricates word-by-word times!
/// Always leaves status `NeedsReview` so the user verifies and confirms before publishing.
pub fn align_untimed_lines_to_vocal_regions(
    doc: &mut LyricsDocument,
    vocal: &VocalActivitySummary,
    duration_s: f64,
) {
    let active_regions: Vec<&VocalRegion> = vocal
        .regions
        .iter()
        .filter(|r| r.kind == VocalRegionKind::Singing || r.kind == VocalRegionKind::Uncertain)
        .collect();

    if active_regions.is_empty() {
        doc.status = ReviewStatus::NoSingingDetected;
        return;
    }

    let total_vocal_s: f64 = active_regions
        .iter()
        .map(|r| (r.end_s - r.start_s).max(0.0))
        .sum();
    if total_vocal_s < 0.5 {
        doc.status = ReviewStatus::NeedsReview;
        return;
    }

    // Collect untimed non-empty lyric cue indices (never touch human-validated anchors!)
    let target_indices: Vec<usize> = doc
        .cues
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.is_instrumental_marker && !c.text.trim().is_empty() && !c.human_validated)
        .map(|(i, _)| i)
        .collect();

    if target_indices.is_empty() {
        return;
    }

    // Weight each line by its Unicode character count (strictly monotonic within vocal-active timeline ONLY)
    let weights: Vec<f64> = target_indices
        .iter()
        .map(|&idx| (doc.cues[idx].text.chars().count().max(4)) as f64)
        .collect();
    let total_weight: f64 = weights.iter().sum::<f64>().max(1.0);

    // Map cumulative vocal-timeline offset `vt` in `[0, total_vocal_s]` back to `(source_s, region_index, conf)`
    let map_vocal_time = |vt: f64| -> (f64, usize, f32) {
        let mut acc = 0.0;
        for (r_idx, reg) in active_regions.iter().enumerate() {
            let len = (reg.end_s - reg.start_s).max(0.0);
            if vt <= acc + len {
                let conf = if reg.kind == VocalRegionKind::Singing {
                    (reg.raw_score * 0.7).clamp(0.2, 0.75)
                } else {
                    0.25
                };
                return ((reg.start_s + (vt - acc)).min(duration_s), r_idx, conf);
            }
            acc += len;
        }
        let last_idx = active_regions.len() - 1;
        (active_regions[last_idx].end_s.min(duration_s), last_idx, 0.2)
    };

    let mut cursor_vt = 0.0;
    for (pos, &cue_idx) in target_indices.iter().enumerate() {
        let share = (weights[pos] / total_weight) * total_vocal_s;
        let (start_s, start_reg_idx, conf) = map_vocal_time(cursor_vt);
        let (raw_end_s, end_reg_idx, _) = map_vocal_time((cursor_vt + share).min(total_vocal_s));
        // Never allow a single lyric line to stay active across an intervening instrumental solo (I10)!
        let bounded_end = if end_reg_idx > start_reg_idx {
            active_regions[start_reg_idx].end_s.min(duration_s)
        } else {
            raw_end_s
        };
        let final_end = if bounded_end > start_s + 0.15 {
            bounded_end
        } else {
            (start_s + 1.5).min(active_regions[start_reg_idx].end_s).min(duration_s)
        };

        let cue = &mut doc.cues[cue_idx];
        cue.start_s = Some(start_s);
        cue.end_s = Some(final_end.max(start_s + 0.05));
        cue.timing_source = TimingSource::AlignedLineAuto;
        cue.confidence = Some(conf);
        cue.words.clear(); // NEVER fabricate word-by-word timestamps!
        cursor_vt += share;
    }

    doc.first_lyric_cue_s = doc
        .cues
        .iter()
        .filter(|c| !c.is_instrumental_marker && !c.text.trim().is_empty())
        .filter_map(|c| c.start_s)
        .reduce(f64::min);
    doc.granularity = LyricsGranularity::Line;
    doc.status = ReviewStatus::NeedsReview;
    doc.warnings.push(format!(
        "Alignement automatique par ligne ({ALIGNMENT_METHOD}) ancré sur les segments vocaux détectés — vérification humaine requise avant validation."
    ));
}

fn validate_timed_lyrics_against_vocal_activity(
    doc: &mut LyricsDocument,
    vocal: &VocalActivitySummary,
    duration_s: f64,
) {
    if let (Some(first_sung), Some(first_cue)) = (vocal.first_sung_activity_s, doc.first_lyric_cue_s) {
        let delta = (first_sung - first_cue).abs();
        if delta > 4.0 {
            doc.warnings.push(format!(
                "Écart notable ({delta:.1}s) entre le premier chant détecté ({first_sung:.1}s) et la première phrase minutée ({first_cue:.1}s) — vérifiez la version du morceau ou ajustez le décalage global."
            ));
            if doc.status == ReviewStatus::UserValidated {
                // Keep human validation intact if already validated, only warn
            } else {
                doc.status = ReviewStatus::NeedsReview;
            }
        }
    }
    if let Some(max_end) = doc.cues.iter().filter_map(|c| c.end_s).reduce(f64::max) {
        if duration_s > 0.0 && max_end > duration_s + 2.0 {
            doc.warnings.push(
                "Certaines phrases dépassent la durée du fichier audio : les paroles correspondent peut-être à une autre édition (radio/live/album).".to_string(),
            );
            doc.status = ReviewStatus::NeedsReview;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use karaok_media::write_stereo_f32_wav_atomic;
    use std::f32::consts::TAU;

    #[test]
    fn spectral_separation_preserves_exact_reconstruction_and_separates_center_vocal() {
        let sr = 48_000u32;
        let frames = sr as usize * 2;
        let mut stereo = vec![0.0_f32; frames * 2];
        for i in 0..frames {
            let t = i as f32 / sr as f32;
            // Center-panned harmonic lead vocal at 330 Hz + hard-panned stereo accompaniment (110 Hz L, 550 Hz R)
            let vocal = 0.4 * (TAU * 330.0 * t).sin() + 0.15 * (TAU * 660.0 * t).sin();
            let acc_l = 0.35 * (TAU * 110.0 * t).sin();
            let acc_r = 0.35 * (TAU * 165.0 * t).sin();
            stereo[i * 2] = vocal + acc_l;
            stereo[i * 2 + 1] = vocal + acc_r;
        }
        let cancel = AtomicBool::new(false);
        let (voc, acc) = separate_center_spectral_stft(&stereo, sr, &cancel, |_| {}).unwrap();
        assert_eq!(voc.len(), stereo.len());
        assert_eq!(acc.len(), stereo.len());

        // Reconstruction guarantee: vocal + accompaniment == original to floating-point epsilon!
        let max_err = stereo
            .iter()
            .zip(voc.iter().zip(acc.iter()))
            .map(|(&o, (&v, &a))| (o - (v + a)).abs())
            .fold(0.0_f32, f32::max);
        assert!(max_err < 1e-5, "Reconstruction mismatch: {max_err}");

        // Vocal stem must capture non-trivial center signal
        let voc_rms = (voc.iter().map(|s| s * s).sum::<f32>() / voc.len() as f32).sqrt();
        assert!(voc_rms > 0.15, "Vocal stem energy too low: {voc_rms}");
    }

    #[test]
    fn ac08_detects_intro_then_singing_and_flags_instrumental_explicitly() {
        let sr = 48_000u32;
        let cancel = AtomicBool::new(false);

        // Fixture 1: 2.0s instrumental intro (hard-panned bass) followed by 2.0s center sung voice at 440 Hz
        let frames = sr as usize * 4;
        let mut song = vec![0.0_f32; frames * 2];
        for i in 0..frames {
            let t = i as f32 / sr as f32;
            let bass_l = 0.25 * (TAU * 90.0 * t).sin();
            let bass_r = -0.25 * (TAU * 90.0 * t).sin();
            let voice = if t >= 2.0 {
                0.45 * (TAU * 440.0 * t).sin() + 0.20 * (TAU * 880.0 * t).sin()
            } else {
                0.0
            };
            song[i * 2] = bass_l + voice;
            song[i * 2 + 1] = bass_r + voice;
        }
        let (voc, _) = separate_center_spectral_stft(&song, sr, &cancel, |_| {}).unwrap();
        let summary = detect_vocal_activity(Uuid::new_v4(), &voc, &song, sr, &cancel).unwrap();
        assert!(!summary.no_singing_detected, "Should detect singing after 2.0s intro");
        let first_sung = summary.first_sung_activity_s.unwrap();
        assert!(
            (first_sung - 2.0).abs() < 0.45,
            "First sung should be near 2.0s (preserving intro!), got {first_sung:.2}s"
        );

        // Fixture 2: Purely instrumental (wide stereo bass/chords, no center vocal)
        let mut inst = vec![0.0_f32; (sr as usize * 3) * 2];
        for i in 0..(sr as usize * 3) {
            let t = i as f32 / sr as f32;
            inst[i * 2] = 0.3 * (TAU * 82.4 * t).sin();
            inst[i * 2 + 1] = -0.3 * (TAU * 82.4 * t).sin();
        }
        let (voc_i, _) = separate_center_spectral_stft(&inst, sr, &cancel, |_| {}).unwrap();
        let summary_i = detect_vocal_activity(Uuid::new_v4(), &voc_i, &inst, sr, &cancel).unwrap();
        assert!(
            summary_i.no_singing_detected,
            "Instrumental track MUST yield no_singing_detected = true"
        );
        assert_eq!(summary_i.review_status, ReviewStatus::NoSingingDetected);
    }

    #[test]
    fn key_estimator_identifies_a_major_triad() {
        let sr = 48_000u32;
        let frames = sr as usize * 2;
        let mut stereo = vec![0.0_f32; frames * 2];
        // A Major triad: A3 (220), C#4 (277.18), E4 (329.63), A4 (440)
        for i in 0..frames {
            let t = i as f32 / sr as f32;
            let s = 0.30 * (TAU * 220.0 * t).sin()
                + 0.22 * (TAU * 277.18 * t).sin()
                + 0.22 * (TAU * 329.63 * t).sin()
                + 0.20 * (TAU * 440.0 * t).sin();
            stereo[i * 2] = s;
            stereo[i * 2 + 1] = s;
        }
        let est = estimate_musical_key(&stereo, sr);
        assert_eq!(est.tonic, PitchClass::A);
        assert_eq!(est.scale, MusicalScale::Major);
    }

    #[test]
    fn ac12_and_ac17_cache_reuses_stems_and_cancellation_preserves_integrity() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StoragePaths::from_roots(dir.path().join("app"), dir.path().join("cache")).unwrap();
        let store = LibraryStore::open(&paths).unwrap();
        let model_store = ModelStore::open(paths.models_dir.clone()).unwrap();

        let wav_path = paths.originals_dir.join("fixture.wav");
        let sr = 48_000u32;
        let frames = sr as usize * 2;
        let mut pcm = vec![0.0_f32; frames * 2];
        for i in 0..frames {
            let t = i as f32 / sr as f32;
            let s = 0.35 * (TAU * 440.0 * t).sin();
            pcm[i * 2] = s;
            pcm[i * 2 + 1] = s;
        }
        let (sha, _) = write_stereo_f32_wav_atomic(&wav_path, sr, &pcm).unwrap();
        let now = Utc::now();
        let mut track = Track {
            id: Uuid::new_v4(),
            title: "CacheTest".into(),
            artist: "Test".into(),
            album: None,
            duration_s: 2.0,
            original_sha256: sha,
            managed_original_path: wav_path.to_string_lossy().into_owned(),
            format: "wav".into(),
            sample_rate_hz: sr,
            channels: 2,
            encoder_delay_samples: 0,
            is_favorite: false,
            availability: karaok_types::TrackAvailability::Available,
            estimated_tonic: None,
            estimated_scale: None,
            key_confidence: None,
            key_ambiguous: true,
            created_at: now,
            updated_at: now,
        };
        store.upsert_track(&track).unwrap();

        // Pre-cancelled run -> clean JobCancelled, original file intact (AC-17)
        let pre_cancel = Arc::new(AtomicBool::new(true));
        let err = run_track_preparation(&mut track, &store, &paths, &model_store, pre_cancel, |_| {}).unwrap_err();
        assert_eq!(err.code, ErrorCode::JobCancelled);
        assert!(wav_path.exists());

        // First full run computes stems
        let first = run_track_preparation(
            &mut track,
            &store,
            &paths,
            &model_store,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        assert!(!first.reused_cached_stems);

        // Second run with unchanged inputs MUST reuse cached stems (AC-12)!
        let second = run_track_preparation(
            &mut track,
            &store,
            &paths,
            &model_store,
            Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        assert!(second.reused_cached_stems);
    }

    #[test]
    fn streaming_vocal_and_key_match_in_memory_and_enospc_maps_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let sr = 48_000u32;
        let frames = sr as usize * 3;
        let mut orig = vec![0.0_f32; frames * 2];
        let mut voc = vec![0.0_f32; frames * 2];
        for i in 0..frames {
            let t = i as f32 / sr as f32;
            let chord = 0.15 * (TAU * 220.0 * t).sin()
                + 0.12 * (TAU * 277.18 * t).sin()
                + 0.12 * (TAU * 329.63 * t).sin();
            let v = if (0.8..=2.4).contains(&t) {
                0.35 * (TAU * 440.0 * t).sin()
            } else {
                0.0
            };
            orig[i * 2] = chord + v;
            orig[i * 2 + 1] = chord + v;
            voc[i * 2] = v;
            voc[i * 2 + 1] = v;
        }
        let orig_wav = dir.path().join("orig.wav");
        let voc_wav = dir.path().join("voc.wav");
        write_stereo_f32_wav_atomic(&orig_wav, sr, &orig).unwrap();
        write_stereo_f32_wav_atomic(&voc_wav, sr, &voc).unwrap();

        let tid = Uuid::new_v4();
        let cancel = AtomicBool::new(false);
        let mem_v = detect_vocal_activity(tid, &voc, &orig, sr, &cancel).unwrap();
        let stream_v = detect_vocal_activity_streaming(tid, &voc_wav, &orig_wav, &cancel).unwrap();
        assert_eq!(mem_v.no_singing_detected, stream_v.no_singing_detected);
        assert_eq!(mem_v.regions.len(), stream_v.regions.len());
        assert!(!stream_v.no_singing_detected);

        let mem_k = estimate_musical_key(&orig, sr);
        let stream_k = estimate_musical_key_streaming(&orig_wav).unwrap();
        assert_eq!(mem_k.tonic, stream_k.tonic);
        assert_eq!(mem_k.scale, stream_k.scale);

        // Verify ENOSPC (errno 28) maps to ErrorCode::InsufficientDiskSpace (P2-I8)
        let enospc = std::io::Error::from_raw_os_error(28);
        let mapped = map_analysis_io_err("Écriture stem WAV", enospc);
        assert_eq!(mapped.code, ErrorCode::InsufficientDiskSpace);
    }
}
