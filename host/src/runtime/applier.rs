use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::sync::mpsc;

use super::TaskContext;
use crate::prolink::beat_listener::BeatEvent;
use crate::prolink::status_listener::StatusEvent;
use crate::state::TrackChange;

pub fn spawn(ctx: TaskContext, track_change_tx: mpsc::Sender<TrackChange>) {
    // The factories keep the sender alive, so the metadata task never sees a
    // closed track change queue.
    let c = ctx.clone();
    super::spawn_supervised("beat applier", move || {
        beat_applier(
            Arc::clone(&c.dj_state),
            Arc::clone(&c.cfg),
            c.beat_tx.subscribe(),
            c.timing_tx.clone(),
        )
    });
    super::spawn_supervised("status applier", move || {
        status_applier(
            Arc::clone(&ctx.dj_state),
            Arc::clone(&ctx.cfg),
            ctx.status_tx.subscribe(),
            track_change_tx.clone(),
        )
    });
}

async fn beat_applier(
    state: Arc<parking_lot::RwLock<crate::state::DjState>>,
    cfg: Arc<parking_lot::RwLock<crate::config::Config>>,
    mut rx: broadcast::Receiver<BeatEvent>,
    timing_tx: tokio::sync::watch::Sender<()>,
) {
    loop {
        match rx.recv().await {
            Ok(BeatEvent::Beat {
                packet: bp,
                received_at,
            }) => {
                let smoothing_ms = cfg.read().midi.smoothing_ms;
                let mut state = state.write();
                state.set_smoothing_ms(smoothing_ms);
                state.apply_beat(&bp, received_at);
                let _ = timing_tx.send(());
            }
            Ok(BeatEvent::AbsPosition {
                packet: ap,
                received_at,
            }) => {
                let smoothing_ms = cfg.read().midi.smoothing_ms;
                let mut state = state.write();
                state.set_smoothing_ms(smoothing_ms);
                state.apply_abs_position(&ap, received_at);
                let _ = timing_tx.send(());
            }
            Ok(BeatEvent::LinkBeat { .. }) => {}
            Err(broadcast::error::RecvError::Closed) => break,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("Beat applier lagged, dropped {n} events");
            }
        }
    }
}

async fn status_applier(
    state: Arc<parking_lot::RwLock<crate::state::DjState>>,
    cfg: Arc<parking_lot::RwLock<crate::config::Config>>,
    mut rx: broadcast::Receiver<StatusEvent>,
    track_change_tx: mpsc::Sender<TrackChange>,
) {
    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(StatusEvent::Cdj(s)) => {
                        let smoothing_ms = cfg.read().midi.smoothing_ms;
                        let mut state = state.write();
                        state.set_smoothing_ms(smoothing_ms);
                        let change = state.apply_cdj_status(&s);
                        drop(state);
                        if let Some(tc) = change {
                            let _ = track_change_tx.try_send(tc);
                        }
                    }
                    Ok(StatusEvent::Mixer(s)) => {
                        let smoothing_ms = cfg.read().midi.smoothing_ms;
                        let mut state = state.write();
                        state.set_smoothing_ms(smoothing_ms);
                        state.apply_mixer_status(&s);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("Status applier lagged, dropped {n} events");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{new_shared, Config};
    use std::time::{Duration, Instant};

    fn beat(device_number: u8, bpm: f64) -> BeatEvent {
        BeatEvent::Beat {
            packet: crate::prolink::packets::BeatPacket {
                device_number,
                next_beat_ms: 500,
                second_beat_ms: 1000,
                next_bar_ms: 2000,
                pitch_raw: crate::prolink::PITCH_NORMAL,
                bpm_raw: (bpm * 100.0) as u16,
                beat_in_bar: 1,
                track_bpm: Some(bpm),
                effective_bpm: bpm,
                pitch_pct: 0.0,
            },
            received_at: Instant::now(),
        }
    }

    #[tokio::test]
    async fn smoothing_setting_changes_take_effect_at_runtime() {
        let cfg = new_shared(Config::default());
        cfg.write().midi.smoothing_ms = 1000;
        let state = crate::state::new_shared(1000);
        let (tx, rx) = broadcast::channel(8);
        let (timing_tx, _timing_rx) = tokio::sync::watch::channel(());

        let handle = tokio::spawn(beat_applier(
            Arc::clone(&state),
            Arc::clone(&cfg),
            rx,
            timing_tx,
        ));

        let _ = tx.send(beat(1, 100.0));
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = tx.send(beat(1, 200.0));
        tokio::time::sleep(Duration::from_millis(20)).await;
        let smoothed = state.read().devices.get(&1).unwrap().effective_bpm;
        assert!((smoothed - 150.0).abs() < 0.1);

        cfg.write().midi.smoothing_ms = 0;
        let _ = tx.send(beat(1, 200.0));
        tokio::time::sleep(Duration::from_millis(20)).await;
        let unsmoothed = state.read().devices.get(&1).unwrap().effective_bpm;
        assert!((unsmoothed - 200.0).abs() < 0.1);

        handle.abort();
    }
}
