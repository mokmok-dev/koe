//! `PulseAudio` server replies and their mapping to [`DeviceInfo`].

use koe_capture_core::{DeviceId, DeviceInfo, Source};

pub const PA_INVALID_INDEX: u32 = u32::MAX;

#[derive(Default)]
pub struct ServerDefaults {
    pub default_sink: Option<String>,
    pub default_source: Option<String>,
}

pub struct SourceRecord {
    pub name: Option<String>,
    pub description: Option<String>,
    pub monitor_of_sink: u32,
    pub monitor_of_sink_name: Option<String>,
    pub sample_rate: u32,
    pub channels: u8,
}

pub fn describe_source(record: SourceRecord, defaults: &ServerDefaults) -> Option<DeviceInfo> {
    let name = record.name?;
    let description = record
        .description
        .filter(|description| !description.is_empty())
        .unwrap_or_else(|| name.clone());
    let is_monitor = record.monitor_of_sink != PA_INVALID_INDEX;
    let source = if is_monitor {
        Source::SystemAudio
    } else {
        Source::Microphone
    };
    let is_default_source = defaults.default_source.as_deref() == Some(name.as_str());
    let is_default = if is_monitor {
        is_default_source
            || record
                .monitor_of_sink_name
                .is_some_and(|sink| defaults.default_sink.as_deref() == Some(sink.as_str()))
    } else {
        is_default_source
    };
    let Ok(id) = DeviceId::new(name) else {
        return None;
    };
    Some(DeviceInfo::new(
        id,
        description,
        source,
        is_default,
        record.sample_rate,
        u16::from(record.channels),
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn record(name: Option<&str>, description: Option<&str>) -> SourceRecord {
        SourceRecord {
            name: name.map(str::to_owned),
            description: description.map(str::to_owned),
            monitor_of_sink: PA_INVALID_INDEX,
            monitor_of_sink_name: None,
            sample_rate: 48_000,
            channels: 2,
        }
    }

    fn describe(record: SourceRecord) -> DeviceInfo {
        describe_source(record, &ServerDefaults::default()).unwrap()
    }

    #[test]
    fn monitor_source_is_system_audio() {
        let mut entry = record(Some("monitor.speakers"), Some("Monitor"));
        entry.monitor_of_sink = 0;
        assert_eq!(describe(entry).source(), Source::SystemAudio);
    }

    #[test]
    fn plain_source_is_a_microphone() {
        assert_eq!(
            describe(record(Some("mic"), Some("Microphone"))).source(),
            Source::Microphone
        );
    }

    #[test]
    fn monitor_of_the_default_sink_is_default() {
        let mut entry = record(Some("monitor.speakers"), Some("Monitor"));
        entry.monitor_of_sink = 0;
        entry.monitor_of_sink_name = Some("speakers".to_owned());
        let defaults = ServerDefaults {
            default_sink: Some("speakers".to_owned()),
            default_source: None,
        };
        assert!(describe_source(entry, &defaults).unwrap().is_default());
    }

    #[test]
    fn source_named_in_the_server_defaults_is_default() {
        let defaults = ServerDefaults {
            default_sink: None,
            default_source: Some("mic".to_owned()),
        };
        assert!(
            describe_source(record(Some("mic"), Some("Microphone")), &defaults)
                .unwrap()
                .is_default()
        );
    }

    #[test]
    fn unlisted_names_are_not_default() {
        let defaults = ServerDefaults {
            default_sink: Some("speakers".to_owned()),
            default_source: Some("mic".to_owned()),
        };
        let mut entry = record(Some("monitor.headphones"), Some("Monitor"));
        entry.monitor_of_sink = 0;
        entry.monitor_of_sink_name = Some("headphones".to_owned());
        assert!(!describe_source(entry, &defaults).unwrap().is_default());
        assert!(
            !describe_source(record(Some("webcam"), Some("Webcam")), &defaults)
                .unwrap()
                .is_default()
        );
    }

    #[test]
    fn missing_or_empty_description_falls_back_to_the_name() {
        assert_eq!(describe(record(Some("mic"), None)).name(), "mic");
        assert_eq!(describe(record(Some("mic"), Some(""))).name(), "mic");
        assert_eq!(
            describe(record(Some("mic"), Some("Built-in"))).name(),
            "Built-in"
        );
    }

    #[test]
    fn missing_or_empty_name_is_skipped() {
        assert!(describe_source(record(None, Some("Ghost")), &ServerDefaults::default()).is_none());
        assert!(
            describe_source(record(Some(""), Some("Ghost")), &ServerDefaults::default()).is_none()
        );
    }

    #[test]
    fn rate_and_channels_pass_through() {
        let mut entry = record(Some("mic"), Some("Microphone"));
        entry.sample_rate = 44_100;
        entry.channels = 1;
        let device = describe(entry);
        assert_eq!(device.default_sample_rate(), 44_100);
        assert_eq!(device.channels(), 1);

        let mut entry = record(Some("wide"), Some("Wide"));
        entry.sample_rate = 96_000;
        entry.channels = u8::MAX;
        let device = describe(entry);
        assert_eq!(device.default_sample_rate(), 96_000);
        assert_eq!(device.channels(), u16::from(u8::MAX));
    }
}
