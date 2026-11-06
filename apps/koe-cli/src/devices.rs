//! The `devices` command.

use koe_capture::DeviceInfo;

use crate::error::AppError;
use crate::host::Host;
use crate::io::Io;

/// Lists the capture devices the platform reports.
///
/// # Errors
///
/// Returns the capture backend's error when it cannot enumerate devices.
pub fn execute(host: &dyn Host, io: &mut Io<'_>) -> Result<(), AppError> {
    let devices = host.devices()?;
    if devices.is_empty() {
        io.line(format_args!("no capture devices"));
        return Ok(());
    }
    io.line(format_args!(
        "{:<12} {:<7} {:>7} {:>8}  {:<24} {}",
        "SOURCE", "DEFAULT", "RATE", "CHANNELS", "ID", "NAME"
    ));
    for device in &devices {
        io.line(format_args!(
            "{:<12} {:<7} {:>7} {:>8}  {:<24} {}",
            device.source().as_str(),
            default_marker(device),
            device.default_sample_rate(),
            device.channels(),
            device.id().as_str(),
            device.name()
        ));
    }
    Ok(())
}

const fn default_marker(device: &DeviceInfo) -> &'static str {
    if device.is_default() { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use koe_capture::{DeviceId, Source};

    use super::*;
    use crate::fakes::FakeHost;

    fn device(id: &str, name: &str, source: Source, default: bool) -> DeviceInfo {
        DeviceInfo::new(DeviceId::new(id).unwrap(), name, source, default, 48_000, 2)
    }

    fn run(host: &FakeHost) -> (Result<(), AppError>, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut io = Io {
            out: &mut out,
            err: &mut err,
        };
        let result = execute(host, &mut io);
        (
            result,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn a_device_list_is_a_table() {
        let host = FakeHost::with_devices(vec![
            device("mic", "Built-in Microphone", Source::Microphone, true),
            device("sys", "Speakers", Source::SystemAudio, false),
        ]);
        let (result, out, err) = run(&host);
        assert!(result.is_ok());
        assert!(err.is_empty());
        assert_eq!(
            out,
            concat!(
                "SOURCE       DEFAULT    RATE CHANNELS  ID                       NAME\n",
                "microphone   yes       48000        2  mic                      Built-in Microphone\n",
                "system audio no        48000        2  sys                      Speakers\n",
            )
        );
    }

    #[test]
    fn an_empty_list_says_so() {
        let host = FakeHost::with_devices(Vec::new());
        let (result, out, err) = run(&host);
        assert!(result.is_ok());
        assert!(err.is_empty());
        assert_eq!(out, "no capture devices\n");
    }

    #[test]
    fn a_device_error_is_returned() {
        let host = FakeHost::without_devices();
        let (result, out, err) = run(&host);
        assert!(matches!(
            result,
            Err(AppError::Capture(koe_capture::Error::Unsupported))
        ));
        assert!(out.is_empty());
        assert!(err.is_empty());
    }

    #[test]
    fn the_default_marker_is_yes_only_for_the_default() {
        assert_eq!(
            default_marker(&device("a", "A", Source::Microphone, true)),
            "yes"
        );
        assert_eq!(
            default_marker(&device("b", "B", Source::Microphone, false)),
            "no"
        );
    }
}
