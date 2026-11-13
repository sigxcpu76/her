use crate::config::{AppConfig, CodecKind, DtmfMode};
use crate::media::{prepare_media_socket, MediaSession, PreparedMedia};
use crate::nat::stun::{discover_local_ip, discover_mapped_addr, test_stun};
use crate::sip::auth::{build_digest_response, parse_digest_challenge};
use crate::sip::message::{
    extract_display_or_uri, extract_tag, extract_uri_from_header, get_all_headers, get_header,
    normalize_sip_uri, parse_cseq, parse_via_rport, random_token, sip_uri_host_port, SipMessage,
    SipRequest, SipResponse,
};
use crate::sip::sdp::{build_sdp, negotiate_codec, parse_sdp};
use crate::sip::{CallState, RegistrationState, SipCommand, SipEvent};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;
use tokio::net::{lookup_host, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};

struct ActiveDialog {
    call_id: String,
    local_tag: String,
    remote_tag: Option<String>,
    local_uri: String,
    remote_uri: String,
    remote_header: String,
    remote_target: SocketAddr,
    cseq: u32,
    route_set: Vec<String>,
    contact_uri: String,
    is_outgoing: bool,
    answered: bool,
    started_at: Option<Instant>,
    codec: CodecKind,
    muted: bool,
    on_hold: bool,
    // For outgoing INVITE auth retry or CANCEL
    last_invite_branch: String,
    auth_retried: bool,
    // For incoming INVITE before Answer/Reject
    incoming_invite: Option<SipRequest>,
    incoming_via: Vec<String>,
    prepared_media: Option<PreparedMedia>,
    media: Option<MediaSession>,
}

struct RegTransaction {
    call_id: String,
    from_tag: String,
    cseq: u32,
    registrar_uri: String,
    auth_retried: bool,
}

pub fn spawn_sip_engine(
    initial_cfg: AppConfig,
) -> (
    mpsc::UnboundedSender<SipCommand>,
    mpsc::UnboundedReceiver<SipEvent>,
) {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SipCommand>();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel::<SipEvent>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("create SIP tokio runtime");
        rt.block_on(async move {
        let mut cfg = initial_cfg;
        let bind_addr = format!("0.0.0.0:{}", cfg.local_sip_port);
        let sock = match UdpSocket::bind(&bind_addr).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                let _ = evt_tx.send(SipEvent::StatusMessage(format!(
                    "Failed to bind SIP port {}: {e}. Falling back to auto.",
                    cfg.local_sip_port
                )));
                Arc::new(UdpSocket::bind("0.0.0.0:0").await.expect("bind sip udp"))
            }
        };

        let local_port = sock.local_addr().map(|a| a.port()).unwrap_or(5060);
        let local_ip = discover_local_ip();
        let mut public_sip_addr: Option<SocketAddr> = None;

        // Initial STUN discovery on the SIP socket before starting the recv loop
        if cfg.nat.stun_enabled {
            tracing::info!("Running STUN discovery via {}...", cfg.nat.stun_server);
            match discover_mapped_addr(&sock, &cfg.nat.stun_server, &cfg.nat.stun_fallback).await {
                Ok(res) => {
                    tracing::info!("SIP STUN mapped {}:{} -> {} ({}ms)", local_ip, local_port, res.mapped_addr, res.rtt_ms);
                    public_sip_addr = Some(res.mapped_addr);
                    let _ = evt_tx.send(SipEvent::NatDiscovered {
                        public_addr: Some(res.mapped_addr),
                        via: format!("STUN ({})", res.server_used),
                    });
                }
                Err(e) => {
                    tracing::warn!("SIP STUN failed: {e}");
                    let _ = evt_tx.send(SipEvent::NatDiscovered {
                        public_addr: None,
                        via: format!("STUN failed: {e}"),
                    });
                }
            }
        } else {
            tracing::info!("STUN disabled, using local {}:{}", local_ip, local_port);
            let _ = evt_tx.send(SipEvent::NatDiscovered {
                public_addr: None,
                via: "STUN disabled".to_string(),
            });
        }

        if cfg.registration.enabled {
            let _ = evt_tx.send(SipEvent::RegistrationChanged(RegistrationState::Unregistered));
        } else {
            let _ = evt_tx.send(SipEvent::RegistrationChanged(RegistrationState::Disabled));
        }

        let mut reg_tx: Option<RegTransaction> = None;
        let mut reg_refresh_at: Option<Instant> = None;
        let mut reg_target_addr: Option<SocketAddr> = None;
        let mut dialog: Option<ActiveDialog> = None;

        // Auto-register on startup if enabled
        if cfg.registration.enabled && !cfg.registration.domain.trim().is_empty() {
            send_register(
                &sock,
                &cfg,
                local_ip,
                local_port,
                public_sip_addr,
                false,
                None,
                &mut reg_tx,
                &mut reg_target_addr,
                &evt_tx,
            )
            .await;
        }

        let mut buf = [0u8; 8192];
        let mut tick = interval(Duration::from_secs(1));
        let mut last_keepalive = Instant::now();

        loop {
            tokio::select! {
                Some(cmd) = cmd_rx.recv() => {
                    match cmd {
                        SipCommand::UpdateConfig(new_cfg) => {
                            let reg_was_enabled = cfg.registration.enabled;
                            cfg = new_cfg;
                            if !cfg.registration.enabled {
                                if reg_was_enabled {
                                    send_register(
                                        &sock, &cfg, local_ip, local_port, public_sip_addr,
                                        true, None, &mut reg_tx, &mut reg_target_addr, &evt_tx,
                                    ).await;
                                }
                                reg_refresh_at = None;
                                let _ = evt_tx.send(SipEvent::RegistrationChanged(RegistrationState::Disabled));
                            } else if !reg_was_enabled {
                                let _ = evt_tx.send(SipEvent::RegistrationChanged(RegistrationState::Unregistered));
                            }
                        }
                        SipCommand::Register => {
                            if cfg.registration.enabled {
                                send_register(
                                    &sock, &cfg, local_ip, local_port, public_sip_addr,
                                    false, None, &mut reg_tx, &mut reg_target_addr, &evt_tx,
                                ).await;
                            }
                        }
                        SipCommand::Unregister => {
                            reg_refresh_at = None;
                            send_register(
                                &sock, &cfg, local_ip, local_port, public_sip_addr,
                                true, None, &mut reg_tx, &mut reg_target_addr, &evt_tx,
                            ).await;
                            let _ = evt_tx.send(SipEvent::RegistrationChanged(RegistrationState::Unregistered));
                        }
                        SipCommand::RefreshNat => {
                            if cfg.nat.stun_enabled {
                                match test_stun(&cfg.nat.stun_server, &cfg.nat.stun_fallback).await {
                                    Ok(res) => {
                                        let _ = evt_tx.send(SipEvent::NatDiscovered {
                                            public_addr: Some(res.mapped_addr),
                                            via: format!("STUN {} ({}ms)", res.server_used, res.rtt_ms),
                                        });
                                    }
                                    Err(e) => {
                                        let _ = evt_tx.send(SipEvent::NatDiscovered {
                                            public_addr: public_sip_addr,
                                            via: format!("STUN error: {e}"),
                                        });
                                    }
                                }
                            }
                        }
                        SipCommand::Dial(raw_uri) => {
                            if dialog.is_some() {
                                let _ = evt_tx.send(SipEvent::StatusMessage("Already in a call".into()));
                                continue;
                            }
                            let default_dom = if cfg.registration.enabled && !cfg.registration.domain.trim().is_empty() {
                                Some(cfg.registration.domain.as_str())
                            } else {
                                None
                            };
                            let target_uri = normalize_sip_uri(&raw_uri, default_dom);
                            if target_uri.is_empty() {
                                continue;
                            }

                            let remote_addr = match resolve_sip_destination(&target_uri, &cfg).await {
                                Ok(a) => a,
                                Err(e) => {
                                    tracing::warn!("Dial resolve failed for {target_uri}: {e}");
                                    let _ = evt_tx.send(SipEvent::StatusMessage(format!("Dial failed: {e}")));
                                    continue;
                                }
                            };
                            tracing::info!("Dialing {target_uri} -> {remote_addr}");

                            let prepared = match prepare_media_socket(&cfg).await {
                                Ok(p) => p,
                                Err(e) => {
                                    let _ = evt_tx.send(SipEvent::StatusMessage(format!("Media error: {e}")));
                                    continue;
                                }
                            };

                            let call_id = format!("{}@{}", random_token(12), local_ip);
                            let local_tag = random_token(8);
                            let branch = format!("z9hG4bK{}", random_token(10));
                            let local_uri = build_local_uri(&cfg, local_ip, local_port, public_sip_addr);
                            let contact_uri = build_contact_uri(&cfg, local_ip, local_port, public_sip_addr);
                            let codecs = cfg.codecs.enabled_codecs();
                            let sdp = build_sdp(prepared.advertised_addr, &codecs, "sendrecv");
                            let adv_sip = advertised_sip_addr(local_ip, local_port, public_sip_addr);

                            let via = build_via_header(adv_sip, &branch, cfg.nat.use_rport);
                            let from_hdr = if !cfg.registration.display_name.trim().is_empty() {
                                format!("\"{}\" <{}>;tag={}", cfg.registration.display_name.trim(), local_uri, local_tag)
                            } else {
                                format!("<{}>;tag={}", local_uri, local_tag)
                            };

                            let req = SipRequest {
                                method: "INVITE".to_string(),
                                uri: target_uri.clone(),
                                headers: vec![
                                    ("Via".into(), via),
                                    ("Max-Forwards".into(), "70".into()),
                                    ("From".into(), from_hdr),
                                    ("To".into(), format!("<{}>", target_uri)),
                                    ("Call-ID".into(), call_id.clone()),
                                    ("CSeq".into(), "1 INVITE".into()),
                                    ("Contact".into(), format!("<{}>", contact_uri)),
                                    ("Allow".into(), "INVITE, ACK, CANCEL, BYE, OPTIONS, INFO".into()),
                                    ("User-Agent".into(), "HerSIP/0.1".into()),
                                    ("Content-Type".into(), "application/sdp".into()),
                                ],
                                body: sdp,
                            };

                            tracing::debug!("Sending INVITE:\n{}", String::from_utf8_lossy(&req.to_bytes()));
                            let _ = sock.send_to(&req.to_bytes(), remote_addr).await;
                            let _ = evt_tx.send(SipEvent::CallStateChanged(CallState::Dialing {
                                peer: target_uri.clone(),
                            }));

                            dialog = Some(ActiveDialog {
                                call_id,
                                local_tag,
                                remote_tag: None,
                                local_uri,
                                remote_uri: target_uri.clone(),
                                remote_header: format!("<{}>", target_uri),
                                remote_target: remote_addr,
                                cseq: 1,
                                route_set: Vec::new(),
                                contact_uri: target_uri,
                                is_outgoing: true,
                                answered: false,
                                started_at: None,
                                codec: codecs[0],
                                muted: false,
                                on_hold: false,
                                last_invite_branch: branch,
                                auth_retried: false,
                                incoming_invite: None,
                                incoming_via: Vec::new(),
                                prepared_media: Some(prepared),
                                media: None,
                            });
                        }
                        SipCommand::Answer => {
                            if let Some(ref mut dlg) = dialog {
                                if !dlg.is_outgoing && !dlg.answered {
                                    if let Some(inv) = dlg.incoming_invite.take() {
                                        let prepared = match prepare_media_socket(&cfg).await {
                                            Ok(p) => p,
                                            Err(e) => {
                                                let _ = evt_tx.send(SipEvent::StatusMessage(format!("Media error: {e}")));
                                                continue;
                                            }
                                        };
                                        let remote_sdp = match parse_sdp(&inv.body) {
                                            Some(s) => s,
                                            None => {
                                                let _ = evt_tx.send(SipEvent::StatusMessage("Invalid SDP in INVITE".into()));
                                                continue;
                                            }
                                        };
                                        let local_codecs = cfg.codecs.enabled_codecs();
                                        let chosen = negotiate_codec(&remote_sdp.codecs, &local_codecs);
                                        let sdp_answer = build_sdp(prepared.advertised_addr, &[chosen], "sendrecv");
                                        let contact = build_contact_uri(&cfg, local_ip, local_port, public_sip_addr);

                                        let mut headers = Vec::new();
                                        for v in &dlg.incoming_via {
                                            headers.push(("Via".into(), v.clone()));
                                        }
                                        if let Some(f) = get_header(&inv.headers, "From") {
                                            headers.push(("From".into(), f.to_string()));
                                        }
                                        headers.push(("To".into(), dlg.remote_header.clone()));
                                        headers.push(("Call-ID".into(), dlg.call_id.clone()));
                                        if let Some(cseq) = get_header(&inv.headers, "CSeq") {
                                            headers.push(("CSeq".into(), cseq.to_string()));
                                        }
                                        headers.push(("Contact".into(), format!("<{}>", contact)));
                                        headers.push(("Allow".into(), "INVITE, ACK, CANCEL, BYE, OPTIONS, INFO".into()));
                                        headers.push(("User-Agent".into(), "HerSIP/0.1".into()));
                                        headers.push(("Content-Type".into(), "application/sdp".into()));

                                        let ok_resp = SipResponse {
                                            status_code: 200,
                                            reason: "OK".to_string(),
                                            headers,
                                            body: sdp_answer,
                                        };
                                        let _ = sock.send_to(&ok_resp.to_bytes(), dlg.remote_target).await;

                                        match MediaSession::start(
                                            prepared,
                                            remote_sdp.remote_rtp,
                                            chosen,
                                            cfg.nat.symmetric_rtp,
                                            cfg.clone(),
                                        ) {
                                            Ok(ms) => {
                                                dlg.media = Some(ms);
                                                dlg.answered = true;
                                                dlg.codec = chosen;
                                                dlg.started_at = Some(Instant::now());
                                                let _ = evt_tx.send(SipEvent::CallStateChanged(CallState::Active {
                                                    peer: dlg.remote_uri.clone(),
                                                    codec: chosen,
                                                    muted: false,
                                                    on_hold: false,
                                                }));
                                            }
                                            Err(e) => {
                                                let _ = evt_tx.send(SipEvent::StatusMessage(format!("Audio error: {e}")));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        SipCommand::Reject => {
                            if let Some(mut dlg) = dialog.take() {
                                if let Some(inv) = dlg.incoming_invite.take() {
                                    let resp = build_simple_response(&inv, 486, "Busy Here", &dlg.remote_header);
                                    let _ = sock.send_to(&resp.to_bytes(), dlg.remote_target).await;
                                }
                                let _ = evt_tx.send(SipEvent::CallStateChanged(CallState::Idle));
                                let _ = evt_tx.send(SipEvent::CallEnded {
                                    peer: dlg.remote_uri,
                                    duration_secs: 0,
                                    reason: "Rejected".into(),
                                });
                            }
                        }
                        SipCommand::Hangup => {
                            if let Some(mut dlg) = dialog.take() {
                                let dur = dlg.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                                if let Some(m) = dlg.media.take() {
                                    m.stop();
                                }
                                let adv_sip = advertised_sip_addr(local_ip, local_port, public_sip_addr);
                                if dlg.answered {
                                    dlg.cseq += 1;
                                    let branch = format!("z9hG4bK{}", random_token(10));
                                    let bye = build_in_dialog_request(
                                        "BYE",
                                        &dlg,
                                        adv_sip,
                                        &branch,
                                        cfg.nat.use_rport,
                                        None,
                                        String::new(),
                                    );
                                    let _ = sock.send_to(&bye.to_bytes(), dlg.remote_target).await;
                                } else if dlg.is_outgoing {
                                    let cancel = SipRequest {
                                        method: "CANCEL".to_string(),
                                        uri: dlg.remote_uri.clone(),
                                        headers: vec![
                                            ("Via".into(), build_via_header(adv_sip, &dlg.last_invite_branch, cfg.nat.use_rport)),
                                            ("Max-Forwards".into(), "70".into()),
                                            ("From".into(), format!("<{}>;tag={}", dlg.local_uri, dlg.local_tag)),
                                            ("To".into(), dlg.remote_header.clone()),
                                            ("Call-ID".into(), dlg.call_id.clone()),
                                            ("CSeq".into(), format!("{} CANCEL", dlg.cseq)),
                                            ("User-Agent".into(), "HerSIP/0.1".into()),
                                        ],
                                        body: String::new(),
                                    };
                                    let _ = sock.send_to(&cancel.to_bytes(), dlg.remote_target).await;
                                } else if let Some(inv) = dlg.incoming_invite.take() {
                                    let resp = build_simple_response(&inv, 486, "Busy Here", &dlg.remote_header);
                                    let _ = sock.send_to(&resp.to_bytes(), dlg.remote_target).await;
                                }

                                let _ = evt_tx.send(SipEvent::CallStateChanged(CallState::Idle));
                                let _ = evt_tx.send(SipEvent::CallEnded {
                                    peer: dlg.remote_uri,
                                    duration_secs: dur,
                                    reason: "Hangup".into(),
                                });
                            }
                        }
                        SipCommand::ToggleMute(m) => {
                            if let Some(ref mut dlg) = dialog {
                                dlg.muted = m;
                                if let Some(ref media) = dlg.media {
                                    media.set_muted(m);
                                }
                                if dlg.answered {
                                    let _ = evt_tx.send(SipEvent::CallStateChanged(CallState::Active {
                                        peer: dlg.remote_uri.clone(),
                                        codec: dlg.codec,
                                        muted: dlg.muted,
                                        on_hold: dlg.on_hold,
                                    }));
                                }
                            }
                        }
                        SipCommand::ToggleHold(h) => {
                            if let Some(ref mut dlg) = dialog {
                                dlg.on_hold = h;
                                if let Some(ref media) = dlg.media {
                                    media.set_hold(h);
                                }
                                if dlg.answered {
                                    let _ = evt_tx.send(SipEvent::CallStateChanged(CallState::Active {
                                        peer: dlg.remote_uri.clone(),
                                        codec: dlg.codec,
                                        muted: dlg.muted,
                                        on_hold: dlg.on_hold,
                                    }));
                                }
                            }
                        }
                        SipCommand::SendDtmf(ch) => {
                            if let Some(ref mut dlg) = dialog {
                                if dlg.answered {
                                    if matches!(cfg.codecs.dtmf_mode, DtmfMode::Rfc4733 | DtmfMode::Both) {
                                        if let Some(ref media) = dlg.media {
                                            media.send_dtmf(ch);
                                        }
                                    }
                                    if matches!(cfg.codecs.dtmf_mode, DtmfMode::SipInfo | DtmfMode::Both) {
                                        dlg.cseq += 1;
                                        let adv_sip = advertised_sip_addr(local_ip, local_port, public_sip_addr);
                                        let branch = format!("z9hG4bK{}", random_token(10));
                                        let body = format!("Signal={}\r\nDuration=160\r\n", ch.to_ascii_uppercase());
                                        let info = build_in_dialog_request(
                                            "INFO",
                                            dlg,
                                            adv_sip,
                                            &branch,
                                            cfg.nat.use_rport,
                                            Some("application/dtmf-relay"),
                                            body,
                                        );
                                        let _ = sock.send_to(&info.to_bytes(), dlg.remote_target).await;
                                    }
                                }
                            }
                        }
                    }
                }

                recv_res = sock.recv_from(&mut buf) => {
                    if let Ok((len, src)) = recv_res {
                        let data = &buf[..len];
                        if data.iter().all(|&b| b == b'\r' || b == b'\n') {
                            continue; // NAT CRLF keepalive pong
                        }
                        tracing::debug!("Received SIP from {src}:\n{}", String::from_utf8_lossy(data));
                        if let Some(msg) = SipMessage::parse(data) {
                            match msg {
                                SipMessage::Response(resp) => {
                                    // Learn NAT mapped SIP address from Via rport/received (RFC 3581)
                                    if cfg.nat.use_rport {
                                        if let Some(via) = get_header(&resp.headers, "Via") {
                                            if let Some(rport_addr) = parse_via_rport(via) {
                                                if public_sip_addr != Some(rport_addr) {
                                                    public_sip_addr = Some(rport_addr);
                                                    let _ = evt_tx.send(SipEvent::NatDiscovered {
                                                        public_addr: Some(rport_addr),
                                                        via: "SIP rport".to_string(),
                                                    });
                                                }
                                            }
                                        }
                                    }

                                    let cseq_method = get_header(&resp.headers, "CSeq")
                                        .and_then(parse_cseq)
                                        .map(|(_, m)| m);

                                    if cseq_method.as_deref() == Some("REGISTER") {
                                        handle_register_response(
                                            &resp,
                                            &sock,
                                            &cfg,
                                            local_ip,
                                            local_port,
                                            public_sip_addr,
                                            &mut reg_tx,
                                            &mut reg_refresh_at,
                                            &mut reg_target_addr,
                                            &evt_tx,
                                        ).await;
                                    } else {
                                        handle_call_response(
                                            &resp,
                                            src,
                                            &sock,
                                            &cfg,
                                            local_ip,
                                            local_port,
                                            public_sip_addr,
                                            &mut dialog,
                                            &evt_tx,
                                        ).await;
                                    }
                                }
                                SipMessage::Request(req) => {
                                    handle_incoming_request(
                                        &req,
                                        src,
                                        &sock,
                                        &cfg,
                                        local_ip,
                                        local_port,
                                        public_sip_addr,
                                        &mut dialog,
                                        &evt_tx,
                                    ).await;
                                }
                            }
                        }
                    }
                }

                _ = tick.tick() => {
                    // Periodic REGISTER refresh
                    if cfg.registration.enabled {
                        if let Some(at) = reg_refresh_at {
                            if Instant::now() >= at {
                                reg_refresh_at = None;
                                send_register(
                                    &sock, &cfg, local_ip, local_port, public_sip_addr,
                                    false, None, &mut reg_tx, &mut reg_target_addr, &evt_tx,
                                ).await;
                            }
                        }
                    }

                    // Periodic NAT CRLF keepalive
                    if cfg.nat.keepalive_secs > 0
                        && last_keepalive.elapsed().as_secs() >= cfg.nat.keepalive_secs
                    {
                        last_keepalive = Instant::now();
                        if let Some(target) = reg_target_addr {
                            let _ = sock.send_to(b"\r\n\r\n", target).await;
                        }
                    }
                }
            }
        }
        });
    });

    (cmd_tx, evt_rx)
}

fn advertised_sip_addr(
    local_ip: IpAddr,
    local_port: u16,
    public_sip: Option<SocketAddr>,
) -> SocketAddr {
    public_sip.unwrap_or_else(|| SocketAddr::new(local_ip, local_port))
}

fn build_via_header(addr: SocketAddr, branch: &str, use_rport: bool) -> String {
    if use_rport {
        format!("SIP/2.0/UDP {};branch={};rport", addr, branch)
    } else {
        format!("SIP/2.0/UDP {};branch={}", addr, branch)
    }
}

fn build_local_uri(
    cfg: &AppConfig,
    local_ip: IpAddr,
    local_port: u16,
    public_sip: Option<SocketAddr>,
) -> String {
    if cfg.registration.enabled && !cfg.registration.username.trim().is_empty() && !cfg.registration.domain.trim().is_empty() {
        format!(
            "sip:{}@{}",
            cfg.registration.username.trim(),
            cfg.registration.domain.trim()
        )
    } else {
        let addr = advertised_sip_addr(local_ip, local_port, public_sip);
        let user = if cfg.registration.username.trim().is_empty() {
            "hersip"
        } else {
            cfg.registration.username.trim()
        };
        format!("sip:{}@{}", user, addr)
    }
}

fn build_contact_uri(
    cfg: &AppConfig,
    local_ip: IpAddr,
    local_port: u16,
    public_sip: Option<SocketAddr>,
) -> String {
    let addr = advertised_sip_addr(local_ip, local_port, public_sip);
    let user = if cfg.registration.username.trim().is_empty() {
        "hersip"
    } else {
        cfg.registration.username.trim()
    };
    format!("sip:{}@{}", user, addr)
}

async fn resolve_host_port(host: &str, port: u16) -> Result<SocketAddr, String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    let query = format!("{}:{}", host, port);
    let addrs = lookup_host(&query)
        .await
        .map_err(|e| format!("DNS lookup failed for {query}: {e}"))?;
    for a in addrs {
        if a.is_ipv4() {
            return Ok(a);
        }
    }
    Err(format!("No IPv4 address for {query}"))
}

async fn resolve_sip_destination(uri: &str, cfg: &AppConfig) -> Result<SocketAddr, String> {
    if !cfg.registration.outbound_proxy.trim().is_empty() {
        let (h, p) = sip_uri_host_port(&cfg.registration.outbound_proxy)
            .ok_or_else(|| "Invalid outbound proxy".to_string())?;
        return resolve_host_port(&h, p).await;
    }
    let (h, p) = sip_uri_host_port(uri).ok_or_else(|| "Invalid SIP URI".to_string())?;
    if cfg.registration.enabled && !cfg.registration.registrar.trim().is_empty() && h == cfg.registration.domain.trim() {
        let (rh, rp) = sip_uri_host_port(&cfg.registration.registrar)
            .ok_or_else(|| "Invalid registrar".to_string())?;
        return resolve_host_port(&rh, rp).await;
    }
    resolve_host_port(&h, p).await
}

#[allow(clippy::too_many_arguments)]
async fn send_register(
    sock: &UdpSocket,
    cfg: &AppConfig,
    local_ip: IpAddr,
    local_port: u16,
    public_sip: Option<SocketAddr>,
    unregister: bool,
    auth_header: Option<(String, String)>,
    reg_tx: &mut Option<RegTransaction>,
    reg_target_addr: &mut Option<SocketAddr>,
    evt_tx: &mpsc::UnboundedSender<SipEvent>,
) {
    let domain = cfg.registration.domain.trim();
    let user = cfg.registration.username.trim();
    if domain.is_empty() || user.is_empty() {
        let _ = evt_tx.send(SipEvent::RegistrationChanged(RegistrationState::Failed(
            "Domain and username required".into(),
        )));
        return;
    }

    let registrar_host = if cfg.registration.registrar.trim().is_empty() {
        domain
    } else {
        cfg.registration.registrar.trim()
    };
    let proxy_or_reg = if !cfg.registration.outbound_proxy.trim().is_empty() {
        cfg.registration.outbound_proxy.trim()
    } else {
        registrar_host
    };

    let (h, p) = match sip_uri_host_port(proxy_or_reg) {
        Some(hp) => hp,
        None => {
            let _ = evt_tx.send(SipEvent::RegistrationChanged(RegistrationState::Failed(
                "Invalid registrar address".into(),
            )));
            return;
        }
    };
    let target = match resolve_host_port(&h, p).await {
        Ok(a) => a,
        Err(e) => {
            let _ = evt_tx.send(SipEvent::RegistrationChanged(RegistrationState::Failed(e)));
            return;
        }
    };
    *reg_target_addr = Some(target);

    let (call_id, from_tag, cseq, auth_retried) = if let Some(prev) = reg_tx.take() {
        (prev.call_id, prev.from_tag, prev.cseq + 1, auth_header.is_some())
    } else {
        (format!("reg-{}@{}", random_token(10), local_ip), random_token(8), 1, false)
    };

    let registrar_uri = format!("sip:{}", domain);
    let aor = format!("sip:{}@{}", user, domain);
    let contact = build_contact_uri(cfg, local_ip, local_port, public_sip);
    let adv_sip = advertised_sip_addr(local_ip, local_port, public_sip);
    let branch = format!("z9hG4bK{}", random_token(10));
    let expires = if unregister { 0 } else { cfg.registration.expires_secs };

    let mut headers = vec![
        ("Via".into(), build_via_header(adv_sip, &branch, cfg.nat.use_rport)),
        ("Max-Forwards".into(), "70".into()),
        ("From".into(), format!("<{}>;tag={}", aor, from_tag)),
        ("To".into(), format!("<{}>", aor)),
        ("Call-ID".into(), call_id.clone()),
        ("CSeq".into(), format!("{} REGISTER", cseq)),
        ("Contact".into(), format!("<{}>", contact)),
        ("Expires".into(), expires.to_string()),
        ("User-Agent".into(), "HerSIP/0.1".into()),
    ];
    if let Some((hk, hv)) = auth_header {
        headers.push((hk, hv));
    }

    let req = SipRequest {
        method: "REGISTER".to_string(),
        uri: registrar_uri.clone(),
        headers,
        body: String::new(),
    };

    if !unregister {
        let _ = evt_tx.send(SipEvent::RegistrationChanged(RegistrationState::Registering));
    }
    let _ = sock.send_to(&req.to_bytes(), target).await;

    *reg_tx = Some(RegTransaction {
        call_id,
        from_tag,
        cseq,
        registrar_uri,
        auth_retried,
    });
}

#[allow(clippy::too_many_arguments)]
async fn handle_register_response(
    resp: &SipResponse,
    sock: &UdpSocket,
    cfg: &AppConfig,
    local_ip: IpAddr,
    local_port: u16,
    public_sip: Option<SocketAddr>,
    reg_tx: &mut Option<RegTransaction>,
    reg_refresh_at: &mut Option<Instant>,
    reg_target_addr: &mut Option<SocketAddr>,
    evt_tx: &mpsc::UnboundedSender<SipEvent>,
) {
    let call_id = get_header(&resp.headers, "Call-ID").unwrap_or("");
    let matches = reg_tx.as_ref().map(|t| t.call_id == call_id).unwrap_or(false);
    if !matches {
        return;
    }

    match resp.status_code {
        100..=199 => {}
        200..=299 => {
            if cfg.registration.enabled {
                let exp = get_header(&resp.headers, "Expires")
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .unwrap_or(cfg.registration.expires_secs as u64);
                if exp > 0 {
                    let refresh_secs = (exp * 4 / 5).max(30);
                    *reg_refresh_at = Some(Instant::now() + Duration::from_secs(refresh_secs));
                    let aor = format!(
                        "{}@{}",
                        cfg.registration.username.trim(),
                        cfg.registration.domain.trim()
                    );
                    let _ = evt_tx.send(SipEvent::RegistrationChanged(
                        RegistrationState::Registered(aor),
                    ));
                }
            }
        }
        401 | 407 => {
            let already_retried = reg_tx.as_ref().map(|t| t.auth_retried).unwrap_or(true);
            if already_retried {
                let _ = evt_tx.send(SipEvent::RegistrationChanged(RegistrationState::Failed(
                    format!("Auth failed ({})", resp.status_code),
                )));
                return;
            }
            let (hdr_name, out_hdr) = if resp.status_code == 401 {
                ("WWW-Authenticate", "Authorization")
            } else {
                ("Proxy-Authenticate", "Proxy-Authorization")
            };
            if let Some(chal_str) = get_header(&resp.headers, hdr_name) {
                if let Some(chal) = parse_digest_challenge(chal_str) {
                    let auth_user = if cfg.registration.auth_username.trim().is_empty() {
                        cfg.registration.username.trim()
                    } else {
                        cfg.registration.auth_username.trim()
                    };
                    let uri = reg_tx
                        .as_ref()
                        .map(|t| t.registrar_uri.clone())
                        .unwrap_or_default();
                    let auth_val = build_digest_response(
                        &chal,
                        auth_user,
                        &cfg.registration.password,
                        "REGISTER",
                        &uri,
                        1,
                    );
                    send_register(
                        sock,
                        cfg,
                        local_ip,
                        local_port,
                        public_sip,
                        false,
                        Some((out_hdr.to_string(), auth_val)),
                        reg_tx,
                        reg_target_addr,
                        evt_tx,
                    )
                    .await;
                    return;
                }
            }
            let _ = evt_tx.send(SipEvent::RegistrationChanged(RegistrationState::Failed(
                format!("{} {}", resp.status_code, resp.reason),
            )));
        }
        code => {
            let _ = evt_tx.send(SipEvent::RegistrationChanged(RegistrationState::Failed(
                format!("{} {}", code, resp.reason),
            )));
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_call_response(
    resp: &SipResponse,
    _src: SocketAddr,
    sock: &UdpSocket,
    cfg: &AppConfig,
    local_ip: IpAddr,
    local_port: u16,
    public_sip: Option<SocketAddr>,
    dialog: &mut Option<ActiveDialog>,
    evt_tx: &mpsc::UnboundedSender<SipEvent>,
) {
    let call_id = get_header(&resp.headers, "Call-ID").unwrap_or("");
    let cseq_method = get_header(&resp.headers, "CSeq")
        .and_then(parse_cseq)
        .map(|(_, m)| m)
        .unwrap_or_default();

    let matches = dialog.as_ref().map(|d| d.call_id == call_id).unwrap_or(false);
    if !matches {
        return;
    }
    if cseq_method != "INVITE" {
        return;
    }

    let adv_sip = advertised_sip_addr(local_ip, local_port, public_sip);

    match resp.status_code {
        100 => {}
        180 | 183 => {
            if let Some(ref mut dlg) = dialog {
                if let Some(to_hdr) = get_header(&resp.headers, "To") {
                    dlg.remote_header = to_hdr.to_string();
                    dlg.remote_tag = extract_tag(to_hdr);
                }
                let _ = evt_tx.send(SipEvent::CallStateChanged(CallState::RingingOutgoing {
                    peer: dlg.remote_uri.clone(),
                }));
            }
        }
        200..=299 => {
            if let Some(ref mut dlg) = dialog {
                if let Some(to_hdr) = get_header(&resp.headers, "To") {
                    dlg.remote_header = to_hdr.to_string();
                    dlg.remote_tag = extract_tag(to_hdr);
                }
                if let Some(contact_hdr) = get_header(&resp.headers, "Contact") {
                    dlg.contact_uri = extract_uri_from_header(contact_hdr);
                }
                dlg.route_set = get_all_headers(&resp.headers, "Record-Route")
                    .into_iter()
                    .rev()
                    .map(|s| s.to_string())
                    .collect();

                // Send ACK
                let branch = format!("z9hG4bK{}", random_token(10));
                let ack = build_in_dialog_request(
                    "ACK",
                    dlg,
                    adv_sip,
                    &branch,
                    cfg.nat.use_rport,
                    None,
                    String::new(),
                );
                let _ = sock.send_to(&ack.to_bytes(), dlg.remote_target).await;

                if !dlg.answered {
                    if let (Some(prepared), Some(remote_sdp)) =
                        (dlg.prepared_media.take(), parse_sdp(&resp.body))
                    {
                        let chosen = negotiate_codec(&remote_sdp.codecs, &cfg.codecs.enabled_codecs());
                        match MediaSession::start(
                            prepared,
                            remote_sdp.remote_rtp,
                            chosen,
                            cfg.nat.symmetric_rtp,
                            cfg.clone(),
                        ) {
                            Ok(ms) => {
                                dlg.media = Some(ms);
                                dlg.answered = true;
                                dlg.codec = chosen;
                                dlg.started_at = Some(Instant::now());
                                let _ = evt_tx.send(SipEvent::CallStateChanged(CallState::Active {
                                    peer: dlg.remote_uri.clone(),
                                    codec: chosen,
                                    muted: false,
                                    on_hold: false,
                                }));
                            }
                            Err(e) => {
                                tracing::error!("Audio error on call start: {e}");
                                let _ = evt_tx.send(SipEvent::StatusMessage(format!("Audio error: {e}")));
                                if let Some(mut failed_dlg) = dialog.take() {
                                    failed_dlg.cseq += 1;
                                    let branch = format!("z9hG4bK{}", random_token(10));
                                    let bye = build_in_dialog_request(
                                        "BYE",
                                        &failed_dlg,
                                        adv_sip,
                                        &branch,
                                        cfg.nat.use_rport,
                                        None,
                                        String::new(),
                                    );
                                    let _ = sock.send_to(&bye.to_bytes(), failed_dlg.remote_target).await;
                                    let _ = evt_tx.send(SipEvent::CallStateChanged(CallState::Idle));
                                }
                            }
                        }
                    }
                }
            }
        }
        401 | 407 => {
            let can_retry = dialog.as_ref().map(|d| !d.auth_retried).unwrap_or(false);
            if can_retry && !cfg.registration.username.trim().is_empty() {
                if let Some(ref mut dlg) = dialog {
                    dlg.auth_retried = true;
                    // Send ACK for the 401/407 first
                    let to_hdr = get_header(&resp.headers, "To").unwrap_or(&dlg.remote_header);
                    let ack = SipRequest {
                        method: "ACK".to_string(),
                        uri: dlg.remote_uri.clone(),
                        headers: vec![
                            ("Via".into(), build_via_header(adv_sip, &dlg.last_invite_branch, cfg.nat.use_rport)),
                            ("Max-Forwards".into(), "70".into()),
                            ("From".into(), format!("<{}>;tag={}", dlg.local_uri, dlg.local_tag)),
                            ("To".into(), to_hdr.to_string()),
                            ("Call-ID".into(), dlg.call_id.clone()),
                            ("CSeq".into(), format!("{} ACK", dlg.cseq)),
                        ],
                        body: String::new(),
                    };
                    let _ = sock.send_to(&ack.to_bytes(), dlg.remote_target).await;

                    let (hdr_name, out_hdr) = if resp.status_code == 401 {
                        ("WWW-Authenticate", "Authorization")
                    } else {
                        ("Proxy-Authenticate", "Proxy-Authorization")
                    };
                    if let Some(chal_str) = get_header(&resp.headers, hdr_name) {
                        if let Some(chal) = parse_digest_challenge(chal_str) {
                            let auth_user = if cfg.registration.auth_username.trim().is_empty() {
                                cfg.registration.username.trim()
                            } else {
                                cfg.registration.auth_username.trim()
                            };
                            let auth_val = build_digest_response(
                                &chal,
                                auth_user,
                                &cfg.registration.password,
                                "INVITE",
                                &dlg.remote_uri,
                                1,
                            );
                            dlg.cseq += 1;
                            let new_branch = format!("z9hG4bK{}", random_token(10));
                            dlg.last_invite_branch = new_branch.clone();
                            let contact = build_contact_uri(cfg, local_ip, local_port, public_sip);
                            let codecs = cfg.codecs.enabled_codecs();
                            let sdp = if let Some(ref p) = dlg.prepared_media {
                                build_sdp(p.advertised_addr, &codecs, "sendrecv")
                            } else {
                                String::new()
                            };
                            let inv = SipRequest {
                                method: "INVITE".to_string(),
                                uri: dlg.remote_uri.clone(),
                                headers: vec![
                                    ("Via".into(), build_via_header(adv_sip, &new_branch, cfg.nat.use_rport)),
                                    ("Max-Forwards".into(), "70".into()),
                                    ("From".into(), format!("<{}>;tag={}", dlg.local_uri, dlg.local_tag)),
                                    ("To".into(), format!("<{}>", dlg.remote_uri)),
                                    ("Call-ID".into(), dlg.call_id.clone()),
                                    ("CSeq".into(), format!("{} INVITE", dlg.cseq)),
                                    ("Contact".into(), format!("<{}>", contact)),
                                    (out_hdr.into(), auth_val),
                                    ("User-Agent".into(), "HerSIP/0.1".into()),
                                    ("Content-Type".into(), "application/sdp".into()),
                                ],
                                body: sdp,
                            };
                            let _ = sock.send_to(&inv.to_bytes(), dlg.remote_target).await;
                            return;
                        }
                    }
                }
            }
            fail_outgoing_call(resp, sock, adv_sip, dialog, evt_tx).await;
        }
        _ => {
            fail_outgoing_call(resp, sock, adv_sip, dialog, evt_tx).await;
        }
    }
}

async fn fail_outgoing_call(
    resp: &SipResponse,
    sock: &UdpSocket,
    adv_sip: SocketAddr,
    dialog: &mut Option<ActiveDialog>,
    evt_tx: &mpsc::UnboundedSender<SipEvent>,
) {
    if let Some(mut dlg) = dialog.take() {
        if let Some(m) = dlg.media.take() {
            m.stop();
        }
        let to_hdr = get_header(&resp.headers, "To").unwrap_or(&dlg.remote_header);
        let ack = SipRequest {
            method: "ACK".to_string(),
            uri: dlg.remote_uri.clone(),
            headers: vec![
                ("Via".into(), build_via_header(adv_sip, &dlg.last_invite_branch, true)),
                ("Max-Forwards".into(), "70".into()),
                ("From".into(), format!("<{}>;tag={}", dlg.local_uri, dlg.local_tag)),
                ("To".into(), to_hdr.to_string()),
                ("Call-ID".into(), dlg.call_id.clone()),
                ("CSeq".into(), format!("{} ACK", dlg.cseq)),
            ],
            body: String::new(),
        };
        let _ = sock.send_to(&ack.to_bytes(), dlg.remote_target).await;
        let _ = evt_tx.send(SipEvent::CallStateChanged(CallState::Idle));
        let _ = evt_tx.send(SipEvent::CallEnded {
            peer: dlg.remote_uri,
            duration_secs: 0,
            reason: format!("{} {}", resp.status_code, resp.reason),
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_incoming_request(
    req: &SipRequest,
    src: SocketAddr,
    sock: &UdpSocket,
    cfg: &AppConfig,
    local_ip: IpAddr,
    local_port: u16,
    public_sip: Option<SocketAddr>,
    dialog: &mut Option<ActiveDialog>,
    evt_tx: &mpsc::UnboundedSender<SipEvent>,
) {
    let call_id = get_header(&req.headers, "Call-ID").unwrap_or("").to_string();

    match req.method.as_str() {
        "OPTIONS" => {
            let to_hdr = get_header(&req.headers, "To")
                .map(|t| format!("{};tag={}", t, random_token(6)))
                .unwrap_or_default();
            let resp = build_simple_response(req, 200, "OK", &to_hdr);
            let _ = sock.send_to(&resp.to_bytes(), src).await;
        }
        "INVITE" => {
            if dialog.is_some() {
                let to_hdr = get_header(&req.headers, "To")
                    .map(|t| format!("{};tag={}", t, random_token(6)))
                    .unwrap_or_default();
                let resp = build_simple_response(req, 486, "Busy Here", &to_hdr);
                let _ = sock.send_to(&resp.to_bytes(), src).await;
                return;
            }

            let local_tag = random_token(8);
            let from_hdr = get_header(&req.headers, "From").unwrap_or("");
            let peer_uri = extract_uri_from_header(from_hdr);
            let display = extract_display_or_uri(from_hdr);
            let to_base = get_header(&req.headers, "To").unwrap_or("");
            let to_with_tag = if extract_tag(to_base).is_some() {
                to_base.to_string()
            } else {
                format!("{};tag={}", to_base, local_tag)
            };

            let trying = build_simple_response(req, 100, "Trying", to_base);
            let _ = sock.send_to(&trying.to_bytes(), src).await;

            let ringing = build_simple_response(req, 180, "Ringing", &to_with_tag);
            let _ = sock.send_to(&ringing.to_bytes(), src).await;

            let vias: Vec<String> = get_all_headers(&req.headers, "Via")
                .into_iter()
                .map(|s| s.to_string())
                .collect();
            let contact_uri = get_header(&req.headers, "Contact")
                .map(extract_uri_from_header)
                .unwrap_or_else(|| peer_uri.clone());
            let route_set: Vec<String> = get_all_headers(&req.headers, "Record-Route")
                .into_iter()
                .map(|s| s.to_string())
                .collect();

            let _ = evt_tx.send(SipEvent::CallStateChanged(CallState::RingingIncoming {
                peer: peer_uri.clone(),
                display,
            }));

            *dialog = Some(ActiveDialog {
                call_id,
                local_tag,
                remote_tag: extract_tag(from_hdr),
                local_uri: build_local_uri(cfg, local_ip, local_port, public_sip),
                remote_uri: peer_uri,
                remote_header: to_with_tag,
                remote_target: src,
                cseq: 1,
                route_set,
                contact_uri,
                is_outgoing: false,
                answered: false,
                started_at: None,
                codec: CodecKind::Pcmu,
                muted: false,
                on_hold: false,
                last_invite_branch: String::new(),
                auth_retried: false,
                incoming_invite: Some(req.clone()),
                incoming_via: vias,
                prepared_media: None,
                media: None,
            });
        }
        "ACK" => {}
        "CANCEL" => {
            let matches = dialog.as_ref().map(|d| d.call_id == call_id).unwrap_or(false);
            if matches {
                if let Some(mut dlg) = dialog.take() {
                    let ok = build_simple_response(req, 200, "OK", &dlg.remote_header);
                    let _ = sock.send_to(&ok.to_bytes(), src).await;
                    if let Some(inv) = dlg.incoming_invite.take() {
                        let term = build_simple_response(&inv, 487, "Request Terminated", &dlg.remote_header);
                        let _ = sock.send_to(&term.to_bytes(), src).await;
                    }
                    let _ = evt_tx.send(SipEvent::CallStateChanged(CallState::Idle));
                    let _ = evt_tx.send(SipEvent::CallEnded {
                        peer: dlg.remote_uri,
                        duration_secs: 0,
                        reason: "Cancelled".into(),
                    });
                }
            }
        }
        "BYE" => {
            let matches = dialog.as_ref().map(|d| d.call_id == call_id).unwrap_or(false);
            if matches {
                if let Some(mut dlg) = dialog.take() {
                    let to_hdr = get_header(&req.headers, "To").unwrap_or(&dlg.remote_header);
                    let ok = build_simple_response(req, 200, "OK", to_hdr);
                    let _ = sock.send_to(&ok.to_bytes(), src).await;
                    let dur = dlg.started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                    if let Some(m) = dlg.media.take() {
                        m.stop();
                    }
                    let _ = evt_tx.send(SipEvent::CallStateChanged(CallState::Idle));
                    let _ = evt_tx.send(SipEvent::CallEnded {
                        peer: dlg.remote_uri,
                        duration_secs: dur,
                        reason: "Remote hangup".into(),
                    });
                }
            }
        }
        "INFO" => {
            let to_hdr = get_header(&req.headers, "To").unwrap_or("");
            let ok = build_simple_response(req, 200, "OK", to_hdr);
            let _ = sock.send_to(&ok.to_bytes(), src).await;
        }
        _ => {
            let to_hdr = get_header(&req.headers, "To").unwrap_or("");
            let resp = build_simple_response(req, 405, "Method Not Allowed", to_hdr);
            let _ = sock.send_to(&resp.to_bytes(), src).await;
        }
    }
}

fn build_simple_response(req: &SipRequest, code: u16, reason: &str, to_hdr: &str) -> SipResponse {
    let mut headers = Vec::new();
    for v in get_all_headers(&req.headers, "Via") {
        headers.push(("Via".into(), v.to_string()));
    }
    if let Some(f) = get_header(&req.headers, "From") {
        headers.push(("From".into(), f.to_string()));
    }
    if !to_hdr.is_empty() {
        headers.push(("To".into(), to_hdr.to_string()));
    }
    if let Some(c) = get_header(&req.headers, "Call-ID") {
        headers.push(("Call-ID".into(), c.to_string()));
    }
    if let Some(cs) = get_header(&req.headers, "CSeq") {
        headers.push(("CSeq".into(), cs.to_string()));
    }
    headers.push(("User-Agent".into(), "HerSIP/0.1".into()));

    SipResponse {
        status_code: code,
        reason: reason.to_string(),
        headers,
        body: String::new(),
    }
}

fn build_in_dialog_request(
    method: &str,
    dlg: &ActiveDialog,
    adv_sip: SocketAddr,
    branch: &str,
    use_rport: bool,
    content_type: Option<&str>,
    body: String,
) -> SipRequest {
    let mut headers = vec![
        ("Via".into(), build_via_header(adv_sip, branch, use_rport)),
        ("Max-Forwards".into(), "70".into()),
        ("From".into(), format!("<{}>;tag={}", dlg.local_uri, dlg.local_tag)),
        ("To".into(), dlg.remote_header.clone()),
        ("Call-ID".into(), dlg.call_id.clone()),
        ("CSeq".into(), format!("{} {}", dlg.cseq, method)),
        ("User-Agent".into(), "HerSIP/0.1".into()),
    ];
    for r in &dlg.route_set {
        headers.push(("Route".into(), r.clone()));
    }
    if let Some(ct) = content_type {
        headers.push(("Content-Type".into(), ct.to_string()));
    }

    SipRequest {
        method: method.to_string(),
        uri: dlg.contact_uri.clone(),
        headers,
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn test_dial_dev_webphone() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("her_sip=debug")
            .try_init();
        let cfg = AppConfig::default();
        let (cmd_tx, mut evt_rx) = spawn_sip_engine(cfg);
        tokio::time::sleep(Duration::from_millis(800)).await;
        let _ = cmd_tx.send(SipCommand::Dial("sip:1000@dev-webphone.tcnp3.com".to_string()));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(200), evt_rx.recv()).await {
                println!("EVENT: {:?}", ev);
            }
        }
        let _ = cmd_tx.send(SipCommand::Hangup);
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}
