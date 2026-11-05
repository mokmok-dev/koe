//! Mixing of microphone and system audio into one interleaved block.

use koe_capture_core::Source;

use crate::error::Error;
use crate::spec::Channels;

/// Highest gain a source accepts.
pub const MAX_GAIN: f32 = 8.0;

/// One source's contribution to a mix block.
#[derive(Clone, Copy, Debug)]
pub struct Input<'a> {
    /// Layout of `samples`.
    pub channels: Channels,
    /// Interleaved samples holding whole frames.
    pub samples: &'a [f32],
}

/// What one mix block produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MixReport {
    /// Frames written to the output.
    pub frames: usize,
    /// Samples that the hard clamp pulled back into `-1.0..=1.0`.
    pub clipped: usize,
}

/// Sums the sources into one interleaved block.
#[derive(Clone, Copy, Debug)]
pub struct Mixer {
    channels: Channels,
    microphone_gain: f32,
    system_audio_gain: f32,
}

impl Mixer {
    /// Creates a mixer for an output layout, with every gain at 1.0.
    #[must_use]
    pub const fn new(channels: Channels) -> Self {
        Self {
            channels,
            microphone_gain: 1.0,
            system_audio_gain: 1.0,
        }
    }

    /// Output layout the mixer writes.
    #[must_use]
    pub const fn channels(&self) -> Channels {
        self.channels
    }

    /// Sets the gain of one source.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidGain`] when `gain` is not a finite number
    /// between 0.0 and [`MAX_GAIN`] inclusive.
    pub fn set_gain(&mut self, source: Source, gain: f32) -> Result<(), Error> {
        if !(gain.is_finite() && (0.0..=MAX_GAIN).contains(&gain)) {
            return Err(Error::InvalidGain { source });
        }
        match source {
            Source::Microphone => self.microphone_gain = gain,
            Source::SystemAudio => self.system_audio_gain = gain,
        }
        Ok(())
    }

    /// Gain the mixer applies to a source.
    #[must_use]
    pub const fn gain(&self, source: Source) -> f32 {
        match source {
            Source::Microphone => self.microphone_gain,
            Source::SystemAudio => self.system_audio_gain,
        }
    }

    /// Sums the present inputs into `out`.
    ///
    /// A missing input contributes silence. Channel conversion follows the
    /// output layout: mono to mono copies, mono to stereo duplicates, stereo to
    /// mono averages both channels, and stereo to stereo copies.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MisalignedBuffer`] when `out` does not hold whole
    /// frames or a present input does not hold exactly one frame per output
    /// frame.
    pub fn mix(
        &self,
        microphone: Option<Input<'_>>,
        system_audio: Option<Input<'_>>,
        out: &mut [f32],
    ) -> Result<MixReport, Error> {
        let output_channels = usize::from(self.channels.count());
        if !out.len().is_multiple_of(output_channels) {
            return Err(Error::MisalignedBuffer {
                len: out.len(),
                channels: output_channels,
            });
        }
        let frames = out.len() / output_channels;
        for input in [microphone, system_audio].into_iter().flatten() {
            let channels = usize::from(input.channels.count());
            if input.samples.len() != frames * channels {
                return Err(Error::MisalignedBuffer {
                    len: input.samples.len(),
                    channels,
                });
            }
        }

        let microphone_gain = self.microphone_gain;
        let system_audio_gain = self.system_audio_gain;
        let mut clipped = 0;
        for frame in 0..frames {
            for channel in 0..output_channels {
                let sum = match (microphone, system_audio) {
                    (Some(microphone), Some(system_audio)) => {
                        contribution(microphone, self.channels, frame, channel, microphone_gain)
                            + contribution(
                                system_audio,
                                self.channels,
                                frame,
                                channel,
                                system_audio_gain,
                            )
                    }
                    (Some(microphone), None) => {
                        contribution(microphone, self.channels, frame, channel, microphone_gain)
                    }
                    (None, Some(system_audio)) => contribution(
                        system_audio,
                        self.channels,
                        frame,
                        channel,
                        system_audio_gain,
                    ),
                    (None, None) => 0.0,
                };
                // A gain product can overflow to infinity, and two opposite
                // infinities sum to NaN, which the clamp below would pass on to
                // the encoder.
                let sum = if sum.is_nan() { 0.0 } else { sum };
                let value = if sum > 1.0 {
                    clipped += 1;
                    1.0
                } else if sum < -1.0 {
                    clipped += 1;
                    -1.0
                } else {
                    sum
                };
                out[frame * output_channels + channel] = value;
            }
        }

        Ok(MixReport { frames, clipped })
    }
}

/// Converts one source sample to `channel` of the output frame.
fn contribution(
    input: Input<'_>,
    output: Channels,
    frame: usize,
    channel: usize,
    gain: f32,
) -> f32 {
    let channels = usize::from(input.channels.count());
    let base = frame * channels;
    let sample = match (input.channels, output) {
        (Channels::Stereo, Channels::Mono) => input.samples[base].midpoint(input.samples[base + 1]),
        (Channels::Stereo, Channels::Stereo) => input.samples[base + channel],
        (Channels::Mono, _) => input.samples[base],
    };
    let sample = if sample.is_finite() { sample } else { 0.0 };
    sample * gain
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::float_cmp,
        clippy::cast_precision_loss,
        clippy::needless_range_loop
    )]

    use super::*;
    use proptest::prelude::*;

    fn mono(samples: &[f32]) -> Input<'_> {
        Input {
            channels: Channels::Mono,
            samples,
        }
    }

    fn stereo(samples: &[f32]) -> Input<'_> {
        Input {
            channels: Channels::Stereo,
            samples,
        }
    }

    #[test]
    fn default_gains_are_unity() {
        let mixer = Mixer::new(Channels::Stereo);
        assert_eq!(mixer.channels(), Channels::Stereo);
        assert_eq!(mixer.gain(Source::Microphone), 1.0);
        assert_eq!(mixer.gain(Source::SystemAudio), 1.0);
    }

    #[test]
    fn a_single_source_is_copied_bit_exactly() {
        let mixer = Mixer::new(Channels::Stereo);
        let input = [-0.75, 0.25, 0.5, -0.125];
        let mut out = [9.0; 4];
        let report = mixer.mix(None, Some(stereo(&input)), &mut out).unwrap();
        assert_eq!(out, input, "a unity-gain source must not be altered");
        assert_eq!(
            report,
            MixReport {
                frames: 2,
                clipped: 0
            }
        );
    }

    #[test]
    fn mono_becomes_stereo_by_duplication() {
        let mixer = Mixer::new(Channels::Stereo);
        let mut out = [0.0; 4];
        mixer.mix(Some(mono(&[0.5, -0.5])), None, &mut out).unwrap();
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5]);
    }

    #[test]
    fn stereo_becomes_mono_by_averaging() {
        let mixer = Mixer::new(Channels::Mono);
        let mut out = [0.0; 2];
        mixer
            .mix(Some(stereo(&[1.0, 0.0, 0.5, 0.25])), None, &mut out)
            .unwrap();
        assert_eq!(out, [0.5, 0.375]);
    }

    #[test]
    fn sources_are_summed_with_their_gains() {
        let mut mixer = Mixer::new(Channels::Mono);
        mixer.set_gain(Source::Microphone, 0.5).unwrap();
        mixer.set_gain(Source::SystemAudio, 0.25).unwrap();
        let mut out = [0.0; 2];
        mixer
            .mix(
                Some(mono(&[0.5, -0.75])),
                Some(stereo(&[0.25, 0.25, -0.5, -0.5])),
                &mut out,
            )
            .unwrap();
        assert_eq!(out, [0.3125, -0.5]);
    }

    #[test]
    fn a_missing_source_contributes_silence() {
        let mut mixer = Mixer::new(Channels::Mono);
        mixer.set_gain(Source::Microphone, 0.5).unwrap();
        let mut out = [7.0; 3];
        let report = mixer
            .mix(None, Some(mono(&[0.2, 0.4, 0.6])), &mut out)
            .unwrap();
        assert_eq!(out, [0.2, 0.4, 0.6]);
        assert_eq!(
            report,
            MixReport {
                frames: 3,
                clipped: 0
            }
        );

        let mut silent = [7.0; 2];
        assert_eq!(
            mixer.mix(None, None, &mut silent).unwrap(),
            MixReport {
                frames: 2,
                clipped: 0
            }
        );
        assert_eq!(silent, [0.0, 0.0]);
    }

    #[test]
    fn exact_full_scale_sums_are_in_range() {
        let mixer = Mixer::new(Channels::Mono);
        let mut out = [0.0; 2];
        let report = mixer
            .mix(Some(mono(&[0.5, -0.5])), Some(mono(&[0.5, -0.5])), &mut out)
            .unwrap();
        assert_eq!(out, [1.0, -1.0]);
        assert_eq!(report.clipped, 0, "a sum exactly at full scale is in range");
    }

    #[test]
    fn sums_outside_the_range_are_clamped_and_counted() {
        let mixer = Mixer::new(Channels::Mono);
        let mut out = [0.0; 3];
        let report = mixer
            .mix(
                Some(mono(&[0.8, -0.8, 0.1])),
                Some(mono(&[0.8, -0.8, 0.1])),
                &mut out,
            )
            .unwrap();
        assert_eq!(out, [1.0, -1.0, 0.2]);
        assert_eq!(report.clipped, 2, "only the two saturated samples count");
    }

    #[test]
    fn non_finite_input_becomes_silence() {
        let mixer = Mixer::new(Channels::Mono);
        let mut out = [1.0; 3];
        let report = mixer
            .mix(
                Some(mono(&[f32::NAN, f32::INFINITY, 0.5])),
                Some(mono(&[f32::NEG_INFINITY, 0.0, 0.0])),
                &mut out,
            )
            .unwrap();
        assert_eq!(out, [0.0, 0.0, 0.5]);
        assert_eq!(report.clipped, 0);
    }

    #[test]
    fn an_overflowing_sum_is_clamped() {
        let mixer = Mixer::new(Channels::Mono);
        let mut out = [7.0; 2];
        let report = mixer
            .mix(
                Some(mono(&[f32::MAX, f32::MIN])),
                Some(mono(&[f32::MAX, f32::MIN])),
                &mut out,
            )
            .unwrap();
        assert_eq!(out, [1.0, -1.0]);
        assert_eq!(report.clipped, 2);
    }

    #[test]
    fn opposite_infinities_mix_to_silence() {
        let mut mixer = Mixer::new(Channels::Mono);
        mixer.set_gain(Source::Microphone, MAX_GAIN).unwrap();
        mixer.set_gain(Source::SystemAudio, MAX_GAIN).unwrap();
        let mut out = [7.0; 1];
        let report = mixer
            .mix(Some(mono(&[f32::MAX])), Some(mono(&[f32::MIN])), &mut out)
            .unwrap();
        assert_eq!(out, [0.0]);
        assert_eq!(report.clipped, 0);
    }

    #[test]
    fn a_misaligned_output_is_rejected() {
        let mixer = Mixer::new(Channels::Stereo);
        assert_eq!(
            mixer.mix(None, None, &mut [0.0; 3]),
            Err(Error::MisalignedBuffer {
                len: 3,
                channels: 2
            })
        );
    }

    #[test]
    fn an_input_of_the_wrong_length_is_rejected() {
        let mixer = Mixer::new(Channels::Mono);
        let mut out = [0.0; 2];
        assert_eq!(
            mixer.mix(Some(mono(&[0.1, 0.2, 0.3])), None, &mut out),
            Err(Error::MisalignedBuffer {
                len: 3,
                channels: 1
            })
        );
        assert_eq!(
            mixer.mix(None, Some(stereo(&[0.1, 0.2])), &mut out),
            Err(Error::MisalignedBuffer {
                len: 2,
                channels: 2
            })
        );
    }

    #[test]
    fn gains_outside_the_range_are_rejected() {
        let mut mixer = Mixer::new(Channels::Mono);
        for gain in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.1, 8.5] {
            assert_eq!(
                mixer.set_gain(Source::Microphone, gain),
                Err(Error::InvalidGain {
                    source: Source::Microphone
                }),
                "gain {gain} must be rejected"
            );
            assert_eq!(mixer.gain(Source::Microphone), 1.0);
        }
    }

    #[test]
    fn the_gain_range_is_inclusive() {
        let mut mixer = Mixer::new(Channels::Mono);
        for gain in [0.0, 1.0, MAX_GAIN] {
            mixer.set_gain(Source::SystemAudio, gain).unwrap();
            assert_eq!(mixer.gain(Source::SystemAudio), gain);
        }
    }

    #[test]
    fn an_empty_block_is_a_no_op() {
        let mixer = Mixer::new(Channels::Stereo);
        let mut out = [];
        assert_eq!(
            mixer.mix(None, None, &mut out).unwrap(),
            MixReport {
                frames: 0,
                clipped: 0
            }
        );
    }

    fn layout() -> impl Strategy<Value = Channels> {
        prop_oneof![Just(Channels::Mono), Just(Channels::Stereo)]
    }

    proptest! {
        #[test]
        fn output_stays_finite_and_in_range(
            channels in layout(),
            frames in 0usize..64,
            microphone_channels in layout(),
            system_audio_channels in layout(),
            microphone in proptest::collection::vec(-1.5f32..1.5, 0..64),
            system_audio in proptest::collection::vec(-1.5f32..1.5, 0..64),
            microphone_present in any::<bool>(),
            system_audio_present in any::<bool>(),
            microphone_gain in 0.0f32..=MAX_GAIN,
            system_audio_gain in 0.0f32..=MAX_GAIN,
        ) {
            let mut microphone_samples = microphone;
            microphone_samples.resize(frames * usize::from(microphone_channels.count()), 0.25);
            let mut system_audio_samples = system_audio;
            system_audio_samples.resize(frames * usize::from(system_audio_channels.count()), -0.25);

            let microphone = microphone_present.then(|| Input {
                channels: microphone_channels,
                samples: &microphone_samples,
            });
            let system_audio = system_audio_present.then(|| Input {
                channels: system_audio_channels,
                samples: &system_audio_samples,
            });

            let mut mixer = Mixer::new(channels);
            mixer.set_gain(Source::Microphone, microphone_gain).unwrap();
            mixer.set_gain(Source::SystemAudio, system_audio_gain).unwrap();
            let mut out = vec![0.0; frames * usize::from(channels.count())];
            let report = mixer.mix(microphone, system_audio, &mut out).unwrap();
            prop_assert_eq!(report.frames, frames);
            for sample in &out {
                prop_assert!(sample.is_finite(), "sample {sample} is not finite");
                prop_assert!((-1.0..=1.0).contains(sample), "sample {sample} left the range");
            }
            prop_assert!(report.clipped <= out.len());
        }

        #[test]
        fn one_source_at_unity_gain_is_copied(
            channels in layout(),
            samples in proptest::collection::vec(-1.0f32..=1.0, 0..96),
        ) {
            let frames = samples.len() / usize::from(channels.count());
            let samples = &samples[..frames * usize::from(channels.count())];
            let mixer = Mixer::new(channels);
            let mut out = vec![0.0; samples.len()];
            mixer.mix(None, Some(Input { channels, samples }), &mut out).unwrap();
            prop_assert_eq!(out.as_slice(), samples);
        }

        #[test]
        fn re_chunking_a_stream_keeps_the_output(
            channels in layout(),
            microphone in proptest::collection::vec(-1.0f32..=1.0, 1..96),
            system_audio in proptest::collection::vec(-1.0f32..=1.0, 1..96),
            chunks in proptest::collection::vec(1usize..8, 1..16),
        ) {
            let microphone_frames = microphone.len() / usize::from(channels.count());
            let system_audio_frames = system_audio.len() / usize::from(channels.count());
            let frames = microphone_frames.min(system_audio_frames);
            let width = usize::from(channels.count());
            let microphone = &microphone[..frames * width];
            let system_audio = &system_audio[..frames * width];

            let mixer = Mixer::new(channels);
            let mut whole = vec![0.0; frames * width];
            mixer
                .mix(
                    Some(Input { channels, samples: microphone }),
                    Some(Input { channels, samples: system_audio }),
                    &mut whole,
                )
                .unwrap();

            let mut pieces = Vec::new();
            let mut offset = 0;
            for chunk in chunks {
                if offset == frames {
                    break;
                }
                let count = chunk.min(frames - offset);
                let mut out = vec![0.0; count * width];
                mixer
                    .mix(
                        Some(Input { channels, samples: &microphone[offset * width..(offset + count) * width] }),
                        Some(Input { channels, samples: &system_audio[offset * width..(offset + count) * width] }),
                        &mut out,
                    )
                    .unwrap();
                pieces.extend_from_slice(&out);
                offset += count;
            }
            let mut tail = vec![0.0; (frames - offset) * width];
            mixer
                .mix(
                    Some(Input { channels, samples: &microphone[offset * width..] }),
                    Some(Input { channels, samples: &system_audio[offset * width..] }),
                    &mut tail,
                )
                .unwrap();
            pieces.extend_from_slice(&tail);
            prop_assert_eq!(pieces, whole, "chunked mixing must match one block");
        }
    }
}
