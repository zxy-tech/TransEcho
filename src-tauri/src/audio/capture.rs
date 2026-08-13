use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Raw audio data captured from the default microphone input.
pub struct AudioFrame {
    /// PCM samples as f32, interleaved when the microphone has multiple channels.
    pub samples: Vec<f32>,
    /// Sample rate reported by the input device.
    pub sample_rate: u32,
    /// Number of input channels.
    pub channels: u16,
}

/// Handle to control the microphone stream.
pub struct CaptureHandle {
    // A CPAL stream stops when dropped. Option permits an explicit stop operation.
    stream: Option<cpal::Stream>,
}

// CPAL streams are safe to keep in the session task on the supported desktop
// backends. This matches the command layer's requirement to hold the handle
// across await points.
unsafe impl Send for CaptureHandle {}

impl CaptureHandle {
    pub fn stop(mut self) -> Result<(), Box<dyn std::error::Error>> {
        drop(self.stream.take());
        info!("Microphone capture stopped");
        Ok(())
    }
}

/// Start capturing the default microphone input.
pub async fn start_capture(
    buffer_size: usize,
) -> Result<(mpsc::Receiver<AudioFrame>, CaptureHandle), Box<dyn std::error::Error>> {
    let (tx, rx) = mpsc::channel(buffer_size);
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or("No microphone input device found")?;

    let device_name = device.name().unwrap_or_else(|_| "unknown".to_string());
    let config = device.default_input_config()?;
    let sample_rate = config.sample_rate().0;
    let channels = config.channels();
    let sample_format = config.sample_format();

    info!(
        "Using microphone input: {} ({}Hz, {}ch, {:?})",
        device_name, sample_rate, channels, sample_format
    );

    let stream_config: cpal::StreamConfig = config.into();
    let frame_count = Arc::new(AtomicUsize::new(0));
    let err_fn = |err: cpal::StreamError| warn!("Microphone stream error: {}", err);

    let stream = match sample_format {
        SampleFormat::F32 => build_input_stream::<f32, _>(
            &device,
            &stream_config,
            tx,
            frame_count,
            sample_rate,
            channels,
            |sample| sample,
            err_fn,
        )?,
        SampleFormat::I16 => build_input_stream::<i16, _>(
            &device,
            &stream_config,
            tx,
            frame_count,
            sample_rate,
            channels,
            |sample| sample as f32 / i16::MAX as f32,
            err_fn,
        )?,
        SampleFormat::U16 => build_input_stream::<u16, _>(
            &device,
            &stream_config,
            tx,
            frame_count,
            sample_rate,
            channels,
            |sample| sample as f32 / u16::MAX as f32 * 2.0 - 1.0,
            err_fn,
        )?,
        format => return Err(format!("Unsupported microphone sample format: {:?}", format).into()),
    };

    stream.play()?;
    info!("Microphone capture started ({}Hz, {}ch)", sample_rate, channels);

    Ok((rx, CaptureHandle { stream: Some(stream) }))
}

#[allow(clippy::too_many_arguments)]
fn build_input_stream<T, C>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    tx: mpsc::Sender<AudioFrame>,
    frame_count: Arc<AtomicUsize>,
    sample_rate: u32,
    channels: u16,
    convert: C,
    err_fn: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + Copy,
    C: Fn(T) -> f32 + Send + 'static,
{
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let count = frame_count.fetch_add(1, Ordering::Relaxed);
            if data.is_empty() {
                return;
            }

            let samples: Vec<f32> = data.iter().copied().map(&convert).collect();
            if count % 500 == 0 {
                let rms = (samples.iter().map(|sample| sample * sample).sum::<f32>()
                    / samples.len() as f32)
                    .sqrt();
                info!(
                    "Microphone frames captured: {}, samples: {}, rms: {:.6}",
                    count,
                    samples.len(),
                    rms
                );
            }

            let frame = AudioFrame {
                samples,
                sample_rate,
                channels,
            };

            if tx.try_send(frame).is_err() && count % 100 == 0 {
                debug!("Audio channel full, microphone frame dropped (count: {})", count);
            }
        },
        err_fn,
        None,
    )
}
