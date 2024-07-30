//! Feeds back the input stream directly into the output stream.
//!
//! Assumes that the input and output devices can use the same stream configuration and that they
//! support the f32 sample format.
//!
//! Uses a delay of `LATENCY_MS` milliseconds in case the default input and output streams are not
//! precisely synchronised.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::{
    traits::{Consumer, Producer, Split},
    HeapRb,
};

#[derive(Parser, Debug)]
#[command(version, about = "CPAL feedback example", long_about = None)]
struct Opt {
    /// The input audio device to use
    #[arg(short, long, value_name = "IN", default_value_t = String::from("default"))]
    input_device: String,

    /// The output audio device to use
    #[arg(short, long, value_name = "OUT", default_value_t = String::from("default"))]
    output_device: String,

    /// Specify the delay between input and output
    #[arg(short, long, value_name = "DELAY_MS", default_value_t = 150.0)]
    latency: f32,

    /// Use the JACK host
    #[cfg(all(
        any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd"
        ),
        feature = "jack"
    ))]
    #[arg(short, long)]
    #[allow(dead_code)]
    jack: bool,
}

fn main() -> anyhow::Result<()> {
    let opt = Opt::parse();

    // Conditionally compile with jack if the feature is specified.
    #[cfg(all(
        any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd"
        ),
        feature = "jack"
    ))]
    // Manually check for flags. Can be passed through cargo with -- e.g.
    // cargo run --release --example beep --features jack -- --jack
    let host = if opt.jack {
        cpal::host_from_id(cpal::available_hosts()
            .into_iter()
            .find(|id| *id == cpal::HostId::Jack)
            .expect(
                "make sure --features jack is specified. only works on OSes where jack is available",
            )).expect("jack host unavailable")
    } else {
        cpal::default_host()
    };

    #[cfg(any(
        not(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd"
        )),
        not(feature = "jack")
    ))]
    let host = cpal::default_host();

    // Find devices.
    let input_device = if opt.input_device == "default" {
        host.default_input_device()
    } else {
        host.input_devices()?
            .find(|x| x.name().map(|y| y == opt.input_device).unwrap_or(false))
    }
    .expect("failed to find input device");

    let output_device = if opt.output_device == "default" {
        host.default_output_device()
    } else {
        host.output_devices()?
            .find(|x| x.name().map(|y| y == opt.output_device).unwrap_or(false))
    }
    .expect("failed to find output device");

    println!("Using input device: \"{}\"", input_device.name()?);
    println!("Using output device: \"{}\"", output_device.name()?);

    // We'll try and use the same configuration between streams to keep it simple.
    let config: cpal::StreamConfig = input_device.default_input_config()?.into();

    // Create a delay in case the input and output devices aren't synced.
    let latency_frames = (opt.latency / 1_000.0) * config.sample_rate.0 as f32;
    let latency_samples = latency_frames as usize * config.channels as usize;

    // The buffer to share samples
    let ring = HeapRb::<f32>::new(latency_samples * 2);
    let (mut producer, mut consumer) = ring.split();

    // Fill the samples with 0.0 equal to the length of the delay.
    for _ in 0..latency_samples {
        // The ring buffer has twice as much space as necessary to add latency here,
        // so this should never fail
        producer.try_push(0.0).unwrap();
    }

    let mut input_instant: Option<std::time::Instant> = None;
    let inputs: Arc<Mutex<Vec<Duration>>> = Default::default();
    let inputs_clone = inputs.clone();
    let input_data_fn = move |data: &[f32], _: &cpal::InputCallbackInfo| {
        if let Some(instant) = input_instant {
            inputs_clone.lock().unwrap().push(instant.elapsed());
        }
        input_instant = Some(std::time::Instant::now());
        let mut output_fell_behind = false;
        for &sample in data {
            if producer.try_push(sample).is_err() {
                output_fell_behind = true;
            }
        }
        if output_fell_behind {
            eprintln!("output stream fell behind: try increasing latency");
        }
    };

    let mut output_instant: Option<std::time::Instant> = None;
    let outputs: Arc<Mutex<Vec<Duration>>> = Default::default();
    let outputs_clone = outputs.clone();
    let output_data_fn = move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
        if let Some(instant) = output_instant {
            outputs_clone.lock().unwrap().push(instant.elapsed());
        }
        output_instant = Some(std::time::Instant::now());
        let mut input_fell_behind = false;
        for sample in data {
            *sample = match consumer.try_pop() {
                Some(s) => s,
                None => {
                    input_fell_behind = true;
                    0.0
                }
            };
        }
        if input_fell_behind {
            eprintln!("input stream fell behind: try increasing latency");
        }
    };

    // Build streams.
    println!(
        "Attempting to build both streams with f32 samples and `{:?}`.",
        config
    );
    let input_stream = input_device.build_input_stream(&config, input_data_fn, err_fn, None)?;
    let output_stream = output_device.build_output_stream(&config, output_data_fn, err_fn, None)?;
    println!("Successfully built streams.");

    // Play the streams.
    println!(
        "Starting the input and output streams with `{}` milliseconds of latency.",
        opt.latency
    );
    input_stream.play()?;
    output_stream.play()?;

    let time = 90;
    println!("Playing for {time} seconds... ");
    std::thread::sleep(std::time::Duration::from_secs(time));
    drop(input_stream);
    drop(output_stream);
    println!("Done!");

    let input_count = inputs.lock().unwrap().len();
    let output_count = outputs.lock().unwrap().len();

    let inputs_sum = inputs.lock().unwrap().iter().sum::<std::time::Duration>();
    let outputs_sum = outputs.lock().unwrap().iter().sum::<std::time::Duration>();

    println!(
        "Input stream timings: {:?}",
        inputs_sum / input_count as u32
    );
    println!(
        "output stream timings: {:?}",
        outputs_sum / output_count as u32
    );

    Ok(())
}

fn err_fn(err: cpal::StreamError) {
    eprintln!("an error occurred on stream: {}", err);
}

/*

3s:

Using input device: "麦克风阵列 (Realtek(R) Audio)"
Using output device: "扬声器 (Realtek(R) Audio)"
Attempting to build both streams with f32 samples and `StreamConfig { channels: 2, sample_rate: SampleRate(48000), buffer_size: Default }`.
Successfully built streams.
Starting the input and output streams with `150` milliseconds of latency.
Playing for 3 seconds...
Done!
Input stream timings: 10.008363ms
output stream timings: 10.647124ms

30s:
Using input device: "麦克风阵列 (Realtek(R) Audio)"
Using output device: "扬声器 (Realtek(R) Audio)"
Attempting to build both streams with f32 samples and `StreamConfig { channels: 2, sample_rate: SampleRate(48000), buffer_size: Default }`.
Successfully built streams.
Starting the input and output streams with `150` milliseconds of latency.
Playing for 30 seconds...
Done!
Input stream timings: 9.999472ms
output stream timings: 10.664227ms

90s:
Using input device: "麦克风阵列 (Realtek(R) Audio)"
Using output device: "扬声器 (Realtek(R) Audio)"
Attempting to build both streams with f32 samples and `StreamConfig { channels: 2, sample_rate: SampleRate(48000), buffer_size: Default }`.
Successfully built streams.
Starting the input and output streams with `150` milliseconds of latency.
Playing for 90 seconds...
Done!
Input stream timings: 10.000525ms
output stream timings: 10.665383ms

*/
