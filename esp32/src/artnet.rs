//! The `artnet` thread receives Art-Net from a lighting desk. The desk sends ArtDmx for universe 0
//! to UDP port 6454.
//!
//! The thread receives on all interfaces: the own WiFi network, the joined WiFi network and the
//! Ethernet port. It keeps the last frame of universe 0 in `State::artnet`. The `dmx` thread
//! merges that frame into its output (`autoblink_core_dmx::Merge`).

use std::io::ErrorKind;
use std::net::UdpSocket;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use autoblink_core_dmx::{parse_artdmx, Universe, ARTNET_PORT, UNIVERSE};

use crate::State;

/// The Art-Net header (18 bytes) and a full universe. The receive cuts a longer datagram, and
/// the parser then refuses it when its length field is too large.
const PACKET_LEN: usize = 18 + UNIVERSE;

/// The last Art-Net frame of universe 0.
pub struct ArtIn {
    /// The data is on the heap, so that `State` stays small.
    pub data: Box<Universe>,
    /// The time of the frame, with the clock of `State::now_ms`. `None` before the first frame.
    pub at_ms: Option<u64>,
}

impl Default for ArtIn {
    fn default() -> Self {
        Self {
            data: Box::new([0; UNIVERSE]),
            at_ms: None,
        }
    }
}

/// Starts the `artnet` thread. When the thread does not start, the firmware logs a warning and
/// continues without Art-Net.
pub fn start(state: Arc<State>) {
    // ponytail: 4 KB is an estimate, not a measurement. The thread function has a 672-byte frame
    // with the 530-byte buffer. Its deepest direct call path is 2.6 KB (the bind). Measure with
    // /mem on a box.
    let started = crate::spawn(c"artnet", 4 * 1024, move || {
        let sock = match UdpSocket::bind(("0.0.0.0", ARTNET_PORT)) {
            Ok(s) => s,
            Err(e) => {
                log::warn!("Art-Net is off: port {ARTNET_PORT} is not available: {e}");
                return;
            }
        };
        let _ = sock.set_read_timeout(Some(Duration::from_secs(1)));
        log::info!("Art-Net: the firmware receives universe 0 on port {ARTNET_PORT}");
        let mut buf = [0u8; PACKET_LEN];
        loop {
            let len = match sock.recv(&mut buf) {
                Ok(len) => len,
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    continue
                }
                // The sleep lets the lower-priority tasks run after a receive error.
                Err(_) => {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
            };
            let Some((0, data)) = parse_artdmx(&buf[..len]) else {
                continue;
            };
            let now_ms = state.now_ms();
            // The lock is short. The `dmx` thread does not wait for it.
            let mut slot = state.artnet.lock().unwrap();
            slot.data[..data.len()].copy_from_slice(data);
            slot.data[data.len()..].fill(0);
            slot.at_ms = Some(now_ms);
        }
    });
    if let Err(e) = started {
        log::warn!("Art-Net is off: the thread did not start: {e}");
    }
}
