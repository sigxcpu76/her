use crate::config::{
    load_config, load_history, save_config, save_history, AppConfig, CallDirection, DtmfMode,
    HistoryEntry,
};
use crate::media::audio::list_audio_devices;
use crate::nat::stun::test_stun;
use crate::sip::engine::spawn_sip_engine;
use crate::sip::{CallState, RegistrationState, SipCommand, SipEvent};
use chrono::Utc;
use slint::{Color, ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

slint::include_modules!();

struct UiState {
    config: AppConfig,
    history: Vec<HistoryEntry>,
    muted: bool,
    on_hold: bool,
    dtmf_sent: String,
    input_devices: Vec<String>,
    output_devices: Vec<String>,
}

pub fn run_ui() -> Result<(), slint::PlatformError> {
    let config = load_config();
    let history = load_history();
    let (cmd_tx, mut evt_rx) = spawn_sip_engine(config.clone());

    let (mut in_devs, mut out_devs) = list_audio_devices();
    in_devs.insert(0, "System Default".to_string());
    out_devs.insert(0, "System Default".to_string());

    let ui = AppWindow::new()?;
    let state = Rc::new(RefCell::new(UiState {
        config: config.clone(),
        history: history.clone(),
        muted: false,
        on_hold: false,
        dtmf_sent: String::new(),
        input_devices: in_devs.clone(),
        output_devices: out_devs.clone(),
    }));

    sync_history_to_ui(&ui, &history, "");
    sync_config_to_ui(&ui, &config, &in_devs, &out_devs);

    // Dial
    {
        let ui_weak = ui.as_weak();
        let st = state.clone();
        let tx = cmd_tx.clone();
        ui.on_dial(move |raw_uri| {
            let target = raw_uri.trim().to_string();
            if target.is_empty() {
                return;
            }
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_show_history(false);
                ui.set_status_banner("".into());
                let mut s = st.borrow_mut();
                s.dtmf_sent.clear();
                ui.set_dtmf_sent("".into());
                record_history(&mut s.history, &target, CallDirection::Outgoing, 0);
                sync_history_to_ui(&ui, &s.history, &ui.get_url_input());
            }
            let _ = tx.send(SipCommand::Dial(target));
        });
    }

    // Hangup / Answer / Reject / Mute / Hold / DTMF
    {
        let tx = cmd_tx.clone();
        ui.on_hangup(move || {
            let _ = tx.send(SipCommand::Hangup);
        });
    }
    {
        let tx = cmd_tx.clone();
        let st = state.clone();
        let ui_weak = ui.as_weak();
        ui.on_answer(move || {
            st.borrow_mut().dtmf_sent.clear();
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_dtmf_sent("".into());
            }
            let _ = tx.send(SipCommand::Answer);
        });
    }
    {
        let tx = cmd_tx.clone();
        ui.on_decline(move || {
            let _ = tx.send(SipCommand::Reject);
        });
    }
    {
        let tx = cmd_tx.clone();
        let st = state.clone();
        ui.on_toggle_mute(move || {
            let next = !st.borrow().muted;
            st.borrow_mut().muted = next;
            let _ = tx.send(SipCommand::ToggleMute(next));
        });
    }
    {
        let tx = cmd_tx.clone();
        let st = state.clone();
        ui.on_toggle_hold(move || {
            let next = !st.borrow().on_hold;
            st.borrow_mut().on_hold = next;
            let _ = tx.send(SipCommand::ToggleHold(next));
        });
    }
    {
        let tx = cmd_tx.clone();
        let st = state.clone();
        let ui_weak = ui.as_weak();
        ui.on_send_dtmf(move |s_val| {
            if let Some(ch) = s_val.chars().next() {
                let ch = ch.to_ascii_uppercase();
                let mut s = st.borrow_mut();
                s.dtmf_sent.push(ch);
                if s.dtmf_sent.len() > 24 {
                    let drain = s.dtmf_sent.len() - 24;
                    s.dtmf_sent.drain(..drain);
                }
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_dtmf_sent(s.dtmf_sent.clone().into());
                }
                let _ = tx.send(SipCommand::SendDtmf(ch));
            }
        });
    }

    // History
    {
        let ui_weak = ui.as_weak();
        let st = state.clone();
        ui.on_toggle_history(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let next = !ui.get_show_history();
                ui.set_show_history(next);
                if next {
                    let q = ui.get_url_input().to_string();
                    sync_history_to_ui(&ui, &st.borrow().history, &q);
                }
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_select_history(move |uri| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_url_input(uri);
                ui.set_show_history(false);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = state.clone();
        ui.on_clear_history(move || {
            let mut s = st.borrow_mut();
            s.history.clear();
            save_history(&s.history);
            if let Some(ui) = ui_weak.upgrade() {
                sync_history_to_ui(&ui, &s.history, "");
                ui.set_show_history(false);
            }
        });
    }

    // Settings navigation & actions
    {
        let ui_weak = ui.as_weak();
        let st = state.clone();
        ui.on_open_settings(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let (mut in_devs, mut out_devs) = list_audio_devices();
                in_devs.insert(0, "System Default".to_string());
                out_devs.insert(0, "System Default".to_string());
                let mut s = st.borrow_mut();
                s.input_devices = in_devs.clone();
                s.output_devices = out_devs.clone();
                sync_config_to_ui(&ui, &s.config, &in_devs, &out_devs);
                ui.set_show_settings(true);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_close_settings(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_show_settings(false);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = state.clone();
        let tx = cmd_tx.clone();
        ui.on_register_now(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let cfg = read_config_from_ui(&ui, &st.borrow());
                st.borrow_mut().config = cfg.clone();
                save_config(&cfg);
                let _ = tx.send(SipCommand::UpdateConfig(cfg));
                let _ = tx.send(SipCommand::Register);
            }
        });
    }
    {
        let tx = cmd_tx.clone();
        ui.on_unregister_now(move || {
            let _ = tx.send(SipCommand::Unregister);
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_test_stun(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_stun_test_status("Testing STUN...".into());
                let primary = ui.get_cfg_stun_server().to_string();
                let fallback = ui.get_cfg_stun_fallback().to_string();
                let ui_w = ui_weak.clone();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    let msg = match rt {
                        Ok(r) => r.block_on(async {
                            match test_stun(&primary, &fallback).await {
                                Ok(res) => format!(
                                    "Public IP: {} via {} ({} ms)",
                                    res.mapped_addr, res.server_used, res.rtt_ms
                                ),
                                Err(e) => format!("STUN failed: {e}"),
                            }
                        }),
                        Err(e) => format!("Runtime error: {e}"),
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_w.upgrade() {
                            ui.set_stun_test_status(msg.into());
                        }
                    });
                });
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = state.clone();
        let tx = cmd_tx.clone();
        ui.on_save_settings(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let cfg = read_config_from_ui(&ui, &st.borrow());
                st.borrow_mut().config = cfg.clone();
                save_config(&cfg);
                let _ = tx.send(SipCommand::UpdateConfig(cfg.clone()));
                if cfg.nat.stun_enabled {
                    let _ = tx.send(SipCommand::RefreshNat);
                }
                if cfg.registration.enabled {
                    let _ = tx.send(SipCommand::Register);
                }
                ui.set_show_settings(false);
            }
        });
    }

    // Event-driven SIP event bridge (zero polling!)
    {
        let ui_weak = ui.as_weak();
        std::thread::spawn(move || {
            while let Some(ev) = evt_rx.blocking_recv() {
                let ui_w = ui_weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_w.upgrade() {
                        apply_sip_event_to_ui(&ui, ev);
                    }
                });
            }
        });
    }

    // 1-second call timer (only updates UI when call_state == "active")
    let timer = Timer::default();
    let call_start_tracker: Rc<RefCell<Option<Instant>>> = Rc::new(RefCell::new(None));
    {
        let ui_weak = ui.as_weak();
        let tracker = call_start_tracker.clone();
        timer.start(TimerMode::Repeated, Duration::from_secs(1), move || {
            if let Some(ui) = ui_weak.upgrade() {
                if ui.get_call_state() == "active" {
                    let mut tr = tracker.borrow_mut();
                    let start = *tr.get_or_insert_with(Instant::now);
                    let secs = start.elapsed().as_secs();
                    ui.set_call_timer(format!("{:02}:{:02}", secs / 60, secs % 60).into());
                } else {
                    *tracker.borrow_mut() = None;
                }
            }
        });
    }

    ui.run()
}

fn apply_sip_event_to_ui(ui: &AppWindow, ev: SipEvent) {
    match ev {
        SipEvent::RegistrationChanged(st) => {
            let (label, color) = match st {
                RegistrationState::Disabled => (
                    "Direct SIP (No Reg)".to_string(),
                    Color::from_rgb_u8(0xa9, 0xb1, 0xd6),
                ),
                RegistrationState::Unregistered => (
                    "Unregistered".to_string(),
                    Color::from_rgb_u8(0xe0, 0xaf, 0x68),
                ),
                RegistrationState::Registering => (
                    "Registering...".to_string(),
                    Color::from_rgb_u8(0xe0, 0xaf, 0x68),
                ),
                RegistrationState::Registered(aor) => (
                    format!("Registered: {aor}"),
                    Color::from_rgb_u8(0x9e, 0xce, 0x6a),
                ),
                RegistrationState::Failed(err) => (
                    format!("Reg Failed: {err}"),
                    Color::from_rgb_u8(0xf7, 0x76, 0x8e),
                ),
            };
            ui.set_reg_label(label.into());
            ui.set_reg_color(color);
        }
        SipEvent::CallStateChanged(st) => match st {
            CallState::Idle => {
                ui.set_call_state("idle".into());
                ui.set_call_peer("".into());
                ui.set_call_timer("00:00".into());
                ui.set_call_muted(false);
                ui.set_call_hold(false);
            }
            CallState::Dialing { peer } => {
                ui.set_call_state("dialing".into());
                ui.set_call_peer(peer.into());
            }
            CallState::RingingOutgoing { peer } => {
                ui.set_call_state("ringing-out".into());
                ui.set_call_peer(peer.into());
            }
            CallState::RingingIncoming { peer, display } => {
                ui.set_call_state("ringing-in".into());
                ui.set_call_peer(peer.into());
                ui.set_call_display(display.into());
            }
            CallState::Active {
                peer,
                codec,
                muted,
                on_hold,
            } => {
                ui.set_call_state("active".into());
                ui.set_call_peer(peer.into());
                ui.set_call_codec(codec.to_string().into());
                ui.set_call_muted(muted);
                ui.set_call_hold(on_hold);
                ui.invoke_focus_dtmf();
            }
        },
        SipEvent::CallEnded {
            peer,
            duration_secs,
            reason,
        } => {
            ui.set_call_state("idle".into());
            ui.set_call_timer("00:00".into());
            let msg = if duration_secs > 0 {
                format!("Call ended with {peer} ({duration_secs}s, {reason})")
            } else {
                format!("Call ended with {peer} ({reason})")
            };
            ui.set_status_banner(msg.into());
        }
        SipEvent::NatDiscovered { public_addr, via } => {
            let label = if let Some(addr) = public_addr {
                format!("NAT: {} ({})", addr, via)
            } else {
                format!("NAT: {}", via)
            };
            ui.set_nat_label(label.into());
        }
        SipEvent::StatusMessage(msg) => {
            ui.set_status_banner(msg.into());
        }
    }
}

fn sync_history_to_ui(ui: &AppWindow, history: &[HistoryEntry], filter: &str) {
    let q = filter.trim().to_ascii_lowercase();
    let items: Vec<HistoryItem> = history
        .iter()
        .filter(|h| q.is_empty() || h.uri.to_ascii_lowercase().contains(&q))
        .take(8)
        .map(|h| HistoryItem {
            uri: h.uri.clone().into(),
            meta: if h.duration_secs > 0 {
                format!("{}s", h.duration_secs).into()
            } else {
                h.timestamp.format("%m-%d %H:%M").to_string().into()
            },
        })
        .collect();
    ui.set_history_items(ModelRc::new(VecModel::from(items)));
}

fn sync_config_to_ui(
    ui: &AppWindow,
    cfg: &AppConfig,
    in_devs: &[String],
    out_devs: &[String],
) {
    ui.set_cfg_reg_enabled(cfg.registration.enabled);
    ui.set_cfg_reg_domain(cfg.registration.domain.clone().into());
    ui.set_cfg_reg_registrar(cfg.registration.registrar.clone().into());
    ui.set_cfg_reg_username(cfg.registration.username.clone().into());
    ui.set_cfg_reg_auth_username(cfg.registration.auth_username.clone().into());
    ui.set_cfg_reg_password(cfg.registration.password.clone().into());
    ui.set_cfg_reg_display_name(cfg.registration.display_name.clone().into());
    ui.set_cfg_reg_outbound_proxy(cfg.registration.outbound_proxy.clone().into());

    ui.set_cfg_stun_enabled(cfg.nat.stun_enabled);
    ui.set_cfg_stun_server(cfg.nat.stun_server.clone().into());
    ui.set_cfg_stun_fallback(cfg.nat.stun_fallback.clone().into());
    ui.set_cfg_use_rport(cfg.nat.use_rport);
    ui.set_cfg_symmetric_rtp(cfg.nat.symmetric_rtp);
    ui.set_cfg_keepalive(cfg.nat.keepalive_secs.to_string().into());

    ui.set_cfg_g722(cfg.codecs.enable_g722);
    ui.set_cfg_pcmu(cfg.codecs.enable_pcmu);
    ui.set_cfg_pcma(cfg.codecs.enable_pcma);
    ui.set_cfg_g729(cfg.codecs.enable_g729);
    ui.set_cfg_dtmf_index(match cfg.codecs.dtmf_mode {
        DtmfMode::Rfc4733 => 0,
        DtmfMode::SipInfo => 1,
        DtmfMode::Both => 2,
    });

    let in_model: Vec<SharedString> = in_devs.iter().map(|s| s.clone().into()).collect();
    let out_model: Vec<SharedString> = out_devs.iter().map(|s| s.clone().into()).collect();
    ui.set_input_devices(ModelRc::new(VecModel::from(in_model)));
    ui.set_output_devices(ModelRc::new(VecModel::from(out_model)));

    let in_idx = cfg
        .audio
        .input_device
        .as_deref()
        .and_then(|name| in_devs.iter().position(|d| d == name))
        .unwrap_or(0) as i32;
    let out_idx = cfg
        .audio
        .output_device
        .as_deref()
        .and_then(|name| out_devs.iter().position(|d| d == name))
        .unwrap_or(0) as i32;
    ui.set_cfg_input_index(in_idx);
    ui.set_cfg_output_index(out_idx);
    ui.set_cfg_local_port(cfg.local_sip_port.to_string().into());
}

fn read_config_from_ui(ui: &AppWindow, st: &UiState) -> AppConfig {
    let mut cfg = st.config.clone();
    cfg.registration.enabled = ui.get_cfg_reg_enabled();
    cfg.registration.domain = ui.get_cfg_reg_domain().to_string();
    cfg.registration.registrar = ui.get_cfg_reg_registrar().to_string();
    cfg.registration.username = ui.get_cfg_reg_username().to_string();
    cfg.registration.auth_username = ui.get_cfg_reg_auth_username().to_string();
    cfg.registration.password = ui.get_cfg_reg_password().to_string();
    cfg.registration.display_name = ui.get_cfg_reg_display_name().to_string();
    cfg.registration.outbound_proxy = ui.get_cfg_reg_outbound_proxy().to_string();

    cfg.nat.stun_enabled = ui.get_cfg_stun_enabled();
    cfg.nat.stun_server = ui.get_cfg_stun_server().to_string();
    cfg.nat.stun_fallback = ui.get_cfg_stun_fallback().to_string();
    cfg.nat.use_rport = ui.get_cfg_use_rport();
    cfg.nat.symmetric_rtp = ui.get_cfg_symmetric_rtp();
    if let Ok(k) = ui.get_cfg_keepalive().trim().parse::<u64>() {
        cfg.nat.keepalive_secs = k;
    }

    cfg.codecs.enable_g722 = ui.get_cfg_g722();
    cfg.codecs.enable_pcmu = ui.get_cfg_pcmu();
    cfg.codecs.enable_pcma = ui.get_cfg_pcma();
    cfg.codecs.enable_g729 = ui.get_cfg_g729();
    cfg.codecs.dtmf_mode = match ui.get_cfg_dtmf_index() {
        1 => DtmfMode::SipInfo,
        2 => DtmfMode::Both,
        _ => DtmfMode::Rfc4733,
    };

    let in_idx = ui.get_cfg_input_index() as usize;
    cfg.audio.input_device = if in_idx == 0 {
        None
    } else {
        st.input_devices.get(in_idx).cloned()
    };

    let out_idx = ui.get_cfg_output_index() as usize;
    cfg.audio.output_device = if out_idx == 0 {
        None
    } else {
        st.output_devices.get(out_idx).cloned()
    };

    if let Ok(p) = ui.get_cfg_local_port().trim().parse::<u16>() {
        cfg.local_sip_port = p;
    }

    cfg
}

fn record_history(
    history: &mut Vec<HistoryEntry>,
    uri: &str,
    direction: CallDirection,
    duration_secs: u64,
) {
    history.retain(|h| h.uri != uri);
    history.insert(
        0,
        HistoryEntry {
            uri: uri.to_string(),
            direction,
            timestamp: Utc::now(),
            duration_secs,
        },
    );
    if history.len() > 25 {
        history.truncate(25);
    }
    save_history(history);
}
