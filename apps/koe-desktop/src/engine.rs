//! The recorder thread and the events it sends to the UI.
//!
//! One session thread owns the file and one scoped thread per source reads
//! the blocking capture stream. The UI never touches audio, so a stalled
//! device delays only the save, not the frame loop.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use koe_capture::{Source, StreamSpec};
use koe_record::{Channels, Recorder, RecordingSpec, Summary};

use crate::error::DesktopError;
use crate::repaint::Repaint;
use crate::state::Sources;

/// Sample rate every take uses.
pub const SAMPLE_RATE: u32 = 48_000;

/// Frames each capture thread hands the recorder per read.
const BLOCK_FRAMES: usize = 960;

/// How often a source reports its peak level.
const LEVEL_INTERVAL: Duration = Duration::from_millis(100);

/// What the UI asked the recorder to write.
#[derive(Clone, Debug)]
pub struct SessionPlan {
    /// Where the audio goes.
    pub output: PathBuf,
    /// Container and channel layout.
    pub spec: RecordingSpec,
    /// The stream each source opens.
    pub streams: Vec<(Source, StreamSpec)>,
    /// The sources the plan covers.
    pub sources: Sources,
}

impl SessionPlan {
    /// Builds a plan for `sources` at the app's fixed rate.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopError::NoSources`] for an empty set and the capture or
    /// recording error when a fixed value is rejected.
    pub fn meeting(output: PathBuf, sources: Sources) -> Result<Self, DesktopError> {
        if sources.is_empty() {
            return Err(DesktopError::NoSources);
        }
        let spec = RecordingSpec::new(SAMPLE_RATE, Channels::Stereo)?;
        let mut streams = Vec::with_capacity(sources.len());
        for source in sources.iter() {
            let channels = match source {
                Source::Microphone => 1,
                Source::SystemAudio => 2,
            };
            streams.push((source, StreamSpec::new(source, SAMPLE_RATE, channels)?));
        }
        Ok(Self {
            output,
            spec,
            streams,
            sources,
        })
    }
}

/// What the recorder thread reports.
#[derive(Debug)]
pub enum EngineEvent {
    /// A source's device opened.
    Opened(Source),
    /// A source never opened, or stopped delivering.
    Lost {
        /// Source that went away.
        source: Source,
        /// Why it went away.
        message: String,
    },
    /// The peak sample of a block, for the HUD meter.
    Level {
        /// Source the level came from.
        source: Source,
        /// Peak absolute sample in `0.0..=1.0`.
        level: f32,
    },
    /// A non-fatal problem the user should see.
    Warning(String),
    /// The file is complete.
    Finished(Box<Completion>),
}

/// The result of one take.
#[derive(Debug)]
pub struct Completion {
    /// File the recorder wrote.
    pub output: PathBuf,
    /// What the file holds, when the recorder finished cleanly.
    pub summary: Option<Summary>,
    /// Why it did not.
    pub failure: Option<DesktopError>,
}

/// A running recorder.
pub struct Session {
    output: PathBuf,
    stop: Arc<AtomicBool>,
    events: Receiver<EngineEvent>,
}

impl Session {
    /// Starts a recorder thread for `plan`.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopError::Thread`] when the thread cannot be spawned.
    pub fn start(plan: SessionPlan, repaint: Repaint) -> Result<Self, DesktopError> {
        let output = plan.output.clone();
        let (sender, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        thread::Builder::new()
            .name("koe-recorder".to_owned())
            .spawn(move || run(&plan, &thread_stop, &sender, &repaint))
            .map_err(|error| DesktopError::Thread(error.to_string()))?;
        Ok(Self {
            output,
            stop,
            events,
        })
    }

    /// Asks the capture threads to stop after their current block.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Takes the next event without blocking.
    #[must_use]
    pub fn poll(&self) -> Option<EngineEvent> {
        self.events.try_recv().ok()
    }

    /// The file this session writes.
    #[must_use]
    pub fn output(&self) -> &Path {
        &self.output
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(
    plan: &SessionPlan,
    stop: &Arc<AtomicBool>,
    sender: &Sender<EngineEvent>,
    repaint: &Repaint,
) {
    let output = plan.output.clone();
    let mut recorder = match Recorder::create(&plan.output, plan.spec) {
        Ok(recorder) => recorder,
        Err(error) => {
            finish(sender, repaint, output, None, Some(error.into()));
            return;
        }
    };
    for (source, spec) in &plan.streams {
        if let Err(error) = recorder.add_source(*source, spec) {
            let failure = Some(error.into());
            let closed = recorder.finish();
            finish(sender, repaint, output, closed.ok(), failure);
            return;
        }
    }
    let recorder = Mutex::new(recorder);
    thread::scope(|scope| {
        for (source, spec) in &plan.streams {
            let source = *source;
            let spec = *spec;
            let sender = sender.clone();
            let stop = Arc::clone(stop);
            let repaint = Arc::clone(repaint);
            let recorder = &recorder;
            scope.spawn(move || pump(source, spec, &sender, &stop, recorder, &repaint));
        }
    });
    let recorder = recorder
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    let summary = recorder.finish();
    match summary {
        Ok(summary) => finish(sender, repaint, output, Some(summary), None),
        Err(error) => finish(sender, repaint, output, None, Some(error.into())),
    }
}

fn pump(
    source: Source,
    spec: StreamSpec,
    sender: &Sender<EngineEvent>,
    stop: &AtomicBool,
    recorder: &Mutex<Recorder>,
    repaint: &Repaint,
) {
    let mut stream = match koe_capture::open(&spec) {
        Ok(stream) => {
            send(sender, repaint, EngineEvent::Opened(source));
            Some(stream)
        }
        Err(error) => {
            send(
                sender,
                repaint,
                EngineEvent::Lost {
                    source,
                    message: error.to_string(),
                },
            );
            None
        }
    };
    if let Some(stream) = stream.as_mut() {
        let channels = usize::from(spec.channels());
        let mut buffer = vec![0.0_f32; channels * BLOCK_FRAMES];
        let mut last_level = Instant::now();
        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            match stream.read(&mut buffer) {
                Ok(0) => {
                    if !stop.load(Ordering::Acquire) {
                        send(
                            sender,
                            repaint,
                            EngineEvent::Lost {
                                source,
                                message: "the capture stream ended".to_owned(),
                            },
                        );
                    }
                    break;
                }
                Ok(frames) => {
                    let samples = &buffer[..frames * channels];
                    if last_level.elapsed() >= LEVEL_INTERVAL {
                        last_level = Instant::now();
                        send(
                            sender,
                            repaint,
                            EngineEvent::Level {
                                source,
                                level: peak(samples),
                            },
                        );
                    }
                    let mut guard = recorder.lock().unwrap_or_else(PoisonError::into_inner);
                    let written = guard.write(source, samples);
                    drop(guard);
                    if let Err(error) = written {
                        send(
                            sender,
                            repaint,
                            EngineEvent::Lost {
                                source,
                                message: error.to_string(),
                            },
                        );
                        break;
                    }
                }
                Err(error) => {
                    send(
                        sender,
                        repaint,
                        EngineEvent::Lost {
                            source,
                            message: error.to_string(),
                        },
                    );
                    break;
                }
            }
        }
    }
    let mut guard = recorder.lock().unwrap_or_else(PoisonError::into_inner);
    let ended = guard.end(source);
    drop(guard);
    if let Err(error) = ended {
        send(sender, repaint, EngineEvent::Warning(error.to_string()));
    }
}

fn peak(samples: &[f32]) -> f32 {
    samples
        .iter()
        .map(|sample| sample.abs())
        .fold(0.0_f32, f32::max)
}

fn finish(
    sender: &Sender<EngineEvent>,
    repaint: &Repaint,
    output: PathBuf,
    summary: Option<Summary>,
    failure: Option<DesktopError>,
) {
    send(
        sender,
        repaint,
        EngineEvent::Finished(Box::new(Completion {
            output,
            summary,
            failure,
        })),
    );
}

fn send(sender: &Sender<EngineEvent>, repaint: &Repaint, event: EngineEvent) {
    if sender.send(event).is_ok() {
        repaint();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::state::Sources;

    #[test]
    fn a_meeting_plan_asks_for_stereo_output_and_the_usual_stream_layout() {
        let plan = SessionPlan::meeting(PathBuf::from("take.ogg"), Sources::MEETING).unwrap();
        assert_eq!(plan.spec.sample_rate(), SAMPLE_RATE);
        assert_eq!(plan.spec.channels(), Channels::Stereo);
        assert_eq!(plan.sources, Sources::MEETING);
        assert_eq!(plan.streams.len(), 2);
        assert_eq!(plan.streams[0].0, Source::Microphone);
        assert_eq!(plan.streams[0].1.channels(), 1);
        assert_eq!(plan.streams[1].0, Source::SystemAudio);
        assert_eq!(plan.streams[1].1.channels(), 2);
    }

    #[test]
    fn a_plan_with_no_source_is_rejected() {
        assert!(matches!(
            SessionPlan::meeting(PathBuf::from("take.ogg"), Sources::EMPTY),
            Err(DesktopError::NoSources)
        ));
    }

    #[test]
    fn peak_tracks_the_largest_absolute_sample() {
        assert!((peak(&[0.1, -0.8, 0.3]) - 0.8).abs() < f32::EPSILON);
        assert_eq!(peak(&[]), 0.0);
    }
}
