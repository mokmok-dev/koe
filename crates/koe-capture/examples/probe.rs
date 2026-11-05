//! Lists capture devices and reads one short block from each default source.

use koe_capture::{DeviceInfo, Source, StreamSpec};

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

    for source in [Source::Microphone, Source::SystemAudio] {
        probe(source, &devices);
    }
}

fn probe(source: Source, devices: &[DeviceInfo]) {
    let Some(device) = devices
        .iter()
        .find(|device| device.source() == source && device.is_default())
        .or_else(|| devices.iter().find(|device| device.source() == source))
    else {
        println!("no {source} device");
        return;
    };
    let rate = if (koe_capture::MIN_SAMPLE_RATE..=koe_capture::MAX_SAMPLE_RATE)
        .contains(&device.default_sample_rate())
    {
        device.default_sample_rate()
    } else {
        48_000
    };
    let channels = device.channels().max(1);
    let spec = match StreamSpec::new(source, rate, channels) {
        Ok(spec) => spec,
        Err(error) => {
            eprintln!("invalid {source} spec: {error}");
            return;
        }
    };
    let mut capture = match koe_capture::open_device(device, &spec) {
        Ok(capture) => capture,
        Err(error) => {
            eprintln!("cannot open {source}: {error}");
            return;
        }
    };

    let mut samples = vec![0.0_f32; usize::from(channels) * (rate as usize / 10)];
    match capture.read(&mut samples) {
        Ok(frames) => {
            let peak = samples
                .iter()
                .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
            println!("{source}: read {frames} frames, peak {peak}");
        }
        Err(error) => eprintln!("{source}: read failed: {error}"),
    }
}
