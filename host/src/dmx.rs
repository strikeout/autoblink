//! The DMX engine of the host. It sends Art-Net to a DMX node.
//!
//! The task runs the `Lights` engine of `autoblink-core-dmx`, like the `dmx` thread of the
//! firmware, and sends one ArtDmx frame each `FRAME_INTERVAL`. The MIDI mapper sends the scene
//! notes, the fired presets and the master beats through a bounded channel. The mapper never
//! waits for the channel: a full channel drops the message.
//!
//! The host starts the task only when `[dmx] artnet_target` is set. While an AUTOBLINK box is on
//! the Pro DJ Link network, the box drives the lights, and the task sends nothing.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};

use autoblink_core_dmx::{build_artdmx, BeatClock, Lights, Live, Show, Universe, UNIVERSE};
use autoblink_core_midi::scenes::Event;
use parking_lot::RwLock;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::config::{DmxConfig, SharedConfig};
use crate::prolink::discovery::{Device, DeviceTable};
use crate::state::SharedState;

/// The show of the host. The page writes it with `POST /show`, and the engine reads it.
pub type SharedShow = Arc<RwLock<Show>>;

/// The sender of the messages to the engine.
pub type DmxTx = mpsc::Sender<DmxMsg>;

/// The Pro DJ Link name of the box (`NAME` in `esp32/src/prolink.rs`).
pub const BOX_NAME: &str = "autoblink";

/// The time between two Art-Net frames (40 frames per second).
const FRAME_INTERVAL: Duration = Duration::from_millis(25);

/// The number of messages that the channel to the engine holds.
const QUEUE_LEN: usize = 64;

/// A message to the engine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DmxMsg {
    /// The note on of a preset. The value is the preset index.
    On(u8),
    /// The note off of a preset.
    Off(u8),
    /// An event of the scenes fired a preset.
    Fired(Event, u8),
    /// A beat of the master.
    Beat {
        beat_in_bar: u8,
        bpm: f64,
        at: Instant,
    },
}

/// Returns the IP address of an AUTOBLINK box in `devices`, or `None`. A box has the name
/// [`BOX_NAME`] and a device number that is not `own`. With more than one box, the box with the
/// lowest device number wins.
pub fn find_box<'a>(devices: impl IntoIterator<Item = &'a Device>, own: u8) -> Option<Ipv4Addr> {
    devices
        .into_iter()
        .filter(|d| d.name == BOX_NAME && d.device_number != own)
        .min_by_key(|d| d.device_number)
        .map(|d| Ipv4Addr::from(d.ip))
}

/// Starts the engine under `runtime::supervise` when `dmx.artnet_target` is set. Returns the
/// sender for the mapper, or `None` when the engine does not start.
pub fn start(
    dmx: &DmxConfig,
    show: SharedShow,
    dj_state: SharedState,
    cfg: SharedConfig,
    devices: DeviceTable,
) -> Option<DmxTx> {
    let target = dmx.target()?;
    let universe = dmx.universe;
    let (tx, rx) = mpsc::channel(QUEUE_LEN);
    // A restart of the task gets the same receiver, so that the mapper keeps its sender.
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    crate::runtime::spawn_supervised("DMX engine", move || {
        run(
            target,
            universe,
            Arc::clone(&rx),
            Arc::clone(&show),
            Arc::clone(&dj_state),
            Arc::clone(&cfg),
            Arc::clone(&devices),
        )
    });
    tracing::info!(%target, universe, "The DMX engine sends Art-Net");
    Some(tx)
}

async fn run(
    target: SocketAddrV4,
    universe: u16,
    rx: Arc<tokio::sync::Mutex<mpsc::Receiver<DmxMsg>>>,
    show: SharedShow,
    dj_state: SharedState,
    cfg: SharedConfig,
    devices: DeviceTable,
) {
    let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "The DMX engine cannot open a UDP socket");
            return;
        }
    };
    // A broadcast target needs this option.
    if let Err(e) = socket.set_broadcast(true) {
        tracing::warn!(error = %e, "The DMX engine cannot enable broadcast");
    }
    let mut rx = rx.lock().await;
    let start = Instant::now();
    let ms = |t: Instant| t.saturating_duration_since(start).as_millis() as u64;
    let live = Live {
        master: 255,
        strobe_hold: false,
    };
    let mut lights = Lights::default();
    let mut clock = BeatClock::default();
    let mut playing = None;
    let mut frame: Universe = [0; UNIVERSE];
    let mut packet = [0u8; 530];
    // Art-Net sequence 0 turns the sequence check of the node off. The task uses 1 to 255.
    let mut seq: u8 = 0;
    let mut box_ip: Option<Ipv4Addr> = None;
    let mut send_failed = false;
    let mut tick = tokio::time::interval(FRAME_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let now_ms = ms(Instant::now());
        let is_playing = dj_state.read().master.is_playing;
        // `set_playing` keeps the time of the stop. Thus the task calls it only on a change.
        if playing != Some(is_playing) {
            lights.set_playing(is_playing, now_ms);
            playing = Some(is_playing);
        }
        let own = cfg.read().device_number;
        let found = find_box(devices.lock().values(), own);
        if found != box_ip {
            match found {
                Some(ip) => {
                    tracing::info!(%ip, "An AUTOBLINK box is on the network. The host stops its Art-Net output.")
                }
                None => tracing::info!(
                    "The AUTOBLINK box left the network. The host sends Art-Net again."
                ),
            }
            box_ip = found;
        }
        {
            // The block drops the guard before the next await.
            let show = show.read();
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    DmxMsg::On(p) => lights.on(p, now_ms),
                    DmxMsg::Off(p) => lights.off(p, now_ms),
                    DmxMsg::Fired(e, p) => lights.fired(&show, e, p, now_ms),
                    DmxMsg::Beat {
                        beat_in_bar,
                        bpm,
                        at,
                    } => clock.beat(ms(at), beat_in_bar, bpm),
                }
            }
            let beat = clock.now(now_ms);
            lights.frame(&show, beat.as_ref(), &live, now_ms, &mut frame);
        }
        if box_ip.is_some() {
            continue;
        }
        seq = seq.checked_add(1).unwrap_or(1);
        let n = build_artdmx(seq, universe, &frame, &mut packet);
        match socket.send_to(&packet[..n], target).await {
            Ok(_) => send_failed = false,
            Err(e) => {
                // The task logs only the first error after a good send, not 40 lines each second.
                if !send_failed {
                    tracing::warn!(%target, error = %e, "The DMX engine cannot send Art-Net");
                }
                send_failed = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::Ipv4Addr;
    use std::time::{Duration, Instant};

    use parking_lot::Mutex;
    use tokio::net::UdpSocket;

    use super::*;
    use crate::config::{new_shared, Config, DmxConfig};
    use crate::prolink::discovery::Device;

    fn device(num: u8, name: &str, ip: [u8; 4]) -> Device {
        Device {
            device_number: num,
            device_type: 1,
            name: name.into(),
            mac: [0; 6],
            ip,
            last_seen: Instant::now(),
        }
    }

    #[test]
    fn find_box_takes_an_autoblink_with_another_number() {
        let own = 7;
        let devs = [
            device(1, "XDJ-AZ", [10, 0, 0, 2]),
            device(7, "autoblink", [10, 0, 0, 3]),
        ];
        assert_eq!(find_box(&devs, own), None, "the host itself is not a box");

        let devs = [
            device(1, "XDJ-AZ", [10, 0, 0, 2]),
            device(7, "autoblink", [10, 0, 0, 3]),
            device(9, "autoblink", [10, 0, 0, 9]),
            device(8, "autoblink", [10, 0, 0, 8]),
        ];
        assert_eq!(find_box(&devs, own), Some(Ipv4Addr::new(10, 0, 0, 8)));
    }

    fn table() -> DeviceTable {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[tokio::test]
    async fn no_artnet_target_starts_no_task() {
        let tx = start(
            &DmxConfig::default(),
            SharedShow::default(),
            crate::state::new_shared(0),
            new_shared(Config::default()),
            table(),
        );
        assert!(tx.is_none());
    }

    async fn recv_frame(node: &UdpSocket, wait: Duration) -> Option<(u16, Vec<u8>)> {
        let mut buf = [0u8; 600];
        let n = tokio::time::timeout(wait, node.recv(&mut buf))
            .await
            .ok()?
            .unwrap();
        let (u, data) = autoblink_core_dmx::parse_artdmx(&buf[..n])?;
        Some((u, data.to_vec()))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn engine_sends_artdmx_until_a_box_shows() {
        let node = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = node.local_addr().unwrap().port();
        let devices = table();
        let cfg = DmxConfig {
            artnet_target: Some(format!("127.0.0.1:{port}")),
            universe: 2,
        };
        let tx = start(
            &cfg,
            SharedShow::default(),
            crate::state::new_shared(0),
            new_shared(Config::default()),
            Arc::clone(&devices),
        );
        assert!(tx.is_some());

        let (universe, data) = recv_frame(&node, Duration::from_secs(2))
            .await
            .expect("an ArtDmx frame");
        assert_eq!(universe, 2);
        assert_eq!(data.len(), 512);

        devices
            .lock()
            .insert(8, device(8, "autoblink", [127, 0, 0, 1]));
        // A frame can be on the way when the box shows. Read the frames for 100 ms.
        let until = tokio::time::Instant::now() + Duration::from_millis(100);
        while tokio::time::Instant::now() < until {
            let _ = recv_frame(&node, Duration::from_millis(10)).await;
        }
        assert!(
            recv_frame(&node, Duration::from_millis(300))
                .await
                .is_none(),
            "the host sends nothing while it sees a box"
        );

        devices.lock().clear();
        assert!(recv_frame(&node, Duration::from_secs(2)).await.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_scene_note_lights_its_look() {
        let node = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = node.local_addr().unwrap().port();
        // One par on channel 1, and preset 0 sets it to full.
        let show = Show::from_json(
            serde_json::json!({
                "fixtures": [{"name": "par", "address": 1, "channels": ["dimmer"]}],
                "groups": [{"name": "all", "fixtures": [0]}],
                "looks": [{"rows": [{"group": 0, "color": [255, 255, 255, 255], "intensity": 255}]}],
            })
            .to_string()
            .as_bytes(),
        );
        assert!(show.look(0).is_some());
        let cfg = DmxConfig {
            artnet_target: Some(format!("127.0.0.1:{port}")),
            universe: 0,
        };
        let tx = start(
            &cfg,
            Arc::new(RwLock::new(show)),
            crate::state::new_shared(0),
            new_shared(Config::default()),
            table(),
        )
        .unwrap();
        tx.try_send(DmxMsg::On(0)).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let (_, data) = recv_frame(&node, Duration::from_secs(2)).await.unwrap();
            if data[0] > 0 {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "the look shows");
        }
    }
}
