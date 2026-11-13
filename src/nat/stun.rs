use rand::RngCore;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use thiserror::Error;
use tokio::net::{lookup_host, UdpSocket};
use tokio::time::timeout;

const STUN_MAGIC_COOKIE: u32 = 0x2112_A442;
const STUN_BINDING_REQUEST: u16 = 0x0001;
const STUN_BINDING_RESPONSE: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

#[derive(Debug, Error)]
pub enum StunError {
    #[error("DNS resolution failed for {0}: {1}")]
    Resolve(String, String),
    #[error("Socket I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("STUN request timed out")]
    Timeout,
    #[error("Invalid STUN response: {0}")]
    InvalidResponse(String),
}

#[derive(Debug, Clone)]
pub struct StunResult {
    pub mapped_addr: SocketAddr,
    pub server_used: String,
    pub rtt_ms: u128,
}

/// Discover our preferred outbound local IPv4 address without sending any traffic.
pub fn discover_local_ip() -> IpAddr {
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(local) = sock.local_addr() {
                return local.ip();
            }
        }
    }
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

/// Build a 20-byte RFC 5389 STUN Binding Request.
pub fn build_binding_request() -> ([u8; 20], [u8; 12]) {
    let mut tx_id = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut tx_id);

    let mut buf = [0u8; 20];
    buf[0..2].copy_from_slice(&STUN_BINDING_REQUEST.to_be_bytes());
    buf[2..4].copy_from_slice(&0u16.to_be_bytes()); // message length
    buf[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
    buf[8..20].copy_from_slice(&tx_id);
    (buf, tx_id)
}

/// Check if a UDP packet looks like a STUN response for our transaction ID and parse it.
pub fn parse_binding_response(
    pkt: &[u8],
    expected_tx_id: &[u8; 12],
) -> Result<SocketAddr, StunError> {
    if pkt.len() < 20 {
        return Err(StunError::InvalidResponse("packet too short".into()));
    }
    let msg_type = u16::from_be_bytes([pkt[0], pkt[1]]);
    let msg_len = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
    let cookie = u32::from_be_bytes([pkt[4], pkt[5], pkt[6], pkt[7]]);

    if msg_type != STUN_BINDING_RESPONSE {
        return Err(StunError::InvalidResponse(format!(
            "unexpected type 0x{:04x}",
            msg_type
        )));
    }
    if cookie != STUN_MAGIC_COOKIE {
        return Err(StunError::InvalidResponse("bad magic cookie".into()));
    }
    if &pkt[8..20] != expected_tx_id {
        return Err(StunError::InvalidResponse("transaction ID mismatch".into()));
    }
    if pkt.len() < 20 + msg_len {
        return Err(StunError::InvalidResponse("truncated attributes".into()));
    }

    let attrs = &pkt[20..20 + msg_len];
    let mut offset = 0usize;
    let mut mapped_fallback: Option<SocketAddr> = None;

    while offset + 4 <= attrs.len() {
        let attr_type = u16::from_be_bytes([attrs[offset], attrs[offset + 1]]);
        let attr_len = u16::from_be_bytes([attrs[offset + 2], attrs[offset + 3]]) as usize;
        offset += 4;
        if offset + attr_len > attrs.len() {
            break;
        }
        let val = &attrs[offset..offset + attr_len];

        match attr_type {
            ATTR_XOR_MAPPED_ADDRESS => {
                if let Some(addr) = parse_xor_mapped_addr(val, expected_tx_id) {
                    return Ok(addr);
                }
            }
            ATTR_MAPPED_ADDRESS => {
                if let Some(addr) = parse_mapped_addr(val) {
                    mapped_fallback = Some(addr);
                }
            }
            _ => {}
        }

        // Attributes are padded to 4-byte boundaries
        let padded = (attr_len + 3) & !3;
        offset += padded;
    }

    mapped_fallback.ok_or_else(|| {
        StunError::InvalidResponse("no XOR-MAPPED-ADDRESS or MAPPED-ADDRESS".into())
    })
}

fn parse_xor_mapped_addr(val: &[u8], tx_id: &[u8; 12]) -> Option<SocketAddr> {
    if val.len() < 4 {
        return None;
    }
    let family = val[1];
    let xport = u16::from_be_bytes([val[2], val[3]]);
    let port = xport ^ ((STUN_MAGIC_COOKIE >> 16) as u16);

    match family {
        0x01 if val.len() >= 8 => {
            let xip = u32::from_be_bytes([val[4], val[5], val[6], val[7]]);
            let ip = Ipv4Addr::from(xip ^ STUN_MAGIC_COOKIE);
            Some(SocketAddr::new(IpAddr::V4(ip), port))
        }
        0x02 if val.len() >= 20 => {
            let mut xor_key = [0u8; 16];
            xor_key[0..4].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
            xor_key[4..16].copy_from_slice(tx_id);
            let mut ip_bytes = [0u8; 16];
            for i in 0..16 {
                ip_bytes[i] = val[4 + i] ^ xor_key[i];
            }
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip_bytes)), port))
        }
        _ => None,
    }
}

fn parse_mapped_addr(val: &[u8]) -> Option<SocketAddr> {
    if val.len() < 4 {
        return None;
    }
    let family = val[1];
    let port = u16::from_be_bytes([val[2], val[3]]);
    match family {
        0x01 if val.len() >= 8 => {
            let ip = Ipv4Addr::new(val[4], val[5], val[6], val[7]);
            Some(SocketAddr::new(IpAddr::V4(ip), port))
        }
        0x02 if val.len() >= 20 => {
            let mut ip_bytes = [0u8; 16];
            ip_bytes.copy_from_slice(&val[4..20]);
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip_bytes)), port))
        }
        _ => None,
    }
}

/// Perform a STUN Binding query on an already-bound UDP socket.
/// Note: only use when no concurrent recv loop is stealing packets from this socket
/// (e.g., before starting the main loop, on a fresh RTP socket, or for diagnostics).
pub async fn query_stun_on_socket(
    socket: &UdpSocket,
    stun_server: &str,
) -> Result<StunResult, StunError> {
    let server_addr = resolve_stun_server(stun_server).await?;
    let (req, tx_id) = build_binding_request();

    let start = std::time::Instant::now();
    let mut buf = [0u8; 512];

    for attempt in 0..3 {
        socket.send_to(&req, server_addr).await?;
        let wait_ms = 600 + attempt * 500;
        match timeout(Duration::from_millis(wait_ms), socket.recv_from(&mut buf)).await {
            Ok(Ok((len, _src))) => {
                if let Ok(mapped_addr) = parse_binding_response(&buf[..len], &tx_id) {
                    return Ok(StunResult {
                        mapped_addr,
                        server_used: stun_server.to_string(),
                        rtt_ms: start.elapsed().as_millis(),
                    });
                }
            }
            Ok(Err(e)) => return Err(StunError::Io(e)),
            Err(_) => continue,
        }
    }

    Err(StunError::Timeout)
}

/// Query primary STUN server and fall back to secondary if provided.
pub async fn discover_mapped_addr(
    socket: &UdpSocket,
    primary: &str,
    fallback: &str,
) -> Result<StunResult, StunError> {
    let p = primary.trim();
    if !p.is_empty() {
        if let Ok(res) = query_stun_on_socket(socket, p).await {
            return Ok(res);
        }
    }
    let f = fallback.trim();
    if !f.is_empty() {
        return query_stun_on_socket(socket, f).await;
    }
    Err(StunError::Timeout)
}

/// Standalone STUN test (creates a temporary UDP socket) for the Settings UI.
pub async fn test_stun(primary: &str, fallback: &str) -> Result<StunResult, StunError> {
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    discover_mapped_addr(&sock, primary, fallback).await
}

async fn resolve_stun_server(server: &str) -> Result<SocketAddr, StunError> {
    let with_port = if server.contains(':') {
        server.to_string()
    } else {
        format!("{}:19302", server)
    };
    let addrs = lookup_host(&with_port)
        .await
        .map_err(|e| StunError::Resolve(with_port.clone(), e.to_string()))?;

    for addr in addrs {
        if addr.is_ipv4() {
            return Ok(addr);
        }
    }
    Err(StunError::Resolve(
        with_port,
        "no IPv4 address resolved".into(),
    ))
}
