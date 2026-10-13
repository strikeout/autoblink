//! Ableton Link engine — polls the Link session timeline and pushes beat/tempo/phase
//! updates into SharedState so the MIDI clock and mapper tasks can consume them.

use std::time::{Duration, Instant};

use rusty_link::{AblLink, SessionState};
use tokio::sync::broadcast;
use tokio::sync::watch;

use crate::config::{LinkConfig, Source};
use crate::prolink::beat_listener::BeatEvent;
use crate::state::SharedState;

/// Return true if the beat floor moved forward, then store the new floor.
/// The floor advances on every change, also while the transport is stopped.
fn advance_beat_floor(last_beat_floor: &mut i64, beat_floor: i64) -> bool {
    let crossed = *last_beat_floor >= 0 && beat_floor > *last_beat_floor;
    *last_beat_floor = beat_floor;
    crossed
}

/// Run the Ableton Link polling engine.
pub async fn run(
    link_cfg: LinkConfig,
    state: SharedState,
    beat_tx: broadcast::Sender<BeatEvent>,
    timing_tx: watch::Sender<()>,
    mut link: AblLink,
) {
    tracing::info!("Ableton Link engine entering run loop...");
    if !link_cfg.enabled {
        tracing::info!("Ableton Link engine disabled by config");
        return;
    }

    // ── Initialise the Link instance ─────────────────────────────────────────
    tracing::info!("AblLink received, enabling...");
    link.enable(true);
    link.enable_start_stop_sync(true);
    tracing::info!("AblLink enabled (start/stop sync: true)");

    // Do not use the rusty_link callback setters. In rusty_link 0.2.3 they give
    // Link a pointer to a closure on their own stack frame, and Link calls it
    // after the closure is gone. That crashed the app on a peer join or leave.
    // The loop polls the peer count instead.
    let mut last_peers: Option<u64> = None;

    tracing::info!(
        quantum = link_cfg.quantum,
        poll_us = link_cfg.poll_interval_us,
        "Ableton Link engine started (waiting for peers…)"
    );

    let mut session = SessionState::new();

    // A tokio timer has a resolution of 1 ms. A shorter interval only adds wakeups.
    let poll_interval = Duration::from_micros(link_cfg.poll_interval_us.max(1_000));
    let mut link_on = true;
    let quantum = link_cfg.quantum;

    let phase_update_interval = Duration::from_millis(10);
    let mut last_phase_update = Instant::now();

    let mut last_beat_floor: i64 = -1;
    let mut last_is_playing: bool = false;
    let mut last_heartbeat = Instant::now();

    loop {
        // In Pro DJ Link mode, nothing reads the Link timeline. Leave the
        // session and poll slowly, so that Link takes no CPU and no state lock.
        let wanted = state.read().source_mode() != Source::ProLink;
        if wanted != link_on {
            link.enable(wanted);
            link_on = wanted;
            tracing::info!(enabled = wanted, "Ableton Link follows the source setting");
        }
        if !link_on {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }

        tokio::time::sleep(poll_interval).await;

        if last_heartbeat.elapsed() >= Duration::from_millis(100) {
            let n = link.num_peers();
            if last_peers != Some(n) {
                state.write().set_link_peer_count(n as usize);
                if n > 0 {
                    tracing::info!(peers = n, "Ableton Link peer(s) connected");
                } else {
                    tracing::info!("Ableton Link: no peers (session is empty)");
                }
                last_peers = Some(n);
            }
            last_heartbeat = Instant::now();
        }

        link.capture_app_session_state(&mut session);
        let now_us = link.clock_micros();

        let bpm = session.tempo();
        let is_playing = session.is_playing();

        let beat = session.beat_at_time(now_us, quantum);
        let phase = session.phase_at_time(now_us, quantum);

        let beat_floor = beat.floor() as i64;
        let first_sample = last_beat_floor < 0;
        let beat_crossed = advance_beat_floor(&mut last_beat_floor, beat_floor);

        let beat_in_bar = (phase.floor() as u8).saturating_add(1);
        let bar_phase = (phase / quantum).clamp(0.0, 1.0);
        let beat_phase = (phase - phase.floor()).clamp(0.0, 1.0);

        let should_push_phase = {
            let now = Instant::now();
            if now.duration_since(last_phase_update) >= phase_update_interval {
                last_phase_update = now;
                true
            } else {
                false
            }
        };

        let playing_changed = is_playing != last_is_playing;

        if beat_crossed || should_push_phase || playing_changed || first_sample {
            let received_at = Instant::now();
            state.write().apply_link_state(
                bpm,
                beat_in_bar,
                bar_phase,
                beat_phase,
                is_playing,
                beat_crossed,
                received_at,
            );
            let _ = timing_tx.send(());

            if beat_crossed && is_playing {
                let _ = beat_tx.send(BeatEvent::LinkBeat {
                    bpm,
                    beat_in_bar,
                    bar_phase,
                    beat_phase,
                    received_at,
                });
                tracing::debug!(
                    bpm = %format!("{:.2}", bpm),
                    beat_in_bar,
                    bar_phase = %format!("{:.3}", bar_phase),
                    "Link beat"
                );
            }
        }

        if playing_changed {
            tracing::info!(is_playing, "Ableton Link transport state changed");
            last_is_playing = is_playing;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_transport_reports_one_crossing_per_beat() {
        let mut floor = -1;
        assert!(!advance_beat_floor(&mut floor, 4));
        assert!(advance_beat_floor(&mut floor, 5));
        // The caller does not report the crossing as a beat while stopped.
        // The next poll in the same beat must not cross again.
        assert!(!advance_beat_floor(&mut floor, 5));
        assert_eq!(floor, 5);
    }
}
