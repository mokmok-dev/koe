//! Transcribes a WAV file through a live session and prints the text as it
//! arrives.
//!
//! ```text
//! cargo run -p koe-transcribe --features linux --example transcribe -- model.gguf take.wav
//! ```

use std::env;
use std::error::Error;
use std::io::Write;
use std::process::ExitCode;

use koe_transcribe::{AudioSpec, Event, LiveText, Model, Options, Transcriber};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args().skip(1);
    let (Some(model_path), Some(wav_path)) = (args.next(), args.next()) else {
        return Err("usage: transcribe MODEL.gguf AUDIO.wav".into());
    };

    let (pcm, rate, channels) = read_wav(&wav_path)?;
    let spec = AudioSpec::new(rate, channels)?;
    let transcriber = Transcriber::new(&Model::gguf(model_path))?;
    let session = transcriber.live(spec, &Options::new())?;

    let mut live = LiveText::new();
    let block = usize::from(channels) * usize::try_from(rate)?;
    for chunk in pcm.chunks(block) {
        session.feed(chunk)?;
        drain(&session, &mut live)?;
    }
    let transcript = session.finish()?;
    drain_final(&transcript);
    println!("{}", transcript.text());
    if transcript.dropped_ms() > 0 {
        eprintln!("dropped {} ms of audio", transcript.dropped_ms());
    }
    Ok(())
}

fn drain(session: &koe_transcribe::LiveSession, live: &mut LiveText) -> Result<(), Box<dyn Error>> {
    while let Some(event) = session.poll()? {
        live.apply(&event);
        match event {
            Event::Committed { .. } => {
                print!("\r\x1b[2K");
                let _ = writeln!(std::io::stdout(), "{live}");
            }
            Event::Volatile { .. } => {
                print!("\r\x1b[2K  {live}");
                let _ = std::io::stdout().flush();
            }
            _ => {}
        }
    }
    Ok(())
}

fn drain_final(transcript: &koe_transcribe::Transcript) {
    print!("\r\x1b[2K");
    for segment in transcript.segments() {
        let _ = writeln!(
            std::io::stderr(),
            "{:>8}..{:<8} {}",
            segment.start_ms(),
            segment.end_ms(),
            segment.text()
        );
    }
}

fn read_wav(path: &str) -> Result<(Vec<f32>, u32, u16), Box<dyn Error>> {
    let bytes = std::fs::read(path)?;
    if bytes.get(..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err("not a RIFF/WAVE file".into());
    }

    let mut format = 0_u16;
    let mut channels = 0_u16;
    let mut rate = 0_u32;
    let mut bits = 0_u16;
    let mut samples = Vec::new();

    let mut position = 12;
    while position + 8 <= bytes.len() {
        let id = &bytes[position..position + 4];
        let size = u32::from_le_bytes(
            bytes[position + 4..position + 8]
                .try_into()
                .map_err(|_| "truncated chunk header")?,
        ) as usize;
        let body = position + 8;
        let end = body
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or("truncated chunk")?;

        match id {
            b"fmt " => {
                format = u16::from_le_bytes([bytes[body], bytes[body + 1]]);
                channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
                rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into()?);
                bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
            }
            b"data" => {
                if format != 1 || bits != 16 {
                    return Err("only 16-bit PCM WAV is supported".into());
                }
                samples = bytes[body..end]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| f32::from(i16::from_le_bytes(*pair)) / 32_768.0)
                    .collect();
            }
            _ => {}
        }
        position = end + (size & 1);
    }

    if channels == 0 || rate == 0 || samples.is_empty() {
        return Err("WAV has no audio".into());
    }
    Ok((samples, rate, channels))
}
