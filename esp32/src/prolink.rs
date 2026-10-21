//! Receives the Pro DJ Link keepalive and beat packets on the Ethernet port.
//!
//! The players send these packets as broadcasts, so the firmware does not have to join the
//! network as a virtual CDJ to receive them.

use std::net::UdpSocket;
use std::sync::Arc;
use std::thread;

use xdj_core_prolink::{parse_beat, parse_keepalive, PORT_BEAT, PORT_DISCOVERY};

use crate::State;

/// Starts one receive thread for each port. `on_master_beat` gets the device and the beat in
/// the bar (1 to 4) for each beat of the master deck.
pub fn spawn(
    state: Arc<State>,
    on_master_beat: impl Fn(u8, u8) + Send + 'static,
) -> anyhow::Result<()> {
    let discovery = UdpSocket::bind(("0.0.0.0", PORT_DISCOVERY))?;
    let beats = UdpSocket::bind(("0.0.0.0", PORT_BEAT))?;

    let st = state.clone();
    thread::Builder::new()
        .name("prolink-discovery".into())
        .stack_size(6 * 1024)
        .spawn(move || {
            let mut buf = [0u8; 512];
            loop {
                let Ok((len, _)) = discovery.recv_from(&mut buf) else { continue };
                if let Some(ka) = parse_keepalive(&buf[..len]) {
                    let now = st.now_ms();
                    st.tracker.lock().unwrap().on_keepalive(&ka, now);
                }
            }
        })?;

    thread::Builder::new()
        .name("prolink-beats".into())
        .stack_size(6 * 1024)
        .spawn(move || {
            let mut buf = [0u8; 512];
            loop {
                let Ok((len, _)) = beats.recv_from(&mut buf) else { continue };
                let Some(beat) = parse_beat(&buf[..len]) else { continue };
                let now = state.now_ms();
                let is_master = state.tracker.lock().unwrap().on_beat(&beat, now);
                if is_master {
                    if beat.beat_in_bar == 1 {
                        log::info!("Bar: deck {} at {:.2} BPM", beat.device_number, beat.effective_bpm);
                    }
                    on_master_beat(beat.device_number, beat.beat_in_bar);
                }
            }
        })?;
    Ok(())
}
