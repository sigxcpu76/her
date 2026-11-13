pub mod auth;
pub mod engine;
pub mod message;
pub mod sdp;

use crate::config::{AppConfig, CodecKind};
use std::net::SocketAddr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationState {
    Disabled,
    Unregistered,
    Registering,
    Registered(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallState {
    Idle,
    Dialing {
        peer: String,
    },
    RingingOutgoing {
        peer: String,
    },
    RingingIncoming {
        peer: String,
        display: String,
    },
    Active {
        peer: String,
        codec: CodecKind,
        muted: bool,
        on_hold: bool,
    },
}

#[derive(Debug, Clone)]
pub enum SipCommand {
    UpdateConfig(AppConfig),
    Register,
    Unregister,
    Dial(String),
    Answer,
    Reject,
    Hangup,
    ToggleMute(bool),
    ToggleHold(bool),
    SendDtmf(char),
    RefreshNat,
}

#[derive(Debug, Clone)]
pub enum SipEvent {
    RegistrationChanged(RegistrationState),
    CallStateChanged(CallState),
    CallEnded {
        peer: String,
        duration_secs: u64,
        reason: String,
    },
    NatDiscovered {
        public_addr: Option<SocketAddr>,
        via: String,
    },
    StatusMessage(String),
}
