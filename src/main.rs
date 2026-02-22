use std::env;
use std::error::Error;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use hound::{SampleFormat as WavSampleFormat, WavReader};
use rdev::{Event, EventType};

const MAX_ACTIVE_GRAINS: usize = 48;
const GRAIN_MS: f32 = 140.0;

#[derive(Clone)]
struct GrainVoice {
    start_idx: usize,
    position: usize,
    length: usize,
    fade_len: usize,
}

impl GrainVoice {
    fn new(start_idx: usize, length: usize) -> Self {
        Self {
            start_idx,
            position: 0,
            length,
            fade_len: (length / 10).max(16),
        }
    }

    fn is_finished(&self) -> bool {
        self.position >= self.length
    }

    fn envelope(&self) -> f32 {
        let pos = self.position;
        let end_region_start = self.length.saturating_sub(self.fade_len);

        if pos < self.fade_len {
            pos as f32 / self.fade_len as f32
        } else if pos >= end_region_start {
            let remaining = self.length.saturating_sub(pos);
            remaining as f32 / self.fade_len as f32
        } else {
            1.0
        }
    }
}

struct SampleEngine {
    samples: Vec<f32>,
    cursor: usize,
    grain_len: usize,
    active: Vec<GrainVoice>,
}

impl SampleEngine {
    fn new(samples: Vec<f32>, output_sample_rate: u32) -> Self {
        let grain_len = ((GRAIN_MS / 1000.0) * output_sample_rate as f32).round() as usize;
        Self {
            samples,
            cursor: 0,
            grain_len: grain_len.max(32),
            active: Vec::with_capacity(MAX_ACTIVE_GRAINS),
        }
    }

    fn trigger_forward_slice(&mut self) {
        if self.samples.is_empty() {
            return;
        }

        let start = self.cursor;
        self.cursor = (self.cursor + self.grain_len) % self.samples.len();

        if self.active.len() >= MAX_ACTIVE_GRAINS {
            self.active.remove(0);
        }

        self.active.push(GrainVoice::new(start, self.grain_len));
    }

    fn next_sample(&mut self) -> f32 {
        if self.samples.is_empty() {
            return 0.0;
        }

        let mut output = 0.0_f32;
        for voice in &mut self.active {
            let idx = (voice.start_idx + voice.position) % self.samples.len();
            let env = voice.envelope();
            output += self.samples[idx] * env;
            voice.position = voice.position.saturating_add(1);
        }

        self.active.retain(|voice| !voice.is_finished());
        (output * 0.7).clamp(-1.0, 1.0)
    }
}

fn resample_linear(input: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    if input.is_empty() || src_rate == dst_rate {
        return input.to_vec();
    }

    let ratio = dst_rate as f64 / src_rate as f64;
    let out_len = (input.len() as f64 * ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len.max(1));

    for i in 0..out_len {
        let src_pos = i as f64 / ratio;
        let idx = src_pos.floor() as usize;
        let frac = (src_pos - idx as f64) as f32;

        let a = input[idx.min(input.len() - 1)];
        let b = input[(idx + 1).min(input.len() - 1)];
        out.push(a + (b - a) * frac);
    }

    out
}

fn load_wav_as_mono(path: &Path) -> Result<(Vec<f32>, u32), Box<dyn Error>> {
    let mut reader = WavReader::open(path)?;
    let spec = reader.spec();
    let channels = spec.channels as usize;

    if channels == 0 {
        return Err("WAV file has zero channels".into());
    }

    let interleaved: Vec<f32> = match spec.sample_format {
        WavSampleFormat::Float => reader.samples::<f32>().collect::<Result<Vec<_>, _>>()?,
        WavSampleFormat::Int => {
            if spec.bits_per_sample <= 16 {
                reader
                    .samples::<i16>()
                    .map(|s| s.map(|v| v as f32 / i16::MAX as f32))
                    .collect::<Result<Vec<_>, _>>()?
            } else {
                reader
                    .samples::<i32>()
                    .map(|s| s.map(|v| v as f32 / i32::MAX as f32))
                    .collect::<Result<Vec<_>, _>>()?
            }
        }
    };

    let frame_count = interleaved.len() / channels;
    let mut mono = Vec::with_capacity(frame_count);

    for frame_idx in 0..frame_count {
        let frame_start = frame_idx * channels;
        let mut sum = 0.0_f32;
        for ch in 0..channels {
            sum += interleaved[frame_start + ch];
        }
        mono.push(sum / channels as f32);
    }

    Ok((mono, spec.sample_rate))
}

fn build_stream_for_format(
    device: &cpal::Device,
    config: &StreamConfig,
    sample_format: SampleFormat,
    engine: Arc<Mutex<SampleEngine>>,
) -> Result<Stream, cpal::BuildStreamError> {
    let channels = config.channels as usize;
    let error_handler = |err| eprintln!("Audio stream error: {err}");

    match sample_format {
        SampleFormat::F32 => device.build_output_stream(
            config,
            move |buffer: &mut [f32], _| write_data_f32(buffer, channels, &engine),
            error_handler,
            None,
        ),
        SampleFormat::I16 => device.build_output_stream(
            config,
            move |buffer: &mut [i16], _| write_data_i16(buffer, channels, &engine),
            error_handler,
            None,
        ),
        SampleFormat::U16 => device.build_output_stream(
            config,
            move |buffer: &mut [u16], _| write_data_u16(buffer, channels, &engine),
            error_handler,
            None,
        ),
        _ => unreachable!("Unsupported audio sample format"),
    }
}

fn write_data_f32(output: &mut [f32], channels: usize, engine: &Arc<Mutex<SampleEngine>>) {
    if let Ok(mut engine) = engine.lock() {
        for frame in output.chunks_mut(channels) {
            let sample = engine.next_sample();
            for out in frame {
                *out = sample;
            }
        }
    } else {
        output.fill(0.0);
    }
}

fn write_data_i16(output: &mut [i16], channels: usize, engine: &Arc<Mutex<SampleEngine>>) {
    if let Ok(mut engine) = engine.lock() {
        for frame in output.chunks_mut(channels) {
            let sample = engine.next_sample();
            let sample_i16 = (sample * i16::MAX as f32) as i16;
            for out in frame {
                *out = sample_i16;
            }
        }
    } else {
        output.fill(0);
    }
}

fn write_data_u16(output: &mut [u16], channels: usize, engine: &Arc<Mutex<SampleEngine>>) {
    if let Ok(mut engine) = engine.lock() {
        for frame in output.chunks_mut(channels) {
            let sample = engine.next_sample();
            let sample_u16 = ((sample * 0.5 + 0.5) * u16::MAX as f32) as u16;
            for out in frame {
                *out = sample_u16;
            }
        }
    } else {
        output.fill(u16::MAX / 2);
    }
}

fn get_input_wav_path() -> Result<String, Box<dyn Error>> {
    print!("Enter path to .wav file: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    let path = input.trim();
    if path.is_empty() {
        return Err("No input file provided".into());
    }

    Ok(path.to_string())
}

fn run() -> Result<(), Box<dyn Error>> {
    let wav_path = match env::args().nth(1) {
        Some(path) => path,
        None => get_input_wav_path()?,
    };

    let host = cpal::default_host();
    let output_device = host
        .default_output_device()
        .ok_or("No default output audio device found")?;

    let supported_config = output_device.default_output_config()?;
    let output_sample_rate = supported_config.sample_rate();
    let stream_config: StreamConfig = supported_config.clone().into();

    let (source_samples, source_rate) = load_wav_as_mono(Path::new(&wav_path))?;
    let playback_samples = resample_linear(&source_samples, source_rate, output_sample_rate);

    if playback_samples.is_empty() {
        return Err("Provided audio file produced no playable samples".into());
    }

    let engine = Arc::new(Mutex::new(SampleEngine::new(
        playback_samples,
        output_sample_rate,
    )));

    let stream = build_stream_for_format(
        &output_device,
        &stream_config,
        supported_config.sample_format(),
        Arc::clone(&engine),
    )?;
    stream.play()?;

    println!(
        "TypeMusic running. Press any key globally to play the next slice of '{}' (Ctrl+C to stop).",
        wav_path
    );

    let listener_engine = Arc::clone(&engine);
    let callback = move |event: Event| {
        if let EventType::KeyPress(_) = event.event_type {
            if let Ok(mut engine) = listener_engine.lock() {
                engine.trigger_forward_slice();
            }
        }
    };

    if let Err(err) = rdev::listen(callback) {
        eprintln!("Global listener error: {err:?}");
    }

    Ok(())
}

fn main() {
    if let Err(err) = run() {
        eprintln!("TypeMusic failed: {err}");
    }
}
