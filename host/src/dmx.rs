//! The DMX engine of the host. It sends Art-Net to a DMX node.
//!
//! The task runs the `Lights` engine of `autoblink-core-dmx`, like the `dmx` thread of the
//! firmware, and sends one ArtDmx frame each `FRAME_INTERVAL`. The MIDI mapper sends the scene
//! notes, the fired presets and the master beats through a bounded channel. The mapper never
//! waits for the channel: a full channel drops the message.
//!
//! The page sets the live controls (`DmxLive`) and sends the fog, the aims and the preset tests.
//! The task writes the layers and the live controls into `DmxStatus` for the status of the page.
//!
//! The host starts the task only when `[dmx] artnet_target` is set. While an AUTOBLINK box is on
//! the Pro DJ Link network, the box drives the lights, and the task sends nothing.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use autoblink_core_dmx::{build_artdmx, BeatClock, Lights, Live, Show, Universe, UNIVERSE};
use autoblink_core_midi::scenes::Event;
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
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
    /// The page tests a preset.
    Test(u8),
    /// The fog button of the page.
    Fog,
    /// The page aims a head by hand.
    Aim { fixture: u8, pan: u16, tilt: u16 },
    /// A beat of the master.
    Beat {
        beat_in_bar: u8,
        bpm: f64,
        at: Instant,
    },
}

/// The live controls of the page. The task reads them for each frame.
pub struct DmxLive {
    /// 255 is full.
    pub master: AtomicU8,
    pub strobe_hold: AtomicBool,
}

impl Default for DmxLive {
    fn default() -> Self {
        Self {
            master: AtomicU8::new(255),
            strobe_hold: AtomicBool::new(false),
        }
    }
}

/// The DMX part of the status for the page. The box sends the same fields.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DmxStatus {
    /// The presets that show now, bottom first.
    pub layers: Vec<u8>,
    pub master: u8,
    pub strobe_hold: bool,
}

/// The handles of a running engine.
#[derive(Clone)]
pub struct DmxHandle {
    pub tx: DmxTx,
    pub live: Arc<DmxLive>,
    pub status: Arc<Mutex<DmxStatus>>,
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
/// handles for the mapper and the page, or `None` when the engine does not start.
pub fn start(
    dmx: &DmxConfig,
    show: SharedShow,
    dj_state: SharedState,
    cfg: SharedConfig,
    devices: DeviceTable,
) -> Option<DmxHandle> {
    let target = dmx.target()?;
    let universe = dmx.universe;
    if cfg.read().device_number == 7 {
        tracing::warn!(
            "The device number is the default 7. The host cannot find a box with number 7."
        );
    }
    let (tx, rx) = mpsc::channel(QUEUE_LEN);
    // A restart of the task gets the same receiver, so that the mapper keeps its sender.
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let handle = DmxHandle {
        tx,
        live: Arc::default(),
        status: Arc::new(Mutex::new(DmxStatus {
            master: 255,
            ..DmxStatus::default()
        })),
    };
    let task = Task {
        target,
        universe,
        rx,
        page: Arc::clone(&handle.live),
        status: Arc::clone(&handle.status),
        show,
        dj_state,
        cfg,
        devices,
    };
    crate::runtime::spawn_supervised("DMX engine", move || run(task.clone()));
    tracing::info!(%target, universe, "The DMX engine sends Art-Net");
    Some(handle)
}

/// The inputs of the engine task. A restart of the task gets a clone.
#[derive(Clone)]
struct Task {
    target: SocketAddrV4,
    universe: u16,
    rx: Arc<tokio::sync::Mutex<mpsc::Receiver<DmxMsg>>>,
    page: Arc<DmxLive>,
    status: Arc<Mutex<DmxStatus>>,
    show: SharedShow,
    dj_state: SharedState,
    cfg: SharedConfig,
    devices: DeviceTable,
}

async fn run(task: Task) {
    let Task {
        target,
        universe,
        rx,
        page,
        status,
        show,
        dj_state,
        cfg,
        devices,
    } = task;
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
        let live = Live {
            master: page.master.load(Ordering::Relaxed),
            strobe_hold: page.strobe_hold.load(Ordering::Relaxed),
        };
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
                    DmxMsg::Test(p) => lights.test(&show, p, now_ms),
                    DmxMsg::Fog => lights.fog(now_ms),
                    DmxMsg::Aim { fixture, pan, tilt } => lights.aim(fixture, pan, tilt, now_ms),
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
        // The task does not wait for the status lock, and it writes only a change. Thus a frame
        // without a change allocates nothing. The next frame tries again.
        if let Some(mut s) = status.try_lock() {
            if !s.layers.iter().copied().eq(lights.layers()) {
                s.layers.clear();
                s.layers.extend(lights.layers());
            }
            s.master = live.master;
            s.strobe_hold = live.strobe_hold;
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
    use std::sync::atomic::Ordering;
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
        let dmx = start(
            &DmxConfig::default(),
            SharedShow::default(),
            crate::state::new_shared(0),
            new_shared(Config::default()),
            table(),
        );
        assert!(dmx.is_none());
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
        let dmx = start(
            &cfg,
            SharedShow::default(),
            crate::state::new_shared(0),
            new_shared(Config::default()),
            Arc::clone(&devices),
        );
        assert!(dmx.is_some());

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

    /// Starts an engine that sends to a UDP socket of the test.
    async fn engine(show: serde_json::Value) -> (UdpSocket, DmxHandle) {
        let node = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = node.local_addr().unwrap().port();
        let cfg = DmxConfig {
            artnet_target: Some(format!("127.0.0.1:{port}")),
            universe: 0,
        };
        let show = Show::from_json(show.to_string().as_bytes());
        let dmx = start(
            &cfg,
            Arc::new(RwLock::new(show)),
            crate::state::new_shared(0),
            new_shared(Config::default()),
            table(),
        )
        .unwrap();
        (node, dmx)
    }

    /// Reads frames until one passes `pred`.
    async fn wait_frame(node: &UdpSocket, what: &str, pred: impl Fn(&[u8]) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let (_, data) = recv_frame(node, Duration::from_secs(2)).await.unwrap();
            if pred(&data) {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "{what}");
        }
    }

    /// The show has one par on channel 1. Preset 0 sets it to full.
    fn par_show() -> serde_json::Value {
        serde_json::json!({
            "fixtures": [{"name": "par", "address": 1, "channels": ["dimmer"]}],
            "groups": [{"name": "all", "fixtures": [0]}],
            "looks": [{"rows": [{"group": 0, "color": [255, 255, 255, 255], "intensity": 255}]}],
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_scene_note_and_the_live_master_drive_the_frame() {
        let (node, dmx) = engine(par_show()).await;
        dmx.tx.try_send(DmxMsg::On(0)).unwrap();
        wait_frame(&node, "the look shows", |d| d[0] == 255).await;
        assert_eq!(dmx.status.lock().layers, [0]);
        assert_eq!(dmx.status.lock().master, 255);

        dmx.live.master.store(0, Ordering::Relaxed);
        wait_frame(&node, "the master is 0", |d| d[0] == 0).await;
        assert_eq!(dmx.status.lock().master, 0);

        dmx.live.strobe_hold.store(true, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(dmx.status.lock().strobe_hold);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_fog_and_aim_reach_the_frame() {
        let (node, dmx) = engine(par_show()).await;
        dmx.tx.try_send(DmxMsg::Test(0)).unwrap();
        wait_frame(&node, "the test shows the look", |d| d[0] == 255).await;

        let (node, dmx) = engine(serde_json::json!({
            "fixtures": [
                {"name": "fog", "address": 1, "channels": ["fog"]},
                {"name": "head", "address": 2, "channels": ["pan", "tilt"]},
            ],
        }))
        .await;
        dmx.tx.try_send(DmxMsg::Fog).unwrap();
        wait_frame(&node, "the fog blows", |d| d[0] > 0).await;
        dmx.tx
            .try_send(DmxMsg::Aim {
                fixture: 1,
                pan: 0xffff,
                tilt: 0,
            })
            .unwrap();
        wait_frame(&node, "the head aims", |d| d[1] == 255 && d[2] == 0).await;
    }
}
