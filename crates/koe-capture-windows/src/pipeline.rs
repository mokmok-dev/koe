//! The wait and pump loops that turn WASAPI packets into interleaved `f32`.

use std::time::{Duration, Instant};

use koe_capture_core::{Error, SampleFormat, StreamSpec, decode};

use crate::enumerate::MixFormat;

pub const WAIT_OBJECT_0: u32 = 0;
pub const WAIT_TIMEOUT: u32 = 258;

pub const fn failed(operation: &'static str, code: i32) -> Error {
    Error::Backend { operation, code }
}

pub const fn invariant(operation: &'static str) -> Error {
    Error::BackendInvariant { operation }
}

/// One packet as the capture client reports it.
#[derive(Clone, Copy)]
pub struct Packet<'packet> {
    pub frames: u32,
    pub silent: bool,
    pub samples: Option<&'packet [u8]>,
}

/// The calls the wait and pump loops need from a capture client.
pub trait PacketSource {
    fn next_packet(&self) -> Result<u32, Error>;
    fn wait_event(&self, milliseconds: u32) -> Result<u32, Error>;
    fn with_packet<R>(
        &self,
        store: impl FnOnce(Packet<'_>) -> Result<R, Error>,
    ) -> Result<R, Error>;
    fn now(&self) -> Instant;
}

/// Samples read from packets but not yet handed to a caller.
pub struct Readback {
    pending: Vec<f32>,
    front: usize,
}

impl Readback {
    pub const fn new() -> Self {
        Self {
            pending: Vec::new(),
            front: 0,
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.front == self.pending.len()
    }
}

/// Milliseconds the frames occupy, floored at one so a wait always waits.
pub fn timeout_milliseconds(frames: usize, rate: u32) -> u32 {
    let frames = u64::try_from(frames).unwrap_or(u64::MAX);
    let milliseconds = frames.saturating_mul(1_000) / u64::from(rate.max(1));
    u32::try_from(milliseconds).unwrap_or(u32::MAX).max(1)
}

/// Bytes one packet of `frames` interleaved samples occupies.
pub fn packet_bytes(frames: u32, channels: usize, format: SampleFormat) -> Result<usize, Error> {
    sample_count(frames, channels)?
        .checked_mul(format.bytes_per_sample())
        .ok_or_else(|| invariant("IAudioCaptureClient::GetBuffer"))
}

fn sample_count(frames: u32, channels: usize) -> Result<usize, Error> {
    usize::try_from(frames)
        .ok()
        .and_then(|frames| frames.checked_mul(channels))
        .ok_or_else(|| invariant("IAudioCaptureClient::GetBuffer"))
}

/// Appends one packet to the readback, silent packets as zeros.
pub fn store(
    state: &mut Readback,
    packet: Packet<'_>,
    channels: usize,
    format: SampleFormat,
) -> Result<(), Error> {
    if packet.frames == 0 {
        return Ok(());
    }
    let count = sample_count(packet.frames, channels)?;
    // A silent packet carries a null data pointer, so the flag decides before
    // the pointer is checked.
    if packet.silent {
        state.pending.resize(state.pending.len() + count, 0.0);
        return Ok(());
    }
    let Some(bytes) = packet.samples else {
        return Err(invariant("IAudioCaptureClient::GetBuffer"));
    };
    let start = state.pending.len();
    state.pending.resize(start + count, 0.0);
    match decode(bytes, format, &mut state.pending[start..]) {
        Ok(decoded) if decoded == count => Ok(()),
        _ => {
            state.pending.truncate(start);
            Err(invariant("IAudioCaptureClient::GetBuffer"))
        }
    }
}

/// Waits for a packet, returning false when the wait times out.
///
/// A loopback endpoint delivers nothing while the machine is silent, so a
/// timeout is silence rather than a failure.
pub fn wait_for_packet(source: &impl PacketSource, milliseconds: u32) -> Result<bool, Error> {
    let deadline = source.now() + Duration::from_millis(u64::from(milliseconds));
    loop {
        let frames = source.next_packet()?;
        if frames > 0 {
            return Ok(true);
        }
        let remaining = deadline.saturating_duration_since(source.now());
        let wait = u32::try_from(remaining.as_millis()).unwrap_or(u32::MAX);
        match source.wait_event(wait)? {
            WAIT_OBJECT_0 => {}
            WAIT_TIMEOUT => return Ok(false),
            code => return Err(failed("WaitForSingleObject", code.cast_signed())),
        }
    }
}

/// Takes one packet into the readback, returning false when none arrived.
pub fn receive(
    source: &impl PacketSource,
    state: &mut Readback,
    channels: usize,
    format: SampleFormat,
    milliseconds: u32,
) -> Result<bool, Error> {
    if !wait_for_packet(source, milliseconds)? {
        return Ok(false);
    }
    source.with_packet(|packet| store(state, packet, channels, format))?;
    Ok(true)
}

/// Copies waiting samples into `out`, keeping the rest for the next call.
pub fn drain(state: &mut Readback, out: &mut [f32]) -> usize {
    let available = state.pending.len() - state.front;
    let taken = available.min(out.len());
    out[..taken].copy_from_slice(&state.pending[state.front..state.front + taken]);
    state.front += taken;
    if state.is_empty() {
        state.pending.clear();
        state.front = 0;
    }
    taken
}

/// Fills `out` from the readback, waiting for packets as needed.
///
/// A wait that times out zero-fills the rest, so a silent device still answers
/// with the full frame count.
pub fn read_readback(
    source: &impl PacketSource,
    state: &mut Readback,
    channels: usize,
    format: SampleFormat,
    rate: u32,
    out: &mut [f32],
) -> Result<usize, Error> {
    if out.is_empty() {
        return Err(Error::BufferTooSmall { frames: 0 });
    }
    if !out.len().is_multiple_of(channels) {
        return Err(invariant("read"));
    }
    let timeout = timeout_milliseconds(out.len() / channels, rate);
    let mut filled = 0;
    loop {
        filled += drain(state, &mut out[filled..]);
        if filled == out.len() {
            break;
        }
        if !receive(source, state, channels, format, timeout)? {
            out[filled..].fill(0.0);
            return Ok(out.len() / channels);
        }
        // A reported packet leaves samples behind, so an empty readback here
        // means the packet source broke its contract. Without this check the
        // loop would retry forever.
        if state.is_empty() {
            return Err(invariant("IAudioCaptureClient::GetBuffer"));
        }
    }
    Ok(filled / channels)
}

/// Checks that the requested spec can be opened on the endpoint's format.
pub const fn check_spec(spec: StreamSpec, native: MixFormat) -> Result<(), Error> {
    if spec.sample_rate() != native.sample_rate || spec.channels() != native.channels {
        return Err(Error::DeviceFormat {
            sample_rate: spec.sample_rate(),
            channels: spec.channels(),
        });
    }
    if native.sample.is_none() {
        return Err(Error::UnsupportedSample);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::float_cmp, clippy::cast_precision_loss)]

    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    use koe_capture_core::Source;
    use proptest::prelude::*;

    /// A pump that stops making progress must fail the test instead of hanging
    /// it, because a hang is reported as a timeout rather than a caught mutant.
    const SOURCE_CALLS: usize = 1_000;

    use super::*;

    #[derive(Clone)]
    struct Scripted {
        frames: u32,
        silent: bool,
        samples: Option<Vec<u8>>,
    }

    fn packet(frames: u32, silent: bool, samples: Option<Vec<u8>>) -> Scripted {
        Scripted {
            frames,
            silent,
            samples,
        }
    }

    fn loud(samples: impl IntoIterator<Item = f32>) -> Scripted {
        let bytes: Vec<u8> = samples.into_iter().flat_map(f32::to_ne_bytes).collect();
        packet(u32::try_from(bytes.len() / 4).unwrap(), false, Some(bytes))
    }

    fn interleaved(samples: impl IntoIterator<Item = f32>, channels: usize) -> Scripted {
        let bytes: Vec<u8> = samples.into_iter().flat_map(f32::to_ne_bytes).collect();
        packet(
            u32::try_from(bytes.len() / 4 / channels).unwrap(),
            false,
            Some(bytes),
        )
    }

    fn as_packet(scripted: &Scripted) -> Packet<'_> {
        Packet {
            frames: scripted.frames,
            silent: scripted.silent,
            samples: scripted.samples.as_deref(),
        }
    }

    struct FakeSource {
        sizes: RefCell<VecDeque<u32>>,
        waits: RefCell<VecDeque<u32>>,
        packets: RefCell<VecDeque<Scripted>>,
        waits_taken: Cell<u32>,
        requested: RefCell<Vec<u32>>,
        calls: Cell<usize>,
        clock: Instant,
        timeout_when_zero: bool,
    }

    impl FakeSource {
        fn new(
            sizes: impl IntoIterator<Item = u32>,
            waits: impl IntoIterator<Item = u32>,
            packets: impl IntoIterator<Item = Scripted>,
        ) -> Self {
            Self {
                sizes: RefCell::new(sizes.into_iter().collect()),
                waits: RefCell::new(waits.into_iter().collect()),
                packets: RefCell::new(packets.into_iter().collect()),
                waits_taken: Cell::new(0),
                requested: RefCell::new(Vec::new()),
                calls: Cell::new(0),
                clock: Instant::now(),
                timeout_when_zero: false,
            }
        }

        fn timing_out_when_zero(mut self) -> Self {
            self.timeout_when_zero = true;
            self
        }

        fn counting(&self) {
            let calls = self.calls.get() + 1;
            assert!(
                calls <= SOURCE_CALLS,
                "the pump loop stopped making progress"
            );
            self.calls.set(calls);
        }
    }

    impl PacketSource for FakeSource {
        fn next_packet(&self) -> Result<u32, Error> {
            self.counting();
            Ok(self.sizes.borrow_mut().pop_front().unwrap_or(0))
        }

        fn wait_event(&self, milliseconds: u32) -> Result<u32, Error> {
            self.counting();
            self.waits_taken.set(self.waits_taken.get() + 1);
            self.requested.borrow_mut().push(milliseconds);
            if self.timeout_when_zero && milliseconds == 0 {
                return Ok(WAIT_TIMEOUT);
            }
            Ok(self.waits.borrow_mut().pop_front().unwrap_or(WAIT_TIMEOUT))
        }

        fn with_packet<R>(
            &self,
            store: impl FnOnce(Packet<'_>) -> Result<R, Error>,
        ) -> Result<R, Error> {
            self.counting();
            let scripted = self.packets.borrow_mut().pop_front().unwrap();
            store(as_packet(&scripted))
        }

        fn now(&self) -> Instant {
            self.clock
        }
    }

    fn mix(sample_rate: u32, channels: u16, sample: Option<SampleFormat>) -> MixFormat {
        MixFormat {
            sample_rate,
            channels,
            sample,
        }
    }

    #[test]
    fn packet_bytes_counts_interleaved_samples() {
        assert_eq!(packet_bytes(2, 2, SampleFormat::F32).unwrap(), 16);
        assert_eq!(packet_bytes(1, 1, SampleFormat::I24).unwrap(), 3);
        assert_eq!(packet_bytes(0, 8, SampleFormat::F32).unwrap(), 0);
    }

    #[test]
    fn packet_bytes_rejects_overflow() {
        assert_eq!(
            packet_bytes(2, usize::MAX, SampleFormat::I16),
            Err(Error::BackendInvariant {
                operation: "IAudioCaptureClient::GetBuffer"
            })
        );
        assert_eq!(
            packet_bytes(1, usize::MAX / 2 + 1, SampleFormat::F32),
            Err(Error::BackendInvariant {
                operation: "IAudioCaptureClient::GetBuffer"
            })
        );
    }

    #[test]
    fn zero_frame_packets_store_nothing() {
        let mut state = Readback::new();
        store(
            &mut state,
            Packet {
                frames: 0,
                silent: false,
                samples: None,
            },
            2,
            SampleFormat::F32,
        )
        .unwrap();
        assert!(state.is_empty());
        assert_eq!(drain(&mut state, &mut [0.0; 2]), 0);
    }

    #[test]
    fn silent_packets_append_zeros_without_a_pointer() {
        let mut state = Readback::new();
        store(
            &mut state,
            Packet {
                frames: 2,
                silent: true,
                samples: None,
            },
            2,
            SampleFormat::F32,
        )
        .unwrap();
        let mut out = [1.0; 4];
        assert_eq!(drain(&mut state, &mut out), 4);
        assert_eq!(out, [0.0; 4]);
    }

    #[test]
    fn a_loud_packet_without_a_pointer_is_an_invariant() {
        let mut state = Readback::new();
        assert_eq!(
            store(
                &mut state,
                Packet {
                    frames: 1,
                    silent: false,
                    samples: None,
                },
                1,
                SampleFormat::F32,
            ),
            Err(Error::BackendInvariant {
                operation: "IAudioCaptureClient::GetBuffer"
            })
        );
        assert!(state.is_empty());
    }

    #[test]
    fn a_short_packet_is_an_invariant() {
        let mut state = Readback::new();
        assert_eq!(
            store(
                &mut state,
                Packet {
                    frames: 2,
                    silent: false,
                    samples: Some(&[0, 0, 0, 0]),
                },
                1,
                SampleFormat::F32,
            ),
            Err(Error::BackendInvariant {
                operation: "IAudioCaptureClient::GetBuffer"
            })
        );
        assert!(state.is_empty());
    }

    #[test]
    fn a_timed_out_wait_is_not_an_error() {
        let source = FakeSource::new([0], [WAIT_TIMEOUT], []);
        assert!(!wait_for_packet(&source, 5).unwrap());
    }

    #[test]
    fn a_spurious_wake_checks_the_size_again() {
        let source = FakeSource::new([0, 0, 1], [WAIT_OBJECT_0, WAIT_OBJECT_0], []);
        assert!(wait_for_packet(&source, 5).unwrap());
        assert_eq!(source.waits_taken.get(), 2);
    }

    #[test]
    fn a_failed_wait_is_reported() {
        let source = FakeSource::new([0], [0xFFFF_FFFF], []);
        assert_eq!(
            wait_for_packet(&source, 5),
            Err(Error::Backend {
                operation: "WaitForSingleObject",
                code: -1,
            })
        );
    }

    #[test]
    fn a_wait_inside_the_window_keeps_polling() {
        let source = FakeSource::new([0, 1], [WAIT_OBJECT_0], []).timing_out_when_zero();
        assert!(wait_for_packet(&source, 5).unwrap());
        assert_eq!(source.requested.borrow().as_slice(), [5]);
    }

    #[test]
    fn drain_resumes_across_reads_and_resets_after_a_full_read() {
        let mut state = Readback::new();
        let first = loud([1.0, 2.0, 3.0]);
        store(&mut state, as_packet(&first), 1, SampleFormat::F32).unwrap();
        let mut out = [0.0; 2];
        assert_eq!(drain(&mut state, &mut out), 2);
        assert_eq!(out, [1.0, 2.0]);
        assert!(!state.is_empty());
        assert_eq!(drain(&mut state, &mut out), 1);
        assert_eq!(out[0], 3.0);
        assert!(state.is_empty());
        assert_eq!(drain(&mut state, &mut out), 0);
        let next = loud([4.0]);
        store(&mut state, as_packet(&next), 1, SampleFormat::F32).unwrap();
        assert_eq!(drain(&mut state, &mut out), 1);
        assert_eq!(out[0], 4.0);
    }

    #[test]
    fn a_timeout_zero_fills_the_tail_and_reports_every_frame() {
        let source = FakeSource::new([1, 0], [WAIT_TIMEOUT], [loud([7.0])]);
        let mut state = Readback::new();
        let mut out = [9.0; 4];
        assert_eq!(
            read_readback(&source, &mut state, 1, SampleFormat::F32, 48_000, &mut out).unwrap(),
            4
        );
        assert_eq!(out, [7.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn packets_split_across_waits_stay_ordered() {
        let source = FakeSource::new(
            [1, 0, 1, 1, 1],
            [WAIT_OBJECT_0],
            [loud([1.0]), loud([2.0]), loud([3.0]), loud([4.0])],
        );
        let mut state = Readback::new();
        let mut out = [0.0; 4];
        assert_eq!(
            read_readback(&source, &mut state, 1, SampleFormat::F32, 48_000, &mut out).unwrap(),
            4
        );
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        assert!(state.is_empty());
    }

    #[test]
    fn a_two_channel_read_requests_the_frames_window() {
        let source = FakeSource::new([1], [WAIT_TIMEOUT], [interleaved([1.0, 2.0], 2)]);
        let mut state = Readback::new();
        let mut out = [0.0; 960];
        assert_eq!(
            read_readback(&source, &mut state, 2, SampleFormat::F32, 48_000, &mut out).unwrap(),
            480
        );
        assert_eq!(out[..2], [1.0, 2.0]);
        assert!(source.requested.borrow().iter().all(|ms| *ms == 10));
    }

    #[test]
    fn a_full_read_does_not_consume_another_packet() {
        let source = FakeSource::new([2], [], [loud([1.0, 2.0])]);
        let mut state = Readback::new();
        let mut out = [0.0; 2];
        assert_eq!(
            read_readback(&source, &mut state, 1, SampleFormat::F32, 48_000, &mut out).unwrap(),
            2
        );
        assert!(source.requested.borrow().is_empty());
    }

    proptest! {
        #[test]
        fn read_readback_keeps_packet_order(
            channels in 1usize..=2,
            wanted in 1usize..=6,
            cuts in proptest::collection::vec(1usize..=3, 0..6),
        ) {
            let mut remaining = wanted;
            let mut sizes = Vec::new();
            let mut packets = Vec::new();
            let mut next = 1.0f32;
            let cover = |frames: usize, sizes: &mut Vec<u32>, packets: &mut Vec<Scripted>, next: &mut f32| {
                sizes.push(u32::try_from(frames).unwrap());
                let mut samples = Vec::new();
                for _ in 0..frames {
                    for _ in 0..channels {
                        samples.push(*next);
                    }
                    *next += 1.0;
                }
                packets.push(interleaved(samples, channels));
            };
            for cut in cuts {
                if remaining == 0 {
                    break;
                }
                let frames = cut.min(remaining);
                remaining -= frames;
                cover(frames, &mut sizes, &mut packets, &mut next);
            }
            if remaining > 0 {
                cover(remaining, &mut sizes, &mut packets, &mut next);
            }

            let source = FakeSource::new(sizes, [], packets);
            let mut state = Readback::new();
            let mut out = vec![-1.0f32; wanted * channels];
            prop_assert_eq!(
                read_readback(
                    &source,
                    &mut state,
                    channels,
                    SampleFormat::F32,
                    48_000,
                    &mut out,
                )
                .unwrap(),
                wanted
            );
            for (index, sample) in out.iter().enumerate() {
                prop_assert_eq!(*sample, 1.0 + (index / channels) as f32);
            }
            prop_assert!(source.requested.borrow().is_empty());
        }
    }

    #[test]
    fn empty_and_misaligned_output_are_rejected() {
        let source = FakeSource::new([], [], []);
        let mut state = Readback::new();
        assert_eq!(
            read_readback(&source, &mut state, 2, SampleFormat::F32, 48_000, &mut []),
            Err(Error::BufferTooSmall { frames: 0 })
        );
        assert_eq!(
            read_readback(
                &source,
                &mut state,
                2,
                SampleFormat::F32,
                48_000,
                &mut [0.0; 3]
            ),
            Err(Error::BackendInvariant { operation: "read" })
        );
    }

    #[test]
    fn timeout_milliseconds_floors_at_one_and_saturates() {
        assert_eq!(timeout_milliseconds(0, 48_000), 1);
        assert_eq!(timeout_milliseconds(1, 384_000), 1);
        assert_eq!(timeout_milliseconds(480, 48_000), 10);
        assert_eq!(timeout_milliseconds(usize::MAX, 1), u32::MAX);
    }

    #[test]
    fn check_spec_rejects_a_rate_or_channel_mismatch() {
        let spec = StreamSpec::new(Source::Microphone, 48_000, 2).unwrap();
        assert_eq!(
            check_spec(spec, mix(48_000, 2, Some(SampleFormat::F32))),
            Ok(())
        );
        assert_eq!(
            check_spec(spec, mix(44_100, 2, Some(SampleFormat::F32))),
            Err(Error::DeviceFormat {
                sample_rate: 48_000,
                channels: 2,
            })
        );
        assert_eq!(
            check_spec(spec, mix(48_000, 1, Some(SampleFormat::F32))),
            Err(Error::DeviceFormat {
                sample_rate: 48_000,
                channels: 2,
            })
        );
    }

    #[test]
    fn check_spec_rejects_a_format_it_cannot_decode() {
        let spec = StreamSpec::new(Source::Microphone, 48_000, 2).unwrap();
        assert_eq!(
            check_spec(spec, mix(48_000, 2, None)),
            Err(Error::UnsupportedSample)
        );
        assert_eq!(
            check_spec(spec, mix(44_100, 2, None)),
            Err(Error::DeviceFormat {
                sample_rate: 48_000,
                channels: 2,
            })
        );
    }
}
