use crate::config::CodecKind;
use oxideav_g711::{alaw, mulaw};
use oxideav_g722::{Decoder as G722Decoder, Encoder as G722Encoder, Mode as G722Mode};
use oxideav_g729::encode_chain::{FrameEncoder as G729FrameEncoder, L_FRAME as G729_L_FRAME};
use oxideav_g729::fx::decoder::FrameDecoderFx as G729FrameDecoderFx;
use oxideav_g729::fx::postfilter::PostfilterFx as G729PostfilterFx;
use oxideav_g729::parameters::{pack_bit_array, unpack_bit_array};
use oxideav_g729::tables::BITS_PER_FRAME as G729_BITS_PER_FRAME;

const G729_BYTES_PER_FRAME: usize = G729_BITS_PER_FRAME / 8;

pub struct AudioCodec {
    kind: CodecKind,
    g722_enc: Option<G722Encoder>,
    g722_dec: Option<G722Decoder>,
    g729_enc: Option<G729FrameEncoder>,
    g729_dec: Option<(G729FrameDecoderFx, G729PostfilterFx)>,
}

impl AudioCodec {
    pub fn new(kind: CodecKind) -> Self {
        match kind {
            CodecKind::G722 => Self {
                kind,
                g722_enc: Some(G722Encoder::new()),
                g722_dec: Some(G722Decoder::new(G722Mode::Mode1)),
                g729_enc: None,
                g729_dec: None,
            },
            CodecKind::G729 => Self {
                kind,
                g722_enc: None,
                g722_dec: None,
                g729_enc: Some(G729FrameEncoder::new()),
                g729_dec: Some((G729FrameDecoderFx::new(), G729PostfilterFx::new())),
            },
            CodecKind::Pcmu | CodecKind::Pcma => Self {
                kind,
                g722_enc: None,
                g722_dec: None,
                g729_enc: None,
                g729_dec: None,
            },
        }
    }

    pub fn kind(&self) -> CodecKind {
        self.kind
    }

    /// Encode PCM samples at `self.kind.sample_rate()` (8kHz for G.711/G.729, 16kHz for G.722)
    /// into RTP payload bytes.
    pub fn encode(&mut self, pcm: &[i16]) -> Vec<u8> {
        match self.kind {
            CodecKind::Pcmu => pcm.iter().map(|&s| mulaw::encode_sample(s)).collect(),
            CodecKind::Pcma => pcm.iter().map(|&s| alaw::encode_sample(s)).collect(),
            CodecKind::G722 => {
                let enc = self.g722_enc.as_mut().expect("g722 encoder");
                // ITU-T G.722 expects 14-bit signed PCM (-8192..8191)
                let scaled: Vec<i32> = pcm.iter().map(|&s| (s >> 2) as i32).collect();
                enc.encode(&scaled)
            }
            CodecKind::G729 => {
                let enc = self.g729_enc.as_mut().expect("g729 encoder");
                let mut out = Vec::with_capacity((pcm.len() / G729_L_FRAME) * G729_BYTES_PER_FRAME);
                for chunk in pcm.chunks_exact(G729_L_FRAME) {
                    let frame_f32: [f32; G729_L_FRAME] =
                        std::array::from_fn(|i| chunk[i] as f32);
                    let encoded = enc.encode_frame(&frame_f32);
                    let bits = pack_bit_array(&encoded.params);
                    let mut octets = [0u8; G729_BYTES_PER_FRAME];
                    for (i, &bit) in bits.iter().enumerate() {
                        if bit {
                            octets[i / 8] |= 1 << (7 - (i % 8));
                        }
                    }
                    out.extend_from_slice(&octets);
                }
                out
            }
        }
    }

    /// Decode RTP payload bytes into PCM samples at `self.kind.sample_rate()`.
    pub fn decode(&mut self, payload: &[u8]) -> Vec<i16> {
        match self.kind {
            CodecKind::Pcmu => payload.iter().map(|&b| mulaw::decode_sample(b)).collect(),
            CodecKind::Pcma => payload.iter().map(|&b| alaw::decode_sample(b)).collect(),
            CodecKind::G722 => {
                let dec = self.g722_dec.as_mut().expect("g722 decoder");
                dec.decode(payload)
                    .into_iter()
                    .map(|s| (s * 4).clamp(i16::MIN as i32, i16::MAX as i32) as i16)
                    .collect()
            }
            CodecKind::G729 => {
                let (chain, postfilter) = self.g729_dec.as_mut().expect("g729 decoder");
                let mut out = Vec::with_capacity(
                    (payload.len() / G729_BYTES_PER_FRAME) * G729_L_FRAME,
                );
                for chunk in payload.chunks_exact(G729_BYTES_PER_FRAME) {
                    let bits: [bool; G729_BITS_PER_FRAME] =
                        std::array::from_fn(|i| (chunk[i / 8] >> (7 - (i % 8))) & 1 == 1);
                    let params = unpack_bit_array(&bits);
                    let dec = chain.decode_frame(&params);
                    let int_t1 = usize::try_from(dec.sub[0].t_int.max(1)).unwrap_or(1);
                    let mut periodic = false;
                    for s in 0..2 {
                        let speech: [i16; 40] =
                            std::array::from_fn(|n| dec.speech[s * 40 + n]);
                        let (filtered, decision) =
                            postfilter.process_subframe(&speech, &dec.sub[s].a_q12, int_t1);
                        periodic |= decision.gain_q15 > 0;
                        out.extend_from_slice(&filtered);
                    }
                    chain.erasure_periodic = periodic;
                }
                out
            }
        }
    }
}

/// Stateful linear resampler with continuity across packet/chunk boundaries
/// and a gentle 1-pole low-pass filter to prevent aliasing/imaging crackles.
pub struct StatefulResampler {
    in_rate: u32,
    out_rate: u32,
    phase: f64,
    prev_sample: f64,
    lp_state: f64,
}

impl StatefulResampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            in_rate,
            out_rate,
            phase: 0.0,
            prev_sample: 0.0,
            lp_state: 0.0,
        }
    }

    pub fn process(&mut self, input: &[i16]) -> Vec<i16> {
        if input.is_empty() || self.in_rate == self.out_rate || self.in_rate == 0 || self.out_rate == 0 {
            if let Some(&last) = input.last() {
                self.prev_sample = last as f64;
            }
            return input.to_vec();
        }

        let step = self.in_rate as f64 / self.out_rate as f64;
        let est_out = ((input.len() as f64) / step).ceil() as usize + 2;
        let mut out = Vec::with_capacity(est_out);

        // Low-pass cutoff factor: smooths upsampled steps or downsampled aliasing
        let alpha = if self.out_rate > self.in_rate {
            0.35
        } else {
            (self.out_rate as f64 / self.in_rate as f64).clamp(0.15, 0.85)
        };

        // Downsampling: pre-filter input to avoid aliasing
        let filtered_in: Vec<f64> = if self.out_rate < self.in_rate {
            let mut tmp = Vec::with_capacity(input.len());
            for &s in input {
                self.lp_state += alpha * (s as f64 - self.lp_state);
                tmp.push(self.lp_state);
            }
            tmp
        } else {
            input.iter().map(|&s| s as f64).collect()
        };

        // `phase` is relative to [prev_sample, filtered_in[0], filtered_in[1], ...]
        // where index 0 is `prev_sample` and index 1..=len are `filtered_in`.
        let total_len = filtered_in.len();
        while self.phase < total_len as f64 {
            let idx = self.phase.floor() as usize;
            let frac = self.phase - idx as f64;
            let s0 = if idx == 0 {
                self.prev_sample
            } else {
                filtered_in[idx - 1]
            };
            let s1 = filtered_in[idx.min(total_len - 1)];
            let mut val = s0 + (s1 - s0) * frac;

            if self.out_rate > self.in_rate {
                self.lp_state += alpha * (val - self.lp_state);
                val = self.lp_state;
            }

            out.push(val.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
            self.phase += step;
        }

        self.phase -= total_len as f64;
        if let Some(&last) = filtered_in.last() {
            self.prev_sample = last;
        }
        out
    }
}

/// Simple stateless linear resampler (kept for tests / one-shot buffers).
#[allow(dead_code)]
pub fn resample_linear(input: &[i16], in_rate: u32, out_rate: u32) -> Vec<i16> {
    if input.is_empty() || in_rate == out_rate || in_rate == 0 || out_rate == 0 {
        return input.to_vec();
    }
    let out_len = ((input.len() as u64 * out_rate as u64) / in_rate as u64).max(1) as usize;
    let mut out = Vec::with_capacity(out_len);
    let ratio = in_rate as f64 / out_rate as f64;
    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let idx = pos as usize;
        let frac = pos - idx as f64;
        let s0 = input[idx.min(input.len() - 1)] as f64;
        let s1 = input[(idx + 1).min(input.len() - 1)] as f64;
        let val = s0 + (s1 - s0) * frac;
        out.push(val.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_codecs_roundtrip_frame() {
        for (kind, samples_in, expected_bytes) in [
            (CodecKind::Pcmu, 160, 160),
            (CodecKind::Pcma, 160, 160),
            (CodecKind::G722, 320, 160),
            (CodecKind::G729, 160, 20),
        ] {
            let mut codec = AudioCodec::new(kind);
            let pcm: Vec<i16> = (0..samples_in)
                .map(|i| ((i as f32 * 0.2).sin() * 8000.0) as i16)
                .collect();
            let encoded = codec.encode(&pcm);
            assert_eq!(encoded.len(), expected_bytes, "codec {:?}", kind);
            let decoded = codec.decode(&encoded);
            assert_eq!(decoded.len(), samples_in, "codec {:?}", kind);
        }
    }

    #[test]
    fn test_resampler() {
        let input: Vec<i16> = (0..480).map(|i| (i * 10) as i16).collect();
        let down = resample_linear(&input, 48000, 8000);
        assert_eq!(down.len(), 80);
        let up = resample_linear(&down, 8000, 48000);
        assert_eq!(up.len(), 480);
    }
}
