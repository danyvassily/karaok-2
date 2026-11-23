//! Local MP3/WAV inspection, hashing, metadata extraction, and canonical PCM decoding (§7.2, REQ-006).
//!
//! Invariants:
//! - Original files are copied immutably into managed storage and SHA-256 hashed.
//! - Rejects unsupported, truncated, empty, oversized, or corrupted files cleanly with zero partial state.
//! - Canonical timeline is the decoded PCM `f32` stereo stream at original sample rate, intro preserved.
//! - Uses Symphonia (MPL-2.0) with strictly pinned `mp3`, `wav`, `pcm` features.

use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use karaok_types::{ErrorCode, KaraokError, KaraokResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use uuid::Uuid;

fn map_io_err(context: &str, e: std::io::Error) -> KaraokError {
    if e.kind() == std::io::ErrorKind::StorageFull || e.raw_os_error() == Some(28) {
        KaraokError::with_hint(
            ErrorCode::InsufficientDiskSpace,
            format!("Espace disque insuffisant ({context}) : {e}"),
            "Libérez de l'espace disque ou purgez le cache reconstructible dans Diagnostics.",
        )
    } else {
        KaraokError::new(ErrorCode::IoError, format!("{context} : {e}"))
    }
}
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey};
use symphonia::core::probe::Hint;

/// Fast O(1) WAV header check for cached canonical stereo f32 PCM files (avoids full decode/SHA-256 on load).
pub fn is_valid_canonical_wav_header(path: &Path) -> bool {
    match WavReader::open(path) {
        Ok(reader) => {
            let spec = reader.spec();
            spec.channels == 2
                && spec.sample_format == SampleFormat::Float
                && spec.bits_per_sample == 32
                && reader.duration() > 0
        }
        Err(_) => false,
    }
}

pub const DECODER_PIPELINE_VERSION: &str = "symphonia-0.5.4-canonical-pcm-v1";
/// Defensive file size cap (500 MiB) to protect against malformed/hostile media inputs (§11.3).
pub const MAX_IMPORT_FILE_BYTES: u64 = 500 * 1024 * 1024;
/// Defensive max decoded duration (4 hours) to avoid unbounded disk expansion.
pub const MAX_TRACK_DURATION_S: f64 = 4.0 * 3600.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbedMediaInfo {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub format: String,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub encoder_delay_samples: u32,
    pub duration_s: f64,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecodedCanonicalPcm {
    pub wav_path: PathBuf,
    pub sha256: String,
    pub size_bytes: u64,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub total_frames: u64,
    pub duration_s: f64,
    pub waveform_peaks: Vec<f32>,
}

/// Compute SHA-256 hex digest of a file on disk using streaming reads.
pub fn hash_file_sha256(path: &Path) -> KaraokResult<(String, u64)> {
    let file = File::open(path).map_err(|e| {
        KaraokError::with_hint(
            ErrorCode::IoError,
            format!("Impossible d'ouvrir le fichier : {e}"),
            "Vérifiez que le fichier existe et que macOS autorise sa lecture.",
        )
    })?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| {
            KaraokError::new(ErrorCode::IoError, format!("Erreur de lecture : {e}"))
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    let hex = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((hex, total))
}

/// Detect extension and enforce P0 MP3/WAV support cleanly (§3.6, REQ-006).
fn validate_supported_extension(path: &Path) -> KaraokResult<String> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "mp3" | "wav" | "wave" => Ok(if ext == "wave" { "wav".into() } else { ext }),
        other => Err(KaraokError::with_hint(
            ErrorCode::FormatUnsupported,
            format!("Format audio « .{other} » non pris en charge (P0 : MP3 et WAV non protégés)."),
            "Importez un fichier local MP3 ou WAV sans DRM.",
        )),
    }
}

/// Inspect a local MP3/WAV file safely, verifying real audio stream decodability before import.
pub fn probe_media_file(path: &Path) -> KaraokResult<ProbedMediaInfo> {
    let ext = validate_supported_extension(path)?;
    let meta = fs::metadata(path).map_err(|e| {
        KaraokError::new(
            ErrorCode::InvalidMediaFile,
            format!("Fichier inaccessible : {e}"),
        )
    })?;
    if !meta.is_file() || meta.len() == 0 {
        return Err(KaraokError::new(
            ErrorCode::InvalidMediaFile,
            "Le fichier audio est vide ou invalide.",
        ));
    }
    if meta.len() > MAX_IMPORT_FILE_BYTES {
        return Err(KaraokError::new(
            ErrorCode::InvalidMediaFile,
            "Le fichier dépasse la limite de sécurité de 500 Mo.",
        ));
    }

    let (sha256, size_bytes) = hash_file_sha256(path)?;

    let file = File::open(path).map_err(|e| {
        KaraokError::new(ErrorCode::IoError, format!("Ouverture impossible : {e}"))
    })?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(&ext);

    let mut probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(|e| {
            KaraokError::with_hint(
                ErrorCode::InvalidMediaFile,
                format!("Fichier illisible, corrompu ou protégé par DRM : {e}"),
                "Vérifiez que le fichier est un MP3 ou WAV valide non chiffré.",
            )
        })?;

    let mut title: Option<String> = None;
    let mut artist: Option<String> = None;
    let mut album: Option<String> = None;

    if let Some(meta_rev) = probed.metadata.get().and_then(|m| m.current().cloned()) {
        extract_tags(&meta_rev, &mut title, &mut artist, &mut album);
    }
    if let Some(meta_rev) = probed.format.metadata().current().cloned() {
        extract_tags(&meta_rev, &mut title, &mut artist, &mut album);
    }

    let track = probed
        .format
        .default_track()
        .ok_or_else(|| {
            KaraokError::new(
                ErrorCode::InvalidMediaFile,
                "Aucune piste audio valide détectée dans le fichier.",
            )
        })?;

    let codec_params = track.codec_params.clone();
    let sample_rate_hz = codec_params.sample_rate.ok_or_else(|| {
        KaraokError::new(
            ErrorCode::InvalidMediaFile,
            "Fréquence d'échantillonnage absente du flux audio.",
        )
    })?;
    let channels = codec_params
        .channels
        .map(|c| c.count() as u16)
        .filter(|&c| c > 0)
        .ok_or_else(|| {
            KaraokError::new(
                ErrorCode::InvalidMediaFile,
                "Nombre de canaux invalide dans le flux audio.",
            )
        })?;
    let encoder_delay_samples = codec_params.delay.unwrap_or(0);

    // Decode-verify frames to get exact canonical duration (handles MP3 VBR accurately §7.2!).
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|e| {
            KaraokError::new(
                ErrorCode::FormatUnsupported,
                format!("Codec non supporté pour ce fichier : {e}"),
            )
        })?;

    let mut total_frames: u64 = 0;
    loop {
        let packet = match probed.format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(ref io_err))
                if io_err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => {
                return Err(KaraokError::new(
                    ErrorCode::InvalidMediaFile,
                    format!("Flux audio tronqué ou endommagé : {e}"),
                ));
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(audio_buf) => {
                total_frames += audio_buf.frames() as u64;
                if total_frames as f64 / sample_rate_hz as f64 > MAX_TRACK_DURATION_S {
                    return Err(KaraokError::new(
                        ErrorCode::InvalidMediaFile,
                        "Durée audio supérieure à la limite de 4 heures.",
                    ));
                }
            }
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => {
                return Err(KaraokError::new(
                    ErrorCode::InvalidMediaFile,
                    format!("Erreur de décodage du fichier : {e}"),
                ));
            }
        }
    }

    if total_frames == 0 {
        return Err(KaraokError::new(
            ErrorCode::InvalidMediaFile,
            "Le fichier ne contient aucun échantillon audio décodable.",
        ));
    }

    let duration_s = total_frames as f64 / sample_rate_hz as f64;
    let fallback_title = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Sans titre")
        .to_string();

    Ok(ProbedMediaInfo {
        title: title.filter(|s| !s.trim().is_empty()).unwrap_or(fallback_title),
        artist: artist
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Artiste inconnu".to_string()),
        album: album.filter(|s| !s.trim().is_empty()),
        format: ext,
        sample_rate_hz,
        channels,
        encoder_delay_samples,
        duration_s,
        sha256,
        size_bytes,
    })
}

fn extract_tags(
    rev: &symphonia::core::meta::MetadataRevision,
    title: &mut Option<String>,
    artist: &mut Option<String>,
    album: &mut Option<String>,
) {
    for tag in rev.tags() {
        match tag.std_key {
            Some(StandardTagKey::TrackTitle) if title.is_none() => {
                *title = Some(tag.value.to_string());
            }
            Some(StandardTagKey::Artist) | Some(StandardTagKey::AlbumArtist) if artist.is_none() => {
                *artist = Some(tag.value.to_string());
            }
            Some(StandardTagKey::Album) if album.is_none() => {
                *album = Some(tag.value.to_string());
            }
            _ => {}
        }
    }
}

/// Copy original file atomically into managed library folder and verify SHA-256 after copy (§3.2, §10).
pub fn copy_original_to_managed_store(
    source_path: &Path,
    originals_dir: &Path,
    expected_sha256: &str,
    ext: &str,
) -> KaraokResult<PathBuf> {
    fs::create_dir_all(originals_dir).map_err(|e| {
        KaraokError::new(
            ErrorCode::IoError,
            format!("Impossible de créer le dossier des originaux : {e}"),
        )
    })?;
    let dest = originals_dir.join(format!("{expected_sha256}.{ext}"));
    if dest.exists() {
        let (existing_hash, _) = hash_file_sha256(&dest)?;
        if existing_hash == expected_sha256 {
            return Ok(dest);
        }
    }
    let tmp = originals_dir.join(format!(".tmp-{expected_sha256}-{}.part", Uuid::new_v4()));
    fs::copy(source_path, &tmp).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        map_io_err("Échec de la copie de l'original", e)
    })?;
    let (copied_hash, _) = hash_file_sha256(&tmp)?;
    if copied_hash != expected_sha256 {
        let _ = fs::remove_file(&tmp);
        return Err(KaraokError::new(
            ErrorCode::IoError,
            "L'empreinte SHA-256 de la copie originale ne correspond pas à la source.",
        ));
    }
    fs::rename(&tmp, &dest).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        map_io_err("Publication atomique de l'original échouée", e)
    })?;
    Ok(dest)
}

/// Decode MP3/WAV into a canonical stereo `f32` WAV file preserving intro and exact timeline (§7.2).
/// Writes to temp path first, then atomically renames to `output_wav_path`.
pub fn decode_to_canonical_stereo_wav(
    source_path: &Path,
    output_wav_path: &Path,
    waveform_bins: usize,
) -> KaraokResult<DecodedCanonicalPcm> {
    let ext = validate_supported_extension(source_path)?;
    if let Some(parent) = output_wav_path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            KaraokError::new(ErrorCode::IoError, format!("Création dossier cache impossible : {e}"))
        })?;
    }

    let file = File::open(source_path).map_err(|e| {
        KaraokError::new(ErrorCode::IoError, format!("Impossible de lire la source : {e}"))
    })?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(&ext);

    let mut probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(|e| {
            KaraokError::new(
                ErrorCode::InvalidMediaFile,
                format!("Échec d'analyse du conteneur : {e}"),
            )
        })?;

    let track = probed.format.default_track().ok_or_else(|| {
        KaraokError::new(ErrorCode::InvalidMediaFile, "Aucune piste audio.")
    })?;
    let track_id = track.id;
    let codec_params = track.codec_params.clone();
    let sample_rate_hz = codec_params.sample_rate.ok_or_else(|| {
        KaraokError::new(ErrorCode::InvalidMediaFile, "Fréquence absente.")
    })?;

    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|e| {
            KaraokError::new(
                ErrorCode::FormatUnsupported,
                format!("Décodeur indisponible : {e}"),
            )
        })?;

    let tmp_path = output_wav_path.with_extension(format!("tmp-{}.wav", Uuid::new_v4()));
    let spec = WavSpec {
        channels: 2,
        sample_rate: sample_rate_hz,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };

    let mut writer = WavWriter::create(&tmp_path, spec).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        map_hound_err(e)
    })?;

    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut total_frames: u64 = 0;
    let mut block_peaks: Vec<f32> = Vec::new();
    let frames_per_peak_block = (sample_rate_hz as usize / 20).max(1); // 50 ms raw peak blocks
    let mut current_block_count = 0usize;
    let mut current_block_peak = 0.0_f32;

    loop {
        let packet = match probed.format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(ref io_err))
                if io_err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp_path);
                return Err(KaraokError::new(
                    ErrorCode::InvalidMediaFile,
                    format!("Erreur de lecture pendant décodage canonique : {e}"),
                ));
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => {
                let _ = fs::remove_file(&tmp_path);
                return Err(KaraokError::new(
                    ErrorCode::InvalidMediaFile,
                    format!("Décodage échoué : {e}"),
                ));
            }
        };

        let ch_count = decoded.spec().channels.count();
        let frames = decoded.frames();
        if frames == 0 || ch_count == 0 {
            continue;
        }

        let buf = sample_buf.get_or_insert_with(|| {
            SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec())
        });
        if buf.capacity() < frames * ch_count {
            *buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
        }
        buf.copy_interleaved_ref(decoded);
        let samples = buf.samples();

        for f in 0..frames {
            let base = f * ch_count;
            let (l, r) = if ch_count == 1 {
                let s = sanitize(samples[base]);
                (s, s)
            } else {
                (sanitize(samples[base]), sanitize(samples[base + 1]))
            };
            writer.write_sample(l).map_err(|e| {
                let _ = fs::remove_file(&tmp_path);
                map_hound_err(e)
            })?;
            writer.write_sample(r).map_err(|e| {
                let _ = fs::remove_file(&tmp_path);
                map_hound_err(e)
            })?;

            let pk = l.abs().max(r.abs());
            if pk > current_block_peak {
                current_block_peak = pk;
            }
            current_block_count += 1;
            if current_block_count >= frames_per_peak_block {
                block_peaks.push(current_block_peak);
                current_block_peak = 0.0;
                current_block_count = 0;
            }
        }
        total_frames += frames as u64;
    }

    if current_block_count > 0 {
        block_peaks.push(current_block_peak);
    }

    writer.finalize().map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        map_hound_err(e)
    })?;
    fs::rename(&tmp_path, output_wav_path).map_err(|e| {
        let _ = fs::remove_file(&tmp_path);
        map_io_err("Publication atomique du PCM canonique échouée", e)
    })?;

    let (sha256, size_bytes) = hash_file_sha256(output_wav_path)?;
    let waveform_peaks = downsample_peaks(&block_peaks, waveform_bins.max(32));

    Ok(DecodedCanonicalPcm {
        wav_path: output_wav_path.to_path_buf(),
        sha256,
        size_bytes,
        sample_rate_hz,
        channels: 2,
        total_frames,
        duration_s: total_frames as f64 / sample_rate_hz as f64,
        waveform_peaks,
    })
}

/// Load a canonical stereo `f32` WAV into interleaved `[L, R, L, R, ...]` samples.
pub fn read_stereo_f32_wav(path: &Path) -> KaraokResult<(u32, Vec<f32>)> {
    let mut reader = WavReader::open(path).map_err(|e| {
        KaraokError::new(
            ErrorCode::IoError,
            format!("Lecture WAV canonique impossible : {e}"),
        )
    })?;
    let spec = reader.spec();
    if spec.sample_format != SampleFormat::Float || spec.channels != 2 {
        return Err(KaraokError::new(
            ErrorCode::InvalidMediaFile,
            "Le fichier PCM canonique attendu doit être stéréo f32.",
        ));
    }
    let samples: Vec<f32> = reader
        .samples::<f32>()
        .map(|s| s.map(sanitize))
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_hound_err)?;
    Ok((spec.sample_rate, samples))
}

/// Write interleaved stereo `f32` samples atomically to a WAV file and return `(sha256, size_bytes)`.
pub fn write_stereo_f32_wav_atomic(
    path: &Path,
    sample_rate_hz: u32,
    interleaved_stereo: &[f32],
) -> KaraokResult<(String, u64)> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            KaraokError::new(ErrorCode::IoError, format!("Création dossier impossible : {e}"))
        })?;
    }
    let tmp = path.with_extension(format!("tmp-{}.wav", Uuid::new_v4()));
    let spec = WavSpec {
        channels: 2,
        sample_rate: sample_rate_hz,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    let mut writer = WavWriter::create(&tmp, spec).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        map_hound_err(e)
    })?;
    for &s in interleaved_stereo {
        writer.write_sample(sanitize(s)).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            map_hound_err(e)
        })?;
    }
    writer.finalize().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        map_hound_err(e)
    })?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        map_io_err("Renommage atomique WAV impossible", e)
    })?;
    hash_file_sha256(path)
}

#[inline]
fn sanitize(s: f32) -> f32 {
    if s.is_finite() {
        s.clamp(-2.0, 2.0)
    } else {
        0.0
    }
}

fn map_hound_err(e: hound::Error) -> KaraokError {
    if let hound::Error::IoError(ref io_e) = e {
        if io_e.kind() == std::io::ErrorKind::StorageFull || io_e.raw_os_error() == Some(28) {
            return KaraokError::with_hint(
                ErrorCode::InsufficientDiskSpace,
                format!("Espace disque insuffisant lors de l'écriture WAV : {io_e}"),
                "Libérez de l'espace disque ou purgez le cache reconstructible.",
            );
        }
    }
    KaraokError::new(ErrorCode::IoError, format!("Erreur WAV : {e}"))
}

fn downsample_peaks(raw: &[f32], target_bins: usize) -> Vec<f32> {
    if raw.is_empty() {
        return Vec::new();
    }
    if raw.len() <= target_bins {
        return raw.to_vec();
    }
    let mut out = Vec::with_capacity(target_bins);
    for i in 0..target_bins {
        let start = i * raw.len() / target_bins;
        let end = ((i + 1) * raw.len() / target_bins).max(start + 1).min(raw.len());
        let max_val = raw[start..end].iter().copied().fold(0.0_f32, f32::max);
        out.push(max_val);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;
    use tempfile::tempdir;

    #[test]
    fn probes_and_decodes_synthetic_wav_preserving_duration_and_hash() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("fixture_intro.wav");
        let sr = 48_000u32;
        let frames = sr * 2; // 2.0s exact
        let mut samples = Vec::with_capacity(frames as usize * 2);
        for i in 0..frames {
            let t = i as f32 / sr as f32;
            // 1s silence intro + 1s 440Hz tone
            let v = if t >= 1.0 { (TAU * 440.0 * t).sin() * 0.4 } else { 0.0 };
            samples.push(v);
            samples.push(v);
        }
        write_stereo_f32_wav_atomic(&src, sr, &samples).unwrap();

        let probed = probe_media_file(&src).unwrap();
        assert_eq!(probed.format, "wav");
        assert_eq!(probed.sample_rate_hz, 48_000);
        assert_eq!(probed.channels, 2);
        assert!((probed.duration_s - 2.0).abs() < 1e-4);

        let canonical_path = dir.path().join("cache/canonical.wav");
        let decoded = decode_to_canonical_stereo_wav(&src, &canonical_path, 64).unwrap();
        assert_eq!(decoded.total_frames, 96_000);
        assert!((decoded.duration_s - 2.0).abs() < 1e-4);
        // Verify intro silence is preserved at start of waveform!
        assert!(decoded.waveform_peaks[0] < 1e-5);
        assert!(decoded.waveform_peaks.last().copied().unwrap() > 0.3);
    }

    #[test]
    fn rejects_invalid_or_corrupt_files_cleanly() {
        let dir = tempdir().unwrap();
        let bad_ext = dir.path().join("song.m4p");
        fs::write(&bad_ext, b"drm-fake").unwrap();
        assert_eq!(
            probe_media_file(&bad_ext).unwrap_err().code,
            ErrorCode::FormatUnsupported
        );

        let corrupt_mp3 = dir.path().join("corrupt.mp3");
        fs::write(&corrupt_mp3, b"not an mp3 at all 12345").unwrap();
        assert_eq!(
            probe_media_file(&corrupt_mp3).unwrap_err().code,
            ErrorCode::InvalidMediaFile
        );
    }

    #[test]
    fn probes_and_decodes_real_vbr_mp3_when_ffmpeg_available() {
        let dir = tempdir().unwrap();
        let wav = dir.path().join("src.wav");
        let mp3 = dir.path().join("src.mp3");
        let sr = 48_000u32;
        let frames = sr * 2;
        let mut samples = Vec::with_capacity(frames as usize * 2);
        for i in 0..frames {
            let t = i as f32 / sr as f32;
            let v = if t >= 0.8 { (TAU * 330.0 * t).sin() * 0.35 } else { 0.0 };
            samples.push(v);
            samples.push(v);
        }
        write_stereo_f32_wav_atomic(&wav, sr, &samples).unwrap();

        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-i",
                wav.to_str().unwrap(),
                "-codec:a",
                "libmp3lame",
                "-qscale:a",
                "4",
                "-metadata",
                "title=Test MP3 VBR",
                "-metadata",
                "artist=Karaok Fixture",
                mp3.to_str().unwrap(),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        if let Ok(st) = status {
            if st.success() {
                let probed = probe_media_file(&mp3).unwrap();
                assert_eq!(probed.format, "mp3");
                assert_eq!(probed.title, "Test MP3 VBR");
                assert_eq!(probed.artist, "Karaok Fixture");
                assert!((probed.duration_s - 2.0).abs() < 0.15);

                let canonical = dir.path().join("canonical_from_mp3.wav");
                let decoded = decode_to_canonical_stereo_wav(&mp3, &canonical, 64).unwrap();
                assert!((decoded.duration_s - probed.duration_s).abs() < 0.05);
                assert!(decoded.waveform_peaks[0] < 0.02);
            }
        }
    }
}
