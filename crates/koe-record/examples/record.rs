//! Writes one second of a microphone tone and a stereo system-audio tone into
//! one file.
//!
//! ```text
//! cargo run -p koe-record --example record -- /tmp/koe-record-example.ogg
//! cargo run -p koe-record --example record -- /tmp/koe-record-example.wav
//! ```

use std::env;
use std::error::Error;
use std::f32::consts::TAU;
use std::process::ExitCode;

use koe_record::{Channels, Recorder, RecordingSpec, Source, StreamSpec};

const RATE: u32 = 48_000;

/// 20 ms at [`RATE`].
const BLOCK_FRAMES: usize = 960;

const BLOCKS: usize = 50;

fn main() -> ExitCode {
    let path = env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/koe-record-example.ogg".to_owned());
    match run(&path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(path: &str) -> Result<(), Box<dyn Error>> {
    let mut recorder = Recorder::create(path, RecordingSpec::new(RATE, Channels::Stereo)?)?;
    recorder.add_source(
        Source::Microphone,
        &StreamSpec::new(Source::Microphone, RATE, 1)?,
    )?;
    recorder.add_source(
        Source::SystemAudio,
        &StreamSpec::new(Source::SystemAudio, RATE, 2)?,
    )?;

    let mut microphone = vec![0.0_f32; BLOCK_FRAMES];
    let mut system_audio = vec![0.0_f32; BLOCK_FRAMES * 2];
    let mut phases = [0.0_f32; 3];
    let steps = [
        TAU * 440.0 / 48_000.0,
        TAU * 880.0 / 48_000.0,
        TAU * 1320.0 / 48_000.0,
    ];

    for _ in 0..BLOCKS {
        for frame in 0..BLOCK_FRAMES {
            microphone[frame] = 0.25 * phases[0].sin();
            system_audio[frame * 2] = 0.25 * phases[1].sin();
            system_audio[frame * 2 + 1] = 0.25 * phases[2].sin();
            for (phase, step) in phases.iter_mut().zip(steps) {
                *phase += step;
                if *phase > TAU {
                    *phase -= TAU;
                }
            }
        }
        recorder.write(Source::Microphone, &microphone)?;
        recorder.write(Source::SystemAudio, &system_audio)?;
    }

    let summary = recorder.finish()?;
    println!(
        "wrote {path}: {} frames, {} clipped samples, {:.3} s",
        summary.frames,
        summary.clipped_samples,
        summary.duration.as_secs_f64()
    );
    Ok(())
}
