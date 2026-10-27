use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::TaskContext;
use crate::prolink::discovery::DeviceTable;
use crate::state::TrackChange;

/// The time between two checks of the network and of the task group.
const CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// Starts the Pro DJ Link supervisor.
///
/// The supervisor finds the network interface and runs all Pro DJ Link tasks
/// as one group. It rebuilds the group when the IP address goes away, when a
/// better interface comes up in auto mode, when the interface setting or the
/// device number changes, or when a task in the group stops. The app never
/// exits for a network fault.
pub fn spawn(
    ctx: &TaskContext,
    startup_cfg: crate::config::Config,
    device_table: DeviceTable,
    track_change_rx: mpsc::Receiver<TrackChange>,
) {
    let ctx = ctx.clone();
    let track_change_rx = Arc::new(tokio::sync::Mutex::new(track_change_rx));
    super::spawn_supervised("Pro DJ Link supervisor", move || {
        let ctx = ctx.clone();
        let startup_cfg = startup_cfg.clone();
        let device_table = device_table.clone();
        let track_change_rx = Arc::clone(&track_change_rx);
        async move {
            loop {
                let (hint, bind_ip, bcast_ip, mac) = wait_for_interface(&ctx).await;
                tracing::info!(%bind_ip, %bcast_ip, "Network interface initialized");

                let mut group = JoinSet::new();
                let device_number = ctx.cfg.read().device_number;
                spawn_group(
                    &mut group,
                    &ctx,
                    &startup_cfg,
                    &device_table,
                    &track_change_rx,
                    device_number,
                    bind_ip,
                    bcast_ip,
                    mac,
                );

                let reason = watch_group(
                    &mut group,
                    &ctx,
                    &hint,
                    device_number,
                    bind_ip,
                    &device_table,
                )
                .await;
                tracing::warn!(%reason, "Rebuilding the Pro DJ Link network tasks");
                group.shutdown().await;
                tokio::time::sleep(CHECK_INTERVAL).await;
            }
        }
    });
}

/// Detects the interface again and again until one is found.
/// Returns the interface setting that was used, and the address data.
async fn wait_for_interface(ctx: &TaskContext) -> (String, Ipv4Addr, Ipv4Addr, [u8; 6]) {
    let mut warned = false;
    loop {
        let hint = ctx.cfg.read().interface.clone();
        let hint2 = hint.clone();
        // The detection can probe for keepalives, which blocks for up to 2 s.
        match tokio::task::spawn_blocking(move || crate::app::detect_interface(&hint2)).await {
            Ok(Ok((bind_ip, bcast_ip, mac))) => return (hint, bind_ip, bcast_ip, mac),
            Ok(Err(e)) if !warned => {
                tracing::warn!(
                    "Waiting for the network: {e}. The app tries again every {} s.",
                    CHECK_INTERVAL.as_secs()
                );
                warned = true;
            }
            Ok(Err(_)) => {}
            Err(e) => tracing::error!("Network interface detection failed: {e}"),
        }
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

/// Waits until the group must be rebuilt. Returns the reason.
async fn watch_group(
    group: &mut JoinSet<()>,
    ctx: &TaskContext,
    hint: &str,
    device_number: u8,
    bind_ip: Ipv4Addr,
    device_table: &DeviceTable,
) -> String {
    let mut ticker = tokio::time::interval(CHECK_INTERVAL);
    ticker.tick().await;
    loop {
        tokio::select! {
            ended = group.join_next() => {
                return match ended {
                    Some(Err(e)) if e.is_panic() => "a network task panicked".to_string(),
                    _ => "a network task stopped".to_string(),
                };
            }
            _ = ticker.tick() => {
                if ctx.cfg.read().interface != hint {
                    return "the interface setting changed".to_string();
                }
                if ctx.cfg.read().device_number != device_number {
                    return "the device number changed".to_string();
                }
                // `getifaddrs` can block, so keep it off the async workers.
                let hint2 = hint.to_string();
                let players = !device_table.lock().is_empty();
                let changed = tokio::task::spawn_blocking(move || network_changed(&hint2, bind_ip, players))
                    .await
                    .unwrap_or_else(|e| Some(format!("the interface check panicked: {e}")));
                if let Some(reason) = changed {
                    return reason;
                }
            }
        }
    }
}

/// Returns a reason when `bind_ip` is gone, or when auto mode has a better
/// interface than the one in use.
///
/// `players` is true while players send keep-alives on the interface in use. Then a better
/// interface does not cause a rebuild. For example, a WiFi port whose DHCP failed gets a
/// 169.254.x.x address and the best priority, but the players are on the Ethernet port.
fn network_changed(hint: &str, bind_ip: Ipv4Addr, players: bool) -> Option<String> {
    let candidates = match crate::app::interface_candidates(hint) {
        Ok(c) => c,
        Err(e) => return Some(format!("the interface list is not available: {e}")),
    };
    let Some(current) = candidates.iter().find(|c| c.ip == bind_ip) else {
        return Some(format!("the address {bind_ip} is gone"));
    };
    match candidates.first() {
        Some(best) if hint == "auto" && !players && best.priority < current.priority => Some(
            format!("a better interface is up: {} ({})", best.name, best.ip),
        ),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_group(
    group: &mut JoinSet<()>,
    ctx: &TaskContext,
    startup_cfg: &crate::config::Config,
    device_table: &DeviceTable,
    track_change_rx: &Arc<tokio::sync::Mutex<mpsc::Receiver<TrackChange>>>,
    device_number: u8,
    bind_ip: Ipv4Addr,
    bcast_ip: Ipv4Addr,
    mac: [u8; 6],
) {
    let disc_table = Arc::clone(device_table);
    let disc_tx = ctx.device_tx.clone();
    let disc_name = startup_cfg.device_name.clone();
    group.spawn(async move {
        if let Err(e) =
            crate::prolink::discovery::run(bind_ip, disc_name, disc_table, disc_tx).await
        {
            tracing::error!("Discovery task error: {e}");
        }
    });

    // Subscribe before the virtual CDJ can announce, so the event is not lost.
    let mut status_ready_rx = ctx.vcdjready_tx.subscribe();
    let mut meta_ready_rx = ctx.vcdjready_tx.subscribe();

    let vcdj_name = startup_cfg.device_name.clone();
    let vcdj_ready_tx = ctx.vcdjready_tx.clone();
    let vcdj_dev_num = device_number;
    let vcdj_table = Arc::clone(device_table);
    group.spawn(async move {
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

    let beat_tx = ctx.beat_tx.clone();
    group.spawn(async move {
        if let Err(e) = crate::prolink::beat_listener::run(bind_ip, beat_tx).await {
            tracing::error!("Beat listener error: {e}");
        }
    });

    let status_tx = ctx.status_tx.clone();
    group.spawn(async move {
        let ready = match status_ready_rx.recv().await {
            Ok(ready) => ready,
            Err(e) => {
                tracing::error!("Status listener could not receive virtual CDJ ready event: {e}");
                return;
            }
        };

        if let Err(e) =
            crate::prolink::status_listener::run(bind_ip, ready.device_number, status_tx).await
        {
            tracing::error!("Status listener error: {e}");
        }
    });

    let meta_table = Arc::clone(device_table);
    let meta_state = Arc::clone(&ctx.dj_state);
    let meta_rx = Arc::clone(track_change_rx);
    group.spawn(async move {
        // The dbserver setup needs the claimed device number, not the configured one.
        let ready = match meta_ready_rx.recv().await {
            Ok(ready) => ready,
            Err(e) => {
                tracing::error!("Metadata task could not receive virtual CDJ ready event: {e}");
                return;
            }
        };
        // The receiver cannot be cloned. Each group borrows it in turn.
        let mut rx = meta_rx.lock_owned().await;
        crate::prolink::metadata::run(ready.device_number, meta_table, meta_state, &mut rx).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_address_asks_for_a_rebuild() {
        // 203.0.113.0/24 is TEST-NET-3, so no interface on the test machine has it.
        let reason = network_changed("auto", Ipv4Addr::new(203, 0, 113, 7), false);
        assert!(reason.is_some_and(|r| r.contains("is gone")));
    }
}
