use rand::Rng;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct DigestChallenge {
    pub realm: String,
    pub nonce: String,
    pub qop: Option<String>,
    pub opaque: Option<String>,
    pub algorithm: String,
}

pub fn parse_digest_challenge(header_val: &str) -> Option<DigestChallenge> {
    let trimmed = header_val.trim();
    let rest = if let Some(s) = trimmed.strip_prefix("Digest ") {
        s
    } else if let Some(s) = trimmed.strip_prefix("digest ") {
        s
    } else {
        return None;
    };

    let params = parse_kv_params(rest);
    let realm = params.get("realm")?.clone();
    let nonce = params.get("nonce")?.clone();
    let qop = params.get("qop").cloned();
    let opaque = params.get("opaque").cloned();
    let algorithm = params
        .get("algorithm")
        .cloned()
        .unwrap_or_else(|| "MD5".to_string());

    Some(DigestChallenge {
        realm,
        nonce,
        qop,
        opaque,
        algorithm,
    })
}

pub fn build_digest_response(
    challenge: &DigestChallenge,
    username: &str,
    password: &str,
    method: &str,
    uri: &str,
    nc_counter: u32,
) -> String {
    let algo_upper = challenge.algorithm.to_uppercase();
    let is_sha256 = algo_upper.contains("SHA-256");

    let ha1 = hash_hex(
        &format!("{}:{}:{}", username, challenge.realm, password),
        is_sha256,
    );
    let ha2 = hash_hex(&format!("{}:{}", method, uri), is_sha256);

    let use_qop_auth = challenge
        .qop
        .as_deref()
        .map(|q| q.split(',').any(|t| t.trim().eq_ignore_ascii_case("auth")))
        .unwrap_or(false);

    let cnonce: String = format!("{:08x}", rand::thread_rng().gen::<u32>());
    let nc = format!("{:08x}", nc_counter);

    let response = if use_qop_auth {
        hash_hex(
            &format!(
                "{}:{}:{}:{}:auth:{}",
                ha1, challenge.nonce, nc, cnonce, ha2
            ),
            is_sha256,
        )
    } else {
        hash_hex(&format!("{}:{}:{}", ha1, challenge.nonce, ha2), is_sha256)
    };

    let mut parts = vec![
        format!("username=\"{}\"", username),
        format!("realm=\"{}\"", challenge.realm),
        format!("nonce=\"{}\"", challenge.nonce),
        format!("uri=\"{}\"", uri),
        format!("response=\"{}\"", response),
        format!("algorithm={}", challenge.algorithm),
    ];

    if use_qop_auth {
        parts.push("qop=auth".to_string());
        parts.push(format!("nc={}", nc));
        parts.push(format!("cnonce=\"{}\"", cnonce));
    }
    if let Some(ref op) = challenge.opaque {
        parts.push(format!("opaque=\"{}\"", op));
    }

    format!("Digest {}", parts.join(", "))
}

fn hash_hex(input: &str, sha256: bool) -> String {
    if sha256 {
        let mut h = Sha256::new();
        h.update(input.as_bytes());
        format!("{:x}", h.finalize())
    } else {
        format!("{:x}", md5::compute(input.as_bytes()))
    }
}

fn parse_kv_params(s: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut chars = s.chars().peekable();

    while chars.peek().is_some() {
        while matches!(chars.peek(), Some(' ' | '\t' | ',')) {
            chars.next();
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' || c == ',' {
                break;
            }
            key.push(c);
            chars.next();
        }
        let key = key.trim().to_ascii_lowercase();
        if chars.peek() != Some(&'=') {
            while let Some(&c) = chars.peek() {
                chars.next();
                if c == ',' {
                    break;
                }
            }
            continue;
        }
        chars.next(); // consume '='

        while matches!(chars.peek(), Some(' ' | '\t')) {
            chars.next();
        }

        let mut val = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    if let Some(esc) = chars.next() {
                        val.push(esc);
                    }
                } else if c == '"' {
                    break;
                } else {
                    val.push(c);
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                val.push(c);
                chars.next();
            }
            val = val.trim().to_string();
        }

        if !key.is_empty() {
            map.insert(key, val);
        }
    }

    map
}
