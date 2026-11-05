//! Transcribes a WAV file with a GGUF model.
//!
//! ```text
//! cargo run --example transcribe -- model.gguf recording.wav
//! ```

use std::env;
use std::error::Error;
use std::fs;

use koe_transcribe_core::{
    AudioSpec, Converter, EngineEvent, EngineSession, Model, Options, Segment, Transcript,
};
use koe_transcribe_ggml::engine;

/// Samples handed to the session per feed, 100 ms at 16 kHz.
const FEED_SAMPLES: usize = 1_600;

const USAGE: &str = "usage: transcribe MODEL.gguf AUDIO.wav";

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args().skip(1);
    let model_path = args.next().ok_or(USAGE)?;
    let audio_path = args.next().ok_or(USAGE)?;

    let wav = fs::read(audio_path)?;
    let (sample_rate, channels, samples) = read_wav(&wav)?;
    let spec = AudioSpec::new(sample_rate, channels)?;

    let backend = engine(&Model::gguf(model_path))?;
    let mut session = backend.new_session(&Options::new())?;

    let mut converter = Converter::new(spec);
    let mut pcm = Vec::new();
    converter.push(&samples, &mut pcm);

    let mut segments = Vec::new();
    for chunk in pcm.chunks(FEED_SAMPLES) {
        session.feed(chunk)?;
        collect(&mut *session, &mut segments)?;
    }
    session.finish()?;
    collect(&mut *session, &mut segments)?;

    println!("{}", Transcript::new(segments, 0).text());
    Ok(())
}

fn collect(
    session: &mut dyn EngineSession,
    segments: &mut Vec<Segment>,
) -> Result<(), Box<dyn Error>> {
    while let Some(event) = session.poll()? {
        match event {
            EngineEvent::Committed {
                text,
                start_ms,
                end_ms,
            } => {
                eprintln!("{start_ms:>6}..{end_ms:<6} {text}");
                segments.push(Segment::new(start_ms, end_ms, text));
            }
            EngineEvent::Volatile { text } => eprintln!("       ...    {text}"),
            _ => {}
        }
    }
    Ok(())
}

fn read_wav(bytes: &[u8]) -> Result<(u32, u16, Vec<f32>), Box<dyn Error>> {
    if bytes.get(..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err("audio is not a RIFF/WAVE file".into());
    }
    let mut format = None;
    let mut data = None;
    let mut cursor = 12;
    while cursor + 8 <= bytes.len() {
        let id = bytes.get(cursor..cursor + 4).ok_or("truncated WAV chunk")?;
        let size: [u8; 4] = bytes
            .get(cursor + 4..cursor + 8)
            .ok_or("truncated WAV chunk")?
            .try_into()?;
        let size = usize::try_from(u32::from_le_bytes(size))?;
        let body = cursor + 8;
        let end = body
            .checked_add(size)
            .ok_or("oversized WAV chunk")?
            .min(bytes.len());
        let chunk = bytes.get(body..end).ok_or("truncated WAV chunk")?;
        if id == b"fmt " {
            format = Some(parse_format(chunk)?);
        } else if id == b"data" {
            data = Some(chunk);
        }
        cursor = end.saturating_add(size & 1);
    }

    let (channels, sample_rate) = format.ok_or("WAV file has no fmt chunk")?;
    if channels == 0 {
        return Err("WAV file declares no channels".into());
    }
    let data = data.ok_or("WAV file has no data chunk")?;
    let mut samples: Vec<f32> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| f32::from(i16::from_le_bytes(*pair)) / 32_768.0)
        .collect();
    let width = usize::from(channels);
    samples.truncate(samples.len() - samples.len() % width);
    Ok((sample_rate, channels, samples))
}

fn parse_format(chunk: &[u8]) -> Result<(u16, u32), Box<dyn Error>> {
    let encoding: [u8; 2] = chunk.get(0..2).ok_or("short fmt chunk")?.try_into()?;
    let channels: [u8; 2] = chunk.get(2..4).ok_or("short fmt chunk")?.try_into()?;
    let sample_rate: [u8; 4] = chunk.get(4..8).ok_or("short fmt chunk")?.try_into()?;
    let bits: [u8; 2] = chunk.get(14..16).ok_or("short fmt chunk")?.try_into()?;
    if u16::from_le_bytes(encoding) != 1 || u16::from_le_bytes(bits) != 16 {
        return Err("only 16-bit PCM WAV files are supported".into());
    }
    Ok((
        u16::from_le_bytes(channels),
        u32::from_le_bytes(sample_rate),
    ))
}
