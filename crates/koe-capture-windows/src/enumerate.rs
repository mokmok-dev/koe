//! Endpoint records and the decisions that turn them into capture devices.

use koe_capture_core::{DeviceId, DeviceInfo, SampleFormat, Source};

/// `EDataFlow::eCapture`.
pub const FLOW_CAPTURE: u32 = 1;
/// `EDataFlow::eRender`.
pub const FLOW_RENDER: u32 = 0;

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
pub const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// Bytes a `WAVEFORMATEXTENSIBLE` adds past the `WAVEFORMATEX` prefix.
pub const EXTENSIBLE_EXTRA_SIZE: u16 = 22;

/// A `GUID`, which names an interface or an extensible format's subtype.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    pub const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

const KSDATAFORMAT_SUBTYPE_PCM: Guid = Guid::new(
    0x0000_0001,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: Guid = Guid::new(
    0x0000_0003,
    0x0000,
    0x0010,
    [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
);

/// The endpoint format a stream is opened with and decodes packets from.
#[derive(Clone, Copy, Debug)]
pub struct MixFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample: Option<SampleFormat>,
}

/// What enumeration read from one endpoint.
pub struct EndpointRecord {
    pub id: String,
    pub friendly_name: Option<String>,
    pub mix: Option<MixFormat>,
}

/// Maps a `GetMixFormat` tag, container bit depth, extra size, and subtype.
pub fn choose_sample(
    tag: u16,
    bits: u16,
    extra_size: u16,
    sub_format: Option<Guid>,
) -> Option<SampleFormat> {
    match tag {
        WAVE_FORMAT_IEEE_FLOAT if bits == 32 => Some(SampleFormat::F32),
        WAVE_FORMAT_PCM => pcm_sample(bits),
        WAVE_FORMAT_EXTENSIBLE if extra_size >= EXTENSIBLE_EXTRA_SIZE => {
            let sub_format = sub_format?;
            if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && bits == 32 {
                Some(SampleFormat::F32)
            } else if sub_format == KSDATAFORMAT_SUBTYPE_PCM {
                pcm_sample(bits)
            } else {
                None
            }
        }
        _ => None,
    }
}

const fn pcm_sample(bits: u16) -> Option<SampleFormat> {
    match bits {
        16 => Some(SampleFormat::I16),
        24 => Some(SampleFormat::I24),
        32 => Some(SampleFormat::I32),
        _ => None,
    }
}

/// Describes one endpoint, or skips it when it cannot be opened.
pub fn describe_endpoint(
    record: EndpointRecord,
    source: Source,
    default_id: Option<&str>,
) -> Option<DeviceInfo> {
    if record.id.is_empty() {
        return None;
    }
    let native = record.mix?;
    let is_default = default_id == Some(record.id.as_str());
    let name = record.friendly_name.unwrap_or_else(|| record.id.clone());
    let id = DeviceId::new(record.id).ok()?;
    Some(DeviceInfo::new(
        id,
        name,
        source,
        is_default,
        native.sample_rate,
        native.channels,
    ))
}

/// Maps an `EDataFlow` to the capture source its endpoints feed.
pub const fn source_of(flow: u32) -> Source {
    if flow == FLOW_CAPTURE {
        Source::Microphone
    } else {
        Source::SystemAudio
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn record(id: &str, name: Option<&str>, mix: Option<MixFormat>) -> EndpointRecord {
        EndpointRecord {
            id: id.to_owned(),
            friendly_name: name.map(ToOwned::to_owned),
            mix,
        }
    }

    fn mix() -> MixFormat {
        MixFormat {
            sample_rate: 48_000,
            channels: 2,
            sample: Some(SampleFormat::F32),
        }
    }

    fn subtype_float() -> Guid {
        KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    }

    fn subtype_pcm() -> Guid {
        KSDATAFORMAT_SUBTYPE_PCM
    }

    fn subtype_unknown() -> Guid {
        Guid::new(
            0x0000_0092,
            0x0000,
            0x0010,
            [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
        )
    }

    #[test]
    fn data_flows_pair_with_their_sources() {
        assert_eq!(source_of(FLOW_CAPTURE), Source::Microphone);
        assert_eq!(source_of(FLOW_RENDER), Source::SystemAudio);
    }

    #[test]
    fn only_the_default_id_is_marked() {
        let marked = describe_endpoint(
            record("default", Some("Speakers"), Some(mix())),
            Source::SystemAudio,
            Some("default"),
        )
        .unwrap();
        assert!(marked.is_default());

        let plain = describe_endpoint(
            record("other", Some("Speakers"), Some(mix())),
            Source::SystemAudio,
            Some("default"),
        )
        .unwrap();
        assert!(!plain.is_default());

        let unmarked = describe_endpoint(
            record("other", Some("Speakers"), Some(mix())),
            Source::SystemAudio,
            None,
        )
        .unwrap();
        assert!(!unmarked.is_default());
    }

    #[test]
    fn missing_friendly_name_falls_back_to_the_id() {
        let info = describe_endpoint(
            record("{0.0.1.00000000}", None, Some(mix())),
            Source::Microphone,
            None,
        )
        .unwrap();
        assert_eq!(info.id().as_str(), "{0.0.1.00000000}");
        assert_eq!(info.name(), "{0.0.1.00000000}");
        assert_eq!(info.source(), Source::Microphone);
        assert_eq!(info.default_sample_rate(), 48_000);
        assert_eq!(info.channels(), 2);
    }

    #[test]
    fn missing_mix_format_skips_the_endpoint() {
        assert_eq!(
            describe_endpoint(record("id", Some("Name"), None), Source::Microphone, None),
            None
        );
    }

    #[test]
    fn empty_id_is_skipped() {
        assert_eq!(
            describe_endpoint(
                record("", Some("Name"), Some(mix())),
                Source::Microphone,
                None
            ),
            None
        );
    }

    #[test]
    fn plain_tags_select_by_tag_and_depth() {
        assert_eq!(
            choose_sample(WAVE_FORMAT_IEEE_FLOAT, 32, 0, None),
            Some(SampleFormat::F32)
        );
        assert_eq!(
            choose_sample(WAVE_FORMAT_PCM, 16, 0, None),
            Some(SampleFormat::I16)
        );
        assert_eq!(
            choose_sample(WAVE_FORMAT_PCM, 24, 0, None),
            Some(SampleFormat::I24)
        );
        assert_eq!(
            choose_sample(WAVE_FORMAT_PCM, 32, 0, None),
            Some(SampleFormat::I32)
        );
    }

    #[test]
    fn extensible_tags_select_by_subtype() {
        assert_eq!(
            choose_sample(
                WAVE_FORMAT_EXTENSIBLE,
                32,
                EXTENSIBLE_EXTRA_SIZE,
                Some(subtype_float())
            ),
            Some(SampleFormat::F32)
        );
        assert_eq!(
            choose_sample(
                WAVE_FORMAT_EXTENSIBLE,
                16,
                EXTENSIBLE_EXTRA_SIZE,
                Some(subtype_pcm())
            ),
            Some(SampleFormat::I16)
        );
        assert_eq!(
            choose_sample(
                WAVE_FORMAT_EXTENSIBLE,
                32,
                EXTENSIBLE_EXTRA_SIZE,
                Some(subtype_pcm())
            ),
            Some(SampleFormat::I32)
        );
    }

    #[test]
    fn unsupported_tags_depths_and_subtypes_are_rejected() {
        assert_eq!(choose_sample(WAVE_FORMAT_IEEE_FLOAT, 64, 0, None), None);
        assert_eq!(choose_sample(WAVE_FORMAT_PCM, 8, 0, None), None);
        assert_eq!(choose_sample(WAVE_FORMAT_PCM, 20, 0, None), None);
        assert_eq!(choose_sample(0x0002, 16, 0, None), None);
        assert_eq!(
            choose_sample(
                WAVE_FORMAT_EXTENSIBLE,
                32,
                EXTENSIBLE_EXTRA_SIZE,
                Some(subtype_unknown())
            ),
            None
        );
        assert_eq!(
            choose_sample(
                WAVE_FORMAT_EXTENSIBLE,
                16,
                EXTENSIBLE_EXTRA_SIZE,
                Some(subtype_float())
            ),
            None
        );
        assert_eq!(
            choose_sample(
                WAVE_FORMAT_EXTENSIBLE,
                16,
                EXTENSIBLE_EXTRA_SIZE - 1,
                Some(subtype_pcm())
            ),
            None
        );
        assert_eq!(
            choose_sample(WAVE_FORMAT_EXTENSIBLE, 16, EXTENSIBLE_EXTRA_SIZE, None),
            None
        );
    }
}
