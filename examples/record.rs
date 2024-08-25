/*
wget https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx
cargo run silero_vad.onnx

Todo: Apply loudness normalization to ensure consistent and sufficiently loud audio, possibly using ebur128.
*/

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Sample;
use eyre::{bail, Result};
use once_cell::sync::Lazy;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use vad_rs::{Vad, VadStatus};

static MIN_SPEECH_DUR: Lazy<usize> = Lazy::new(|| 50); // 0.6s
static MIN_SILENCE_DUR: Lazy<usize> = Lazy::new(|| 500); // 1s
static VAD_BUF: Lazy<Mutex<Vec<f32>>> = Lazy::new(|| Mutex::new(Vec::new()));

fn main() -> Result<()> {
    let vad_model_path = std::env::args()
        .nth(1)
        .expect("Please specify vad model filename");
    let vad = Vad::new(vad_model_path, 16000).unwrap();
    let vad_handle = Arc::new(Mutex::new(vad));

    let host = cpal::default_host();

    // Set up the input device and stream with the default input config.
    let device = host
        .default_input_device()
        .expect("failed to find input device");

    println!("Input device: {}", device.name()?);

    let config = device
        .default_input_config()
        .expect("Failed to get default input config");
    println!("Default input config: {:?}", config);

    // A flag to indicate that recording is in progress.
    println!("Begin recording...");

    let err_fn = move |err| {
        eprintln!("an error occurred on stream: {}", err);
    };

    let sample_rate = config.sample_rate().0;
    let channels = config.channels();
    let is_speech = Arc::new(AtomicBool::new(false));
    let speech_dur = Arc::new(AtomicUsize::new(0));
    let silence_dur = Arc::new(AtomicUsize::new(0));

    let stream = match config.sample_format() {
        cpal::SampleFormat::I8 => device.build_input_stream(
            &config.into(),
            move |data, _: &_| {
                on_stream_data::<i8, i8>(
                    data,
                    sample_rate,
                    channels,
                    vad_handle.clone(),
                    is_speech.clone(),
                    speech_dur.clone(),
                    silence_dur.clone(),
                )
            },
            err_fn,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config.into(),
            move |data, _: &_| {
                on_stream_data::<i16, i16>(
                    data,
                    sample_rate,
                    channels,
                    vad_handle.clone(),
                    is_speech.clone(),
                    speech_dur.clone(),
                    silence_dur.clone(),
                )
            },
            err_fn,
            None,
        )?,
        cpal::SampleFormat::I32 => device.build_input_stream(
            &config.into(),
            move |data, _: &_| {
                on_stream_data::<i32, i32>(
                    data,
                    sample_rate,
                    channels,
                    vad_handle.clone(),
                    is_speech.clone(),
                    speech_dur.clone(),
                    silence_dur.clone(),
                )
            },
            err_fn,
            None,
        )?,
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config.into(),
            move |data, _: &_| {
                on_stream_data::<f32, f32>(
                    data,
                    sample_rate,
                    channels,
                    vad_handle.clone(),
                    is_speech.clone(),
                    speech_dur.clone(),
                    silence_dur.clone(),
                )
            },
            err_fn,
            None,
        )?,
        sample_format => {
            bail!("Unsupported sample format '{sample_format}'")
        }
    };

    stream.play()?;

    // Keep main thread alive
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

pub fn audio_resample(
    data: &[f32],
    sample_rate0: u32,
    sample_rate: u32,
    channels: u16,
) -> Vec<f32> {
    use samplerate::{convert, ConverterType};
    convert(
        sample_rate0 as _,
        sample_rate as _,
        channels as _,
        ConverterType::SincBestQuality,
        data,
    )
    .unwrap_or_default()
}

pub fn stereo_to_mono(stereo_data: &[f32]) -> Result<Vec<f32>> {
    if stereo_data.len() % 2 != 0 {
        bail!("Stereo data length should be even.")
    }

    let mut mono_data = Vec::with_capacity(stereo_data.len() / 2);

    for chunk in stereo_data.chunks_exact(2) {
        let average = (chunk[0] + chunk[1]) / 2.0;
        mono_data.push(average);
    }

    Ok(mono_data)
}

fn on_stream_data<T, U>(
    input: &[T],
    sample_rate: u32,
    channels: u16,
    vad_handle: Arc<Mutex<Vad>>,
    is_speech: Arc<AtomicBool>,
    speech_dur: Arc<AtomicUsize>,
    silence_dur: Arc<AtomicUsize>,
) where
    T: Sample,
    U: Sample,
{
    // Convert the input samples to f32
    let samples: Vec<f32> = input
        .iter()
        .map(|s| s.to_float_sample().to_sample())
        .collect();

    // Resample the stereo audio to the desired sample rate
    let mut resampled: Vec<f32> = audio_resample(&samples, sample_rate, 16000, channels);

    if channels > 1 {
        resampled = stereo_to_mono(&resampled).unwrap();
    }

    let chunk_size = (30 * sample_rate / 1000) as usize;
    let mut vad = vad_handle.lock().unwrap();

    let mut vad_buf = VAD_BUF.lock().unwrap();
    vad_buf.extend(resampled.clone());

    if vad_buf.len() as f32 > sample_rate as f32 * 0.1 {
        // 0.1s
        // Start timing
        let start_time = Instant::now();

        // println!("compute {:?}", vad_buf.len());
        if let Ok(mut result) = vad.compute(&vad_buf) {
            // Calculate the elapsed time
            let elapsed_time = start_time.elapsed();
            let elapsed_ms = elapsed_time.as_secs_f64() * 1000.0;

            // Log or handle the situation if computation time exceeds a threshold
            if elapsed_ms > 100.0 {
                eprintln!(
                    "Warning: VAD computation took too long: {} ms (expected < 30 ms)",
                    elapsed_ms
                );
            }

            match result.status() {
                VadStatus::Speech => {
                    speech_dur.fetch_add(chunk_size, Ordering::Relaxed);
                    if speech_dur.load(Ordering::Relaxed) >= *MIN_SPEECH_DUR
                        && !is_speech.load(Ordering::Relaxed)
                    {
                        println!("Speech Start");
                        silence_dur.store(0, Ordering::Relaxed);
                        is_speech.store(true, Ordering::Relaxed);
                        vad_buf.extend(resampled.clone());
                    }
                }
                VadStatus::Silence => {
                    silence_dur.fetch_add(chunk_size, Ordering::Relaxed);
                    if silence_dur.load(Ordering::Relaxed) >= *MIN_SILENCE_DUR
                        && is_speech.load(Ordering::Relaxed)
                    {
                        println!("Speech End");
                        speech_dur.store(0, Ordering::Relaxed);
                        is_speech.store(false, Ordering::Relaxed);
                    }
                }
                _ => {}
            }
        }
        vad_buf.clear();
    }
}
