use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub fn list_audio_devices() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    let inputs = host
        .input_devices()
        .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default();
    let outputs = host
        .output_devices()
        .map(|devs| devs.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default();
    (inputs, outputs)
}

#[derive(Debug, Clone, Copy)]
pub struct ToneState {
    f1: f32,
    f2: f32,
    phase1: f32,
    phase2: f32,
    remaining_samples: usize,
}

impl ToneState {
    pub fn from_dtmf(ch: char, sample_rate: u32) -> Option<Self> {
        let (f1, f2) = match ch.to_ascii_uppercase() {
            '1' => (697.0, 1209.0),
            '2' => (697.0, 1336.0),
            '3' => (697.0, 1477.0),
            'A' => (697.0, 1633.0),
            '4' => (770.0, 1209.0),
            '5' => (770.0, 1336.0),
            '6' => (770.0, 1477.0),
            'B' => (770.0, 1633.0),
            '7' => (852.0, 1209.0),
            '8' => (852.0, 1336.0),
            '9' => (852.0, 1477.0),
            'C' => (852.0, 1633.0),
            '*' => (941.0, 1209.0),
            '0' => (941.0, 1336.0),
            '#' => (941.0, 1477.0),
            'D' => (941.0, 1633.0),
            _ => return None,
        };
        Some(Self {
            f1,
            f2,
            phase1: 0.0,
            phase2: 0.0,
            remaining_samples: (sample_rate as usize * 120) / 1000,
        })
    }

    fn next_sample(&mut self, sample_rate: u32) -> f32 {
        if self.remaining_samples == 0 {
            return 0.0;
        }
        self.remaining_samples -= 1;
        let step1 = 2.0 * std::f32::consts::PI * self.f1 / sample_rate as f32;
        let step2 = 2.0 * std::f32::consts::PI * self.f2 / sample_rate as f32;
        let s = (self.phase1.sin() + self.phase2.sin()) * 0.12;
        self.phase1 = (self.phase1 + step1) % (2.0 * std::f32::consts::PI);
        self.phase2 = (self.phase2 + step2) % (2.0 * std::f32::consts::PI);
        s
    }
}

pub struct AudioHandle {
    _stop_tx: std::sync::mpsc::Sender<()>,
    pub input_rate: u32,
    pub output_rate: u32,
    playout_queue: Arc<Mutex<VecDeque<i16>>>,
    sidetone: Arc<Mutex<Option<ToneState>>>,
}

impl AudioHandle {
    pub fn start(
        input_name: Option<&str>,
        output_name: Option<&str>,
        mic_tx: mpsc::UnboundedSender<Vec<i16>>,
    ) -> Result<Self, String> {
        let input_name = input_name.map(|s| s.to_string());
        let output_name = output_name.map(|s| s.to_string());
        let (init_tx, init_rx) =
            std::sync::mpsc::channel::<Result<(u32, u32, Arc<Mutex<VecDeque<i16>>>, Arc<Mutex<Option<ToneState>>>), String>>();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

        std::thread::spawn(move || {
            let res = Self::start_inner(
                input_name.as_deref(),
                output_name.as_deref(),
                mic_tx,
            );
            match res {
                Ok((in_stream, out_stream, input_rate, output_rate, playout_queue, sidetone)) => {
                    let _ = init_tx.send(Ok((input_rate, output_rate, playout_queue, sidetone)));
                    // Keep streams alive until AudioHandle is dropped
                    let _ = stop_rx.recv();
                    drop(in_stream);
                    drop(out_stream);
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                }
            }
        });

        let (input_rate, output_rate, playout_queue, sidetone) = init_rx
            .recv()
            .map_err(|_| "Audio thread exited unexpectedly".to_string())??;

        Ok(Self {
            _stop_tx: stop_tx,
            input_rate,
            output_rate,
            playout_queue,
            sidetone,
        })
    }

    fn open_best_input(
        host: &cpal::Host,
        preferred_name: Option<&str>,
    ) -> Option<(cpal::Device, cpal::SupportedStreamConfig)> {
        let mut candidates: Vec<cpal::Device> = Vec::new();
        if let Ok(devs) = host.input_devices() {
            let all: Vec<cpal::Device> = devs.collect();
            if let Some(name) = preferred_name {
                if let Some(d) = all.iter().find(|d| d.name().ok().as_deref() == Some(name)) {
                    candidates.push(d.clone());
                }
            }
            for wanted in ["pipewire", "pulse", "default"] {
                if let Some(d) = all.iter().find(|d| d.name().ok().as_deref() == Some(wanted)) {
                    candidates.push(d.clone());
                }
            }
            if let Some(d) = host.default_input_device() {
                candidates.push(d);
            }
            candidates.extend(all);
        } else if let Some(d) = host.default_input_device() {
            candidates.push(d);
        }

        for dev in candidates {
            if let Ok(cfg) = dev.default_input_config() {
                tracing::info!("Selected audio input: {:?}", dev.name().ok());
                return Some((dev, cfg));
            }
        }
        None
    }

    fn open_best_output(
        host: &cpal::Host,
        preferred_name: Option<&str>,
    ) -> Result<(cpal::Device, cpal::SupportedStreamConfig), String> {
        let mut candidates: Vec<cpal::Device> = Vec::new();
        if let Ok(devs) = host.output_devices() {
            let all: Vec<cpal::Device> = devs.collect();
            if let Some(name) = preferred_name {
                if let Some(d) = all.iter().find(|d| d.name().ok().as_deref() == Some(name)) {
                    candidates.push(d.clone());
                }
            }
            for wanted in ["pipewire", "pulse", "default"] {
                if let Some(d) = all.iter().find(|d| d.name().ok().as_deref() == Some(wanted)) {
                    candidates.push(d.clone());
                }
            }
            if let Some(d) = host.default_output_device() {
                candidates.push(d);
            }
            candidates.extend(all);
        } else if let Some(d) = host.default_output_device() {
            candidates.push(d);
        }

        for dev in candidates {
            if let Ok(cfg) = dev.default_output_config() {
                tracing::info!("Selected audio output: {:?}", dev.name().ok());
                return Ok((dev, cfg));
            }
        }
        Err("No working audio output device available".to_string())
    }

    fn start_inner(
        input_name: Option<&str>,
        output_name: Option<&str>,
        mic_tx: mpsc::UnboundedSender<Vec<i16>>,
    ) -> Result<(Option<Stream>, Stream, u32, u32, Arc<Mutex<VecDeque<i16>>>, Arc<Mutex<Option<ToneState>>>), String> {
        let host = cpal::default_host();

        let (out_device, out_cfg) = Self::open_best_output(&host, output_name)?;
        let maybe_in = Self::open_best_input(&host, input_name);

        let input_rate = maybe_in
            .as_ref()
            .map(|(_, c)| c.sample_rate().0)
            .unwrap_or(16000);

        let output_rate = out_cfg.sample_rate().0;
        let out_channels = out_cfg.channels() as usize;
        let out_format = out_cfg.sample_format();
        let out_stream_cfg: StreamConfig = out_cfg.into();

        let playout_queue = Arc::new(Mutex::new(VecDeque::<i16>::with_capacity(
            output_rate as usize / 2,
        )));
        let sidetone = Arc::new(Mutex::new(None::<ToneState>));

        let err_fn = |err| tracing::warn!("Audio stream error: {err}");

        let in_stream = if let Some((in_device, in_cfg)) = maybe_in {
            let in_channels = in_cfg.channels() as usize;
            let in_format = in_cfg.sample_format();
            let in_stream_cfg: StreamConfig = in_cfg.into();

            let built = match in_format {
                SampleFormat::F32 => in_device.build_input_stream(
                    &in_stream_cfg,
                    move |data: &[f32], _| {
                        let mut mono = Vec::with_capacity(data.len() / in_channels.max(1));
                        for frame in data.chunks(in_channels.max(1)) {
                            let sum: f32 = frame.iter().copied().sum();
                            let avg = sum / in_channels.max(1) as f32;
                            mono.push((avg.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
                        }
                        let _ = mic_tx.send(mono);
                    },
                    err_fn,
                    None,
                ),
                SampleFormat::I16 => in_device.build_input_stream(
                    &in_stream_cfg,
                    move |data: &[i16], _| {
                        let mut mono = Vec::with_capacity(data.len() / in_channels.max(1));
                        for frame in data.chunks(in_channels.max(1)) {
                            let sum: i32 = frame.iter().map(|&s| s as i32).sum();
                            mono.push((sum / in_channels.max(1) as i32) as i16);
                        }
                        let _ = mic_tx.send(mono);
                    },
                    err_fn,
                    None,
                ),
                SampleFormat::U16 => in_device.build_input_stream(
                    &in_stream_cfg,
                    move |data: &[u16], _| {
                        let mut mono = Vec::with_capacity(data.len() / in_channels.max(1));
                        for frame in data.chunks(in_channels.max(1)) {
                            let sum: i32 = frame.iter().map(|&s| s as i32 - 32768).sum();
                            mono.push((sum / in_channels.max(1) as i32) as i16);
                        }
                        let _ = mic_tx.send(mono);
                    },
                    err_fn,
                    None,
                ),
                _ => {
                    tracing::warn!("Unsupported audio input sample format; continuing receive-only");
                    Err(cpal::BuildStreamError::StreamConfigNotSupported)
                }
            };
            match built {
                Ok(s) => {
                    if let Err(e) = s.play() {
                        tracing::warn!("Input play error: {e}");
                        None
                    } else {
                        Some(s)
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to open input stream (continuing receive-only): {e}");
                    None
                }
            }
        } else {
            tracing::warn!("No working audio input device; continuing receive-only");
            None
        };

        let pq_out = playout_queue.clone();
        let tone_out = sidetone.clone();

        let out_stream = match out_format {
            SampleFormat::F32 => {
                let mut last_rx: f32 = 0.0;
                out_device.build_output_stream(
                    &out_stream_cfg,
                    move |data: &mut [f32], _| {
                        let mut q = pq_out.lock().unwrap();
                        let mut t = tone_out.lock().unwrap();
                        for frame in data.chunks_mut(out_channels.max(1)) {
                            let rx = if let Some(s) = q.pop_front() {
                                last_rx = s as f32 / i16::MAX as f32;
                                last_rx
                            } else {
                                last_rx *= 0.985;
                                last_rx
                            };
                            let dt = if let Some(ref mut ts) = *t {
                                let v = ts.next_sample(output_rate);
                                if ts.remaining_samples == 0 {
                                    *t = None;
                                }
                                v
                            } else {
                                0.0
                            };
                            let sample = (rx + dt).clamp(-1.0, 1.0);
                            for ch in frame.iter_mut() {
                                *ch = sample;
                            }
                        }
                    },
                    err_fn,
                    None,
                )
            }
            SampleFormat::I16 => {
                let mut last_rx: i32 = 0;
                out_device.build_output_stream(
                    &out_stream_cfg,
                    move |data: &mut [i16], _| {
                        let mut q = pq_out.lock().unwrap();
                        let mut t = tone_out.lock().unwrap();
                        for frame in data.chunks_mut(out_channels.max(1)) {
                            let rx = if let Some(s) = q.pop_front() {
                                last_rx = s as i32;
                                last_rx
                            } else {
                                last_rx = (last_rx * 985) / 1000;
                                last_rx
                            };
                            let dt = if let Some(ref mut ts) = *t {
                                let v = (ts.next_sample(output_rate) * i16::MAX as f32) as i32;
                                if ts.remaining_samples == 0 {
                                    *t = None;
                                }
                                v
                            } else {
                                0
                            };
                            let sample = (rx + dt).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                            for ch in frame.iter_mut() {
                                *ch = sample;
                            }
                        }
                    },
                    err_fn,
                    None,
                )
            }
            SampleFormat::U16 => {
                let mut last_rx: i32 = 0;
                out_device.build_output_stream(
                    &out_stream_cfg,
                    move |data: &mut [u16], _| {
                        let mut q = pq_out.lock().unwrap();
                        let mut t = tone_out.lock().unwrap();
                        for frame in data.chunks_mut(out_channels.max(1)) {
                            let rx = if let Some(s) = q.pop_front() {
                                last_rx = s as i32;
                                last_rx
                            } else {
                                last_rx = (last_rx * 985) / 1000;
                                last_rx
                            };
                            let dt = if let Some(ref mut ts) = *t {
                                let v = (ts.next_sample(output_rate) * i16::MAX as f32) as i32;
                                if ts.remaining_samples == 0 {
                                    *t = None;
                                }
                                v
                            } else {
                                0
                            };
                            let sample =
                                ((rx + dt).clamp(i16::MIN as i32, i16::MAX as i32) + 32768) as u16;
                            for ch in frame.iter_mut() {
                                *ch = sample;
                            }
                        }
                    },
                    err_fn,
                    None,
                )
            }
            _ => return Err("Unsupported audio output sample format".into()),
        }
        .map_err(|e| format!("Failed to open output stream: {e}"))?;

        out_stream
            .play()
            .map_err(|e| format!("Output play error: {e}"))?;

        Ok((
            in_stream,
            out_stream,
            input_rate,
            output_rate,
            playout_queue,
            sidetone,
        ))
    }

    pub fn push_playout(&self, samples: &[i16]) {
        let mut q = self.playout_queue.lock().unwrap();
        // Cap buffer at ~300ms to prevent runaway latency
        let max_len = (self.output_rate as usize * 3) / 10;
        while q.len() > max_len {
            q.pop_front();
        }
        q.extend(samples);
    }

    pub fn play_dtmf_sidetone(&self, ch: char) {
        if let Some(ts) = ToneState::from_dtmf(ch, self.output_rate) {
            *self.sidetone.lock().unwrap() = Some(ts);
        }
    }
}
