//! Lists capture devices and reads one short block from a default device.

use koe_capture::{Source, StreamSpec};

fn main() {
    let devices = match koe_capture::devices() {
        Ok(devices) => devices,
        Err(error) => {
            eprintln!("cannot list devices: {error}");
            std::process::exit(1);
        }
    };

    for device in &devices {
        println!(
            "{}\t{}\t{:?}\tdefault={}\t{} Hz\t{} ch",
            device.id(),
            device.name(),
            device.source(),
            device.is_default(),
            device.default_sample_rate(),
            device.channels()
        );
    }

    let source = if devices
        .iter()
        .any(|device| device.source() == Source::SystemAudio)
    {
        Source::SystemAudio
    } else {
        Source::Microphone
    };
    let spec = match StreamSpec::new(source, 48_000, 1) {
        Ok(spec) => spec,
        Err(error) => {
            eprintln!("invalid spec: {error}");
            return;
        }
    };
    let mut capture = match koe_capture::open(&spec) {
        Ok(capture) => capture,
        Err(error) => {
            eprintln!("cannot open {source}: {error}");
            return;
        }
    };

    let mut samples = vec![0.0_f32; 4_800];
    match capture.read(&mut samples) {
        Ok(frames) => {
            let peak = samples
                .iter()
                .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
            println!("read {frames} frames, peak {peak}");
        }
        Err(error) => eprintln!("read failed: {error}"),
    }
}
