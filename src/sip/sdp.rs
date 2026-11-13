use crate::config::CodecKind;
use crate::media::rtp::DTMF_PAYLOAD_TYPE;
use std::net::{IpAddr, SocketAddr};

#[derive(Debug, Clone)]
pub struct SdpSession {
    pub remote_rtp: SocketAddr,
    pub codecs: Vec<CodecKind>,
}

pub fn build_sdp(
    local_rtp: SocketAddr,
    codecs: &[CodecKind],
    direction: &str,
) -> String {
    let ip = local_rtp.ip().to_string();
    let port = local_rtp.port();

    let mut pts: Vec<String> = codecs
        .iter()
        .map(|c| c.payload_type().to_string())
        .collect();
    pts.push(DTMF_PAYLOAD_TYPE.to_string());

    let mut lines = vec![
        "v=0".to_string(),
        format!("o=her-sip 1 1 IN IP4 {}", ip),
        "s=Her SIP Call".to_string(),
        format!("c=IN IP4 {}", ip),
        "t=0 0".to_string(),
        format!("m=audio {} RTP/AVP {}", port, pts.join(" ")),
    ];

    for c in codecs {
        lines.push(format!(
            "a=rtpmap:{} {}/{}",
            c.payload_type(),
            c.sdp_name(),
            c.rtp_clock_rate()
        ));
        if *c == CodecKind::G729 {
            lines.push("a=fmtp:18 annexb=no".to_string());
        }
    }

    lines.push(format!(
        "a=rtpmap:{} telephone-event/8000",
        DTMF_PAYLOAD_TYPE
    ));
    lines.push(format!("a=fmtp:{} 0-16", DTMF_PAYLOAD_TYPE));
    lines.push("a=ptime:20".to_string());
    lines.push(format!("a={}", direction));

    format!("{}\r\n", lines.join("\r\n"))
}

pub fn parse_sdp(body: &str) -> Option<SdpSession> {
    let mut session_ip: Option<IpAddr> = None;
    let mut media_ip: Option<IpAddr> = None;
    let mut media_port: Option<u16> = None;
    let mut payload_types: Vec<u8> = Vec::new();
    let mut rtpmap: Vec<(u8, String)> = Vec::new();
    let mut in_audio = false;

    for raw_line in body.lines() {
        let line = raw_line.trim();
        if let Some(rest) = line.strip_prefix("c=IN IP4 ") {
            if let Ok(ip) = rest.split('/').next().unwrap_or("").trim().parse::<IpAddr>() {
                if in_audio {
                    media_ip = Some(ip);
                } else {
                    session_ip = Some(ip);
                }
            }
        } else if let Some(rest) = line.strip_prefix("m=") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.first() == Some(&"audio") {
                in_audio = true;
                if let Some(p) = parts.get(1).and_then(|s| s.parse::<u16>().ok()) {
                    media_port = Some(p);
                }
                for pt_str in parts.iter().skip(3) {
                    if let Ok(pt) = pt_str.parse::<u8>() {
                        payload_types.push(pt);
                    }
                }
            } else {
                in_audio = false;
            }
        } else if in_audio {
            if let Some(rest) = line.strip_prefix("a=rtpmap:") {
                let mut parts = rest.split_whitespace();
                if let (Some(pt_str), Some(codec_str)) = (parts.next(), parts.next()) {
                    if let Ok(pt) = pt_str.parse::<u8>() {
                        let name = codec_str.split('/').next().unwrap_or("").to_ascii_uppercase();
                        rtpmap.push((pt, name));
                    }
                }
            }
        }
    }

    let ip = media_ip.or(session_ip)?;
    let port = media_port?;
    if port == 0 {
        return None;
    }

    let mut codecs = Vec::new();
    for pt in payload_types {
        if let Some(c) = pt_or_name_to_codec(pt, &rtpmap) {
            if !codecs.contains(&c) {
                codecs.push(c);
            }
        }
    }

    Some(SdpSession {
        remote_rtp: SocketAddr::new(ip, port),
        codecs,
    })
}

fn pt_or_name_to_codec(pt: u8, rtpmap: &[(u8, String)]) -> Option<CodecKind> {
    if let Some((_, name)) = rtpmap.iter().find(|(p, _)| *p == pt) {
        match name.as_str() {
            "G722" => return Some(CodecKind::G722),
            "PCMU" => return Some(CodecKind::Pcmu),
            "PCMA" => return Some(CodecKind::Pcma),
            "G729" => return Some(CodecKind::G729),
            _ => {}
        }
    }
    match pt {
        0 => Some(CodecKind::Pcmu),
        8 => Some(CodecKind::Pcma),
        9 => Some(CodecKind::G722),
        18 => Some(CodecKind::G729),
        _ => None,
    }
}

pub fn negotiate_codec(offered: &[CodecKind], local_prefs: &[CodecKind]) -> CodecKind {
    for pref in local_prefs {
        if offered.contains(pref) {
            return *pref;
        }
    }
    offered
        .first()
        .copied()
        .or_else(|| local_prefs.first().copied())
        .unwrap_or(CodecKind::Pcmu)
}
