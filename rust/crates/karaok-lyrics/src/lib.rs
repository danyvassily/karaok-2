//! UTF-8 & LRC parser, revision normalization, and music-driven lyric viewport engine (§6.2, §9.1, §9.3).
//!
//! Key invariants:
//! - Handles UTF-8 BOM, CRLF/LF, multiple timestamps per LRC line, metadata tags, `[offset:ms]`,
//!   fractional seconds, untimed lines, Unicode/CJK/RTL text without byte-index slicing.
//! - LRC `[offset:ms]` convention: standard LRC positive offset shifts lyrics EARLIER (timestamps
//!   displayed earlier), whereas Karaok Party internal `global_offset_ms` positive means display
//!   LATER `[s + d, e + d)`. The LRC offset is converted ONCE on parse and documented in warnings.
//! - Inferred end times (from next cue start) are explicitly marked `TimingSource::LrcInferredEnd`.
//!   The final line is NEVER stretched across a long outro; it receives a bounded reviewable cue end.
//! - Lyrics NEVER advance from microphone level/F0 and NEVER fabricate word-by-word timing.

use chrono::Utc;
use karaok_types::{
    LyricCue, LyricsDocument, LyricsGranularity, ReviewStatus, TimingSource, SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Maximum inferred duration for a last LRC line lacking an explicit end timestamp (seconds).
const MAX_LAST_LINE_INFERRED_DURATION_S: f64 = 6.0;
const DEFAULT_SINGLE_LINE_DURATION_S: f64 = 4.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LrcMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Raw LRC `[offset:ms]` as written in file (positive = earlier in standard LRC).
    pub raw_lrc_offset_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LyricsParseReport {
    pub document: LyricsDocument,
    pub metadata: LrcMetadata,
    pub parsed_timed_lines: usize,
    pub untimed_lines: usize,
    pub invalid_timestamps: usize,
    pub out_of_duration_cues: usize,
}

/// Compute SHA-256 hex digest of normalized UTF-8 text.
pub fn hash_normalized_text(raw: &str) -> String {
    let normalized = normalize_line_endings(strip_utf8_bom(raw));
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn strip_utf8_bom(input: &str) -> &str {
    input.strip_prefix('\u{FEFF}').unwrap_or(input)
}

pub fn normalize_line_endings(input: &str) -> String {
    input.replace("\r\n", "\n").replace('\r', "\n")
}

/// Parse UTF-8 plain text or simple LRC into a `LyricsDocument` anchored to `track_duration_s`.
pub fn parse_lyrics_text(
    track_id: Uuid,
    raw_input: &str,
    track_duration_s: f64,
    provenance: impl Into<String>,
    language: Option<String>,
    previous_revision: u64,
) -> LyricsParseReport {
    let cleaned = normalize_line_endings(strip_utf8_bom(raw_input));
    let text_sha256 = hash_normalized_text(&cleaned);

    let mut metadata = LrcMetadata {
        title: None,
        artist: None,
        album: None,
        raw_lrc_offset_ms: None,
    };
    let mut warnings = Vec::new();
    let mut timed_entries: Vec<(f64, String, usize)> = Vec::new();
    let mut untimed_entries: Vec<(usize, String)> = Vec::new();
    let mut invalid_timestamps = 0usize;
    let mut out_of_duration_cues = 0usize;

    for (line_idx, raw_line) in cleaned.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let (timestamps, remainder, meta_handled, line_invalid) = parse_lrc_line_tags(trimmed);
        invalid_timestamps += line_invalid;

        if let Some((tag, val)) = meta_handled {
            match tag {
                "ti" => metadata.title = Some(val),
                "ar" => metadata.artist = Some(val),
                "al" => metadata.album = Some(val),
                "offset" => match val.trim().parse::<i64>() {
                    Ok(ms) => {
                        metadata.raw_lrc_offset_ms = Some(ms);
                        warnings.push(format!(
                            "Offset LRC [offset:{ms}] détecté et converti une seule fois vers la convention interne (décalage positif = affichage plus tard)."
                        ));
                    }
                    Err(_) => {
                        invalid_timestamps += 1;
                        warnings.push(format!(
                            "Ligne {} : tag [offset:{val}] invalide ignoré.",
                            line_idx + 1
                        ));
                    }
                },
                _ => {}
            }
            continue;
        }

        if timestamps.is_empty() {
            untimed_entries.push((line_idx, remainder.to_string()));
        } else {
            let text = remainder.trim().to_string();
            for ts in timestamps {
                timed_entries.push((ts, text.clone(), line_idx));
            }
        }
    }

    // Standard LRC convention: positive [offset:+500] shifts timestamps 500ms EARLIER.
    // Our internal convention (§6.2): positive global_offset_ms shifts display LATER [s+d, e+d).
    // Convert ONCE into internal `global_offset_ms = -raw_lrc_offset_ms` so raw cue timestamps stay faithful.
    let internal_global_offset_ms = metadata
        .raw_lrc_offset_ms
        .map(|raw| raw.saturating_neg())
        .unwrap_or(0);

    // Stable sort by timestamp, preserving declaration order on ties (supports overlapping cues §9.3).
    timed_entries.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.2.cmp(&b.2))
    });

    let mut cues: Vec<LyricCue> = Vec::new();
    let parsed_timed_lines = timed_entries.len();

    if !timed_entries.is_empty() {
        for i in 0..timed_entries.len() {
            let (start_s, ref text, _) = timed_entries[i];
            if !start_s.is_finite() || start_s < 0.0 {
                invalid_timestamps += 1;
                continue;
            }
            if track_duration_s.is_finite() && track_duration_s > 0.0 && start_s >= track_duration_s
            {
                out_of_duration_cues += 1;
                warnings.push(format!(
                    "Repère à {start_s:.2}s hors durée du morceau ({track_duration_s:.2}s) — vérifiez que les paroles correspondent à cette version."
                ));
                continue;
            }

            // Empty LRC timestamp line acts as explicit instrumental/silence boundary!
            if text.is_empty() {
                cues.push(LyricCue {
                    id: format!("inst-{:04}", cues.len() + 1),
                    text: String::new(),
                    start_s: Some(start_s),
                    end_s: timed_entries
                        .get(i + 1)
                        .map(|n| n.0)
                        .filter(|&e| e > start_s),
                    timing_source: TimingSource::LrcExplicit,
                    confidence: None,
                    is_instrumental_marker: true,
                    human_validated: false,
                    words: Vec::new(),
                });
                continue;
            }

            // Infer end from next timestamp strictly greater than start_s, bounded cleanly (§9.1).
            let next_start = timed_entries
                .iter()
                .skip(i + 1)
                .map(|e| e.0)
                .find(|&t| t > start_s);

            let max_end = if track_duration_s.is_finite() && track_duration_s > 0.0 {
                track_duration_s
            } else {
                start_s + MAX_LAST_LINE_INFERRED_DURATION_S
            };

            let inferred_end = match next_start {
                Some(ns) => ns.min(max_end),
                None => {
                    // Never stretch the final line across a long outro (§9.1).
                    (start_s + DEFAULT_SINGLE_LINE_DURATION_S)
                        .min(start_s + MAX_LAST_LINE_INFERRED_DURATION_S)
                        .min(max_end)
                }
            };

            let end_s = if inferred_end > start_s {
                Some(inferred_end)
            } else {
                None
            };

            cues.push(LyricCue {
                id: format!("line-{:04}", cues.len() + 1),
                text: text.clone(),
                start_s: Some(start_s),
                end_s,
                timing_source: TimingSource::LrcInferredEnd,
                confidence: None,
                is_instrumental_marker: false,
                human_validated: false,
                words: Vec::new(),
            });
        }

        if !untimed_entries.is_empty() {
            warnings.push(format!(
                "{} ligne(s) non minutée(s) conservée(s) sans temps inventé.",
                untimed_entries.len()
            ));
            for (_, text) in &untimed_entries {
                cues.push(LyricCue {
                    id: format!("untimed-{:04}", cues.len() + 1),
                    text: text.clone(),
                    start_s: None,
                    end_s: None,
                    timing_source: TimingSource::Untimed,
                    confidence: None,
                    is_instrumental_marker: false,
                    human_validated: false,
                    words: Vec::new(),
                });
            }
        }
    } else {
        // Pure untimed UTF-8 lyrics text.
        for (_, text) in &untimed_entries {
            cues.push(LyricCue {
                id: format!("line-{:04}", cues.len() + 1),
                text: text.clone(),
                start_s: None,
                end_s: None,
                timing_source: TimingSource::Untimed,
                confidence: None,
                is_instrumental_marker: false,
                human_validated: false,
                words: Vec::new(),
            });
        }
    }

    let first_lyric_cue_s = cues
        .iter()
        .filter(|c| !c.is_instrumental_marker && !c.text.trim().is_empty())
        .filter_map(|c| c.start_s)
        .reduce(f64::min);

    let granularity = if first_lyric_cue_s.is_some() {
        LyricsGranularity::Line
    } else {
        LyricsGranularity::Untimed
    };

    let status = if cues.is_empty() {
        ReviewStatus::LyricsMissing
    } else {
        ReviewStatus::NeedsReview
    };

    let document = LyricsDocument {
        schema_version: SCHEMA_VERSION,
        track_id,
        timebase: "original_decoded_audio_seconds".to_string(),
        text_revision: previous_revision + 1,
        text_sha256,
        raw_text: cleaned,
        language,
        provenance: provenance.into(),
        granularity,
        global_offset_ms: internal_global_offset_ms,
        status,
        first_vocal_activity_s: None,
        first_sung_activity_s: None,
        first_lyric_cue_s,
        warnings,
        cues,
        updated_at: Utc::now(),
    };

    LyricsParseReport {
        document,
        metadata,
        parsed_timed_lines,
        untimed_lines: untimed_entries.len(),
        invalid_timestamps,
        out_of_duration_cues,
    }
}

/// Parses leading LRC tags `[mm:ss.xx]` or metadata tags `[ti:...]`.
fn parse_lrc_line_tags(line: &str) -> (Vec<f64>, &str, Option<(&str, String)>, usize) {
    let mut rest = line;
    let mut timestamps = Vec::new();
    let mut invalid = 0usize;

    // Check single metadata tag line first (e.g. `[ti:My Song]`)
    if rest.starts_with('[') && rest.ends_with(']') && rest.matches('[').count() == 1 {
        let inner = &rest[1..rest.len() - 1];
        if let Some((tag, val)) = inner.split_once(':') {
            let tag_trim = tag.trim();
            if matches!(
                tag_trim,
                "ti" | "ar" | "al" | "au" | "by" | "re" | "ve" | "length" | "offset"
            ) {
                return (Vec::new(), "", Some((tag_trim, val.trim().to_string())), 0);
            }
        }
    }

    while rest.starts_with('[') {
        let Some(close_idx) = rest.find(']') else {
            break;
        };
        let inner = &rest[1..close_idx];
        if let Some(secs) = parse_lrc_timestamp(inner) {
            timestamps.push(secs);
            rest = &rest[close_idx + 1..];
        } else if inner.contains(':') && inner.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            // Looks like a malformed numeric timestamp `[99:ab]`
            invalid += 1;
            rest = &rest[close_idx + 1..];
        } else {
            break;
        }
    }

    (timestamps, rest, None, invalid)
}

/// Parses `mm:ss`, `mm:ss.xx`, `mm:ss.xxx`, or `mm:ss:xx`.
pub fn parse_lrc_timestamp(token: &str) -> Option<f64> {
    let trimmed = token.trim();
    let (min_str, sec_part) = trimmed.split_once(':')?;
    if min_str.is_empty() || !min_str.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let minutes: u64 = min_str.parse().ok()?;

    // Support both `ss.ff` and `ss:ff`
    let (sec_str, frac_str) = if let Some((s, f)) = sec_part.split_once('.') {
        (s, Some(f))
    } else if let Some((s, f)) = sec_part.split_once(':') {
        (s, Some(f))
    } else {
        (sec_part, None)
    };

    if sec_str.is_empty() || !sec_str.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let seconds: u64 = sec_str.parse().ok()?;
    if seconds >= 60 {
        return None;
    }

    let frac_s = match frac_str {
        None => 0.0,
        Some(f) => {
            if f.is_empty() || f.len() > 6 || !f.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let val: u64 = f.parse().ok()?;
            let denom = 10_f64.powi(f.len() as i32);
            val as f64 / denom
        }
    };

    Some(minutes as f64 * 60.0 + seconds as f64 + frac_s)
}

/// Display-ready block state for the 4–5 block karaoke viewport (§3.4, §6.2, REQ-012, REQ-013).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayPhase {
    Intro,
    ActiveLyric,
    InstrumentalBreak,
    Outro,
    Untimed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViewportCue {
    pub cue_index: usize,
    pub id: String,
    pub text: String,
    pub is_active: bool,
    pub is_past: bool,
    pub is_upcoming: bool,
    pub start_s: Option<f64>,
    pub end_s: Option<f64>,
    pub progress: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LyricsViewportState {
    pub phase: DisplayPhase,
    pub active_cue_indices: Vec<usize>,
    /// Countdown in seconds ONLY when anchored to a validated first lyric cue (§3.4).
    pub intro_countdown_s: Option<f64>,
    pub visible_blocks: Vec<ViewportCue>,
}

/// Deterministically evaluates the 4 or 5 visible lyric blocks for a given musical source position.
///
/// Follows §6.2 strictly:
/// - Positive `global_offset_ms` delays display: active interval is `[start + d, end + d)`.
/// - Before first lyric cue: `DisplayPhase::Intro`, no stale active line.
/// - Inside an instrumental gap: `DisplayPhase::InstrumentalBreak`, no past line stays artificially active.
/// - Overlapping cues are preserved in `active_cue_indices`.
pub fn evaluate_viewport(
    doc: &LyricsDocument,
    source_position_s: f64,
    max_blocks: usize,
) -> LyricsViewportState {
    let max_blocks = max_blocks.clamp(4, 5);
    let offset_s = doc.global_offset_ms as f64 / 1000.0;

    let lyric_indices: Vec<usize> = doc
        .cues
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.is_instrumental_marker && !c.text.trim().is_empty())
        .map(|(i, _)| i)
        .collect();

    let mut timed_lyric_indices: Vec<usize> = lyric_indices
        .iter()
        .copied()
        .filter(|&i| doc.cues[i].start_s.is_some())
        .collect();

    // Ensure chronological order even after out-of-order manual cue placements (I10)
    timed_lyric_indices.sort_by(|&a, &b| {
        let sa = doc.cues[a].start_s.unwrap_or(0.0);
        let sb = doc.cues[b].start_s.unwrap_or(0.0);
        sa.partial_cmp(&sb)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cmp(&b))
    });

    if timed_lyric_indices.is_empty() {
        let visible_blocks = lyric_indices
            .iter()
            .take(max_blocks)
            .map(|&idx| {
                let c = &doc.cues[idx];
                ViewportCue {
                    cue_index: idx,
                    id: c.id.clone(),
                    text: c.text.clone(),
                    is_active: false,
                    is_past: false,
                    is_upcoming: true,
                    start_s: None,
                    end_s: None,
                    progress: 0.0,
                }
            })
            .collect();
        return LyricsViewportState {
            phase: DisplayPhase::Untimed,
            active_cue_indices: Vec::new(),
            intro_countdown_s: None,
            visible_blocks,
        };
    }

    let first_idx = timed_lyric_indices[0];
    let first_start = doc.cues[first_idx].start_s.unwrap_or(0.0) + offset_s;

    let mut active_indices = Vec::new();
    let mut last_past_pos: Option<usize> = None;
    let mut first_upcoming_pos: Option<usize> = None;

    for (pos, &cue_idx) in timed_lyric_indices.iter().enumerate() {
        let cue = &doc.cues[cue_idx];
        let s = cue.start_s.unwrap_or(0.0) + offset_s;
        let e = cue.end_s.map(|end| end + offset_s).unwrap_or(s + DEFAULT_SINGLE_LINE_DURATION_S);

        if source_position_s >= s && source_position_s < e {
            active_indices.push(cue_idx);
        } else if source_position_s >= e {
            last_past_pos = Some(pos);
        } else if first_upcoming_pos.is_none() {
            first_upcoming_pos = Some(pos);
        }
    }

    let phase = if !active_indices.is_empty() {
        DisplayPhase::ActiveLyric
    } else if source_position_s < first_start {
        DisplayPhase::Intro
    } else if first_upcoming_pos.is_some() {
        DisplayPhase::InstrumentalBreak
    } else {
        DisplayPhase::Outro
    };

    // Countdown only shown during Intro when first cue is validated (§3.4).
    let intro_countdown_s = if phase == DisplayPhase::Intro
        && (doc.status == ReviewStatus::UserValidated || doc.cues[first_idx].human_validated)
    {
        let remaining = (first_start - source_position_s).max(0.0);
        if remaining <= 5.0 {
            Some(remaining)
        } else {
            None
        }
    } else {
        None
    };

    // Choose anchor position in `timed_lyric_indices` to display 1 previous + active + 2..3 upcoming.
    let anchor_pos = if let Some(&first_active_cue_idx) = active_indices.first() {
        timed_lyric_indices
            .iter()
            .position(|&i| i == first_active_cue_idx)
            .unwrap_or(0)
    } else if let Some(up) = first_upcoming_pos {
        up
    } else {
        timed_lyric_indices.len().saturating_sub(1)
    };

    let start_pos = anchor_pos.saturating_sub(1);
    let end_pos = (start_pos + max_blocks).min(timed_lyric_indices.len());
    let adjusted_start = if end_pos - start_pos < max_blocks {
        end_pos.saturating_sub(max_blocks)
    } else {
        start_pos
    };

    let visible_blocks = timed_lyric_indices[adjusted_start..end_pos]
        .iter()
        .map(|&idx| {
            let cue = &doc.cues[idx];
            let s = cue.start_s.unwrap_or(0.0) + offset_s;
            let e = cue
                .end_s
                .map(|end| end + offset_s)
                .unwrap_or(s + DEFAULT_SINGLE_LINE_DURATION_S);
            let is_active = active_indices.contains(&idx);
            let is_past = source_position_s >= e;
            let is_upcoming = source_position_s < s;
            let progress = if is_active && e > s {
                ((source_position_s - s) / (e - s)).clamp(0.0, 1.0) as f32
            } else if is_past {
                1.0
            } else {
                0.0
            };
            ViewportCue {
                cue_index: idx,
                id: cue.id.clone(),
                text: cue.text.clone(),
                is_active,
                is_past,
                is_upcoming,
                start_s: Some(s),
                end_s: Some(e),
                progress,
            }
        })
        .collect();

    let _ = last_past_pos;

    LyricsViewportState {
        phase,
        active_cue_indices: active_indices,
        intro_countdown_s,
        visible_blocks,
    }
}

/// Apply manual human placement of a cue at a chosen source position (§3.3, §9.3).
/// Sets `timing_source = Manual` and `confidence = None` — never counts as model accuracy.
pub fn place_cue_manually(
    doc: &mut LyricsDocument,
    cue_id: &str,
    start_s: f64,
    end_s: Option<f64>,
    duration_s: f64,
) -> Result<(), String> {
    if !start_s.is_finite() || start_s < 0.0 || (duration_s > 0.0 && start_s >= duration_s) {
        return Err("Position de début invalide ou hors durée du morceau.".to_string());
    }
    if let Some(e) = end_s {
        if !e.is_finite() || e <= start_s || (duration_s > 0.0 && e > duration_s) {
            return Err("Position de fin invalide (doit être > début et <= durée).".to_string());
        }
    }
    let Some(cue) = doc.cues.iter_mut().find(|c| c.id == cue_id) else {
        return Err(format!("Ligne introuvable : {cue_id}"));
    };
    cue.start_s = Some(start_s);
    cue.end_s = end_s.or_else(|| {
        let cap = if duration_s > 0.0 {
            duration_s
        } else {
            start_s + DEFAULT_SINGLE_LINE_DURATION_S
        };
        Some((start_s + DEFAULT_SINGLE_LINE_DURATION_S).min(cap))
    });
    cue.timing_source = TimingSource::Manual;
    cue.confidence = None;
    cue.human_validated = true;

    doc.first_lyric_cue_s = doc
        .cues
        .iter()
        .filter(|c| !c.is_instrumental_marker && !c.text.trim().is_empty())
        .filter_map(|c| c.start_s)
        .reduce(f64::min);
    doc.granularity = LyricsGranularity::Line;
    doc.updated_at = Utc::now();
    Ok(())
}

/// Toggle or mark a cue as an explicit instrumental passage (§3.3, I13).
pub fn mark_cue_instrumental(
    doc: &mut LyricsDocument,
    cue_id: &str,
    is_instrumental: bool,
) -> Result<(), String> {
    let Some(cue) = doc.cues.iter_mut().find(|c| c.id == cue_id) else {
        return Err(format!("Ligne introuvable : {cue_id}"));
    };
    cue.is_instrumental_marker = is_instrumental;
    cue.human_validated = true;
    doc.first_lyric_cue_s = doc
        .cues
        .iter()
        .filter(|c| !c.is_instrumental_marker && !c.text.trim().is_empty())
        .filter_map(|c| c.start_s)
        .reduce(f64::min);
    doc.updated_at = Utc::now();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bom_crlf_multi_timestamps_and_offset_convention() {
        let lrc = "\u{FEFF}[ti:Fixture Chanson]\r\n[ar:Test]\r\n[offset:250]\r\n[00:12.50][00:40.00]Refrain lumineux\r\n[00:16.00]Deuxième phrase\r\n[00:20.00]\r\n[00:50.00]日本語とالعربية\r\n";
        let report = parse_lyrics_text(Uuid::new_v4(), lrc, 120.0, "lrc_import", Some("fr".into()), 0);
        assert_eq!(report.metadata.title.as_deref(), Some("Fixture Chanson"));
        assert_eq!(report.metadata.raw_lrc_offset_ms, Some(250));
        // LRC +250ms (earlier) converts ONCE to internal -250ms (negative = earlier).
        assert_eq!(report.document.global_offset_ms, -250);
        assert_eq!(report.parsed_timed_lines, 5);
        assert_eq!(report.invalid_timestamps, 0);
        assert_eq!(report.document.first_lyric_cue_s, Some(12.5));
        assert!(report
            .document
            .cues
            .iter()
            .any(|c| c.text == "日本語とالعربية"));
        // Instrumental empty cue stops line 2 from bleeding into solo
        let second = report
            .document
            .cues
            .iter()
            .find(|c| c.text == "Deuxième phrase")
            .unwrap();
        assert_eq!(second.end_s, Some(20.0));
        // Last line is NOT stretched to 120s outro!
        let last = report.document.cues.last().unwrap();
        assert!(last.end_s.unwrap() <= 56.0);
    }

    #[test]
    fn viewport_handles_intro_instrumental_and_positive_offset() {
        let lrc = "[00:10.00]Ligne un\n[00:14.00]Ligne deux\n[00:18.00]\n[00:35.00]Ligne trois\n[00:39.00]Ligne quatre\n[00:43.00]Ligne cinq\n";
        let mut report = parse_lyrics_text(Uuid::new_v4(), lrc, 90.0, "test", None, 0);
        report.document.global_offset_ms = 500; // +0.5s => display LATER: Ligne un active on [10.5, 14.5)
        report.document.status = ReviewStatus::UserValidated;

        // Intro at 8.0s => Intro phase with countdown 2.5s
        let vp_intro = evaluate_viewport(&report.document, 8.0, 5);
        assert_eq!(vp_intro.phase, DisplayPhase::Intro);
        assert!(vp_intro.active_cue_indices.is_empty());
        assert!((vp_intro.intro_countdown_s.unwrap() - 2.5).abs() < 1e-6);

        // At 10.2s still Intro because offset is +500ms!
        let vp_before_offset = evaluate_viewport(&report.document, 10.2, 5);
        assert_eq!(vp_before_offset.phase, DisplayPhase::Intro);

        // At 11.0s Ligne un is active
        let vp_active = evaluate_viewport(&report.document, 11.0, 5);
        assert_eq!(vp_active.phase, DisplayPhase::ActiveLyric);
        assert_eq!(vp_active.active_cue_indices.len(), 1);
        assert_eq!(vp_active.visible_blocks.len(), 5);

        // At 25.0s instrumental break: NO past line artificially active!
        let vp_inst = evaluate_viewport(&report.document, 25.0, 4);
        assert_eq!(vp_inst.phase, DisplayPhase::InstrumentalBreak);
        assert!(vp_inst.active_cue_indices.is_empty());
        assert_eq!(vp_inst.visible_blocks.len(), 4);
    }

    #[test]
    fn manual_placement_sets_manual_source_and_null_confidence() {
        let mut report = parse_lyrics_text(Uuid::new_v4(), "Phrase libre", 60.0, "user", None, 0);
        let id = report.document.cues[0].id.clone();
        place_cue_manually(&mut report.document, &id, 5.0, Some(9.5), 60.0).unwrap();
        let cue = &report.document.cues[0];
        assert_eq!(cue.timing_source, TimingSource::Manual);
        assert_eq!(cue.confidence, None);
        assert!(cue.human_validated);
        assert_eq!(cue.start_s, Some(5.0));
        assert_eq!(cue.end_s, Some(9.5));
    }
}
