#![warn(clippy::all)]

mod app;
mod config;
mod link;
mod logbuf;
mod midi;
mod prolink;
mod runtime;
mod settings;
mod state;
mod web;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ctx = app::init()?;
    runtime::run(ctx).await?;
    // ponytail: the runtime waits for blocking tasks when it drops, and an
    // interface detection can block for seconds. MIDI Stop is already sent, so
    // exit now. The upgrade path is an explicit runtime with shutdown_timeout.
    std::process::exit(0)
}
