//! Device records read off `CoreAudio` and their mapping to [`DeviceInfo`].

use koe_capture_core::{DeviceId, DeviceInfo, Error, Source, StreamSpec};

pub const SYSTEM_PREFIX: &str = "system:";

/// One `AudioObject` as the enumeration read it.
pub struct DeviceRecord {
    pub object: u32,
    pub uid: Option<String>,
    pub name: Option<String>,
    pub input_channels: u32,
    pub output_channels: u32,
    pub sample_rate: Option<f64>,
}

/// Default input and output device of the system object.
pub struct Defaults {
    pub input: u32,
    pub output: u32,
}

/// The `AudioObject` a capture request opens.
#[derive(Debug)]
pub enum Target {
    Input(u32),
    Output,
}

/// Channels across every buffer of one stream configuration.
pub fn total_channels(counts: impl IntoIterator<Item = u32>) -> u32 {
    counts.into_iter().fold(0, u32::saturating_add)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
// A float-to-int cast saturates, so a broken rate lands on 0 or `u32::MAX`.
pub fn describe_device(record: DeviceRecord, defaults: &Defaults) -> Vec<DeviceInfo> {
    let mut devices = Vec::new();
    let Some(uid) = record.uid else {
        return devices;
    };
    let name = record
        .name
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| uid.clone());
    let rate = record.sample_rate.unwrap_or(0.0) as u32;
    if record.input_channels > 0
        && let Ok(id) = DeviceId::new(uid.as_str())
    {
        let default = record.object == defaults.input;
        let channels = u16::try_from(record.input_channels).unwrap_or(u16::MAX);
        let entry = DeviceInfo::new(
            id,
            name.clone(),
            Source::Microphone,
            default,
            rate,
            channels,
        );
        devices.push(entry);
    }
    if record.output_channels > 0
        && let Ok(id) = DeviceId::new(format!("{SYSTEM_PREFIX}{uid}"))
    {
        let default = record.object == defaults.output;
        // The stereo global tap always delivers two channels.
        let entry = DeviceInfo::new(id, name, Source::SystemAudio, default, rate, 2);
        devices.push(entry);
    }
    devices
}

/// Rejects a system-audio request the stereo global tap cannot carry.
pub const fn check_spec(source: Source, spec: StreamSpec) -> Result<(), Error> {
    if matches!(source, Source::SystemAudio) && spec.channels() != 2 {
        Err(Error::DeviceFormat {
            sample_rate: spec.sample_rate(),
            channels: spec.channels(),
        })
    } else {
        Ok(())
    }
}

/// Finds the object `device` names, rejecting a system target that has no
/// output streams to tap.
pub fn resolve(device: &DeviceInfo, catalog: &[DeviceRecord]) -> Result<Target, Error> {
    let id = device.id().as_str();
    let uid = match device.source() {
        Source::Microphone => id,
        Source::SystemAudio => id.strip_prefix(SYSTEM_PREFIX).unwrap_or_default(),
    };
    let record = catalog
        .iter()
        .find(|record| record.uid.as_deref() == Some(uid));
    match (device.source(), record) {
        (Source::Microphone, Some(record)) => Ok(Target::Input(record.object)),
        (Source::SystemAudio, Some(record)) if record.output_channels > 0 => Ok(Target::Output),
        _ => Err(Error::DeviceNotFound(device.id().clone())),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn record(object: u32, uid: &str) -> DeviceRecord {
        DeviceRecord {
            object,
            uid: Some(uid.to_owned()),
            name: Some(format!("{uid} device")),
            input_channels: 2,
            output_channels: 2,
            sample_rate: Some(48_000.0),
        }
    }

    fn info(id: &str, source: Source) -> DeviceInfo {
        DeviceInfo::new(
            DeviceId::new(id).unwrap(),
            "device",
            source,
            false,
            48_000,
            2,
        )
    }

    fn defaults() -> Defaults {
        Defaults {
            input: 0,
            output: 0,
        }
    }

    #[test]
    fn no_uid_means_no_entry() {
        let record = DeviceRecord {
            uid: None,
            ..record(1, "uid")
        };
        let devices = describe_device(record, &defaults());
        assert_eq!(devices.len(), 0);
    }

    #[test]
    fn a_missing_or_empty_name_falls_back_to_the_uid() {
        for name in [None, Some(String::new())] {
            let record = DeviceRecord {
                name,
                ..record(1, "uid")
            };
            let devices = describe_device(record, &defaults());
            assert_eq!(devices.len(), 2);
            assert_eq!(devices[0].name(), "uid");
            assert_eq!(devices[1].name(), "uid");
        }
    }

    #[test]
    fn input_then_output_produce_two_entries_in_that_order() {
        let devices = describe_device(record(1, "uid"), &defaults());
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].id().as_str(), "uid");
        assert_eq!(devices[0].source(), Source::Microphone);
        assert_eq!(devices[1].id().as_str(), "system:uid");
        assert_eq!(devices[1].source(), Source::SystemAudio);
    }

    #[test]
    fn a_device_without_streams_produces_no_entry() {
        let record = DeviceRecord {
            input_channels: 0,
            output_channels: 0,
            ..record(1, "uid")
        };
        let devices = describe_device(record, &defaults());
        assert_eq!(devices.len(), 0);
    }

    #[test]
    fn input_channels_saturate_at_u16_max() {
        let record = DeviceRecord {
            input_channels: u32::MAX,
            ..record(1, "uid")
        };
        let devices = describe_device(record, &defaults());
        assert_eq!(devices[0].channels(), u16::MAX);
    }

    #[test]
    fn buffer_channels_are_summed_without_overflow() {
        assert_eq!(total_channels([2, 3]), 5);
        assert_eq!(total_channels([u32::MAX, 1]), u32::MAX);
        assert_eq!(total_channels(std::iter::empty()), 0);
    }

    #[test]
    fn rate_truncates_and_a_broken_rate_saturates() {
        for (reported, expected) in [(48_000.9, 48_000), (f64::NAN, 0), (f64::MAX, u32::MAX)] {
            let record = DeviceRecord {
                sample_rate: Some(reported),
                ..record(1, "uid")
            };
            let devices = describe_device(record, &defaults());
            assert_eq!(devices[0].default_sample_rate(), expected);
        }

        let record = DeviceRecord {
            sample_rate: None,
            ..record(1, "uid")
        };
        let devices = describe_device(record, &defaults());
        assert_eq!(devices[0].default_sample_rate(), 0);
    }

    #[test]
    fn defaults_mark_only_their_own_side() {
        let defaults = Defaults {
            input: 1,
            output: 2,
        };

        let microphone = describe_device(record(1, "mic"), &defaults);
        assert!(microphone[0].is_default());
        assert!(!microphone[1].is_default());

        let system = describe_device(record(2, "out"), &defaults);
        assert!(!system[0].is_default());
        assert!(system[1].is_default());

        let neither = describe_device(record(3, "other"), &defaults);
        assert!(neither.iter().all(|device| !device.is_default()));
    }

    #[test]
    fn a_system_id_resolves_through_the_prefix() {
        let catalog = [record(7, "uid")];
        let target = resolve(&info("system:uid", Source::SystemAudio), &catalog).unwrap();
        assert!(matches!(target, Target::Output));
    }

    #[test]
    fn a_microphone_resolves_to_its_input_object() {
        let catalog = [record(7, "uid")];
        let target = resolve(&info("uid", Source::Microphone), &catalog).unwrap();
        assert!(matches!(target, Target::Input(7)));
    }

    #[test]
    fn an_unknown_uid_is_not_found() {
        let catalog = [record(7, "uid")];
        let device = info("system:absent", Source::SystemAudio);
        assert_eq!(
            resolve(&device, &catalog).err(),
            Some(Error::DeviceNotFound(device.id().clone()))
        );
    }

    #[test]
    fn a_system_target_without_output_channels_is_not_found() {
        let catalog = [DeviceRecord {
            output_channels: 0,
            ..record(7, "uid")
        }];
        let device = info("system:uid", Source::SystemAudio);
        assert_eq!(
            resolve(&device, &catalog).err(),
            Some(Error::DeviceNotFound(device.id().clone()))
        );
    }

    #[test]
    fn a_system_id_without_the_prefix_is_not_found() {
        let catalog = [record(7, "uid")];
        let device = info("uid", Source::SystemAudio);
        assert_eq!(
            resolve(&device, &catalog).err(),
            Some(Error::DeviceNotFound(device.id().clone()))
        );
    }

    #[test]
    fn system_audio_requires_the_stereo_tap() {
        let spec = StreamSpec::new(Source::SystemAudio, 48_000, 1).unwrap();
        assert_eq!(
            check_spec(Source::SystemAudio, spec),
            Err(Error::DeviceFormat {
                sample_rate: 48_000,
                channels: 1,
            })
        );
        let stereo = StreamSpec::new(Source::SystemAudio, 48_000, 2).unwrap();
        assert_eq!(check_spec(Source::SystemAudio, stereo), Ok(()));
    }

    #[test]
    fn microphones_take_any_channel_count() {
        for channels in [1, 2, 6] {
            let spec = StreamSpec::new(Source::Microphone, 48_000, channels).unwrap();
            assert_eq!(check_spec(Source::Microphone, spec), Ok(()));
        }
    }
}
