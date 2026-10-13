//! Beat listener — UDP 50001.
//!
//! Handles:
//! - Beat packets (type 0x28): fired on every beat by CDJs playing an
//!   analysed track, and periodically by the mixer.
//! - Absolute-position packets (type 0x0b): CDJ-3000 / XDJ-XZ only, every
//!   ~30 ms; preferred for phase tracking.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::broadcast;

use super::{
    packets::{parse_abs_position, parse_beat, AbsPositionPacket, BeatPacket},
    PORT_BEAT,
};

use crate::state::timing::LogThrottle;

// ── Beat events ───────────────────────────────────────────────────────────────

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum BeatEvent {
    /// Pro DJ Link beat packet (from hardware CDJ/XDJ on Ethernet).
    Beat {
        packet: BeatPacket,
        received_at: Instant,
    },
    /// Pro DJ Link absolute-position packet (CDJ-3000 only, every ~30ms).
    AbsPosition {
        packet: AbsPositionPacket,
        received_at: Instant,
    },
    /// Ableton Link beat crossing (from rekordbox or other Link peers).
    LinkBeat {
        bpm: f64,
        beat_in_bar: u8,
        bar_phase: f64,
        beat_phase: f64,
        received_at: Instant,
    },
}

// ── Listener task ─────────────────────────────────────────────────────────────

pub async fn run(_bind_ip: Ipv4Addr, tx: broadcast::Sender<BeatEvent>) -> anyhow::Result<()> {
    let raw = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    raw.set_reuse_address(true)?;
    #[cfg(not(windows))]
    raw.set_reuse_port(true)?;
    raw.set_nonblocking(true)?;
    raw.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT_BEAT).into())?;
    let sock = UdpSocket::from_std(std::net::UdpSocket::from(raw))?;

    tracing::info!(port = PORT_BEAT, "Beat listener started");

    let mut buf = [0u8; 2048];
    let mut abspos_trace = LogThrottle::default();
    let mut unknown_trace = LogThrottle::default();
    loop {
        match sock.recv_from(&mut buf).await {
            Ok((len, _src)) => {
                let received_at = Instant::now();
                let data = &buf[..len];
                if let Some(bp) = parse_beat(data) {
                    tracing::trace!(
                        target: "prolink.beat_listener",
                        device = bp.device_number,
                        bpm = %format!("{:.2}", bp.effective_bpm),
                        beat_in_bar = bp.beat_in_bar,
                        next_beat_ms = bp.next_beat_ms,
                        "ProLink BeatPacket"
                    );
                    let _ = tx.send(BeatEvent::Beat {
                        packet: bp,
                        received_at,
                    });
                } else if let Some(ap) = parse_abs_position(data) {
                    if abspos_trace.should_log(received_at, Duration::from_secs(1)) {
                        tracing::trace!(
                            target: "prolink.beat_listener",
                            device = ap.device_number,
                            bpm = %format!("{:.2}", ap.effective_bpm),
                            playhead_ms = ap.playhead_ms,
                            "ProLink AbsPositionPacket"
                        );
                    }
                    let _ = tx.send(BeatEvent::AbsPosition {
                        packet: ap,
                        received_at,
                    });
                } else if unknown_trace.should_log(received_at, Duration::from_secs(1)) {
                    tracing::trace!(
                        target: "prolink.beat_listener",
                        len,
                        hex = %super::hex_preview(data, 32),
                        "Unknown ProLink beat/position packet"
                    );
                }
            }
            Err(e) => {
                tracing::warn!("Beat listener recv error: {e}");
                tokio::time::sleep(super::RECV_ERROR_BACKOFF).await;
            }
        }
    }
}
