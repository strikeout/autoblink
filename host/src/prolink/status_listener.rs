//! Status listener — UDP 50002.
//!
//! Receives detailed CDJ status and mixer status packets.  Real hardware only
//! sends these to virtual CDJs that are broadcasting keep-alive packets first.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::broadcast;

use super::{
    packets::{cdj_state_word, parse_cdj_status, parse_mixer_status, CdjStatus, MixerStatus},
    PORT_STATUS,
};

// ── Status events ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum StatusEvent {
    Cdj(CdjStatus),
    Mixer(MixerStatus),
}

// ── Listener task ─────────────────────────────────────────────────────────────

pub async fn run(
    bind_ip: Ipv4Addr,
    our_device_number: u8,
    tx: broadcast::Sender<StatusEvent>,
) -> anyhow::Result<()> {
    let raw = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    raw.set_reuse_address(true)?;
    #[cfg(not(windows))]
    raw.set_reuse_port(true)?;
    raw.set_nonblocking(true)?;
    raw.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT_STATUS).into())?;
    let sock = UdpSocket::from_std(std::net::UdpSocket::from(raw))?;
    tracing::info!(port = PORT_STATUS, bind = %bind_ip, "Status listener started");

    let mut buf = [0u8; 4096];
    let mut diag_counts: HashMap<u8, u8> = HashMap::new();
    loop {
        match sock.recv_from(&mut buf).await {
            Ok((len, src)) => {
                let data = &buf[..len];
                tracing::trace!(
                    src = %src,
                    len,
                    hex = %super::hex_preview(data, 48),
                    "Status packet received"
                );
                if let Some(s) = parse_cdj_status(data) {
                    if s.device_number == our_device_number {
                        continue;
                    }
                    let cnt = diag_counts.entry(s.device_number).or_insert(0);
                    if *cnt < 3 || s.is_master {
                        *cnt = cnt.saturating_add(1);
                        tracing::debug!(
                            device = s.device_number,
                            src = %src,
                            pkt_len = len,
                            state_raw = cdj_state_word(data)
                                .map(|state| format!("0x{state:04x}"))
                                .unwrap_or_else(|| format!("unavailable(len={len})")),
                            is_master = s.is_master,
                            is_playing = s.is_playing_flag,
                            play_state = ?s.play_state,
                            bpm = format!("{:.2}", s.effective_bpm).as_str(),
                            rekordbox_id = s.rekordbox_id,
                            track_slot = s.track_slot,
                            track_type = s.track_type,
                            track_source = s.track_source_player,
                            loop_ms = ?s.loop_ms,
                            // The raw loop bytes. The XDJ-AZ offsets are not documented.
                            loop_raw = data
                                .get(0x1b6..0x1ca)
                                .map(|b| b.iter().map(|x| format!("{x:02x}")).collect::<String>())
                                .unwrap_or_default(),
                            "CDJ status"
                        );
                    }
                    let _ = tx.send(StatusEvent::Cdj(s));
                } else if let Some(s) = parse_mixer_status(data) {
                    let cnt = diag_counts.entry(s.device_number).or_insert(0);
                    if *cnt < 3 {
                        *cnt = cnt.saturating_add(1);
                        tracing::debug!(
                            device = s.device_number,
                            src = %src,
                            is_master = s.is_master,
                            "Mixer status"
                        );
                    }
                    let _ = tx.send(StatusEvent::Mixer(s));
                }
            }
            Err(e) => {
                tracing::warn!("Status listener recv error: {e}");
                tokio::time::sleep(super::RECV_ERROR_BACKOFF).await;
            }
        }
    }
}
