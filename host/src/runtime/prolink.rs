use std::sync::Arc;

use super::TaskContext;
use crate::prolink::discovery::DeviceTable;

pub fn spawn(
    ctx: &TaskContext,
    startup_cfg: crate::config::Config,
    device_table: DeviceTable,
    track_change_rx: tokio::sync::mpsc::Receiver<crate::state::TrackChange>,
) {
    let TaskContext {
        dj_state: _,
        cfg: _,
        timing_tx: _,
        device_tx,
        beat_tx,
        status_tx,
        vcdjready_tx,
        midi_out: _,
        midi_activity: _,
    } = ctx;

    let bind_ip = startup_cfg.bind_ip;
    let bcast_ip = startup_cfg.bcast_ip;
    let mac = startup_cfg.mac;

    let disc_table = Arc::clone(&device_table);
    let disc_tx = device_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::prolink::discovery::run(bind_ip, disc_table, disc_tx).await {
            tracing::error!("Discovery task error: {e}");
        }
    });

    let vcdj_name = startup_cfg.device_name.clone();
    let vcdj_ready_tx = vcdjready_tx.clone();
    let vcdj_dev_num = startup_cfg.device_number;
    let vcdj_table = Arc::clone(&device_table);
    tokio::spawn(async move {
        if let Err(e) = crate::prolink::virtual_cdj::run(
            bind_ip,
            bcast_ip,
            mac,
            vcdj_dev_num,
            &vcdj_name,
            vcdj_table,
            vcdj_ready_tx,
        )
        .await
        {
            tracing::error!("Virtual CDJ task error: {e}");
        }
    });

    let beat_tx2 = beat_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::prolink::beat_listener::run(bind_ip, beat_tx2).await {
            tracing::error!("Beat listener error: {e}");
        }
    });

    let status_tx2 = status_tx.clone();
    // Subscribe before the virtual CDJ can announce, so the event is not lost.
    let mut vcdjready_rx = vcdjready_tx.subscribe();
    tokio::spawn(async move {
        let ready = match vcdjready_rx.recv().await {
            Ok(ready) => ready,
            Err(e) => {
                tracing::error!("Status listener could not receive virtual CDJ ready event: {e}");
                return;
            }
        };

        if let Err(e) =
            crate::prolink::status_listener::run(bind_ip, ready.device_number, status_tx2).await
        {
            tracing::error!("Status listener error: {e}");
        }
    });

    let meta_table = Arc::clone(&device_table);
    let meta_state = Arc::clone(&ctx.dj_state);
    let mut meta_ready_rx = vcdjready_tx.subscribe();
    tokio::spawn(async move {
        // The dbserver setup needs the claimed device number, not the configured one.
        let ready = match meta_ready_rx.recv().await {
            Ok(ready) => ready,
            Err(e) => {
                tracing::error!("Metadata task could not receive virtual CDJ ready event: {e}");
                return;
            }
        };
        crate::prolink::metadata::run(ready.device_number, meta_table, meta_state, track_change_rx)
            .await;
    });
}
