use rand::Rng;
use std::net::{IpAddr, SocketAddr};

#[derive(Debug, Clone)]
pub struct SipRequest {
    pub method: String,
    pub uri: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct SipResponse {
    pub status_code: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

#[derive(Debug, Clone)]
pub enum SipMessage {
    Request(SipRequest),
    Response(SipResponse),
}

impl SipMessage {
    pub fn parse(raw: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(raw).ok()?;
        let (head, body) = if let Some(idx) = text.find("\r\n\r\n") {
            (&text[..idx], text[idx + 4..].to_string())
        } else if let Some(idx) = text.find("\n\n") {
            (&text[..idx], text[idx + 2..].to_string())
        } else {
            (text, String::new())
        };

        let mut lines = head.lines();
        let start_line = lines.next()?.trim();
        if start_line.is_empty() {
            return None;
        }

        let mut headers = Vec::new();
        for line in lines {
            let line = line.trim_end_matches('\r');
            if let Some(colon) = line.find(':') {
                let name = expand_compact_header(line[..colon].trim());
                let val = line[colon + 1..].trim().to_string();
                headers.push((name, val));
            }
        }

        if start_line.starts_with("SIP/2.0 ") {
            let rest = start_line.strip_prefix("SIP/2.0 ")?.trim();
            let mut parts = rest.splitn(2, ' ');
            let status_code = parts.next()?.parse::<u16>().ok()?;
            let reason = parts.next().unwrap_or("").to_string();
            Some(SipMessage::Response(SipResponse {
                status_code,
                reason,
                headers,
                body,
            }))
        } else {
            let mut parts = start_line.split_whitespace();
            let method = parts.next()?.to_ascii_uppercase();
            let uri = parts.next()?.to_string();
            let version = parts.next()?;
            if !version.starts_with("SIP/2.0") {
                return None;
            }
            Some(SipMessage::Request(SipRequest {
                method,
                uri,
                headers,
                body,
            }))
        }
    }
}

fn expand_compact_header(name: &str) -> String {
    match name {
        "v" | "V" => "Via".to_string(),
        "f" | "F" => "From".to_string(),
        "t" | "T" => "To".to_string(),
        "m" | "M" => "Contact".to_string(),
        "i" | "I" => "Call-ID".to_string(),
        "c" | "C" => "Content-Type".to_string(),
        "l" | "L" => "Content-Length".to_string(),
        "k" | "K" => "Supported".to_string(),
        other => other.to_string(),
    }
}

pub fn get_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

pub fn get_all_headers<'a>(headers: &'a [(String, String)], name: &str) -> Vec<&'a str> {
    headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .collect()
}

pub fn parse_cseq(val: &str) -> Option<(u32, String)> {
    let mut parts = val.split_whitespace();
    let num = parts.next()?.parse::<u32>().ok()?;
    let method = parts.next()?.to_ascii_uppercase();
    Some((num, method))
}

pub fn extract_tag(header_val: &str) -> Option<String> {
    for part in header_val.split(';') {
        let p = part.trim();
        if let Some(rest) = p.strip_prefix("tag=").or_else(|| p.strip_prefix("TAG=")) {
            return Some(rest.trim_matches('"').to_string());
        }
    }
    None
}

pub fn extract_uri_from_header(header_val: &str) -> String {
    if let (Some(start), Some(end)) = (header_val.find('<'), header_val.find('>')) {
        if end > start {
            return header_val[start + 1..end].trim().to_string();
        }
    }
    header_val
        .split(';')
        .next()
        .unwrap_or(header_val)
        .trim()
        .to_string()
}

pub fn extract_display_or_uri(header_val: &str) -> String {
    let trimmed = header_val.trim();
    if let Some(idx) = trimmed.find('<') {
        let name = trimmed[..idx].trim().trim_matches('"').trim();
        if !name.is_empty() {
            return name.to_string();
        }
    }
    extract_uri_from_header(trimmed)
}

/// Extract external IP & port learned from `Via` `received=` and `rport=` parameters (RFC 3581).
pub fn parse_via_rport(via_val: &str) -> Option<SocketAddr> {
    let mut received: Option<IpAddr> = None;
    let mut rport: Option<u16> = None;
    for part in via_val.split(';') {
        let p = part.trim();
        if let Some(ip_str) = p.strip_prefix("received=") {
            received = ip_str.trim().parse::<IpAddr>().ok();
        } else if let Some(port_str) = p.strip_prefix("rport=") {
            rport = port_str.trim().parse::<u16>().ok();
        }
    }
    match (received, rport) {
        (Some(ip), Some(port)) if port > 0 => Some(SocketAddr::new(ip, port)),
        _ => None,
    }
}

pub fn random_token(len: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

pub fn normalize_sip_uri(input: &str, default_domain: Option<&str>) -> String {
    let s = input.trim();
    if s.is_empty() {
        return String::new();
    }
    if s.starts_with("sip:") || s.starts_with("sips:") {
        return s.to_string();
    }
    if s.contains('@') {
        return format!("sip:{}", s);
    }
    if let Some(dom) = default_domain {
        let d = dom.trim();
        if !d.is_empty() {
            return format!("sip:{}@{}", s, d);
        }
    }
    format!("sip:{}", s)
}

/// Extract `host:port` target from a SIP URI (defaults to port 5060).
pub fn sip_uri_host_port(uri: &str) -> Option<(String, u16)> {
    let without_scheme = uri
        .trim()
        .strip_prefix("sip:")
        .or_else(|| uri.trim().strip_prefix("sips:"))
        .unwrap_or(uri.trim());
    let without_params = without_scheme.split(';').next().unwrap_or(without_scheme);
    let host_port = without_params
        .rsplit('@')
        .next()
        .unwrap_or(without_params)
        .trim();
    if host_port.is_empty() {
        return None;
    }
    if let Some((h, p)) = host_port.rsplit_once(':') {
        if let Ok(port) = p.parse::<u16>() {
            return Some((h.to_string(), port));
        }
    }
    Some((host_port.to_string(), 5060))
}

impl SipRequest {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = format!("{} {} SIP/2.0\r\n", self.method, self.uri);
        for (k, v) in &self.headers {
            if !k.eq_ignore_ascii_case("Content-Length") {
                out.push_str(&format!("{}: {}\r\n", k, v));
            }
        }
        out.push_str(&format!("Content-Length: {}\r\n\r\n", self.body.len()));
        out.push_str(&self.body);
        out.into_bytes()
    }
}

impl SipResponse {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = format!("SIP/2.0 {} {}\r\n", self.status_code, self.reason);
        for (k, v) in &self.headers {
            if !k.eq_ignore_ascii_case("Content-Length") {
                out.push_str(&format!("{}: {}\r\n", k, v));
            }
        }
        out.push_str(&format!("Content-Length: {}\r\n\r\n", self.body.len()));
        out.push_str(&self.body);
        out.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_response_and_rport() {
        let raw = b"SIP/2.0 200 OK\r\n\
            Via: SIP/2.0/UDP 192.168.1.5:5060;branch=z9hG4bK123;received=203.0.113.4;rport=41234\r\n\
            From: <sip:alice@example.com>;tag=abc\r\n\
            To: <sip:bob@example.com>;tag=xyz\r\n\
            Call-ID: call123\r\n\
            CSeq: 1 INVITE\r\n\
            Content-Length: 0\r\n\r\n";

        let msg = SipMessage::parse(raw).expect("parse sip");
        match msg {
            SipMessage::Response(r) => {
                assert_eq!(r.status_code, 200);
                let via = get_header(&r.headers, "Via").unwrap();
                let ext = parse_via_rport(via).unwrap();
                assert_eq!(ext, "203.0.113.4:41234".parse().unwrap());
                assert_eq!(extract_tag(get_header(&r.headers, "To").unwrap()).as_deref(), Some("xyz"));
            }
            _ => panic!("expected response"),
        }
    }

    #[test]
    fn test_normalize_uri() {
        assert_eq!(normalize_sip_uri("echo@iptel.org", None), "sip:echo@iptel.org");
        assert_eq!(normalize_sip_uri("100", Some("pbx.local")), "sip:100@pbx.local");
        assert_eq!(normalize_sip_uri("sip:bob@10.0.0.2:5080", None), "sip:bob@10.0.0.2:5080");
    }
}
