//! The `record` command.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use koe_capture::{Source, StreamSpec};
use koe_record::{Error as RecordError, Format, MAX_GAIN, RecordingSpec, Summary};
use koe_transcribe::{AudioSpec, Event, Model, Options};

use crate::cli::RecordArgs;
use crate::error::AppError;
use crate::host::{Host, LiveRecognizer, TrackSink};
use crate::io::Io;
use crate::model::{model_from_path, options};
use crate::transcript::TranscriptWriter;

/// Frames each capture thread hands the recorder per read.
const BLOCK_FRAMES: usize = 960;

type LiveSessions = Vec<(Source, Box<dyn LiveRecognizer>)>;

/// A validated recording request.
#[derive(Debug)]
pub struct RecordPlan {
    output: PathBuf,
    spec: RecordingSpec,
    sources: Vec<(Source, StreamSpec)>,
    gain: f32,
    transcription: Option<TranscriptionPlan>,
}

/// The transcription request that runs beside the recorder.
#[derive(Debug)]
struct TranscriptionPlan {
    model: Model,
    transcript: PathBuf,
    options: Options,
    timestamps: bool,
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
            transcription: transcription_plan(args)?,
        })
    }
}

fn transcription_plan(args: &RecordArgs) -> Result<Option<TranscriptionPlan>, AppError> {
    if args.no_transcribe {
        return Ok(None);
    }
    let model = match &args.model {
        Some(path) => model_from_path(path)?,
        None => Model::system_speech(),
    };
    let transcript = args
        .transcript
        .clone()
        .unwrap_or_else(|| args.output.with_extension("txt"));
    if transcript == args.output {
        return Err(AppError::usage(
            "the transcript path must differ from the output path",
        ));
    }
    Ok(Some(TranscriptionPlan {
        model,
        transcript,
        options: options(args.language.as_deref()),
        timestamps: args.timestamps,
    }))
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

/// Records `args` into its output file and transcript.
pub fn execute(
    args: &RecordArgs,
    host: &dyn Host,
    interrupt: &Arc<AtomicBool>,
    io: &mut Io<'_>,
) -> Result<(), AppError> {
    let plan = RecordPlan::new(args)?;
    let stop = Stop::new(interrupt, args.duration);
    let live = match &plan.transcription {
        Some(transcription) => Some(live_sessions(&plan, transcription, host)?),
        None => None,
    };
    let outcome = pump(&plan, host, &stop, live)?;
    io.line(format_args!(
        "recorded {} frames over {:.3} s to {}",
        outcome.summary.frames,
        outcome.summary.duration.as_secs_f64(),
        plan.output.display()
    ));
    if let Some(transcription) = &plan.transcription {
        io.line(format_args!(
            "transcript: {}",
            transcription.transcript.display()
        ));
    }
    for warning in &outcome.warnings {
        io.warning(format_args!("{warning}"));
    }
    if outcome.summary.clipped_samples > 0 {
        io.warning(format_args!(
            "{} samples clipped",
            outcome.summary.clipped_samples
        ));
    }
    if let Some(error) = outcome.failure {
        return Err(error);
    }
    Ok(())
}

fn live_sessions(
    plan: &RecordPlan,
    transcription: &TranscriptionPlan,
    host: &dyn Host,
) -> Result<LiveSessions, AppError> {
    let transcriber = host.live_transcriber(&transcription.model, &transcription.options)?;
    let mut sessions = Vec::with_capacity(plan.sources.len());
    for (source, spec) in &plan.sources {
        let audio =
            AudioSpec::new(spec.sample_rate(), spec.channels()).map_err(AppError::Transcribe)?;
        sessions.push((*source, transcriber.live(audio)?));
    }
    Ok(sessions)
}

struct RecordOutcome {
    summary: Summary,
    failure: Option<AppError>,
    warnings: Vec<String>,
}

struct Pump<'a> {
    host: &'a dyn Host,
    sink: &'a Mutex<Box<dyn TrackSink>>,
    failure: &'a Mutex<Option<AppError>>,
    stop: &'a Stop,
    writer: Option<&'a Mutex<TranscriptWriter>>,
    ended: [AtomicBool; 2],
    warnings: &'a Mutex<Vec<String>>,
}

fn pump(
    plan: &RecordPlan,
    host: &dyn Host,
    stop: &Stop,
    live: Option<LiveSessions>,
) -> Result<RecordOutcome, AppError> {
    let writer = match &plan.transcription {
        Some(transcription) => Some(Mutex::new(TranscriptWriter::create(
            &transcription.transcript,
            transcription.timestamps,
        )?)),
        None => None,
    };
    let mut sink = host.create_recorder(&plan.output, plan.spec)?;
    for (source, spec) in &plan.sources {
        sink.add_source(*source, *spec)?;
    }
    for (source, _) in &plan.sources {
        sink.set_gain(*source, plan.gain)?;
    }
    let sink = Mutex::new(sink);
    let failure: Mutex<Option<AppError>> = Mutex::new(None);
    let warnings = Mutex::new(Vec::new());
    let mut live: HashMap<Source, Box<dyn LiveRecognizer>> =
        live.unwrap_or_default().into_iter().collect();
    let context = Pump {
        host,
        sink: &sink,
        failure: &failure,
        stop,
        writer: writer.as_ref(),
        ended: [AtomicBool::new(false), AtomicBool::new(false)],
        warnings: &warnings,
    };
    std::thread::scope(|scope| {
        for (source, spec) in &plan.sources {
            let context = &context;
            let live = live.remove(source);
            scope.spawn(move || pump_source(context, *source, *spec, live));
        }
    });
    let mut sink = sink.into_inner().unwrap_or_else(PoisonError::into_inner);
    let finished = sink.finish();
    let writer_finished = writer
        .map(|writer| {
            let writer = writer.into_inner().unwrap_or_else(PoisonError::into_inner);
            writer.finish()
        })
        .transpose();
    let failure = failure.into_inner().unwrap_or_else(PoisonError::into_inner);
    let warnings = warnings
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner);
    let summary = match finished {
        Ok(summary) => summary,
        Err(error) => return Err(failure.unwrap_or(error)),
    };
    writer_finished?;
    Ok(RecordOutcome {
        summary,
        failure,
        warnings,
    })
}

fn pump_source(
    context: &Pump<'_>,
    source: Source,
    spec: StreamSpec,
    mut live: Option<Box<dyn LiveRecognizer>>,
) {
    let mut stream = match context.host.open_capture(spec) {
        Ok(stream) => Some(stream),
        Err(error) => {
            fail(context.failure, context.stop, error);
            None
        }
    };
    if let Some(stream) = stream.as_mut() {
        let channels = usize::from(spec.channels());
        let mut buffer = vec![0.0_f32; channels * BLOCK_FRAMES];
        loop {
            if context.stop.requested() {
                break;
            }
            let read = stream.read(&mut buffer);
            if context.ended[source_index(source)].load(Ordering::Acquire) {
                break;
            }
            match read {
                Ok(0) => break,
                Ok(frames) => {
                    let samples = &buffer[..frames * channels];
                    if let Some(session) = live.as_ref() {
                        if let Err(error) = session.feed(samples) {
                            fail(context.failure, context.stop, error);
                            break;
                        }
                        if let Some(writer) = context.writer
                            && let Err(error) = drain_events(session.as_ref(), writer, source)
                        {
                            fail(context.failure, context.stop, error);
                            break;
                        }
                    }
                    let mut sink = context.sink.lock().unwrap_or_else(PoisonError::into_inner);
                    if let Err(error) = sink.write(source, samples) {
                        if let Err(error) = recover_overflow(sink.as_mut(), context, source, error)
                        {
                            drop(sink);
                            fail(context.failure, context.stop, error);
                            break;
                        }
                        if let Err(error) = sink.write(source, samples) {
                            drop(sink);
                            fail(context.failure, context.stop, error);
                            break;
                        }
                    }
                }
                Err(error) => {
                    fail(context.failure, context.stop, error);
                    break;
                }
            }
        }
    }
    {
        let mut sink = context.sink.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(error) = sink.end(source) {
            fail(context.failure, context.stop, error);
        }
    }
    let transcript = live.take().map(|session| match session.finish() {
        Ok(transcript) => transcript,
        Err(incomplete) => {
            fail(
                context.failure,
                context.stop,
                AppError::Transcribe(incomplete.error().clone()),
            );
            incomplete.into_transcript()
        }
    });
    if let (Some(writer), Some(transcript)) = (context.writer, transcript.as_ref()) {
        let mut writer = writer.lock().unwrap_or_else(PoisonError::into_inner);
        if let Err(error) = writer.finish_source(source, transcript) {
            fail(context.failure, context.stop, error);
        }
    }
}

fn recover_overflow(
    sink: &mut dyn TrackSink,
    context: &Pump<'_>,
    source: Source,
    error: AppError,
) -> Result<(), AppError> {
    match error {
        AppError::Record(RecordError::InputOverflow(_)) => {
            let other = other_source(source);
            sink.end(other)?;
            context.ended[source_index(other)].store(true, Ordering::Release);
            context
                .warnings
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(format!(
                    "{other} delivered no audio; the recording continues without it"
                ));
            // The pair is gone, so this source can drain its backlog alone.
            sink.write(source, &[])?;
            Ok(())
        }
        other => Err(other),
    }
}

const fn source_index(source: Source) -> usize {
    match source {
        Source::Microphone => 0,
        Source::SystemAudio => 1,
    }
}

const fn other_source(source: Source) -> Source {
    match source {
        Source::Microphone => Source::SystemAudio,
        Source::SystemAudio => Source::Microphone,
    }
}

fn drain_events(
    live: &dyn LiveRecognizer,
    writer: &Mutex<TranscriptWriter>,
    source: Source,
) -> Result<(), AppError> {
    while let Some(event) = live.poll()? {
        if let Event::Committed {
            text,
            start_ms,
            end_ms,
        } = event
        {
            let mut writer = writer.lock().unwrap_or_else(PoisonError::into_inner);
            writer.write_committed(source, start_ms, end_ms, text.as_ref())?;
        }
    }
    Ok(())
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
    use koe_transcribe::{Error as TranscribeError, Incomplete, Segment, Transcript};

    use super::*;
    use crate::cli::{ChannelChoice, SourceChoice};
    use crate::fakes::{FakeHost, FakeLiveRecognizer, FakeLiveTranscriber, FakeStream};

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
            model: None,
            transcript: None,
            language: None,
            timestamps: false,
            no_transcribe: true,
        }
    }

    fn transcribing_args(output: &str, transcript: PathBuf) -> RecordArgs {
        let mut args = args(output);
        args.no_transcribe = false;
        args.model = Some(PathBuf::from("tiny.gguf"));
        args.transcript = Some(transcript);
        args
    }

    fn interrupt() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    fn run(
        plan_args: &RecordArgs,
        host: &FakeHost,
    ) -> (Result<RecordOutcome, AppError>, Arc<AtomicBool>) {
        let interrupt = interrupt();
        let plan = RecordPlan::new(plan_args).unwrap();
        let stop = Stop::new(&interrupt, plan_args.duration);
        let live = plan
            .transcription
            .as_ref()
            .map(|transcription| live_sessions(&plan, transcription, host).unwrap());
        let result = pump(&plan, host, &stop, live);
        (result, interrupt)
    }

    fn failure(result: Result<RecordOutcome, AppError>) -> AppError {
        match result {
            Ok(outcome) => outcome.failure.expect("the pump records a source failure"),
            Err(error) => error,
        }
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
        assert!(plan.transcription.is_none());
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
        let result = pump(&plan, &host, &stop, None);
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
        let result = pump(&plan, &host, &stop, None);
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
        let error = failure(result);
        assert!(matches!(
            error,
            AppError::Capture(CaptureError::Unsupported)
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
        let error = failure(result);
        assert!(matches!(
            error,
            AppError::Capture(CaptureError::Backend {
                operation: "read",
                code: 5
            })
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
        match failure(result) {
            AppError::Usage(message) => assert_eq!(message, "write failed"),
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
        match failure(result) {
            AppError::Usage(message) => assert_eq!(message, "end failed"),
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
        match failure(result) {
            AppError::Usage(message) => assert_eq!(message, "finish failed"),
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
        match failure(result) {
            AppError::Usage(message) => assert_eq!(message, "end failed"),
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
            failure(result),
            AppError::Record(RecordError::UnsupportedRate { rate: 44_100 })
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

    #[test]
    fn an_overflow_ends_the_silent_pair_and_keeps_recording() {
        let host = FakeHost::default().with_sink().with_stream(
            Source::Microphone,
            FakeStream::new(1, vec![Ok(vec![0.25; 960]), Ok(Vec::new())]),
        );
        host.sink.lock().unwrap().overflow_write = true;
        let (result, _) = run(&args("take.wav"), &host);
        let outcome = result.unwrap();
        assert!(outcome.failure.is_none());
        assert_eq!(
            outcome.warnings,
            vec!["system audio delivered no audio; the recording continues without it".to_owned()]
        );
        let state = host.sink.lock().unwrap();
        assert!(state.ended.contains(&Source::SystemAudio));
        assert!(state.ended.contains(&Source::Microphone));
        assert!(
            state
                .writes
                .iter()
                .any(|(source, samples)| { *source == Source::Microphone && samples.len() == 960 })
        );
        assert!(state.finished);
    }

    #[test]
    fn an_ended_source_stops_without_failing_the_recording() {
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
                FakeStream::new(2, vec![Ok(vec![0.5; 960]), Ok(Vec::new())])
                    .with_delay(Duration::from_millis(100)),
            );
        host.sink.lock().unwrap().overflow_write = true;
        let (result, _) = run(&args, &host);
        let outcome = result.unwrap();
        assert!(outcome.failure.is_none());
        let state = host.sink.lock().unwrap();
        assert!(state.ended.contains(&Source::SystemAudio));
        assert!(
            !state
                .writes
                .iter()
                .any(|(source, _)| *source == Source::SystemAudio)
        );
        assert!(
            state
                .writes
                .iter()
                .any(|(source, samples)| { *source == Source::Microphone && samples.len() == 960 })
        );
    }

    #[test]
    fn an_overflow_end_failure_is_returned() {
        let host = FakeHost::default().with_sink().with_stream(
            Source::Microphone,
            FakeStream::new(1, vec![Ok(vec![0.25; 960]), Ok(Vec::new())]),
        );
        {
            let mut state = host.sink.lock().unwrap();
            state.overflow_write = true;
            state.fail_end = true;
        }
        let (result, _) = run(&args("take.wav"), &host);
        match failure(result) {
            AppError::Usage(message) => assert_eq!(message, "end failed"),
            other => panic!("expected the end failure, got {other:?}"),
        }
    }

    #[test]
    fn an_overflow_drain_failure_is_returned() {
        let host = FakeHost::default().with_sink().with_stream(
            Source::Microphone,
            FakeStream::new(1, vec![Ok(vec![0.25; 960]), Ok(Vec::new())]),
        );
        {
            let mut state = host.sink.lock().unwrap();
            state.overflow_write = true;
            state.fail_write = true;
        }
        let (result, _) = run(&args("take.wav"), &host);
        match failure(result) {
            AppError::Usage(message) => assert_eq!(message, "write failed"),
            other => panic!("expected the write failure, got {other:?}"),
        }
    }

    #[test]
    fn execute_prints_the_overflow_warning() {
        let host = FakeHost::default().with_sink().with_stream(
            Source::Microphone,
            FakeStream::new(1, vec![Ok(vec![0.25; 960]), Ok(Vec::new())]),
        );
        host.sink.lock().unwrap().overflow_write = true;
        let interrupt = interrupt();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
        };
        execute(&args("take.wav"), &host, &interrupt, &mut io).unwrap();
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("system audio delivered no audio")
        );
    }

    #[test]
    fn source_pairs_name_the_other_source() {
        assert_eq!(source_index(Source::Microphone), 0);
        assert_eq!(source_index(Source::SystemAudio), 1);
        assert_eq!(other_source(Source::Microphone), Source::SystemAudio);
        assert_eq!(other_source(Source::SystemAudio), Source::Microphone);
    }

    #[test]
    fn the_default_transcription_plan_uses_the_system_model_and_a_sibling_txt() {
        let mut args = args("take.wav");
        args.no_transcribe = false;
        let plan = RecordPlan::new(&args).unwrap();
        let transcription = plan.transcription.as_ref().unwrap();
        assert_eq!(transcription.model, Model::system_speech());
        assert_eq!(transcription.transcript, PathBuf::from("take.txt"));
        assert_eq!(transcription.options.language_tag(), None);
        assert!(!transcription.timestamps);
    }

    #[test]
    fn an_explicit_transcription_plan_keeps_its_options() {
        let mut args = args("take.ogg");
        args.no_transcribe = false;
        args.model = Some(PathBuf::from("tiny.gguf"));
        args.transcript = Some(PathBuf::from("notes.txt"));
        args.language = Some("ja".to_owned());
        args.timestamps = true;
        let plan = RecordPlan::new(&args).unwrap();
        let transcription = plan.transcription.as_ref().unwrap();
        assert_eq!(transcription.model, Model::gguf("tiny.gguf"));
        assert_eq!(transcription.transcript, PathBuf::from("notes.txt"));
        assert_eq!(transcription.options.language_tag(), Some("ja"));
        assert!(transcription.timestamps);
    }

    #[test]
    fn a_transcript_path_equal_to_the_output_is_a_usage_error() {
        let mut args = args("take.ogg");
        args.no_transcribe = false;
        args.transcript = Some(PathBuf::from("take.ogg"));
        let error = RecordPlan::new(&args).unwrap_err();
        assert!(matches!(&error, AppError::Usage(_)));
        assert_eq!(error.exit_code(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_model_path_is_a_usage_error() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let mut args = args("take.wav");
        args.no_transcribe = false;
        args.model = Some(PathBuf::from(OsString::from_vec(vec![0xFF])));
        let error = RecordPlan::new(&args).unwrap_err();
        assert!(matches!(&error, AppError::Usage(_)));
        assert_eq!(error.exit_code(), 2);
    }

    fn live_host(transcript: PathBuf, session: FakeLiveRecognizer) -> (RecordArgs, FakeHost) {
        let host = FakeHost::default()
            .with_sink()
            .with_stream(
                Source::Microphone,
                FakeStream::new(1, vec![Ok(vec![0.25; 960]), Ok(Vec::new())]),
            )
            .with_live_transcriber(Box::new(FakeLiveTranscriber::new(vec![session])));
        (transcribing_args("take.wav", transcript), host)
    }

    fn recorded_transcript() -> Transcript {
        Transcript::new(
            vec![
                Segment::new(0, 100, "hello"),
                Segment::new(100, 200, "world"),
            ],
            0,
        )
    }

    #[test]
    fn a_block_is_fed_to_the_live_session_and_the_transcript_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("take.txt");
        let event = Event::Committed {
            text: "hello".into(),
            start_ms: 0,
            end_ms: 100,
        };
        let session = FakeLiveRecognizer::new(Ok(recorded_transcript())).with_events(vec![event]);
        let feeds = Arc::clone(&session.feeds);
        let (mut args, host) = live_host(path.clone(), session);
        args.language = Some("ja".to_owned());
        let (result, _) = run(&args, &host);
        assert!(result.is_ok());
        assert_eq!(host.sink.lock().unwrap().writes.len(), 1);
        assert_eq!(feeds.lock().unwrap().len(), 1);
        assert_eq!(feeds.lock().unwrap()[0], vec![0.25; 960]);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "microphone: hello\nmicrophone: world\n"
        );
        assert_eq!(
            host.live_models_seen.lock().unwrap().as_slice(),
            [Model::gguf("tiny.gguf")]
        );
        assert_eq!(
            host.live_options_seen.lock().unwrap()[0].language_tag(),
            Some("ja")
        );
    }

    #[test]
    fn a_live_feed_failure_is_returned_and_the_final_text_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("take.txt");
        let mut session = FakeLiveRecognizer::new(Ok(recorded_transcript()));
        session.fail_feed = true;
        let (args, host) = live_host(path.clone(), session);
        let (result, _) = run(&args, &host);
        match failure(result) {
            AppError::Usage(message) => assert_eq!(message, "feed failed"),
            other => panic!("expected the feed failure, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "microphone: hello\nmicrophone: world\n"
        );
    }

    #[test]
    fn a_live_poll_failure_is_returned_and_the_final_text_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("take.txt");
        let mut session = FakeLiveRecognizer::new(Ok(recorded_transcript()));
        session.fail_poll = true;
        let (args, host) = live_host(path.clone(), session);
        let (result, _) = run(&args, &host);
        match failure(result) {
            AppError::Usage(message) => assert_eq!(message, "poll failed"),
            other => panic!("expected the poll failure, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "microphone: hello\nmicrophone: world\n"
        );
    }

    #[test]
    fn a_live_finish_failure_is_returned_and_the_committed_text_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("take.txt");
        let incomplete = Incomplete::new(TranscribeError::Unsupported, recorded_transcript());
        let session = FakeLiveRecognizer::new(Err(incomplete));
        let (args, host) = live_host(path.clone(), session);
        let (result, _) = run(&args, &host);
        assert!(matches!(
            failure(result),
            AppError::Transcribe(TranscribeError::Unsupported)
        ));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "microphone: hello\nmicrophone: world\n"
        );
    }

    #[test]
    fn a_transcript_create_failure_is_returned_before_the_recorder() {
        let session = FakeLiveRecognizer::new(Ok(recorded_transcript()));
        let (args, host) = live_host(PathBuf::from("/nonexistent/koe/take.txt"), session);
        let (result, _) = run(&args, &host);
        assert!(matches!(
            failure(result),
            AppError::Io {
                operation: "create transcript",
                ..
            }
        ));
        assert!(host.sink.lock().unwrap().added.is_empty());
    }

    #[test]
    fn execute_returns_a_live_setup_error_before_the_audio_file() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("take.wav");
        let host =
            FakeHost::default().with_live_error(AppError::Transcribe(TranscribeError::Unsupported));
        let args = transcribing_args(output.to_str().unwrap(), dir.path().join("take.txt"));
        let interrupt = interrupt();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
        };
        let error = execute(&args, &host, &interrupt, &mut io).unwrap_err();
        assert!(matches!(
            error,
            AppError::Transcribe(TranscribeError::Unsupported)
        ));
        assert!(out.is_empty());
        assert!(err.is_empty());
        assert!(!output.exists());
    }

    #[test]
    fn execute_reports_the_transcript_path() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("take.wav");
        let transcript = dir.path().join("take.txt");
        let session = FakeLiveRecognizer::new(Ok(Transcript::default()));
        let host = FakeHost::default()
            .with_sink()
            .with_stream(
                Source::Microphone,
                FakeStream::new(1, vec![Ok(vec![0.25; 960]), Ok(Vec::new())]),
            )
            .with_live_transcriber(Box::new(FakeLiveTranscriber::new(vec![session])));
        let args = transcribing_args(output.to_str().unwrap(), transcript.clone());
        let interrupt = interrupt();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
        };
        execute(&args, &host, &interrupt, &mut io).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("transcript: "));
        assert!(text.contains(transcript.to_str().unwrap()));
        assert!(transcript.exists());
    }
}
