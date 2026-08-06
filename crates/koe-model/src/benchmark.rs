//! Chunk-size latency/WER/RTF baselines from `spec/08-roadmap.md` M3.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{
    adapter::{AsrError, Pcm16Mono16k, StreamingAsrSession},
    fixture::FIXTURE_SAMPLE_RATE,
};

/// One recorded chunk-size baseline.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BenchmarkBaseline {
    pub schema_version: u32,
    pub model_id: String,
    pub version: String,
    pub chunk_ms: u64,
    /// Milliseconds from the first append to the first final event.
    pub first_result_latency_ms: u64,
    /// Milliseconds from the first append to the finished transcript.
    pub final_latency_ms: u64,
    /// Word error rate in percent against the supplied reference.
    pub wer_pct: f64,
    /// Real-time factor: wall-clock seconds per audio second.
    pub rtf: f64,
    pub recorded_at_unix_ms: u128,
}

/// Materialized set of baselines for one installed model.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct BenchmarkReport {
    pub baselines: Vec<BenchmarkBaseline>,
}

/// Word error rate (0..=100) computed with token-level Levenshtein distance.
///
/// The reference and hypothesis are split on ASCII whitespace. An empty
/// reference counts as 0% when the hypothesis is also empty and 100%
/// otherwise.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn word_error_rate(
    reference: &str,
    hypothesis: &str,
) -> f64 {
    let reference = reference.split_whitespace().collect::<Vec<_>>();
    let hypothesis = hypothesis.split_whitespace().collect::<Vec<_>>();
    if reference.len() == hypothesis.len() && reference == hypothesis {
        return 0.0;
    }
    if reference.is_empty() {
        return if hypothesis.is_empty() { 0.0 } else { 100.0 };
    }
    let distance = levenshtein(&reference, &hypothesis);
    let denominator = reference.len().max(1);
    (distance as f64) * 100.0 / (denominator as f64)
}

fn levenshtein(
    reference: &[&str],
    hypothesis: &[&str],
) -> usize {
    let reference_len = reference.len();
    let mut previous = (0..=hypothesis.len()).collect::<Vec<_>>();
    let mut current = vec![0; hypothesis.len() + 1];
    for row in 1..=reference_len {
        current[0] = row;
        for column in 1..=hypothesis.len() {
            let substitution =
                previous[column - 1] + usize::from(reference[row - 1] != hypothesis[column - 1]);
            let deletion = previous[column] + 1;
            let insertion = current[column - 1] + 1;
            current[column] = substitution.min(deletion).min(insertion);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[hypothesis.len()]
}

fn latency_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Runs one baseline against a real session, chunking `audio` at `chunk_ms`.
///
/// # Errors
///
/// Returns an ASR error when the session cannot be fed or finalized.
#[allow(clippy::cast_precision_loss)]
pub async fn run_chunk_baseline(
    mut session: Box<dyn StreamingAsrSession>,
    model_id: &str,
    version: &str,
    audio: &[i16],
    chunk_ms: u64,
    reference: &str,
) -> Result<BenchmarkBaseline, AsrError> {
    if chunk_ms == 0 {
        return Err(AsrError::InvalidInput);
    }
    let chunk_samples = usize::try_from(
        chunk_ms
            .saturating_mul(u64::from(FIXTURE_SAMPLE_RATE))
            .checked_div(1_000)
            .ok_or(AsrError::InvalidInput)?,
    )
    .map_err(|_| AsrError::InvalidInput)?
    .max(1);
    let started = Instant::now();
    let mut first_result_latency_ms: Option<u64> = None;
    let mut position_us = 0_u64;
    for chunk in audio.chunks(chunk_samples) {
        session
            .append(Pcm16Mono16k {
                samples: chunk.to_vec(),
                session_start_us: position_us,
            })
            .await?;
        // Poll only for the first-result latency; final events are read from
        // `finish` so the hypothesis is never double-counted.
        if first_result_latency_ms.is_none() && session.poll_results().await?.is_some() {
            first_result_latency_ms = Some(latency_ms(started));
        }
        position_us = position_us.saturating_add(
            u64::try_from(chunk.len()).map_err(|_| AsrError::InvalidInput)? * 1_000_000
                / u64::from(FIXTURE_SAMPLE_RATE),
        );
    }
    let transcript = session.finish().await?;
    let mut hypothesis = String::new();
    for event in transcript.events {
        if first_result_latency_ms.is_none() && event.is_final {
            first_result_latency_ms = Some(latency_ms(started));
        }
        if event.is_final {
            if !hypothesis.is_empty() {
                hypothesis.push(' ');
            }
            hypothesis.push_str(&event.text);
        }
    }
    let final_latency_ms = latency_ms(started);
    let audio_seconds = audio.len() as f64 / f64::from(FIXTURE_SAMPLE_RATE);
    let wall_seconds = final_latency_ms as f64 / 1_000.0;
    let rtf = if audio_seconds > 0.0 {
        wall_seconds / audio_seconds
    } else {
        0.0
    };
    Ok(BenchmarkBaseline {
        schema_version: 1,
        model_id: model_id.to_owned(),
        version: version.to_owned(),
        chunk_ms,
        first_result_latency_ms: first_result_latency_ms.unwrap_or(final_latency_ms),
        final_latency_ms,
        wer_pct: word_error_rate(reference, &hypothesis),
        rtf,
        recorded_at_unix_ms: unix_millis(),
    })
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::word_error_rate;

    #[test]
    fn wer_is_zero_for_identical_text() {
        assert_eq!(word_error_rate("a b c", "a b c"), 0.0);
    }

    #[test]
    fn wer_counts_substitutions() {
        let wer = word_error_rate("a b c", "a x c");
        assert_eq!(wer, 100.0 / 3.0);
    }

    #[test]
    fn wer_is_hundred_for_empty_hypothesis() {
        assert_eq!(word_error_rate("a b", ""), 100.0);
    }

    #[test]
    fn wer_handles_empty_reference() {
        assert_eq!(word_error_rate("", ""), 0.0);
        assert_eq!(word_error_rate("", "x"), 100.0);
    }

    #[test]
    fn wer_counts_insertions() {
        let wer = word_error_rate("a b", "a x b");
        assert_eq!(wer, 50.0);
    }
}
