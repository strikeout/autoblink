use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::broadcast;
use tokio::sync::watch;

use crate::app::AppContext;
use crate::config::SharedConfig;
use crate::midi::activity::MidiActivity;
use crate::prolink::beat_listener::BeatEvent;
use crate::prolink::status_listener::StatusEvent;
use crate::prolink::virtual_cdj::VirtualCdjReady;
use crate::state::SharedState;

pub mod applier;
pub mod link;
pub mod logger;
pub mod midi;
pub mod prolink;

#[derive(Clone)]
pub struct TaskContext {
    pub dj_state: SharedState,
    pub cfg: SharedConfig,
    pub timing_tx: watch::Sender<()>,
    pub device_tx: broadcast::Sender<crate::prolink::discovery::DeviceEvent>,
    pub beat_tx: broadcast::Sender<BeatEvent>,
    pub status_tx: broadcast::Sender<StatusEvent>,
    pub vcdjready_tx: broadcast::Sender<VirtualCdjReady>,
    pub midi_out: crate::midi::MidiOutHandle,
    pub midi_activity: Arc<Mutex<MidiActivity>>,
    /// The channel to the DMX engine, or `None` when the engine does not run.
    pub dmx_tx: Option<crate::dmx::DmxTx>,
}

/// Runs the task that `factory` makes, and starts it again when it panics or
/// returns. The wait before a restart starts at 1 s and doubles to 10 s while
/// the task keeps failing. A task that runs for 60 s resets the wait.
pub async fn supervise<F, Fut>(name: &'static str, mut factory: F)
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        match tokio::spawn(factory()).await {
            Ok(()) => tracing::warn!(task = name, "A task stopped. It starts again."),
            Err(e) if e.is_panic() => {
                tracing::error!(task = name, "A task panicked. It starts again.")
            }
            Err(_) => return,
        }
        if started.elapsed() >= Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

/// Spawns `factory` under [`supervise`] on the current runtime.
pub fn spawn_supervised<F, Fut>(name: &'static str, factory: F)
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(supervise(name, factory));
}

/// Starts a single-thread runtime on its own OS thread for the MIDI clock, the
/// MTC and the MIDI output worker. Network work, the UI and blocking calls on
/// the main runtime then cannot delay a clock pulse. If the thread cannot
/// start, the MIDI tasks use the main runtime.
fn start_midi_runtime() -> tokio::runtime::Handle {
    let (tx, rx) = std::sync::mpsc::channel();
    let started = std::thread::Builder::new()
        .name("midi-rt".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("Cannot build the MIDI runtime: {e}");
                    return;
                }
            };
            let _ = tx.send(rt.handle().clone());
            rt.block_on(std::future::pending::<()>());
        });
    match started.ok().and_then(|_| rx.recv().ok()) {
        Some(handle) => handle,
        None => {
            tracing::error!("The MIDI thread did not start. MIDI uses the main runtime.");
            tokio::runtime::Handle::current()
        }
    }
}

#[cfg(test)]
fn source_flags(source: crate::config::Source) -> (bool, bool) {
    match source {
        crate::config::Source::Auto => (true, true),
        crate::config::Source::Link => (false, true),
        crate::config::Source::ProLink => (true, false),
    }
}

pub async fn run(ctx: AppContext) -> anyhow::Result<()> {
    let AppContext {
        cli,
        startup_cfg,
        midi_conn,
        dj_state,
        cfg,
        device_table,
        log_buf,
        device_tx,
        beat_tx,
        status_tx,
        vcdjready_tx,
        track_change_tx,
        track_change_rx,
        show,
    } = ctx;

    let midi_activity: Arc<Mutex<MidiActivity>> = Arc::new(Mutex::new(MidiActivity::default()));
    let (cfg_change_tx, cfg_change_rx) = watch::channel(());
    let (timing_tx, _timing_rx) = watch::channel(());

    let initial_conn = midi_conn.lock().take().map(|c| {
        Box::new(crate::midi::MidirOutConnection(c)) as Box<dyn crate::midi::MidiOutConnection>
    });

    let midi_rt = start_midi_runtime();

    // Bounded queue: producers must never block on MIDI output.
    // The output worker runs on the MIDI runtime.
    let midi_out = {
        let _enter = midi_rt.enter();
        crate::midi::MidiOutHandle::start(2048, initial_conn)
    };

    // The engine starts only with `[dmx] artnet_target`. It restarts after a panic.
    let dmx_tx = crate::dmx::start(
        &startup_cfg.dmx,
        Arc::clone(&show),
        Arc::clone(&dj_state),
        Arc::clone(&cfg),
        device_table.clone(),
    );

    let task_ctx = TaskContext {
        dj_state: Arc::clone(&dj_state),
        cfg: Arc::clone(&cfg),
        timing_tx: timing_tx.clone(),
        device_tx: device_tx.clone(),
        beat_tx: beat_tx.clone(),
        status_tx: status_tx.clone(),
        vcdjready_tx: vcdjready_tx.clone(),
        midi_out: midi_out.clone(),
        midi_activity: Arc::clone(&midi_activity),
        dmx_tx,
    };

    dj_state.write().set_source_mode(startup_cfg.source.clone());

    {
        let cfg = Arc::clone(&cfg);
        let dj_state = Arc::clone(&dj_state);
        spawn_supervised("source mode", move || {
            let cfg = Arc::clone(&cfg);
            let dj_state = Arc::clone(&dj_state);
            async move {
                loop {
                    let source = cfg.read().source.clone();
                    if dj_state.read().source_mode() != source {
                        dj_state.write().set_source_mode(source);
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        });
    }

    prolink::spawn(
        &task_ctx,
        startup_cfg.clone(),
        device_table.clone(),
        track_change_rx,
    );

    // Each task below restarts after a panic. See `supervise`.
    link::spawn(task_ctx.clone(), startup_cfg.clone());

    applier::spawn(task_ctx.clone(), track_change_tx);

    logger::spawn(task_ctx.clone());

    midi::spawn(task_ctx, cfg_change_rx, &midi_rt);

    {
        let midi_out = midi_out.clone();
        let cfg = Arc::clone(&cfg);
        spawn_supervised("MIDI reconnect", move || {
            midi::reconnect_loop(
                midi_out.clone(),
                Arc::clone(&cfg),
                midi::open_midir_output,
                crate::midi::ports::list_output_ports,
                Duration::from_secs(2),
            )
        });
    }

    crate::midi::input::spawn(Arc::clone(&dj_state));

    let web_bind = cli
        .web_bind
        .clone()
        .unwrap_or_else(|| startup_cfg.web.bind.clone());
    let web_addr = web_bind.parse().unwrap_or_else(|e| {
        tracing::error!(%web_bind, error = %e, "Invalid web UI address; using 0.0.0.0:11111");
        std::net::SocketAddr::from(([0, 0, 0, 0], 11111))
    });
    crate::web::spawn(
        crate::web::WebCtx {
            dj_state: Arc::clone(&dj_state),
            cfg: Arc::clone(&cfg),
            device_table: device_table.clone(),
            midi_activity: Arc::clone(&midi_activity),
            log_buf: log_buf.clone(),
            beat_tx: beat_tx.clone(),
            midi_out: midi_out.clone(),
            cfg_change_tx: cfg_change_tx.clone(),
            show,
        },
        web_addr,
    )
    .await;

    crate::app::headless_loop(dj_state, midi_out).await
}

#[cfg(test)]
mod tests {
    use super::{source_flags, supervise};
    use crate::config::Source;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn supervise_restarts_a_task_that_panics_or_returns() {
        let starts = Arc::new(AtomicUsize::new(0));
        let starts2 = Arc::clone(&starts);
        tokio::spawn(supervise("test", move || {
            let n = starts2.fetch_add(1, Ordering::SeqCst);
            async move {
                match n {
                    0 => panic!("first run panics"),
                    1 => {}
                    _ => std::future::pending::<()>().await,
                }
            }
        }));

        // The waits are 1 s after the panic and 2 s after the return.
        tokio::time::sleep(Duration::from_millis(3500)).await;
        assert_eq!(starts.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn source_flags_auto_enables_both() {
        assert_eq!(source_flags(Source::Auto), (true, true));
    }

    #[test]
    fn source_flags_link_disables_prolink() {
        assert_eq!(source_flags(Source::Link), (false, true));
    }

    #[test]
    fn source_flags_prolink_disables_link() {
        assert_eq!(source_flags(Source::ProLink), (true, false));
    }
}
