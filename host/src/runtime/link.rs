use std::sync::Arc;

use super::TaskContext;
use crate::link::run as link_run;

pub fn spawn(ctx: TaskContext, startup_cfg: crate::config::Config) {
    let link_cfg = startup_cfg.link.clone();
    if !link_cfg.enabled {
        tracing::info!("Ableton Link engine disabled by config");
        return;
    }
    super::spawn_supervised("Ableton Link", move || {
        let link = rusty_link::AblLink::new(120.0);
        link_run(
            link_cfg.clone(),
            Arc::clone(&ctx.dj_state),
            ctx.beat_tx.clone(),
            ctx.timing_tx.clone(),
            link,
        )
    });
}
