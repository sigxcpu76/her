use chrono::{DateTime, Utc};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DtmfMode {
    Rfc4733,
    SipInfo,
    Both,
}

impl std::fmt::Display for DtmfMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DtmfMode::Rfc4733 => write!(f, "RFC 4733 (RTP Event)"),
            DtmfMode::SipInfo => write!(f, "SIP INFO"),
            DtmfMode::Both => write!(f, "Both (RFC 4733 + SIP INFO)"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CodecKind {
    G722,
    Pcmu,
    Pcma,
    G729,
}

impl CodecKind {
    pub fn payload_type(self) -> u8 {
        match self {
            CodecKind::Pcmu => 0,
            CodecKind::Pcma => 8,
            CodecKind::G722 => 9,
            CodecKind::G729 => 18,
        }
    }

    pub fn sdp_name(self) -> &'static str {
        match self {
            CodecKind::Pcmu => "PCMU",
            CodecKind::Pcma => "PCMA",
            CodecKind::G722 => "G722",
            CodecKind::G729 => "G729",
        }
    }

    /// RTP clock rate per RFC 3551 (note: G.722 uses 8000 Hz clock rate in RTP/SDP!)
    pub fn rtp_clock_rate(self) -> u32 {
        8000
    }

    /// Actual PCM sample rate of the codec
    pub fn sample_rate(self) -> u32 {
        match self {
            CodecKind::G722 => 16000,
            CodecKind::Pcmu | CodecKind::Pcma | CodecKind::G729 => 8000,
        }
    }
}

impl std::fmt::Display for CodecKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecKind::G722 => write!(f, "G.722 (16kHz Wideband)"),
            CodecKind::Pcmu => write!(f, "PCMU (G.711 u-law)"),
            CodecKind::Pcma => write!(f, "PCMA (G.711 A-law)"),
            CodecKind::G729 => write!(f, "G.729 (8kbit/s CS-ACELP)"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RegistrationConfig {
    pub enabled: bool,
    pub domain: String,
    pub registrar: String,
    pub username: String,
    pub auth_username: String,
    pub password: String,
    pub display_name: String,
    pub outbound_proxy: String,
    pub expires_secs: u32,
}

impl Default for RegistrationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            domain: String::new(),
            registrar: String::new(),
            username: String::new(),
            auth_username: String::new(),
            password: String::new(),
            display_name: "Her".to_string(),
            outbound_proxy: String::new(),
            expires_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NatConfig {
    pub stun_enabled: bool,
    pub stun_server: String,
    pub stun_fallback: String,
    pub use_rport: bool,
    pub symmetric_rtp: bool,
    pub keepalive_secs: u64,
}

impl Default for NatConfig {
    fn default() -> Self {
        Self {
            stun_enabled: true,
            stun_server: "stun.l.google.com:19302".to_string(),
            stun_fallback: "stun1.l.google.com:19302".to_string(),
            use_rport: true,
            symmetric_rtp: true,
            keepalive_secs: 25,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CodecConfig {
    pub enable_g722: bool,
    pub enable_pcmu: bool,
    pub enable_pcma: bool,
    pub enable_g729: bool,
    pub dtmf_mode: DtmfMode,
}

impl Default for CodecConfig {
    fn default() -> Self {
        Self {
            enable_g722: true,
            enable_pcmu: true,
            enable_pcma: true,
            enable_g729: true,
            dtmf_mode: DtmfMode::Rfc4733,
        }
    }
}

impl CodecConfig {
    pub fn enabled_codecs(&self) -> Vec<CodecKind> {
        let mut list = Vec::new();
        if self.enable_g722 {
            list.push(CodecKind::G722);
        }
        if self.enable_pcmu {
            list.push(CodecKind::Pcmu);
        }
        if self.enable_pcma {
            list.push(CodecKind::Pcma);
        }
        if self.enable_g729 {
            list.push(CodecKind::G729);
        }
        if list.is_empty() {
            list.push(CodecKind::Pcmu);
        }
        list
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub local_sip_port: u16,
    pub registration: RegistrationConfig,
    pub nat: NatConfig,
    pub codecs: CodecConfig,
    pub audio: AudioConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            local_sip_port: 0,
            registration: RegistrationConfig::default(),
            nat: NatConfig::default(),
            codecs: CodecConfig::default(),
            audio: AudioConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallDirection {
    Outgoing,
    Incoming,
    Missed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub uri: String,
    pub direction: CallDirection,
    pub timestamp: DateTime<Utc>,
    pub duration_secs: u64,
}

fn config_dir() -> PathBuf {
    if let Some(proj) = ProjectDirs::from("io", "her-sip", "her-sip") {
        proj.config_dir().to_path_buf()
    } else {
        PathBuf::from(".her-sip")
    }
}

pub fn load_config() -> AppConfig {
    let path = config_dir().join("config.json");
    if let Ok(data) = fs::read_to_string(&path) {
        if let Ok(cfg) = serde_json::from_str(&data) {
            return cfg;
        }
    }
    AppConfig::default()
}

pub fn save_config(cfg: &AppConfig) {
    let dir = config_dir();
    if fs::create_dir_all(&dir).is_ok() {
        if let Ok(json) = serde_json::to_string_pretty(cfg) {
            let _ = fs::write(dir.join("config.json"), json);
        }
    }
}

pub fn load_history() -> Vec<HistoryEntry> {
    let path = config_dir().join("history.json");
    if let Ok(data) = fs::read_to_string(&path) {
        if let Ok(list) = serde_json::from_str(&data) {
            return list;
        }
    }
    Vec::new()
}

pub fn save_history(entries: &[HistoryEntry]) {
    let dir = config_dir();
    if fs::create_dir_all(&dir).is_ok() {
        if let Ok(json) = serde_json::to_string_pretty(entries) {
            let _ = fs::write(dir.join("history.json"), json);
        }
    }
}
