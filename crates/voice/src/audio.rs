use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, SizedSample, Stream, StreamConfig};
use parking_lot::Mutex;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, PolynomialDegree, Resampler};

const REALTIME_RATE: u32 = 24_000;
const INPUT_CHUNK_SAMPLES: usize = 1_200;
const INPUT_CHUNK_SAMPLES_F32: f32 = 1_200.0;
const RESAMPLE_CHUNK_MILLIS: u32 = 10;
const AUDIO_CHANNEL_CAPACITY: usize = 8;

#[allow(clippy::cast_possible_truncation)]
fn normalized_to_i16(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn normalized_to_u16(value: f32) -> u16 {
    ((value.clamp(-1.0, 1.0) + 1.0) * 32_767.5).round() as u16
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AudioInputChunk {
    pub samples: Vec<i16>,
    pub rms: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AudioInputEvent {
    Chunk(AudioInputChunk),
    Error(String),
}

struct InputProcessor {
    channels: usize,
    chunk_frames: usize,
    pending: VecDeque<f32>,
    input: Vec<f32>,
    output: Vec<f32>,
    pcm: VecDeque<i16>,
    resampler: Async<f32>,
}

impl InputProcessor {
    fn new(sample_rate: u32, channels: usize) -> Result<Self> {
        let chunk_frames = usize::try_from(sample_rate / (1_000 / RESAMPLE_CHUNK_MILLIS))
            .unwrap_or(480)
            .max(64);
        let resampler = Async::<f32>::new_poly(
            f64::from(REALTIME_RATE) / f64::from(sample_rate),
            1.1,
            PolynomialDegree::Cubic,
            chunk_frames,
            1,
            FixedAsync::Input,
        )
        .context("create microphone resampler")?;
        let output_frames = resampler.output_frames_max();
        Ok(Self {
            channels,
            chunk_frames,
            pending: VecDeque::with_capacity(chunk_frames * 2),
            input: vec![0.0; chunk_frames],
            output: vec![0.0; output_frames],
            pcm: VecDeque::with_capacity(INPUT_CHUNK_SAMPLES * 2),
            resampler,
        })
    }

    fn process<T: Copy>(
        &mut self,
        data: &[T],
        convert: fn(T) -> f32,
        sender: &SyncSender<AudioInputEvent>,
    ) {
        let channel_count =
            f32::from(u16::try_from(self.channels).expect("CPAL channel count fits in u16"));
        for frame in data.chunks_exact(self.channels) {
            let mono = frame.iter().copied().map(convert).sum::<f32>() / channel_count;
            self.pending.push_back(mono.clamp(-1.0, 1.0));
        }
        while self.pending.len() >= self.chunk_frames {
            for sample in &mut self.input {
                *sample = self.pending.pop_front().unwrap_or_default();
            }
            let input = InterleavedSlice::new(&self.input, 1, self.chunk_frames)
                .expect("valid mono input adapter");
            let output_capacity = self.output.len();
            let mut output = InterleavedSlice::new_mut(&mut self.output, 1, output_capacity)
                .expect("valid mono output adapter");
            let frames = match self
                .resampler
                .process_into_buffer(&input, &mut output, None)
            {
                Ok((_, frames)) => frames,
                Err(error) => {
                    let _ = sender.try_send(AudioInputEvent::Error(format!(
                        "resample microphone audio: {error}"
                    )));
                    return;
                }
            };
            self.pcm.extend(
                self.output[..frames]
                    .iter()
                    .map(|sample| normalized_to_i16(*sample)),
            );
            while self.pcm.len() >= INPUT_CHUNK_SAMPLES {
                let mut samples = Vec::with_capacity(INPUT_CHUNK_SAMPLES);
                samples.extend(self.pcm.drain(..INPUT_CHUNK_SAMPLES));
                let energy = samples
                    .iter()
                    .map(|sample| {
                        let sample = f32::from(*sample) / f32::from(i16::MAX);
                        sample * sample
                    })
                    .sum::<f32>();
                let rms = (energy / INPUT_CHUNK_SAMPLES_F32).sqrt();
                let _ = sender.try_send(AudioInputEvent::Chunk(AudioInputChunk { samples, rms }));
            }
        }
    }
}

#[derive(Debug)]
struct PlaybackChunk {
    item_id: String,
    samples: Vec<f32>,
    cursor: usize,
}

#[derive(Debug, Default)]
struct PlaybackState {
    queue: VecDeque<PlaybackChunk>,
    current_item_id: Option<String>,
    played_device_frames: u64,
}

struct OutputResampler {
    item_id: Option<String>,
    chunk_frames: usize,
    pending: VecDeque<f32>,
    input: Vec<f32>,
    output: Vec<f32>,
    resampler: Async<f32>,
}

impl OutputResampler {
    fn new(device_rate: u32) -> Result<Self> {
        let chunk_frames = usize::try_from(REALTIME_RATE / (1_000 / RESAMPLE_CHUNK_MILLIS))
            .unwrap_or(240)
            .max(64);
        let resampler = Async::<f32>::new_poly(
            f64::from(device_rate) / f64::from(REALTIME_RATE),
            1.1,
            PolynomialDegree::Cubic,
            chunk_frames,
            1,
            FixedAsync::Input,
        )
        .context("create playback resampler")?;
        let output_frames = resampler.output_frames_max();
        Ok(Self {
            item_id: None,
            chunk_frames,
            pending: VecDeque::with_capacity(chunk_frames * 2),
            input: vec![0.0; chunk_frames],
            output: vec![0.0; output_frames],
            resampler,
        })
    }

    fn push(
        &mut self,
        item_id: &str,
        samples: &[i16],
        playback: &Arc<Mutex<PlaybackState>>,
        active: &Arc<AtomicBool>,
    ) -> Result<()> {
        if self
            .item_id
            .as_deref()
            .is_some_and(|current| current != item_id)
        {
            self.flush(playback, active)?;
        }
        self.item_id = Some(item_id.to_owned());
        self.pending.extend(
            samples
                .iter()
                .map(|sample| f32::from(*sample) / f32::from(i16::MAX)),
        );
        self.process_full_chunks(playback, active)
    }

    fn finish(
        &mut self,
        playback: &Arc<Mutex<PlaybackState>>,
        active: &Arc<AtomicBool>,
    ) -> Result<()> {
        self.flush(playback, active)
    }

    fn flush(
        &mut self,
        playback: &Arc<Mutex<PlaybackState>>,
        active: &Arc<AtomicBool>,
    ) -> Result<()> {
        if self.pending.is_empty() {
            self.item_id = None;
            return Ok(());
        }
        let valid = self.pending.len().min(self.chunk_frames);
        self.input.fill(0.0);
        for sample in self.input.iter_mut().take(valid) {
            *sample = self.pending.pop_front().unwrap_or_default();
        }
        self.process_chunk(playback, active, Some(valid))?;
        self.item_id = None;
        Ok(())
    }

    fn process_full_chunks(
        &mut self,
        playback: &Arc<Mutex<PlaybackState>>,
        active: &Arc<AtomicBool>,
    ) -> Result<()> {
        while self.pending.len() >= self.chunk_frames {
            for sample in &mut self.input {
                *sample = self.pending.pop_front().unwrap_or_default();
            }
            self.process_chunk(playback, active, None)?;
        }
        Ok(())
    }

    fn process_chunk(
        &mut self,
        playback: &Arc<Mutex<PlaybackState>>,
        active: &Arc<AtomicBool>,
        partial_len: Option<usize>,
    ) -> Result<()> {
        let input = InterleavedSlice::new(&self.input, 1, self.chunk_frames)
            .expect("valid playback input adapter");
        let output_capacity = self.output.len();
        let mut output = InterleavedSlice::new_mut(&mut self.output, 1, output_capacity)
            .expect("valid playback output adapter");
        let indexing = partial_len.map(|length| rubato::Indexing::new().partial_len(length));
        let (_, frames) = self
            .resampler
            .process_into_buffer(&input, &mut output, indexing.as_ref())
            .context("resample assistant audio")?;
        let item_id = self
            .item_id
            .clone()
            .context("playback item id is missing")?;
        playback.lock().queue.push_back(PlaybackChunk {
            item_id,
            samples: self.output[..frames].to_vec(),
            cursor: 0,
        });
        active.store(true, Ordering::Release);
        Ok(())
    }
}

pub(crate) struct AudioSystem {
    input_stream: Stream,
    _output_stream: Stream,
    input: Receiver<AudioInputEvent>,
    playback: Arc<Mutex<PlaybackState>>,
    playback_active: Arc<AtomicBool>,
    output_resampler: Mutex<OutputResampler>,
    output_rate: u32,
    input_enabled: bool,
}

impl std::fmt::Debug for AudioSystem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AudioSystem")
            .field("playback_active", &self.playback_active())
            .field("output_rate", &self.output_rate)
            .field("input_enabled", &self.input_enabled)
            .finish_non_exhaustive()
    }
}

impl AudioSystem {
    pub(crate) fn start() -> Result<Self> {
        let host = cpal::default_host();
        let input_device = host
            .default_input_device()
            .context("no default microphone")?;
        let output_device = host
            .default_output_device()
            .context("no default audio output")?;
        let input_supported = input_device
            .default_input_config()
            .context("read default microphone configuration")?;
        let output_supported = output_device
            .default_output_config()
            .context("read default output configuration")?;
        let input_format = input_supported.sample_format();
        let output_format = output_supported.sample_format();
        let input_config: StreamConfig = input_supported.into();
        let output_config: StreamConfig = output_supported.into();
        let (sender, input) = std::sync::mpsc::sync_channel(AUDIO_CHANNEL_CAPACITY);
        let processor =
            InputProcessor::new(input_config.sample_rate, usize::from(input_config.channels))?;
        let input_stream = build_input_stream(
            &input_device,
            &input_config,
            input_format,
            processor,
            sender,
        )?;

        let playback = Arc::new(Mutex::new(PlaybackState::default()));
        let playback_active = Arc::new(AtomicBool::new(false));
        let output_stream = build_output_stream(
            &output_device,
            &output_config,
            output_format,
            Arc::clone(&playback),
            Arc::clone(&playback_active),
        )?;
        input_stream.play().context("start microphone stream")?;
        output_stream.play().context("start audio output stream")?;
        let output_rate = output_config.sample_rate;
        Ok(Self {
            input_stream,
            _output_stream: output_stream,
            input,
            playback,
            playback_active,
            output_resampler: Mutex::new(OutputResampler::new(output_rate)?),
            output_rate,
            input_enabled: true,
        })
    }

    pub(crate) fn try_input(&self) -> Option<AudioInputEvent> {
        self.input.try_recv().ok()
    }

    pub(crate) fn set_mic_enabled(&mut self, enabled: bool) -> Result<()> {
        if self.input_enabled == enabled {
            return Ok(());
        }
        if enabled {
            self.input_stream
                .play()
                .context("resume microphone stream")?;
        } else {
            self.input_stream
                .pause()
                .context("pause microphone stream")?;
        }
        self.input_enabled = enabled;
        Ok(())
    }

    pub(crate) fn push_output(&self, item_id: &str, samples: &[i16]) -> Result<()> {
        self.output_resampler
            .lock()
            .push(item_id, samples, &self.playback, &self.playback_active)
    }

    pub(crate) fn finish_output(&self) -> Result<()> {
        self.output_resampler
            .lock()
            .finish(&self.playback, &self.playback_active)
    }

    pub(crate) fn playback_active(&self) -> bool {
        self.playback_active.load(Ordering::Acquire)
    }
    /// `(played_ms, total_ms)` for the item currently playing. `total` grows as
    /// more audio streams in, so callers must treat the fraction as monotonic
    /// only after clamping.
    pub(crate) fn playback_progress(&self) -> Option<(u64, u64)> {
        if !self.playback_active() {
            return None;
        }
        let playback = self.playback.lock();
        let current = playback
            .current_item_id
            .clone()
            .or_else(|| playback.queue.front().map(|chunk| chunk.item_id.clone()))?;
        let queued: u64 = playback
            .queue
            .iter()
            .filter(|chunk| chunk.item_id == current)
            .map(|chunk| u64::try_from(chunk.samples.len() - chunk.cursor).unwrap_or_default())
            .sum();
        let rate = u64::from(self.output_rate);
        let played_ms = playback.played_device_frames.saturating_mul(1_000) / rate;
        let total_ms = playback
            .played_device_frames
            .saturating_add(queued)
            .saturating_mul(1_000)
            / rate;
        Some((played_ms, total_ms))
    }

    pub(crate) fn stop_and_clear(&self) -> (Option<String>, u64) {
        self.output_resampler.lock().pending.clear();
        let mut playback = self.playback.lock();
        playback.queue.clear();
        let item_id = playback.current_item_id.take();
        let played_ms =
            playback.played_device_frames.saturating_mul(1_000) / u64::from(self.output_rate);
        playback.played_device_frames = 0;
        self.playback_active.store(false, Ordering::Release);
        (item_id, played_ms)
    }
}

fn build_input_stream(
    device: &Device,
    config: &StreamConfig,
    format: SampleFormat,
    processor: InputProcessor,
    sender: SyncSender<AudioInputEvent>,
) -> Result<Stream> {
    match format {
        SampleFormat::F32 => input_stream::<f32>(device, config, processor, sender, |value| value),
        SampleFormat::I16 => input_stream::<i16>(device, config, processor, sender, |value| {
            f32::from(value) / f32::from(i16::MAX)
        }),
        SampleFormat::U16 => input_stream::<u16>(device, config, processor, sender, |value| {
            (f32::from(value) - 32_768.0) / 32_768.0
        }),
        unsupported => bail!("unsupported microphone sample format {unsupported}"),
    }
}

fn input_stream<T: SizedSample + Copy + Send + 'static>(
    device: &Device,
    config: &StreamConfig,
    mut processor: InputProcessor,
    sender: SyncSender<AudioInputEvent>,
    convert: fn(T) -> f32,
) -> Result<Stream> {
    let error_sender = sender.clone();
    device
        .build_input_stream(
            *config,
            move |data: &[T], _| processor.process(data, convert, &sender),
            move |error| {
                let _ = error_sender.try_send(AudioInputEvent::Error(format!(
                    "microphone stream: {error}"
                )));
            },
            None,
        )
        .context("build microphone stream")
}

fn build_output_stream(
    device: &Device,
    config: &StreamConfig,
    format: SampleFormat,
    playback: Arc<Mutex<PlaybackState>>,
    active: Arc<AtomicBool>,
) -> Result<Stream> {
    match format {
        SampleFormat::F32 => {
            output_stream::<f32>(device, config, playback, active, std::convert::identity)
        }
        SampleFormat::I16 => {
            output_stream::<i16>(device, config, playback, active, normalized_to_i16)
        }
        SampleFormat::U16 => {
            output_stream::<u16>(device, config, playback, active, normalized_to_u16)
        }
        unsupported => bail!("unsupported output sample format {unsupported}"),
    }
}

fn output_stream<T: SizedSample + Copy + Send + 'static>(
    device: &Device,
    config: &StreamConfig,
    playback: Arc<Mutex<PlaybackState>>,
    active: Arc<AtomicBool>,
    convert: fn(f32) -> T,
) -> Result<Stream> {
    let channels = usize::from(config.channels);
    let error_active = Arc::clone(&active);
    device
        .build_output_stream(
            *config,
            move |data: &mut [T], _| {
                let mut playback = playback.lock();
                for frame in data.chunks_mut(channels) {
                    while playback
                        .queue
                        .front()
                        .is_some_and(|chunk| chunk.cursor >= chunk.samples.len())
                    {
                        playback.queue.pop_front();
                    }
                    let next_item_id = playback.queue.front().and_then(|chunk| {
                        (playback.current_item_id.as_deref() != Some(chunk.item_id.as_str()))
                            .then(|| chunk.item_id.clone())
                    });
                    if let Some(item_id) = next_item_id {
                        playback.current_item_id = Some(item_id);
                        playback.played_device_frames = 0;
                    }
                    let next = playback.queue.front_mut().map(|chunk| {
                        let sample = chunk.samples[chunk.cursor];
                        chunk.cursor += 1;
                        sample
                    });
                    let sample = if let Some(sample) = next {
                        playback.played_device_frames =
                            playback.played_device_frames.saturating_add(1);
                        sample
                    } else {
                        active.store(false, Ordering::Release);
                        0.0
                    };
                    frame.fill(convert(sample));
                }
            },
            move |_error| {
                error_active.store(false, Ordering::Release);
            },
            None,
        )
        .context("build audio output stream")
}
