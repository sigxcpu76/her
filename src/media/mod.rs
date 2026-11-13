pub mod audio;
pub mod codecs;
pub mod rtp;

use crate::config::{AppConfig, CodecKind};
use crate::media::audio::AudioHandle;
use crate::media::codecs::{AudioCodec, StatefulResampler};
use crate::media::rtp::{
    build_dtmf_event_payload, dtmf_char_to_code, JitterBuffer, RtpPacket, DTMF_PAYLOAD_TYPE,
};
use crate::nat::stun::{discover_local_ip, discover_mapped_addr};
use rand::Rng;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};

#[derive(Debug)]
pub enum MediaCommand {
    SetMuted(bool),
    SetHold(bool),
    SendDtmf(char),
    Stop,
}

pub struct PreparedMedia {
    pub socket: Arc<UdpSocket>,
    pub advertised_addr: SocketAddr,
}

/// Bind an RTP UDP socket and discover its NAT-mapped address via STUN if enabled.
pub async fn prepare_media_socket(cfg: &AppConfig) -> Result<PreparedMedia, String> {
    let sock = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| format!("Failed to bind RTP socket: {e}"))?;
    let local_addr = sock
        .local_addr()
        .map_err(|e| format!("Failed to read RTP local_addr: {e}"))?;

    let mut advertised_addr = SocketAddr::new(discover_local_ip(), local_addr.port());

    if cfg.nat.stun_enabled {
        match discover_mapped_addr(&sock, &cfg.nat.stun_server, &cfg.nat.stun_fallback).await {
            Ok(res) => {
                tracing::info!(
                    "RTP STUN mapped {} -> {} via {}",
                    local_addr,
                    res.mapped_addr,
                    res.server_used
                );
                advertised_addr = res.mapped_addr;
            }
            Err(e) => {
                tracing::warn!("RTP STUN discovery failed, using local IP: {e}");
            }
        }
    }

    Ok(PreparedMedia {
        socket: Arc::new(sock),
        advertised_addr,
    })
}

pub struct MediaSession {
    cmd_tx: mpsc::UnboundedSender<MediaCommand>,
    muted: Arc<AtomicBool>,
    on_hold: Arc<AtomicBool>,
}

impl MediaSession {
    pub fn start(
        prepared: PreparedMedia,
        remote_addr: SocketAddr,
        codec_kind: CodecKind,
        symmetric_rtp: bool,
        cfg: AppConfig,
    ) -> Result<Self, String> {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<MediaCommand>();
        let (mic_tx, mut mic_rx) = mpsc::unbounded_channel::<Vec<i16>>();

        let audio = AudioHandle::start(
            cfg.audio.input_device.as_deref(),
            cfg.audio.output_device.as_deref(),
            mic_tx,
        )?;

        let muted = Arc::new(AtomicBool::new(false));
        let on_hold = Arc::new(AtomicBool::new(false));
        let muted_flag = muted.clone();
        let hold_flag = on_hold.clone();

        tokio::spawn(async move {
            let sock = prepared.socket;
            let mut remote = remote_addr;
            let mut enc_codec = AudioCodec::new(codec_kind);
            let mut dec_codec = AudioCodec::new(codec_kind);
            let mut tx_resampler = StatefulResampler::new(audio.input_rate, enc_codec.kind().sample_rate());
            let mut rx_resampler = StatefulResampler::new(dec_codec.kind().sample_rate(), audio.output_rate);
            let mut jitter = JitterBuffer::new(3, 20);

            let ssrc: u32 = rand::thread_rng().gen();
            let mut seq: u16 = rand::thread_rng().gen();
            let mut ts: u32 = rand::thread_rng().gen();
            let mut mic_buf: Vec<i16> = Vec::with_capacity(1024);
            let mut recv_buf = [0u8; 2048];
            let mut playout_tick = interval(Duration::from_millis(20));
            let mut latched = false;

            loop {
                tokio::select! {
                    Some(cmd) = cmd_rx.recv() => {
                        match cmd {
                            MediaCommand::SetMuted(m) => {
                                muted_flag.store(m, Ordering::Relaxed);
                            }
                            MediaCommand::SetHold(h) => {
                                hold_flag.store(h, Ordering::Relaxed);
                            }
                            MediaCommand::SendDtmf(ch) => {
                                audio.play_dtmf_sidetone(ch);
                                if let Some(code) = dtmf_char_to_code(ch) {
                                    let event_ts = ts;
                                    // Send 3 active packets + 3 end packets per RFC 4733
                                    for i in 0..3 {
                                        let pkt = RtpPacket {
                                            marker: i == 0,
                                            payload_type: DTMF_PAYLOAD_TYPE,
                                            sequence_number: seq,
                                            timestamp: event_ts,
                                            ssrc,
                                            payload: build_dtmf_event_payload(code, false, 10, (i + 1) * 160),
                                        };
                                        seq = seq.wrapping_add(1);
                                        let _ = sock.send_to(&pkt.to_bytes(), remote).await;
                                    }
                                    for _ in 0..3 {
                                        let pkt = RtpPacket {
                                            marker: false,
                                            payload_type: DTMF_PAYLOAD_TYPE,
                                            sequence_number: seq,
                                            timestamp: event_ts,
                                            ssrc,
                                            payload: build_dtmf_event_payload(code, true, 10, 960),
                                        };
                                        seq = seq.wrapping_add(1);
                                        let _ = sock.send_to(&pkt.to_bytes(), remote).await;
                                    }
                                    ts = ts.wrapping_add(160);
                                }
                            }
                            MediaCommand::Stop => {
                                break;
                            }
                        }
                    }

                    res = sock.recv_from(&mut recv_buf) => {
                        if let Ok((len, src)) = res {
                            if symmetric_rtp && !latched && src != remote && !is_unspecified(&src.ip()) {
                                tracing::info!("Symmetric RTP latched peer {} -> {}", remote, src);
                                remote = src;
                                latched = true;
                            }
                            if let Some(pkt) = RtpPacket::parse(&recv_buf[..len]) {
                                if pkt.payload_type != DTMF_PAYLOAD_TYPE {
                                    jitter.push(pkt);
                                }
                            }
                        }
                    }

                    _ = playout_tick.tick() => {
                        if !hold_flag.load(Ordering::Relaxed) {
                            if let Some(pkt) = jitter.pop() {
                                if let Some(k) = payload_type_to_codec(pkt.payload_type) {
                                    if dec_codec.kind() != k {
                                        dec_codec = AudioCodec::new(k);
                                        rx_resampler = StatefulResampler::new(k.sample_rate(), audio.output_rate);
                                    }
                                    let pcm = dec_codec.decode(&pkt.payload);
                                    let out = rx_resampler.process(&pcm);
                                    audio.push_playout(&out);
                                }
                            }
                        }
                    }

                    Some(raw_mic) = mic_rx.recv() => {
                        let is_muted = muted_flag.load(Ordering::Relaxed);
                        let is_hold = hold_flag.load(Ordering::Relaxed);
                        if is_hold {
                            mic_buf.clear();
                            continue;
                        }
                        let codec_rate = enc_codec.kind().sample_rate();
                        let resampled = tx_resampler.process(&raw_mic);
                        if is_muted {
                            mic_buf.extend(std::iter::repeat(0i16).take(resampled.len()));
                        } else {
                            mic_buf.extend_from_slice(&resampled);
                        }

                        // 20ms frame size: 160 samples @ 8kHz, 320 samples @ 16kHz
                        let frame_samples = (codec_rate as usize * 20) / 1000;
                        while mic_buf.len() >= frame_samples {
                            let frame: Vec<i16> = mic_buf.drain(..frame_samples).collect();
                            let payload = enc_codec.encode(&frame);
                            let pkt = RtpPacket {
                                marker: false,
                                payload_type: enc_codec.kind().payload_type(),
                                sequence_number: seq,
                                timestamp: ts,
                                ssrc,
                                payload,
                            };
                            seq = seq.wrapping_add(1);
                            // RFC 3551: G.722 RTP timestamp advances by 160 per 20ms (8kHz clock)
                            ts = ts.wrapping_add((enc_codec.kind().rtp_clock_rate() * 20) / 1000);
                            let _ = sock.send_to(&pkt.to_bytes(), remote).await;
                        }
                    }
                }
            }
        });

        Ok(Self {
            cmd_tx,
            muted,
            on_hold,
        })
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
        let _ = self.cmd_tx.send(MediaCommand::SetMuted(muted));
    }

    pub fn set_hold(&self, hold: bool) {
        self.on_hold.store(hold, Ordering::Relaxed);
        let _ = self.cmd_tx.send(MediaCommand::SetHold(hold));
    }

    pub fn send_dtmf(&self, ch: char) {
        let _ = self.cmd_tx.send(MediaCommand::SendDtmf(ch));
    }

    pub fn stop(&self) {
        let _ = self.cmd_tx.send(MediaCommand::Stop);
    }
}

fn payload_type_to_codec(pt: u8) -> Option<CodecKind> {
    match pt {
        0 => Some(CodecKind::Pcmu),
        8 => Some(CodecKind::Pcma),
        9 => Some(CodecKind::G722),
        18 => Some(CodecKind::G729),
        _ => None,
    }
}

fn is_unspecified(ip: &IpAddr) -> bool {
    ip.is_unspecified()
}
