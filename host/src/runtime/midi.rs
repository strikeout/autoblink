use std::sync::Arc;

use tokio::sync::broadcast;
use tokio::sync::watch;

use super::TaskContext;
use crate::prolink::beat_listener::BeatEvent;
use crate::prolink::status_listener::StatusEvent;

pub fn spawn(
    ctx: TaskContext,
    beat_rx: broadcast::Receiver<BeatEvent>,
    status_rx: broadcast::Receiver<StatusEvent>,
    cfg_change_rx: watch::Receiver<()>,
    timing_rx: watch::Receiver<()>,
) {
    let midi_out = ctx.midi_out.clone();
    let dj_state = Arc::clone(&ctx.dj_state);
    let cfg = Arc::clone(&ctx.cfg);
    let midi_activity = Arc::clone(&ctx.midi_activity);

    let mapper_midi: Arc<dyn crate::midi::MidiTransport> = Arc::new(midi_out.clone());
    tokio::spawn(crate::midi::mapper::run(
        mapper_midi,
        dj_state.clone(),
        beat_rx.resubscribe(),
        status_rx,
        cfg.clone(),
        midi_activity.clone(),
    ));

    let dj_state2 = Arc::clone(&ctx.dj_state);
    let cfg2 = Arc::clone(&ctx.cfg);
    let midi_activity2 = Arc::clone(&ctx.midi_activity);
    let beat_rx2 = beat_rx.resubscribe();
    let timing_rx2 = timing_rx.clone();

    let midi_out_for_clock = midi_out.clone();
    tokio::spawn(async move {
        let midi_transport: Arc<dyn crate::midi::MidiTransport> = Arc::new(midi_out_for_clock);
        crate::midi::clock::run(
            midi_transport,
            dj_state2,
            beat_rx2,
            cfg2,
            midi_activity2,
            cfg_change_rx,
            timing_rx2,
        )
        .await;
    });

    let dj_state3 = Arc::clone(&ctx.dj_state);
    let cfg3 = Arc::clone(&ctx.cfg);
    let midi_activity3 = Arc::clone(&ctx.midi_activity);
    let beat_rx3 = beat_rx.resubscribe();
    let timing_rx3 = timing_rx;

    let mtc_midi: Arc<dyn crate::midi::MidiTransport> = Arc::new(midi_out);
    tokio::spawn(crate::midi::timecode::run(
        mtc_midi,
        dj_state3,
        cfg3,
        midi_activity3,
        beat_rx3,
        timing_rx3,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex as ParkingMutex;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::config::{new_shared, Config};
    use crate::state::timing::TimingMeasurement;
    use crate::state::{new_shared as new_shared_state, BeatSource, MasterState};
    use crate::tui::state::MidiActivity;

    #[tokio::test]
    async fn mtc_can_be_enabled_and_disabled_after_startup() {
        let mut cfg = Config::default();
        cfg.midi.clock_enabled = false;
        cfg.midi.mtc.enabled = false;
        let cfg = new_shared(cfg);

        let state = new_shared_state(30);
        {
            let mut st = state.write();
            st.master = MasterState {
                source: Some(BeatSource::AbletonLink),
                bpm: 120.0,
                is_playing: true,
                ..Default::default()
            };
            // MTC now derives its position from the authoritative timing model.
            st.timing.observe(TimingMeasurement::from_link(
                120.0,
                1,
                0.0,
                0.0,
                true,
                Instant::now(),
            ));
        }

        let (beat_tx, beat_rx) = broadcast::channel(8);
        let (status_tx, status_rx) = broadcast::channel(8);
        let (device_tx, _) = broadcast::channel(8);
        let (vcdjready_tx, _) = broadcast::channel(8);
        let (_cfg_change_tx, cfg_change_rx) = watch::channel(());
        let (timing_tx, timing_rx) = watch::channel(());

        let midi_activity = Arc::new(ParkingMutex::new(MidiActivity::default()));
        let midi_out = crate::midi::MidiOutHandle::start(64, None);
        let ctx = TaskContext {
            dj_state: Arc::clone(&state),
            cfg: Arc::clone(&cfg),
            timing_tx,
            device_tx,
            beat_tx,
            status_tx,
            vcdjready_tx,
            midi_out,
            midi_activity: Arc::clone(&midi_activity),
        };

        spawn(ctx, beat_rx, status_rx, cfg_change_rx, timing_rx);

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(midi_activity.lock().mtc_quarter_frames, 0);

        cfg.write().midi.mtc.enabled = true;
        tokio::time::sleep(Duration::from_millis(120)).await;
        let qf_after_enable = midi_activity.lock().mtc_quarter_frames;
        assert!(qf_after_enable > 0);

        cfg.write().midi.mtc.enabled = false;
        tokio::time::sleep(Duration::from_millis(80)).await;
        let qf_after_disable = midi_activity.lock().mtc_quarter_frames;
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(midi_activity.lock().mtc_quarter_frames, qf_after_disable);
    }
}
