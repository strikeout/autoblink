//! The metadata and the analysis of the loaded tracks, from the dbserver of the players.
//!
//! One thread fetches the tracks one after the other, because the ESP32 has little RAM and few
//! sockets. A fetch takes up to about 8 s. The host fetches in parallel.
//!
//! When a deck loads more tracks during a fetch, the thread fetches only the last one. When a
//! fetch fails, the thread tries again once after 3 s, if the deck still holds the track.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use xdj_core_dbserver::{asking_player, fetch_with, SongStructure, TrackMetadata, TrackOverview};
use xdj_core_prolink::TrackRef;

use crate::State;

const RETRY_DELAY: Duration = Duration::from_secs(3);

/// The firmware keeps the preview with this number of columns or fewer. A player sends 1200
/// columns of 6 bytes. 600 columns keep 3600 bytes of heap for each deck, and the page draws
/// them across the full width.
const PREVIEW_COLUMNS: usize = 600;

/// The fetched data of the track on one deck.
#[derive(Default)]
pub struct TrackData {
    pub rekordbox_id: u32,
    pub metadata: Option<TrackMetadata>,
    /// `None` while the fetch runs. `Err` holds the reason that no song structure is available.
    pub structure: Option<Result<SongStructure, String>>,
    pub overview: Option<Arc<TrackOverview>>,
}

/// The tracks by deck number.
#[derive(Default)]
pub struct Tracks(pub Mutex<HashMap<u8, TrackData>>);

/// Starts the fetch thread. Returns the sender for the track changes.
pub fn start(state: Arc<State>) -> anyhow::Result<Sender<TrackRef>> {
    let (tx, rx) = mpsc::channel::<TrackRef>();
    crate::spawn(c"dbserver", 7 * 1024, move || {
        while let Ok(first) = rx.recv() {
            // Keep only the last change of each deck. The older changes are stale.
            let mut latest = vec![first];
            for track in rx.try_iter() {
                latest.retain(|t| t.device != track.device);
                latest.push(track);
            }
            for track in latest {
                let mut tracks = state.tracks.0.lock().unwrap();
                if track.rekordbox_id == 0 {
                    tracks.remove(&track.device);
                    continue;
                }
                tracks.insert(track.device, TrackData { rekordbox_id: track.rekordbox_id, ..Default::default() });
                drop(tracks);
                if fetch_one(&state, track) {
                    continue;
                }
                thread::sleep(RETRY_DELAY);
                let loaded = state.tracker.lock().unwrap().deck(track.device).map(|d| d.track.rekordbox_id);
                if loaded == Some(track.rekordbox_id) {
                    log::info!("Deck {} track {}: the fetch starts again", track.device, track.rekordbox_id);
                    fetch_one(&state, track);
                }
            }
        }
    })?;
    Ok(tx)
}

/// Fetches one track and stores the result. Returns `false` when the fetch failed.
fn fetch_one(state: &State, track: TrackRef) -> bool {
    let (source, players) = {
        let tracker = state.tracker.lock().unwrap();
        // A track from rekordbox has a source player above 15, which is not a deck.
        let source = tracker.device_address(track.source_player).map(|(name, ip)| (ip, name.to_owned()));
        let players: Vec<(u8, u8)> =
            tracker.decks().iter().map(|d| (d.num, d.track.source_player)).collect();
        (source, players)
    };
    let result = match source {
        Some((ip, name)) if ip != [0; 4] => {
            let own = state.own_device.load(Ordering::Relaxed);
            let asking = asking_player(own, track.source_player, &name, players);
            // Connect on the interface of the virtual CDJ.
            let netif = state.link.lock().unwrap().map_or("", |l| l.netif);
            let connect = |addr: &_, timeout| crate::net::connect_on(netif, addr, timeout);
            fetch_with(&connect, ip.into(), asking, track.slot, track.track_type, track.rekordbox_id)
                .map_err(|e| e.to_string())
        }
        _ => Err(format!("player {} is not on the network", track.source_player)),
    };

    // The tracker checks the track ID itself. Lock it before the tracks, never inside them.
    if let Ok(f) = &result {
        if let Some(secs) = f.metadata.duration_s {
            state.tracker.lock().unwrap().set_track_length(track.device, track.rekordbox_id, secs);
        }
    }
    let mut tracks = state.tracks.0.lock().unwrap();
    // A newer track on the deck makes this result stale.
    let Some(data) = tracks.get_mut(&track.device).filter(|d| d.rekordbox_id == track.rekordbox_id) else {
        return true;
    };
    match result {
        Ok(f) => {
            for w in &f.warnings {
                log::warn!("Deck {} track {}: {w}", track.device, track.rekordbox_id);
            }
            log::info!("Deck {}: {} - {}", track.device, f.metadata.artist, f.metadata.title);
            data.metadata = Some(f.metadata);
            data.structure = Some(f.song_structure);
            data.overview = f.overview.map(|mut o| {
                o.preview = shrink(&o.preview);
                Arc::new(o)
            });
            true
        }
        Err(e) => {
            log::warn!("Deck {} track {}: the fetch failed: {e}", track.device, track.rekordbox_id);
            data.structure = Some(Err(e));
            false
        }
    }
}

/// Reduces the preview to `PREVIEW_COLUMNS` columns or fewer. Each new column has the largest
/// value of each byte of the columns that it replaces.
fn shrink(preview: &[u8]) -> Vec<u8> {
    let group = (preview.len() / 6).div_ceil(PREVIEW_COLUMNS).max(1) * 6;
    preview
        .chunks(group)
        .flat_map(|g| (0..6).map(move |b| g.iter().skip(b).step_by(6).copied().max().unwrap_or(0)))
        .collect()
}
