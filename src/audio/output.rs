use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, SampleRate, StreamConfig};
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Split};

use crate::audio::ring::{SampleConsumer, SampleProducer};

/// Number of interleaved samples in the output ring buffer.
///
/// 8192 samples = 4096 frames stereo ≈ 85 ms at 48 kHz. Small enough that
/// post-seek lag is unnoticeable; large enough to absorb GUI-thread hiccups.
const RING_BUFFER_SAMPLES: usize = 8192;

/// A live output stream + the producer half of its ring buffer.
///
/// Drop this to stop and tear down the stream.
pub struct ActiveOutput {
    /// Kept alive: dropping the stream closes the audio device callback.
    _stream: cpal::Stream,
    pub producer: SampleProducer,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Open the default output device.
///
/// First tries to build a stream at the track's exact rate/channels/F32; on
/// failure falls back to the device's default config and warns about any
/// rate or channel mismatch.
///
/// We deliberately do **not** call `supported_output_configs()`. On Linux
/// with pipewire-alsa, that probe errors out ("device no longer available")
/// even when the device is perfectly happy to build a stream — see the
/// "Open questions" section of `ARCHITECTURE.md` for the full story.
pub fn open(track_sample_rate: u32, track_channels: u16) -> Result<ActiveOutput> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no default output device"))?;
    let device_name = device.name().unwrap_or_else(|_| "<unknown>".into());

    let desired = StreamConfig {
        channels: track_channels,
        sample_rate: SampleRate(track_sample_rate),
        buffer_size: BufferSize::Default,
    };

    match build_f32_stream(&device, &desired) {
        Ok((stream, producer)) => {
            log::info!(
                "opened output on '{device_name}' at {track_sample_rate} Hz, {track_channels} ch (matched track)",
            );
            return Ok(ActiveOutput {
                _stream: stream,
                producer,
                sample_rate: track_sample_rate,
                channels: track_channels,
            });
        }
        Err(e) => {
            log::warn!(
                "couldn't open '{device_name}' at {track_sample_rate} Hz / {track_channels} ch f32: {e:#}; \
                 trying device default",
            );
        }
    }

    // Fallback: use whatever the device offers by default.
    let default = device
        .default_output_config()
        .context("default output config")?;
    if default.sample_format() != SampleFormat::F32 {
        return Err(anyhow!(
            "device default format is {:?} but only F32 is supported right now",
            default.sample_format()
        ));
    }
    let actual_sample_rate = default.sample_rate().0;
    let actual_channels = default.channels();
    let fallback_config = default.config();

    if actual_sample_rate != track_sample_rate {
        log::warn!(
            "track is {track_sample_rate} Hz but device default is {actual_sample_rate} Hz; \
             playback rate will be off until the resampler stage lands"
        );
    }
    if actual_channels != track_channels {
        log::warn!(
            "track has {track_channels} channels but device default has {actual_channels}; \
             channel layout mismatch is not yet handled — audio may sound wrong"
        );
    }

    let (stream, producer) = build_f32_stream(&device, &fallback_config)
        .context("building stream at device default config")?;

    log::info!(
        "opened output on '{device_name}' at {actual_sample_rate} Hz, {actual_channels} ch (device default)",
    );

    Ok(ActiveOutput {
        _stream: stream,
        producer,
        sample_rate: actual_sample_rate,
        channels: actual_channels,
    })
}

fn build_f32_stream(
    device: &cpal::Device,
    config: &StreamConfig,
) -> Result<(cpal::Stream, SampleProducer)> {
    let rb = HeapRb::<f32>::new(RING_BUFFER_SAMPLES);
    let (producer, mut consumer): (SampleProducer, SampleConsumer) = rb.split();

    let err_fn = |err| log::error!("cpal stream error: {err}");

    let stream = device
        .build_output_stream(
            config,
            move |out: &mut [f32], _info| {
                let n = consumer.pop_slice(out);
                if n < out.len() {
                    for s in &mut out[n..] {
                        *s = 0.0;
                    }
                }
            },
            err_fn,
            None,
        )
        .context("building output stream")?;

    stream.play().context("starting stream")?;
    Ok((stream, producer))
}
