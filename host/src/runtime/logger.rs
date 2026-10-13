use std::sync::Arc;
use tokio::sync::broadcast;

use super::TaskContext;
use crate::prolink::discovery::DeviceEvent;

pub fn spawn(ctx: TaskContext) {
    super::spawn_supervised("device logger", move || {
        run(Arc::clone(&ctx.dj_state), ctx.device_tx.subscribe())
    });
}

async fn run(dj_state: crate::state::SharedState, mut rx: broadcast::Receiver<DeviceEvent>) {
    loop {
        match rx.recv().await {
            Ok(DeviceEvent::Appeared(d)) => {
                tracing::info!(
                    device = d.device_number,
                    name = %d.name,
                    ip = ?d.ip,
                    "DJ device appeared on network"
                );
                dj_state.write().mark_prolink_seen();
            }
            Ok(DeviceEvent::Disappeared(num)) => {
                tracing::info!(device = num, "DJ device left network");
                dj_state.write().remove_device(num);
            }
            Err(broadcast::error::RecvError::Closed) => break,
            Err(_) => {}
        }
    }
}
