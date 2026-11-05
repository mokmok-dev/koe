//! The `record` command.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use koe_capture::{Source, StreamSpec};
use koe_record::{Format, MAX_GAIN, RecordingSpec, Summary};

use crate::cli::RecordArgs;
use crate::error::AppError;
use crate::host::{Host, TrackSink};
use crate::io::Io;

/// Frames each capture thread hands the recorder per read.
const BLOCK_FRAMES: usize = 960;

/// A validated recording request.
#[derive(Debug)]
pub struct RecordPlan {
    output: PathBuf,
    spec: RecordingSpec,
    sources: Vec<(Source, StreamSpec)>,
    gain: f32,
}

impl RecordPlan {
    fn new(args: &RecordArgs) -> Result<Self, AppError> {
        if Format::from_path(&args.output).is_none() {
            return Err(AppError::usage(format!(
                "no recording format matches `{}`; use .ogg, .oga, .opus, or .wav",
                args.output.display()
            )));
        }
        let spec = RecordingSpec::new(args.rate, args.channels.channels())
            .map_err(|error| AppError::usage(error.to_string()))?;
        let gain = gain_from_db(args.gain)?;
        let mut sources = Vec::with_capacity(args.source.sources().len());
        for source in args.source.sources() {
            let channels = match source {
                Source::Microphone => 1,
                Source::SystemAudio => args.channels.channels().count(),
            };
            let stream = StreamSpec::new(*source, args.rate, channels)
                .map_err(|error| AppError::usage(error.to_string()))?;
            sources.push((*source, stream));
        }
        Ok(Self {
            output: args.output.clone(),
            spec,
            sources,
            gain,
        })
    }
}

/// Converts a decibel gain to the linear factor the mixer takes.
fn gain_from_db(decibels: f32) -> Result<f32, AppError> {
    let gain = 10.0_f32.powf(decibels / 20.0);
    if decibels.is_finite() && (0.0..=MAX_GAIN).contains(&gain) {
        Ok(gain)
    } else {
        Err(AppError::usage(format!(
            "gain `{decibels}` dB is outside the accepted range"
        )))
    }
}

type Clock = fn() -> Instant;

/// Why the pump should stop.
struct Stop {
    interrupt: Arc<AtomicBool>,
    deadline: Option<Instant>,
    now: Clock,
}

impl Stop {
    fn new(interrupt: &Arc<AtomicBool>, duration: Option<Duration>) -> Self {
        Self::with_clock(interrupt, duration, Instant::now)
    }

    fn with_clock(interrupt: &Arc<AtomicBool>, duration: Option<Duration>, now: Clock) -> Self {
        let deadline = duration.and_then(|duration| now().checked_add(duration));
        Self {
            interrupt: Arc::clone(interrupt),
            deadline,
            now,
        }
    }

    fn requested(&self) -> bool {
        self.interrupt.load(std::sync::atomic::Ordering::Acquire)
            || self
                .deadline
                .is_some_and(|deadline| (self.now)() >= deadline)
    }

    fn cancel(&self) {
        self.interrupt
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

/// Records `args` into its output file.
pub fn execute(
    args: &RecordArgs,
    host: &dyn Host,
    interrupt: &Arc<AtomicBool>,
    io: &mut Io<'_>,
) -> Result<(), AppError> {
    let plan = RecordPlan::new(args)?;
    let stop = Stop::new(interrupt, args.duration);
    let summary = pump(&plan, host, &stop)?;
    io.line(format_args!(
        "recorded {} frames over {:.3} s to {}",
        summary.frames,
        summary.duration.as_secs_f64(),
        plan.output.display()
    ));
    if summary.clipped_samples > 0 {
        io.warning(format_args!("{} samples clipped", summary.clipped_samples));
    }
    Ok(())
}

fn pump(plan: &RecordPlan, host: &dyn Host, stop: &Stop) -> Result<Summary, AppError> {
    let mut sink = host.create_recorder(&plan.output, plan.spec)?;
    for (source, spec) in &plan.sources {
        sink.add_source(*source, *spec)?;
    }
    for (source, _) in &plan.sources {
        sink.set_gain(*source, plan.gain)?;
    }
    let sink = Mutex::new(sink);
    let failure: Mutex<Option<AppError>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for (source, spec) in &plan.sources {
            let sink = &sink;
            let failure = &failure;
            scope.spawn(move || pump_source(host, sink, failure, stop, *source, *spec));
        }
    });
    let mut sink = sink.into_inner().unwrap_or_else(PoisonError::into_inner);
    let finished = sink.finish();
    if let Some(error) = failure.into_inner().unwrap_or_else(PoisonError::into_inner) {
        return Err(error);
    }
    finished
}

fn pump_source(
    host: &dyn Host,
    sink: &Mutex<Box<dyn TrackSink>>,
    failure: &Mutex<Option<AppError>>,
    stop: &Stop,
    source: Source,
    spec: StreamSpec,
) {
    let mut stream = match host.open_capture(spec) {
        Ok(stream) => Some(stream),
        Err(error) => {
            fail(failure, stop, error);
            None
        }
    };
    if let Some(stream) = stream.as_mut() {
        let channels = usize::from(spec.channels());
        let mut buffer = vec![0.0_f32; channels * BLOCK_FRAMES];
        loop {
            if stop.requested() {
                break;
            }
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(frames) => {
                    let samples = &buffer[..frames * channels];
                    let mut sink = sink.lock().unwrap_or_else(PoisonError::into_inner);
                    if let Err(error) = sink.write(source, samples) {
                        drop(sink);
                        fail(failure, stop, error);
                        break;
                    }
                }
                Err(error) => {
                    fail(failure, stop, error);
                    break;
                }
            }
        }
    }
    let mut sink = sink.lock().unwrap_or_else(PoisonError::into_inner);
    if let Err(error) = sink.end(source) {
        fail(failure, stop, error);
    }
}

fn fail(failure: &Mutex<Option<AppError>>, stop: &Stop, error: AppError) {
    stop.cancel();
    let mut slot = failure.lock().unwrap_or_else(PoisonError::into_inner);
    if slot.is_none() {
        *slot = Some(error);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp)]

    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    use koe_capture::Error as CaptureError;
    use koe_record::{Channels, Error as RecordError, Summary};

    use super::*;
    use crate::cli::{ChannelChoice, SourceChoice};
    use crate::fakes::{FakeHost, FakeStream};

    static NOW_MS: AtomicU64 = AtomicU64::new(0);
    static CLOCK: Mutex<()> = Mutex::new(());

    fn test_clock() -> Instant {
        static BASE: OnceLock<Instant> = OnceLock::new();
        *BASE.get_or_init(Instant::now) + Duration::from_millis(NOW_MS.load(Ordering::Acquire))
    }

    fn args(output: &str) -> RecordArgs {
        RecordArgs {
            output: PathBuf::from(output),
            source: SourceChoice::Microphone,
            duration: None,
            rate: 48_000,
            channels: ChannelChoice::Mono,
            gain: 0.0,
        }
    }

    fn interrupt() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    fn run(
        plan_args: &RecordArgs,
        host: &FakeHost,
    ) -> (Result<Summary, AppError>, Arc<AtomicBool>) {
        let interrupt = interrupt();
        let plan = RecordPlan::new(plan_args).unwrap();
        let stop = Stop::new(&interrupt, plan_args.duration);
        let result = pump(&plan, host, &stop);
        (result, interrupt)
    }

    #[test]
    fn the_default_plan_records_both_sources_at_stereo() {
        let mut args = args("take.wav");
        args.source = SourceChoice::Both;
        args.channels = ChannelChoice::Stereo;
        args.gain = 0.0;
        let plan = RecordPlan::new(&args).unwrap();
        assert_eq!(plan.output, PathBuf::from("take.wav"));
        assert_eq!(plan.spec.sample_rate(), 48_000);
        assert_eq!(plan.spec.channels(), Channels::Stereo);
        assert_eq!(plan.gain, 1.0);
        assert_eq!(plan.sources.len(), 2);
        assert_eq!(plan.sources[0].0, Source::Microphone);
        assert_eq!(plan.sources[0].1.channels(), 1);
        assert_eq!(plan.sources[1].0, Source::SystemAudio);
        assert_eq!(plan.sources[1].1.channels(), 2);
    }

    #[test]
    fn a_mono_plan_asks_system_audio_for_one_channel() {
        let mut args = args("take.wav");
        args.source = SourceChoice::System;
        args.channels = ChannelChoice::Mono;
        let plan = RecordPlan::new(&args).unwrap();
        assert_eq!(plan.sources[0].0, Source::SystemAudio);
        assert_eq!(plan.sources[0].1.channels(), 1);
    }

    #[test]
    fn an_unknown_extension_is_a_usage_error() {
        for output in ["take.mp3", "take", "take.wave"] {
            assert_eq!(
                RecordPlan::new(&args(output)).unwrap_err().exit_code(),
                2,
                "`{output}` must be rejected"
            );
        }
    }

    #[test]
    fn an_out_of_range_rate_is_a_usage_error() {
        let mut args = args("take.wav");
        args.rate = 1_000;
        assert!(RecordPlan::new(&args).is_err());
        args.rate = 500_000;
        assert!(RecordPlan::new(&args).is_err());
    }

    #[test]
    fn decibel_gain_converts_to_the_linear_range() {
        assert_eq!(gain_from_db(0.0).unwrap(), 1.0);
        assert!((gain_from_db(-6.020_6).unwrap() - 0.5).abs() < 0.000_1);
        assert!((gain_from_db(18.061_8).unwrap() - 8.0).abs() < 0.000_1);
        for bad in [20.0, 100.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(gain_from_db(bad).is_err(), "`{bad}` dB must be rejected");
        }
    }

    #[test]
    fn a_flag_or_a_passed_deadline_stops_the_pump() {
        let _clock = CLOCK.lock().unwrap();
        NOW_MS.store(0, Ordering::Release);
        let interrupt = interrupt();
        let stop = Stop::with_clock(&interrupt, Some(Duration::from_millis(10)), test_clock);
        assert!(!stop.requested());
        NOW_MS.store(9, Ordering::Release);
        assert!(!stop.requested());
        NOW_MS.store(10, Ordering::Release);
        assert!(stop.requested());
        NOW_MS.store(11, Ordering::Release);
        assert!(stop.requested());
        stop.cancel();
        assert!(stop.requested());
    }

    #[test]
    fn a_stop_without_a_deadline_never_expires() {
        let _clock = CLOCK.lock().unwrap();
        NOW_MS.store(u64::MAX, Ordering::Release);
        let interrupt = interrupt();
        assert!(!Stop::with_clock(&interrupt, None, test_clock).requested());
    }

    #[test]
    fn a_deadline_that_overflows_never_expires() {
        let _clock = CLOCK.lock().unwrap();
        NOW_MS.store(0, Ordering::Release);
        let interrupt = interrupt();
        assert!(!Stop::with_clock(&interrupt, Some(Duration::MAX), test_clock).requested());
    }

    #[test]
    fn the_first_failure_wins_and_cancels_the_stop() {
        let interrupt = interrupt();
        let stop = Stop::new(&interrupt, None);
        let failure = Mutex::new(None);
        fail(&failure, &stop, AppError::usage("first"));
        fail(&failure, &stop, AppError::usage("second"));
        match failure.into_inner().unwrap() {
            Some(AppError::Usage(message)) => assert_eq!(message, "first"),
            other => panic!("expected the first usage error, got {other:?}"),
        }
        assert!(interrupt.load(Ordering::Acquire));
    }

    #[test]
    fn a_block_is_written_and_the_sink_is_finished() {
        let host = FakeHost::default().with_sink().with_stream(
            Source::Microphone,
            FakeStream::new(1, vec![Ok(vec![0.25; 960]), Ok(Vec::new())]),
        );
        let (result, _) = run(&args("take.wav"), &host);
        assert!(result.is_ok());
        let state = host.sink.lock().unwrap();
        assert_eq!(state.added.len(), 1);
        assert_eq!(state.added[0].0, Source::Microphone);
        assert_eq!(state.gains, [(Source::Microphone, 1.0)]);
        assert_eq!(state.writes.len(), 1);
        assert_eq!(state.writes[0].0, Source::Microphone);
        assert_eq!(state.writes[0].1.len(), 960);
        assert_eq!(state.ended, [Source::Microphone]);
        assert!(state.finished);
    }

    #[test]
    fn a_stop_before_the_first_read_writes_nothing() {
        let interrupt = interrupt();
        interrupt.store(true, Ordering::Release);
        let plan = RecordPlan::new(&args("take.wav")).unwrap();
        let stop = Stop::new(&interrupt, None);
        let host = FakeHost::default().with_sink().with_stream(
            Source::Microphone,
            FakeStream::new(1, vec![Ok(vec![0.25; 960])]),
        );
        let result = pump(&plan, &host, &stop);
        assert!(result.is_ok());
        let state = host.sink.lock().unwrap();
        assert!(state.writes.is_empty());
        assert_eq!(state.ended, [Source::Microphone]);
    }

    #[test]
    fn a_deadline_before_the_first_read_writes_nothing() {
        let _clock = CLOCK.lock().unwrap();
        NOW_MS.store(0, Ordering::Release);
        let interrupt = interrupt();
        let stop = Stop::with_clock(&interrupt, Some(Duration::from_millis(1)), test_clock);
        NOW_MS.store(2, Ordering::Release);
        let plan = RecordPlan::new(&args("take.wav")).unwrap();
        let host = FakeHost::default().with_sink().with_stream(
            Source::Microphone,
            FakeStream::new(1, vec![Ok(vec![0.25; 960])]),
        );
        let result = pump(&plan, &host, &stop);
        assert!(result.is_ok());
        assert!(host.sink.lock().unwrap().writes.is_empty());
    }

    #[test]
    fn both_sources_are_pumped_independently() {
        let mut args = args("take.wav");
        args.source = SourceChoice::Both;
        args.channels = ChannelChoice::Stereo;
        let host = FakeHost::default()
            .with_sink()
            .with_stream(
                Source::Microphone,
                FakeStream::new(1, vec![Ok(vec![0.25; 960]), Ok(Vec::new())]),
            )
            .with_stream(
                Source::SystemAudio,
                FakeStream::new(2, vec![Ok(vec![0.5; 1_920]), Ok(Vec::new())]),
            );
        let (result, _) = run(&args, &host);
        assert!(result.is_ok());
        let state = host.sink.lock().unwrap();
        assert!(
            state
                .writes
                .iter()
                .any(|(source, samples)| { *source == Source::Microphone && samples.len() == 960 })
        );
        assert!(
            state
                .writes
                .iter()
                .any(|(source, samples)| *source == Source::SystemAudio && samples.len() == 1_920)
        );
        let mut ended = state.ended.clone();
        ended.sort_unstable();
        assert_eq!(ended, [Source::Microphone, Source::SystemAudio]);
        assert!(state.finished);
    }

    #[test]
    fn an_open_failure_is_returned_and_the_file_is_finished() {
        let host = FakeHost::default().with_sink().with_open_error(
            Source::Microphone,
            AppError::Capture(CaptureError::Unsupported),
        );
        let (result, interrupt) = run(&args("take.wav"), &host);
        assert!(matches!(
            result,
            Err(AppError::Capture(CaptureError::Unsupported))
        ));
        assert!(interrupt.load(Ordering::Acquire));
        let state = host.sink.lock().unwrap();
        assert_eq!(state.ended, [Source::Microphone]);
        assert!(state.finished);
    }

    #[test]
    fn a_read_failure_is_returned() {
        let error = AppError::Capture(CaptureError::Backend {
            operation: "read",
            code: 5,
        });
        let host = FakeHost::default()
            .with_sink()
            .with_stream(Source::Microphone, FakeStream::new(1, vec![Err(error)]));
        let (result, _) = run(&args("take.wav"), &host);
        assert!(matches!(
            result,
            Err(AppError::Capture(CaptureError::Backend {
                operation: "read",
                code: 5
            }))
        ));
        assert_eq!(host.sink.lock().unwrap().ended, [Source::Microphone]);
    }

    #[test]
    fn a_write_failure_is_returned() {
        let host = FakeHost::default().with_sink().with_stream(
            Source::Microphone,
            FakeStream::new(1, vec![Ok(vec![0.25; 960]), Ok(Vec::new())]),
        );
        host.sink.lock().unwrap().fail_write = true;
        let (result, _) = run(&args("take.wav"), &host);
        match result {
            Err(AppError::Usage(message)) => assert_eq!(message, "write failed"),
            other => panic!("expected the write failure, got {other:?}"),
        }
        let state = host.sink.lock().unwrap();
        assert_eq!(state.ended, [Source::Microphone]);
        assert!(state.finished);
    }

    #[test]
    fn an_end_failure_is_returned() {
        let host = FakeHost::default()
            .with_sink()
            .with_stream(Source::Microphone, FakeStream::new(1, vec![Ok(Vec::new())]));
        host.sink.lock().unwrap().fail_end = true;
        let (result, _) = run(&args("take.wav"), &host);
        match result {
            Err(AppError::Usage(message)) => assert_eq!(message, "end failed"),
            other => panic!("expected the end failure, got {other:?}"),
        }
        assert!(host.sink.lock().unwrap().finished);
    }

    #[test]
    fn a_finish_failure_is_returned() {
        let host = FakeHost::default()
            .with_sink()
            .with_stream(Source::Microphone, FakeStream::new(1, vec![Ok(Vec::new())]));
        host.sink.lock().unwrap().fail_finish = true;
        let (result, _) = run(&args("take.wav"), &host);
        match result {
            Err(AppError::Usage(message)) => assert_eq!(message, "finish failed"),
            other => panic!("expected the finish failure, got {other:?}"),
        }
    }

    #[test]
    fn a_source_failure_wins_over_a_finish_failure() {
        let host = FakeHost::default()
            .with_sink()
            .with_stream(Source::Microphone, FakeStream::new(1, vec![Ok(Vec::new())]));
        {
            let mut state = host.sink.lock().unwrap();
            state.fail_end = true;
            state.fail_finish = true;
        }
        let (result, _) = run(&args("take.wav"), &host);
        match result {
            Err(AppError::Usage(message)) => assert_eq!(message, "end failed"),
            other => panic!("expected the end failure, got {other:?}"),
        }
    }

    #[test]
    fn a_create_failure_is_returned_before_a_source_opens() {
        let host =
            FakeHost::default().with_create_error(AppError::Record(RecordError::UnsupportedRate {
                rate: 44_100,
            }));
        let mut args = args("take.ogg");
        args.rate = 44_100;
        let (result, _) = run(&args, &host);
        assert!(matches!(
            result,
            Err(AppError::Record(RecordError::UnsupportedRate {
                rate: 44_100
            }))
        ));
        assert!(host.opens_seen.lock().unwrap().is_empty());
    }

    #[test]
    fn execute_prints_the_summary_and_a_clipping_warning() {
        let host = FakeHost::default().with_sink().with_stream(
            Source::Microphone,
            FakeStream::new(1, vec![Ok(vec![0.25; 960]), Ok(Vec::new())]),
        );
        {
            let mut state = host.sink.lock().unwrap();
            state.summary = Summary {
                frames: 960,
                clipped_samples: 3,
                duration: Duration::from_millis(20),
            };
        }
        let interrupt = interrupt();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
        };
        execute(&args("take.wav"), &host, &interrupt, &mut io).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "recorded 960 frames over 0.020 s to take.wav\n"
        );
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "warning: 3 samples clipped\n"
        );
    }

    #[test]
    fn execute_does_not_warn_when_nothing_clipped() {
        let host = FakeHost::default()
            .with_sink()
            .with_stream(Source::Microphone, FakeStream::new(1, vec![Ok(Vec::new())]));
        {
            let mut state = host.sink.lock().unwrap();
            state.summary = Summary {
                frames: 0,
                clipped_samples: 0,
                duration: Duration::ZERO,
            };
        }
        let interrupt = interrupt();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
        };
        execute(&args("take.wav"), &host, &interrupt, &mut io).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "recorded 0 frames over 0.000 s to take.wav\n"
        );
        assert!(err.is_empty());
    }
}
