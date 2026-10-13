use std::sync::Arc;
use std::time::Duration;

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

/// Reopens the configured MIDI output port after the device goes away.
///
/// The output worker drops the connection on the first send error. This loop
/// checks the connection every `period`. When the connection is gone, it opens
/// the port again by the name in `cfg.midi.output`. The loop never ends.
pub async fn reconnect_loop<F>(
    midi_out: crate::midi::MidiOutHandle,
    cfg: crate::config::SharedConfig,
    open: F,
    period: Duration,
) where
    F: Fn(&str) -> anyhow::Result<(Box<dyn crate::midi::MidiOutConnection>, String)>,
{
    let mut warned = false;
    loop {
        tokio::time::sleep(period).await;
        if midi_out.is_connected() {
            warned = false;
            continue;
        }

        let _port = midi_out.port_lock().lock().await;
        // A port switch from the UI can connect while this task waits for the lock.
        if midi_out.is_connected() {
            continue;
        }
        let wanted = cfg.read().midi.output.clone();
        match open(&wanted) {
            Ok((conn, name)) => {
                midi_out.switch_connection(Some(conn), false).await;
                cfg.write().midi.output = name.clone();
                tracing::info!(port = %name, "MIDI output reconnected");
                warned = false;
            }
            Err(e) => {
                if !warned {
                    tracing::warn!(
                        port = %wanted,
                        error = %e,
                        "MIDI output not available. The app tries again every {} s.",
                        period.as_secs()
                    );
                    warned = true;
                }
            }
        }
    }
}

/// Opens a real MIDI output port for [`reconnect_loop`].
pub fn open_midir_output(
    port_name: &str,
) -> anyhow::Result<(Box<dyn crate::midi::MidiOutConnection>, String)> {
    let (conn, name) = crate::midi::open_midi_output(port_name)?;
    Ok((Box::new(crate::midi::MidirOutConnection(conn)), name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex as ParkingMutex;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crate::config::{new_shared, Config};
    use crate::midi::activity::MidiActivity;
    use crate::state::timing::TimingMeasurement;
    use crate::state::{new_shared as new_shared_state, BeatSource, MasterState};

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

    struct FakeConn {
        fail: bool,
    }

    impl crate::midi::MidiOutConnection for FakeConn {
        fn send(&mut self, _msg: &[u8]) -> crate::midi::transport::MidiResult {
            if self.fail {
                Err(crate::midi::MidiError::SendError("unplugged".to_string()))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn reconnect_loop_reopens_the_port_after_a_send_error() {
        use crate::midi::MidiTransport;

        let mut cfg = Config::default();
        cfg.midi.output = "XDJ".to_string();
        let cfg = new_shared(cfg);
        let midi_out =
            crate::midi::MidiOutHandle::start(64, Some(Box::new(FakeConn { fail: true })));

        // The first send fails, so the worker drops the connection.
        let _ = midi_out.send_message(&[0xF8]);
        midi_out.barrier().await;
        assert!(!midi_out.is_connected());

        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempts2 = Arc::clone(&attempts);
        tokio::spawn(reconnect_loop(
            midi_out.clone(),
            Arc::clone(&cfg),
            move |wanted: &str| {
                assert_eq!(wanted, "XDJ");
                // The device is still away on the first attempt.
                if attempts2.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    anyhow::bail!("port not found");
                }
                Ok((
                    Box::new(FakeConn { fail: false }) as Box<dyn crate::midi::MidiOutConnection>,
                    "XDJ-RX3 MIDI".to_string(),
                ))
            },
            Duration::from_millis(10),
        ));

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(midi_out.is_connected());
        assert_eq!(cfg.read().midi.output, "XDJ-RX3 MIDI");
        assert!(midi_out.send_message(&[0xF8]).is_ok());
    }
}
