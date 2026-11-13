use std::collections::BTreeMap;

pub const DTMF_PAYLOAD_TYPE: u8 = 101;

#[derive(Debug, Clone)]
pub struct RtpPacket {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload: Vec<u8>,
}

impl RtpPacket {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12 + self.payload.len());
        buf.push(0x80); // V=2, P=0, X=0, CC=0
        buf.push((if self.marker { 0x80 } else { 0x00 }) | (self.payload_type & 0x7F));
        buf.extend_from_slice(&self.sequence_number.to_be_bytes());
        buf.extend_from_slice(&self.timestamp.to_be_bytes());
        buf.extend_from_slice(&self.ssrc.to_be_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < 12 {
            return None;
        }
        let version = (data[0] >> 6) & 0x03;
        if version != 2 {
            return None;
        }
        let padding = (data[0] & 0x20) != 0;
        let extension = (data[0] & 0x10) != 0;
        let csrc_count = (data[0] & 0x0F) as usize;
        let marker = (data[1] & 0x80) != 0;
        let payload_type = data[1] & 0x7F;
        let sequence_number = u16::from_be_bytes([data[2], data[3]]);
        let timestamp = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

        let mut offset = 12 + csrc_count * 4;
        if offset > data.len() {
            return None;
        }
        if extension {
            if offset + 4 > data.len() {
                return None;
            }
            let ext_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
            offset += 4 + ext_len * 4;
            if offset > data.len() {
                return None;
            }
        }
        let mut end = data.len();
        if padding && end > offset {
            let pad_len = data[end - 1] as usize;
            if pad_len > 0 && pad_len <= end - offset {
                end -= pad_len;
            }
        }

        Some(Self {
            marker,
            payload_type,
            sequence_number,
            timestamp,
            ssrc,
            payload: data[offset..end].to_vec(),
        })
    }
}

pub fn dtmf_char_to_code(ch: char) -> Option<u8> {
    match ch {
        '0'..='9' => Some((ch as u8) - b'0'),
        '*' => Some(10),
        '#' => Some(11),
        'A' | 'a' => Some(12),
        'B' | 'b' => Some(13),
        'C' | 'c' => Some(14),
        'D' | 'd' => Some(15),
        _ => None,
    }
}

/// Build an RFC 4733 telephone-event RTP payload (4 bytes).
pub fn build_dtmf_event_payload(event: u8, end: bool, volume: u8, duration: u16) -> Vec<u8> {
    let flags = (if end { 0x80 } else { 0x00 }) | (volume & 0x3F);
    let dur = duration.to_be_bytes();
    vec![event, flags, dur[0], dur[1]]
}

/// Simple ordered jitter buffer for 20ms RTP frames.
pub struct JitterBuffer {
    packets: BTreeMap<u16, RtpPacket>,
    last_popped_seq: Option<u16>,
    min_depth: usize,
    max_depth: usize,
}

impl JitterBuffer {
    pub fn new(min_depth: usize, max_depth: usize) -> Self {
        Self {
            packets: BTreeMap::new(),
            last_popped_seq: None,
            min_depth,
            max_depth,
        }
    }

    pub fn push(&mut self, pkt: RtpPacket) {
        if let Some(last) = self.last_popped_seq {
            // Drop stale/late packets (handling u16 wraparound)
            let diff = pkt.sequence_number.wrapping_sub(last);
            if diff == 0 || diff > 32768 {
                return;
            }
        }
        self.packets.insert(pkt.sequence_number, pkt);
        while self.packets.len() > self.max_depth {
            if let Some(&first_key) = self.packets.keys().next() {
                self.packets.remove(&first_key);
            }
        }
    }

    pub fn pop(&mut self) -> Option<RtpPacket> {
        if self.last_popped_seq.is_none() && self.packets.len() < self.min_depth {
            return None;
        }
        let next_key = if let Some(last) = self.last_popped_seq {
            let expected = last.wrapping_add(1);
            if self.packets.contains_key(&expected) {
                Some(expected)
            } else if self.packets.len() >= self.min_depth {
                // Only skip ahead if enough buffered packets exist
                self.packets
                    .keys()
                    .copied()
                    .min_by_key(|&k| k.wrapping_sub(last))
            } else {
                None
            }
        } else {
            self.packets.keys().next().copied()
        };

        if let Some(k) = next_key {
            self.last_popped_seq = Some(k);
            self.packets.remove(&k)
        } else {
            None
        }
    }
}
