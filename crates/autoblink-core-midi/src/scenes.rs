//! The DMX scenes. The user maps the presets of a DMX controller to events of the master deck.
//! A preset is one MIDI note: the first note plus the preset index.
//!
//! The rows of the Phrases, Transport and Mixer groups set the current scene. The rows of the
//! Rhythm group (beat, downbeat, phrase change, fill-in) are overlays. An overlay does not
//! change the current scene.
//!
//! The beat count comes from the CDJ status, up to 200 ms after the beat. Thus, on a beat, the
//! engine predicts a phrase start and a fill-in start from the last status. The phrase rows fire
//! when the status shows the new phrase.

use serde::{Deserialize, Serialize};

use crate::Out;

/// The maximum number of presets.
pub const MAX_PRESETS: u8 = 32;
/// A row entry of this value and up is an extra note: `EXTRA_BASE` is `extra[0]`.
pub const EXTRA_BASE: u8 = 32;
/// The maximum number of extra notes.
const MAX_EXTRA: usize = 16;
/// The maximum number of presets in one row.
const MAX_ROW_PRESETS: usize = 16;
/// The maximum number of characters in a preset label.
const MAX_LABEL: usize = 12;
const MAX_DECAY_MS: u16 = 10_000;
const MAX_DECAY_BEATS: f64 = 32.0;
/// The permitted values of `Row::advance`. 0 steps at each trigger.
pub const ADVANCE_BARS: [u8; 7] = [0, 1, 2, 4, 8, 16, 32];

/// An event of the master deck. Each event has one row on the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    Intro,
    Groove,
    Build,
    Drop,
    Break,
    Outro,
    Beat,
    Downbeat,
    PhraseChange,
    Fill,
    Stop,
    Play,
    TrackLoad,
    MasterChange,
    BassCut,
    BassReturn,
}

const EVENT_COUNT: usize = 16;

impl Event {
    pub const ALL: [Event; EVENT_COUNT] = [
        Event::Intro,
        Event::Groove,
        Event::Build,
        Event::Drop,
        Event::Break,
        Event::Outro,
        Event::Beat,
        Event::Downbeat,
        Event::PhraseChange,
        Event::Fill,
        Event::Stop,
        Event::Play,
        Event::TrackLoad,
        Event::MasterChange,
        Event::BassCut,
        Event::BassReturn,
    ];

    /// Returns the bucket of a phrase kind code (Intro 0, Verse 1 to 6, Bridge 7, Chorus 8,
    /// Up 9, Down 10, Outro 11).
    pub fn bucket(code: u8) -> Option<Event> {
        match code {
            0 => Some(Event::Intro),
            1..=6 => Some(Event::Groove),
            7 | 9 => Some(Event::Build),
            8 => Some(Event::Drop),
            10 => Some(Event::Break),
            11 => Some(Event::Outro),
            _ => None,
        }
    }

    /// Returns true for the events of the phrase structure.
    pub fn is_phrase_event(self) -> bool {
        matches!(
            self,
            Event::Intro
                | Event::Groove
                | Event::Build
                | Event::Drop
                | Event::Break
                | Event::Outro
                | Event::PhraseChange
                | Event::Fill
        )
    }

    /// Returns true for the rows of the Rhythm group.
    pub fn is_overlay(self) -> bool {
        matches!(
            self,
            Event::Beat | Event::Downbeat | Event::PhraseChange | Event::Fill
        )
    }
}

/// The order of the steps in a row with more than one preset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    #[default]
    Up,
    PingPong,
    /// The next preset is random. The same preset does not come two times in sequence.
    Random,
}

/// How long the note of a preset stays on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    /// The note on and the note off go out together.
    #[default]
    Pulse,
    /// The note stays on until the next scene fires.
    Hold,
    /// The note goes off after the decay.
    Flash,
}

/// The decay of a flash, in milliseconds or in beats of the master.
///
/// The beats are `f64`. LLVM for Xtensa cannot compile the serde code of an `f32` here
/// ("Cannot select: XtensaISD::PCREL_WRAPPER").
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decay {
    Ms(u16),
    Beats(f64),
}

impl Default for Decay {
    fn default() -> Self {
        Decay::Ms(250)
    }
}

/// The presets of one event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub event: Event,
    /// The 0-based preset indices, in the order of the steps. An index of `EXTRA_BASE` and up
    /// is an extra note.
    pub presets: Vec<u8>,
    /// 0 gives the next preset at each trigger. N gives the next preset every N bars.
    #[serde(default)]
    pub advance: u8,
    #[serde(default)]
    pub order: Order,
    #[serde(default)]
    pub gate: Gate,
    #[serde(default)]
    pub decay: Decay,
    /// The hold of each entry of `presets`, in beats of the master. 0 or a missing entry means no
    /// hold. The beats are `f64` because LLVM for Xtensa cannot compile serde code for an `f32`.
    /// A row without holds leaves the key out, so that the JSON stays small.
    #[serde(default, skip_serializing_if = "no_holds")]
    pub holds: Vec<f64>,
}

fn no_holds(holds: &[f64]) -> bool {
    holds.iter().all(|&h| h == 0.0)
}

/// A note outside the range of the presets, with a name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extra {
    pub note: u8,
    #[serde(default)]
    pub label: String,
}

/// The scene settings. The page sends the full settings after each change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScenesConfig {
    pub enabled: bool,
    /// 0-based. The page shows 1 to 16.
    pub channel: u8,
    /// The note of preset 1.
    pub first_note: u8,
    /// The number of presets.
    pub count: u8,
    /// The controller shows one preset at a time. Its note off can then end a preset with a
    /// different note. When true, the engine sends no note off after a newer note on, and it
    /// sends the held note again after an overlay goes off.
    pub one_at_a_time: bool,
    /// The names of the presets. An empty name or a missing name shows no name.
    pub labels: Vec<String>,
    /// The notes outside the range of the presets.
    pub extra: Vec<Extra>,
    /// Only the rows with presets. Each event has one row at most.
    pub rows: Vec<Row>,
}

impl Default for ScenesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            channel: 0,
            first_note: 0,
            count: 16,
            one_at_a_time: true,
            labels: Vec::new(),
            extra: Vec::new(),
            rows: Vec::new(),
        }
    }
}

impl ScenesConfig {
    /// Makes the settings valid. It clamps the numbers and cuts the labels. It removes the
    /// presets out of range, the empty rows and a second row of an event.
    pub fn sanitize(&mut self) {
        self.channel = self.channel.min(15);
        self.count = self.count.clamp(1, MAX_PRESETS);
        self.first_note = self.first_note.min(128 - self.count);
        self.labels.truncate(usize::from(self.count));
        let clean = |label: &str| {
            label
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_LABEL)
                .collect()
        };
        for label in &mut self.labels {
            *label = clean(label);
        }
        self.extra.truncate(MAX_EXTRA);
        for x in &mut self.extra {
            x.note = x.note.min(127);
            x.label = clean(&x.label);
        }
        let count = self.count;
        let extras = self.extra.len();
        let mut seen = Vec::new();
        self.rows.retain_mut(|row| {
            // A hold stays with its preset, so the lists are cut together.
            row.holds.resize(row.presets.len(), 0.0);
            let valid =
                |p: u8| p < count || (p >= EXTRA_BASE && usize::from(p - EXTRA_BASE) < extras);
            let mut keep_entry = row.presets.iter().map(|&p| valid(p));
            row.holds.retain(|_| keep_entry.next().unwrap_or(false));
            row.presets.retain(|&p| valid(p));
            row.presets.truncate(MAX_ROW_PRESETS);
            row.holds.truncate(row.presets.len());
            for h in &mut row.holds {
                *h = if h.is_finite() && *h > 0.0 {
                    h.clamp(0.0625, MAX_DECAY_BEATS)
                } else {
                    0.0
                };
            }
            if !ADVANCE_BARS.contains(&row.advance) {
                row.advance = 0;
            }
            row.decay = match row.decay {
                Decay::Ms(ms) => Decay::Ms(ms.clamp(1, MAX_DECAY_MS)),
                Decay::Beats(b) if b.is_finite() => Decay::Beats(b.clamp(0.0625, MAX_DECAY_BEATS)),
                Decay::Beats(_) => Decay::default(),
            };
            let keep = !row.presets.is_empty() && !seen.contains(&row.event);
            seen.push(row.event);
            keep
        });
    }

    /// Returns the row of `event`, when it has presets.
    pub fn row(&self, event: Event) -> Option<&Row> {
        self.rows
            .iter()
            .find(|r| r.event == event && !r.presets.is_empty())
    }

    /// Returns the note of a 0-based preset index, or of an extra note.
    pub fn note(&self, preset: u8) -> u8 {
        match preset.checked_sub(EXTRA_BASE) {
            Some(k) => self.extra.get(usize::from(k)).map_or(0, |x| x.note),
            None => self.first_note.saturating_add(preset).min(127),
        }
    }
}

/// The phrase of the master deck, for the scenes.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ScenePhrase {
    /// The index of the phrase in the song structure.
    pub idx: usize,
    /// The kind code. `None` for a kind that has no code.
    pub code: Option<u8>,
    /// The beats until the next phrase starts. 1 on the last beat of the phrase.
    pub beats_left: u32,
    /// The beats until the fill-in starts. The value is 0 in the fill-in, and `None` for a
    /// phrase without a fill-in.
    pub fill_beats_left: Option<u32>,
}

/// The state of the master deck, for the scenes.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SceneInput {
    /// The player number of the master. 0 means no master.
    pub device: u8,
    /// The rekordbox ID of the master track. 0 means no track.
    pub track: u32,
    pub playing: bool,
    pub bpm: f64,
    pub phrase: Option<ScenePhrase>,
    /// True while the bass is cut on all live mixer channels. It is `None` without mixer data.
    pub bass_cut: Option<bool>,
}

/// The current scene, for the page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SceneStatus {
    /// The row of the current scene.
    pub row: Option<Event>,
    /// The position in the presets of the row that is on now.
    pub step: Option<u8>,
    /// The position of the next preset. `None` for a random order.
    pub next: Option<u8>,
    /// The bars until the next step, for a row that steps every N bars.
    pub bars_left: Option<u8>,
    /// The phrase bucket of the master deck now.
    pub phrase: Option<Event>,
    /// The number of times that each event occurred, in the order of `Event::ALL`. Each event
    /// counts, also without presets in its row. The count wraps at 256. The page flashes the
    /// title of a row when its count changes.
    pub events: [u8; EVENT_COUNT],
    /// The position of the preset that each row sent last, in the order of `Event::ALL`.
    pub last: [Option<u8>; EVENT_COUNT],
}

#[derive(Debug, Clone, Copy, Default)]
struct Cursor {
    /// The position of the preset that fires next.
    pos: u8,
    /// True while a ping-pong row goes down.
    back: bool,
    /// The downbeats since the last trigger or step.
    bars: u8,
    /// The time of the last trigger.
    fired_ms: u64,
    /// The hold that keeps the row busy: (end time, channel, note).
    hold: Option<(u64, u8, u8)>,
    /// A trigger came while the row was busy. It fires when the hold ends.
    pending: bool,
    /// A bar step is among the pending triggers. The row advances before it fires.
    pending_step: bool,
}

/// The scene engine has no clock and no I/O, like the mapper.
#[derive(Debug, Clone)]
pub struct Scenes {
    cursors: [Cursor; EVENT_COUNT],
    rng: u32,
    current: Option<Event>,
    current_step: u8,
    /// The note of a hold row that is on: (channel, note).
    held: Option<(u8, u8)>,
    /// The flash notes that are on: (note off time, channel, note).
    flashes: Vec<(u64, u8, u8)>,
    prev: SceneInput,
    /// The (device, track, phrase) that the phrase row fired for.
    phrase_key: Option<(u8, u32, usize)>,
    /// The (device, track, phrase) that the fill-in row fired for.
    fill_key: Option<(u8, u32, usize)>,
    /// The phrase index that a beat predicted. The status change to it fires no overlay.
    predicted_idx: Option<usize>,
    /// The time of the last phrase change or fill-in overlay from the status.
    status_overlay_ms: Option<u64>,
    ms_per_beat: f64,
    events: [u8; EVENT_COUNT],
    last: [Option<u8>; EVENT_COUNT],
    /// The presets that rows sent since the last `drain_fired`: (event, preset entry).
    fired: Vec<(Event, u8)>,
}

impl Default for Scenes {
    fn default() -> Self {
        Self {
            cursors: [Cursor::default(); EVENT_COUNT],
            rng: 0x2545_f491,
            current: None,
            current_step: 0,
            held: None,
            flashes: Vec::new(),
            prev: SceneInput::default(),
            phrase_key: None,
            fill_key: None,
            predicted_idx: None,
            status_overlay_ms: None,
            ms_per_beat: 500.0,
            events: [0; EVENT_COUNT],
            last: [None; EVENT_COUNT],
            fired: Vec::with_capacity(8),
        }
    }
}

/// The most entries that `Scenes::fired` keeps.
const FIRED_MAX: usize = 16;

fn index(e: Event) -> usize {
    e as usize
}

impl Scenes {
    /// Call on each beat of the master. It fires one overlay: the fill-in, the phrase change, the
    /// downbeat or the beat, in this order of priority. Only a row with presets counts. On beat 1,
    /// it also steps the rows that step every N bars.
    pub fn beat(
        &mut self,
        cfg: &ScenesConfig,
        now_ms: u64,
        beat_in_bar: u8,
        out: &mut impl FnMut(Out),
    ) {
        let key = self
            .prev
            .phrase
            .map(|p| (self.prev.device, self.prev.track, p.idx));
        let p = self.prev.phrase;
        // An overlay from the status in the last half beat belongs to this beat.
        let from_status = self
            .status_overlay_ms
            .take()
            .is_some_and(|t| now_ms.saturating_sub(t) < (self.ms_per_beat / 2.0) as u64);
        let fill_next = p.is_some_and(|p| p.fill_beats_left == Some(1)) && self.fill_key != key;
        let change_next = p.is_some_and(|p| p.beats_left == 1) && self.predicted_idx.is_none();

        self.count(Event::Beat);
        if beat_in_bar == 1 {
            self.count(Event::Downbeat);
        }
        if fill_next {
            self.fill_key = key;
            self.count(Event::Fill);
        }
        if change_next {
            self.predicted_idx = p.map(|p| p.idx + 1);
            self.count(Event::PhraseChange);
        }

        let has = |e| cfg.row(e).is_some();
        let overlay = if from_status {
            None
        } else if fill_next && has(Event::Fill) {
            Some(Event::Fill)
        } else if change_next && has(Event::PhraseChange) {
            Some(Event::PhraseChange)
        } else if beat_in_bar == 1 && has(Event::Downbeat) {
            Some(Event::Downbeat)
        } else {
            Some(Event::Beat)
        };

        if beat_in_bar == 1 && cfg.enabled {
            self.step_bars(cfg, now_ms, out);
        }
        if let Some(e) = overlay {
            self.fire(cfg, e, now_ms, out);
        }
    }

    /// Call after each status change and at least every 10 ms. It fires the rows of the
    /// phrases, the transport and the mixer. It sends the note off of each flash that ended.
    /// The engine counts the events also while the scenes are off.
    pub fn update(
        &mut self,
        cfg: &ScenesConfig,
        now_ms: u64,
        s: &SceneInput,
        out: &mut impl FnMut(Out),
    ) {
        self.tick(cfg, now_ms, out);
        if !cfg.enabled {
            self.release_all(out);
            self.current = None;
        }
        if s.bpm > 0.0 {
            self.ms_per_beat = 60_000.0 / s.bpm;
        }
        let p = self.prev;

        if s.device != 0 && p.device != 0 && s.device != p.device {
            self.event(cfg, Event::MasterChange, now_ms, out);
        } else if s.track != 0 && s.track != p.track && s.device == p.device {
            self.event(cfg, Event::TrackLoad, now_ms, out);
        }
        if s.device != 0 && s.playing && !p.playing {
            self.event(cfg, Event::Play, now_ms, out);
            if cfg.row(Event::Play).is_none() {
                // Without a play row, the phrase row fires again below.
                self.phrase_key = None;
            }
        } else if !s.playing && p.playing {
            self.event(cfg, Event::Stop, now_ms, out);
        }
        if let (Some(was), Some(cut)) = (p.bass_cut, s.bass_cut) {
            if was != cut {
                let e = if cut {
                    Event::BassCut
                } else {
                    Event::BassReturn
                };
                self.event(cfg, e, now_ms, out);
            }
        }

        if let (Some(a), Some(b)) = (p.phrase, s.phrase) {
            if a.idx != b.idx {
                if self.predicted_idx != Some(b.idx) {
                    self.count(Event::PhraseChange);
                    if cfg.row(Event::PhraseChange).is_some() {
                        self.fire(cfg, Event::PhraseChange, now_ms, out);
                        self.status_overlay_ms = Some(now_ms);
                    }
                }
                self.predicted_idx = None;
            }
        }
        match s.phrase {
            Some(ph) => {
                let key = Some((s.device, s.track, ph.idx));
                if self.phrase_key != key {
                    self.phrase_key = key;
                    if let Some(e) = ph.code.and_then(Event::bucket) {
                        self.event(cfg, e, now_ms, out);
                    }
                }
                if ph.fill_beats_left == Some(0) && self.fill_key != key {
                    self.fill_key = key;
                    self.count(Event::Fill);
                    if cfg.row(Event::Fill).is_some() {
                        self.fire(cfg, Event::Fill, now_ms, out);
                        self.status_overlay_ms = Some(now_ms);
                    }
                }
            }
            None => self.phrase_key = None,
        }
        self.prev = *s;
    }

    /// Returns the time of the next flash note off, so that the caller can wake up for it.
    pub fn next_off_ms(&self) -> Option<u64> {
        let holds = self.cursors.iter().filter_map(|c| c.hold.map(|h| h.0));
        self.flashes.iter().map(|f| f.0).chain(holds).min()
    }

    /// Returns the current scene for the page.
    pub fn status(&self, cfg: &ScenesConfig) -> SceneStatus {
        let phrase = self
            .prev
            .phrase
            .and_then(|p| p.code)
            .and_then(Event::bucket);
        let Some((e, row)) = self.current.and_then(|e| Some((e, cfg.row(e)?))) else {
            return SceneStatus {
                phrase,
                events: self.events,
                last: self.last,
                ..SceneStatus::default()
            };
        };
        let len = row.presets.len() as u8;
        let c = self.cursors[index(e)];
        let next = match row.order {
            Order::Random => None,
            _ if row.advance == 0 => Some(c.pos % len),
            order => Some(peek(c, order, len)),
        };
        SceneStatus {
            row: Some(e),
            step: Some(self.current_step.min(len - 1)),
            next,
            bars_left: (row.advance > 0).then(|| row.advance.saturating_sub(c.bars)),
            phrase,
            events: self.events,
            last: self.last,
        }
    }

    fn count(&mut self, e: Event) {
        let n = &mut self.events[index(e)];
        *n = n.wrapping_add(1);
    }

    /// Counts the event and fires its row.
    fn event(&mut self, cfg: &ScenesConfig, e: Event, now_ms: u64, out: &mut impl FnMut(Out)) {
        self.count(e);
        self.fire(cfg, e, now_ms, out);
    }

    /// Fires the current preset of a row, and moves the row to its next preset.
    fn fire(&mut self, cfg: &ScenesConfig, e: Event, now_ms: u64, out: &mut impl FnMut(Out)) {
        if !cfg.enabled || cfg.row(e).is_none() {
            return;
        }
        let c = &mut self.cursors[index(e)];
        if c.hold.is_some() {
            c.pending = true;
            return;
        }
        self.fire_now(cfg, e, now_ms, out);
    }

    /// Fires the row at once, also when it is busy.
    fn fire_now(&mut self, cfg: &ScenesConfig, e: Event, now_ms: u64, out: &mut impl FnMut(Out)) {
        let Some(row) = cfg.row(e) else { return };
        let len = row.presets.len() as u8;
        let c = &mut self.cursors[index(e)];
        let step = c.pos % len;
        c.bars = 0;
        c.fired_ms = now_ms;
        if row.advance == 0 {
            advance(c, row.order, len, &mut self.rng);
        }
        self.send(cfg, row, step, now_ms, !e.is_overlay(), out);
        if !e.is_overlay() {
            self.current = Some(e);
            self.current_step = step;
        }
    }

    /// Counts a bar in each row that steps every N bars. The current scene sends its new preset.
    fn step_bars(&mut self, cfg: &ScenesConfig, now_ms: u64, out: &mut impl FnMut(Out)) {
        let half_beat = (self.ms_per_beat / 2.0) as u64;
        for row in cfg
            .rows
            .iter()
            .filter(|r| r.advance > 0 && !r.presets.is_empty())
        {
            let len = row.presets.len() as u8;
            let c = &mut self.cursors[index(row.event)];
            // The bar of the trigger itself does not count.
            if c.fired_ms != 0 && now_ms.saturating_sub(c.fired_ms) < half_beat {
                continue;
            }
            c.bars += 1;
            if c.bars < row.advance {
                continue;
            }
            c.bars = 0;
            if c.hold.is_some() && self.current == Some(row.event) {
                c.pending = true;
                c.pending_step = true;
                continue;
            }
            advance(c, row.order, len, &mut self.rng);
            let step = c.pos % len;
            if self.current == Some(row.event) {
                self.current_step = step;
                self.send(cfg, row, step, now_ms, true, out);
            }
        }
    }

    fn send(
        &mut self,
        cfg: &ScenesConfig,
        row: &Row,
        step: u8,
        now_ms: u64,
        scene: bool,
        out: &mut impl FnMut(Out),
    ) {
        self.last[index(row.event)] = Some(step);
        if self.fired.len() >= FIRED_MAX {
            self.fired.remove(0);
        }
        self.fired.push((row.event, row.presets[usize::from(step)]));
        let ch = cfg.channel;
        let note = cfg.note(row.presets[usize::from(step)]);
        // A flash of the same note stays on. The new flash replaces its off time.
        // With one preset at a time, a flash of a different note ends before the new note starts.
        let held = self.held;
        self.flashes.retain(|&(_, c, n)| {
            if (c, n) == (ch, note) {
                return false;
            }
            if !cfg.one_at_a_time {
                return true;
            }
            if held != Some((c, n)) {
                out(Out::NoteOff { ch: c, note: n });
            }
            false
        });
        let prev = if scene || row.gate == Gate::Hold {
            self.held.take()
        } else {
            None
        };
        // A note off before a note on of the same note makes the light go out for a moment.
        if let Some(p) = prev.filter(|&p| p != (ch, note)) {
            out(Out::NoteOff { ch: p.0, note: p.1 });
        }
        out(Out::NoteOn { ch, note, vel: 127 });
        let hold = row.holds.get(usize::from(step)).copied().unwrap_or(0.0);
        if hold > 0.0 {
            let end = now_ms + (hold * self.ms_per_beat).max(1.0) as u64;
            self.cursors[index(row.event)].hold = Some((end, ch, note));
            return;
        }
        match row.gate {
            Gate::Pulse => {
                out(Out::NoteOff { ch, note });
                self.restore_held(cfg, out);
            }
            Gate::Hold => self.held = Some((ch, note)),
            Gate::Flash => {
                let ms = match row.decay {
                    Decay::Ms(ms) => f64::from(ms),
                    Decay::Beats(b) => b * self.ms_per_beat,
                };
                self.flashes.push((now_ms + ms.max(1.0) as u64, ch, note));
            }
        }
    }

    /// Calls `f(event, preset)` for each preset that a row sent since the last call, in order.
    /// The list keeps the newest 16 entries.
    pub fn drain_fired(&mut self, mut f: impl FnMut(Event, u8)) {
        for (e, p) in self.fired.drain(..) {
            f(e, p);
        }
    }

    /// Sends the note off of each flash that ended. A note that a hold row keeps on stays on.
    fn tick(&mut self, cfg: &ScenesConfig, now_ms: u64, out: &mut impl FnMut(Out)) {
        let held = self.held;
        let mut ended = false;
        self.flashes.retain(|&(t, ch, note)| {
            if t > now_ms {
                return true;
            }
            if held != Some((ch, note)) {
                out(Out::NoteOff { ch, note });
                ended = true;
            }
            false
        });
        if ended {
            self.restore_held(cfg, out);
        }
        for (i, e) in Event::ALL.into_iter().enumerate() {
            let c = &mut self.cursors[i];
            let Some((end, ch, note)) = c.hold else {
                continue;
            };
            if end > now_ms {
                continue;
            }
            c.hold = None;
            let pending = std::mem::take(&mut c.pending);
            let step = std::mem::take(&mut c.pending_step);
            if held != Some((ch, note)) {
                out(Out::NoteOff { ch, note });
            }
            self.restore_held(cfg, out);
            // A scene row that is no longer the current scene drops its queued fire.
            if pending && cfg.enabled && (e.is_overlay() || self.current == Some(e)) {
                if let Some(row) = cfg.row(e).filter(|r| step && r.advance > 0) {
                    let c = &mut self.cursors[i];
                    advance(c, row.order, row.presets.len() as u8, &mut self.rng);
                }
                self.fire_now(cfg, e, now_ms, out);
            }
        }
    }

    /// Sends the held note again after an overlay went off. Only with one preset at a time.
    fn restore_held(&self, cfg: &ScenesConfig, out: &mut impl FnMut(Out)) {
        if !cfg.one_at_a_time {
            return;
        }
        // A hold entry of a scene row is on too, and it shows the preset. Any newer held note
        // took its place in `held` when it fired.
        let hold = self
            .cursors
            .iter()
            .zip(Event::ALL)
            .filter(|(c, e)| c.hold.is_some() && !e.is_overlay())
            .max_by_key(|(c, _)| c.fired_ms)
            .and_then(|(c, _)| c.hold.map(|h| (h.1, h.2)));
        if let Some((ch, note)) = self.held.or(hold) {
            out(Out::NoteOn { ch, note, vel: 127 });
        }
    }

    fn release_all(&mut self, out: &mut impl FnMut(Out)) {
        for (_, ch, note) in self.flashes.drain(..) {
            out(Out::NoteOff { ch, note });
        }
        if let Some((ch, note)) = self.held.take() {
            out(Out::NoteOff { ch, note });
        }
        for c in &mut self.cursors {
            if let Some((_, ch, note)) = c.hold.take() {
                out(Out::NoteOff { ch, note });
            }
            c.pending = false;
            c.pending_step = false;
        }
    }
}

/// Returns the next position without a change to the cursor. Not for a random order.
fn peek(c: Cursor, order: Order, len: u8) -> u8 {
    let mut c = c;
    advance(&mut c, order, len, &mut 1);
    c.pos % len
}

fn advance(c: &mut Cursor, order: Order, len: u8, rng: &mut u32) {
    let pos = c.pos % len;
    if len <= 1 {
        c.pos = 0;
        return;
    }
    c.pos = match order {
        Order::Up => (pos + 1) % len,
        Order::PingPong => {
            if pos == 0 {
                c.back = false;
            } else if pos == len - 1 {
                c.back = true;
            }
            if c.back {
                pos - 1
            } else {
                pos + 1
            }
        }
        Order::Random => {
            // xorshift32
            *rng ^= *rng << 13;
            *rng ^= *rng >> 17;
            *rng ^= *rng << 5;
            let r = (*rng % u32::from(len - 1)) as u8;
            if r >= pos {
                r + 1
            } else {
                r
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(event: Event, presets: &[u8]) -> Row {
        Row {
            event,
            presets: presets.to_vec(),
            advance: 0,
            order: Order::Up,
            gate: Gate::Pulse,
            decay: Decay::default(),
            holds: Vec::new(),
        }
    }

    fn cfg(rows: Vec<Row>) -> ScenesConfig {
        ScenesConfig {
            enabled: true,
            channel: 9,
            rows,
            ..ScenesConfig::default()
        }
    }

    fn notes(out: &[Out]) -> Vec<u8> {
        out.iter()
            .filter_map(|o| match *o {
                Out::Note { note, .. } | Out::NoteOn { note, .. } => Some(note),
                _ => None,
            })
            .collect()
    }

    fn input(idx: usize, code: u8, beats_left: u32) -> SceneInput {
        SceneInput {
            device: 1,
            track: 7,
            playing: true,
            bpm: 120.0,
            phrase: Some(ScenePhrase {
                idx,
                code: Some(code),
                beats_left,
                fill_beats_left: None,
            }),
            bass_cut: None,
        }
    }

    #[test]
    fn drain_fired_reports_event_and_preset() {
        let c = cfg(vec![row(Event::Drop, &[3, 4])]);
        let mut s = Scenes::default();
        s.update(&c, 1000, &input(0, 8, 16), &mut |_| {});
        let mut got = Vec::new();
        s.drain_fired(|e, p| got.push((e, p)));
        assert_eq!(got, vec![(Event::Drop, 3)]);
        let mut again = Vec::new();
        s.drain_fired(|e, p| again.push((e, p)));
        assert!(again.is_empty());
    }

    #[test]
    fn drain_fired_reports_bar_steps() {
        let mut drop = row(Event::Drop, &[0, 1]);
        drop.advance = 1;
        let c = cfg(vec![drop]);
        let mut s = Scenes::default();
        s.update(&c, 1000, &input(0, 8, 32), &mut |_| {});
        s.beat(&c, 3000, 1, &mut |_| {});
        let mut got = Vec::new();
        s.drain_fired(|e, p| got.push((e, p)));
        assert_eq!(got, vec![(Event::Drop, 0), (Event::Drop, 1)]);
    }

    #[test]
    fn drain_fired_keeps_16_and_skips_disabled() {
        let mut c = cfg(vec![row(Event::Beat, &[1])]);
        let mut s = Scenes::default();
        for i in 0..20 {
            s.beat(&c, 1000 + i * 500, 2, &mut |_| {});
        }
        let mut n = 0;
        s.drain_fired(|_, _| n += 1);
        assert_eq!(n, 16);
        c.enabled = false;
        s.beat(&c, 20000, 2, &mut |_| {});
        s.drain_fired(|_, _| n += 1);
        assert_eq!(n, 16);
    }

    #[test]
    fn phrase_events() {
        assert!(Event::Drop.is_phrase_event());
        assert!(Event::Fill.is_phrase_event());
        assert!(!Event::Beat.is_phrase_event());
        assert!(!Event::Stop.is_phrase_event());
    }

    #[test]
    fn buckets_fold_the_rekordbox_kinds() {
        let kinds: Vec<_> = (0..=12).map(Event::bucket).collect();
        use Event::*;
        assert_eq!(
            kinds,
            [
                Some(Intro),
                Some(Groove),
                Some(Groove),
                Some(Groove),
                Some(Groove),
                Some(Groove),
                Some(Groove),
                Some(Build),
                Some(Drop),
                Some(Build),
                Some(Break),
                Some(Outro),
                None
            ]
        );
    }

    #[test]
    fn the_orders_step_through_the_presets() {
        let mut rng = 1;
        let run = |order, rng: &mut u32| {
            let mut c = Cursor::default();
            (0..7)
                .map(|_| {
                    let p = c.pos;
                    advance(&mut c, order, 4, rng);
                    p
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(run(Order::Up, &mut rng), [0, 1, 2, 3, 0, 1, 2]);
        assert_eq!(run(Order::PingPong, &mut rng), [0, 1, 2, 3, 2, 1, 0]);
        let r = run(Order::Random, &mut rng);
        assert!(r.windows(2).all(|w| w[0] != w[1] && w[1] < 4));
    }

    #[test]
    fn a_phrase_bucket_fires_once_and_steps_on_each_trigger() {
        let cfg = cfg(vec![row(Event::Drop, &[4, 5])]);
        let mut s = Scenes::default();
        let mut out = Vec::new();
        s.update(&cfg, 10, &input(0, 8, 32), &mut |o| out.push(o));
        s.update(&cfg, 20, &input(0, 8, 31), &mut |o| out.push(o));
        assert_eq!(
            out,
            [
                Out::NoteOn {
                    ch: 9,
                    note: 4,
                    vel: 127
                },
                Out::NoteOff { ch: 9, note: 4 }
            ]
        );
        s.update(&cfg, 30, &input(1, 0, 32), &mut |_| {});
        s.update(&cfg, 40, &input(2, 8, 32), &mut |o| out.push(o));
        assert_eq!(notes(&out), [4, 5]);
        assert_eq!(s.status(&cfg).row, Some(Event::Drop));
    }

    #[test]
    fn a_hold_scene_goes_off_when_the_next_scene_fires() {
        let mut held = row(Event::Intro, &[0]);
        held.gate = Gate::Hold;
        let cfg = cfg(vec![held, row(Event::Drop, &[3])]);
        let mut s = Scenes::default();
        let mut out = Vec::new();
        s.update(&cfg, 10, &input(0, 0, 32), &mut |o| out.push(o));
        s.update(&cfg, 20, &input(1, 8, 32), &mut |o| out.push(o));
        assert_eq!(
            out,
            [
                Out::NoteOn {
                    ch: 9,
                    note: 0,
                    vel: 127
                },
                Out::NoteOff { ch: 9, note: 0 },
                Out::NoteOn {
                    ch: 9,
                    note: 3,
                    vel: 127
                },
                Out::NoteOff { ch: 9, note: 3 },
            ]
        );
    }

    #[test]
    fn a_hold_on_each_beat_sends_the_note_off_before_the_next_note_on() {
        let mut beat = row(Event::Beat, &[1, 2]);
        beat.gate = Gate::Hold;
        let cfg = cfg(vec![beat]);
        let mut s = Scenes::default();
        s.update(&cfg, 0, &input(0, 0, 32), &mut |_| {});
        let mut out = Vec::new();
        s.beat(&cfg, 500, 2, &mut |o| out.push(o));
        s.beat(&cfg, 1000, 3, &mut |o| out.push(o));
        assert_eq!(
            out,
            [
                Out::NoteOn {
                    ch: 9,
                    note: 1,
                    vel: 127
                },
                Out::NoteOff { ch: 9, note: 1 },
                Out::NoteOn {
                    ch: 9,
                    note: 2,
                    vel: 127
                },
            ]
        );
    }

    #[test]
    fn a_long_flash_goes_off_before_the_next_note_and_never_after_it() {
        let mut beat = row(Event::Beat, &[1, 2]);
        beat.gate = Gate::Flash;
        beat.decay = Decay::Beats(2.0);
        let cfg = cfg(vec![beat]);
        let mut s = Scenes::default();
        s.update(&cfg, 0, &input(0, 0, 32), &mut |_| {});
        let mut out = Vec::new();
        s.beat(&cfg, 500, 2, &mut |o| out.push(o));
        s.beat(&cfg, 1000, 3, &mut |o| out.push(o));
        s.update(&cfg, 1999, &input(0, 0, 30), &mut |o| out.push(o));
        assert_eq!(
            out,
            [
                Out::NoteOn {
                    ch: 9,
                    note: 1,
                    vel: 127
                },
                Out::NoteOff { ch: 9, note: 1 },
                Out::NoteOn {
                    ch: 9,
                    note: 2,
                    vel: 127
                },
            ]
        );
        out.clear();
        // Two beats at 120 BPM are 1000 ms.
        s.update(&cfg, 2000, &input(0, 0, 30), &mut |o| out.push(o));
        assert_eq!(out, [Out::NoteOff { ch: 9, note: 2 }]);
    }

    #[test]
    fn with_layers_a_long_flash_keeps_its_decay_and_the_held_note_is_not_sent_again() {
        let mut held = row(Event::Intro, &[0]);
        held.gate = Gate::Hold;
        let mut beat = row(Event::Beat, &[1, 2]);
        beat.gate = Gate::Flash;
        beat.decay = Decay::Beats(2.0);
        let mut cfg = cfg(vec![held, beat]);
        cfg.one_at_a_time = false;
        let mut s = Scenes::default();
        s.update(&cfg, 0, &input(0, 0, 32), &mut |_| {});
        let mut out = Vec::new();
        s.beat(&cfg, 500, 2, &mut |o| out.push(o));
        s.beat(&cfg, 1000, 3, &mut |o| out.push(o));
        s.update(&cfg, 1500, &input(0, 0, 30), &mut |o| out.push(o));
        assert_eq!(
            out,
            [
                Out::NoteOn {
                    ch: 9,
                    note: 1,
                    vel: 127
                },
                Out::NoteOn {
                    ch: 9,
                    note: 2,
                    vel: 127
                },
                Out::NoteOff { ch: 9, note: 1 },
            ]
        );
    }

    #[test]
    fn the_held_scene_comes_back_after_an_overlay() {
        let mut held = row(Event::Intro, &[0]);
        held.gate = Gate::Hold;
        let mut beat = row(Event::Beat, &[5]);
        beat.gate = Gate::Flash;
        beat.decay = Decay::Ms(100);
        let cfg = cfg(vec![held, row(Event::Downbeat, &[6]), beat]);
        let mut s = Scenes::default();
        let mut out = Vec::new();
        s.update(&cfg, 0, &input(0, 0, 32), &mut |_| {});
        s.beat(&cfg, 500, 2, &mut |o| out.push(o));
        s.update(&cfg, 600, &input(0, 0, 31), &mut |o| out.push(o));
        s.beat(&cfg, 1000, 1, &mut |o| out.push(o));
        assert_eq!(
            out,
            [
                Out::NoteOn {
                    ch: 9,
                    note: 5,
                    vel: 127
                },
                Out::NoteOff { ch: 9, note: 5 },
                Out::NoteOn {
                    ch: 9,
                    note: 0,
                    vel: 127
                },
                Out::NoteOn {
                    ch: 9,
                    note: 6,
                    vel: 127
                },
                Out::NoteOff { ch: 9, note: 6 },
                Out::NoteOn {
                    ch: 9,
                    note: 0,
                    vel: 127
                },
            ]
        );
    }

    #[test]
    fn the_same_note_on_each_beat_never_goes_off() {
        let mut hold = row(Event::Beat, &[1]);
        hold.gate = Gate::Hold;
        let mut flash = row(Event::Downbeat, &[2]);
        flash.gate = Gate::Flash;
        flash.decay = Decay::Ms(5000);
        let cfg = cfg(vec![hold, flash]);
        let mut s = Scenes::default();
        s.update(&cfg, 0, &input(0, 0, 32), &mut |_| {});
        let mut out = Vec::new();
        s.beat(&cfg, 500, 2, &mut |o| out.push(o));
        s.beat(&cfg, 1000, 3, &mut |o| out.push(o));
        s.beat(&cfg, 2000, 1, &mut |o| out.push(o));
        s.beat(&cfg, 4000, 1, &mut |o| out.push(o));
        assert!(!out.contains(&Out::NoteOff { ch: 9, note: 1 }));
        assert!(!out.contains(&Out::NoteOff { ch: 9, note: 2 }));
        assert_eq!(s.next_off_ms(), Some(9000));
    }

    #[test]
    fn a_flash_goes_off_after_its_decay() {
        let mut beat = row(Event::Beat, &[11]);
        beat.gate = Gate::Flash;
        beat.decay = Decay::Beats(0.5);
        let cfg = cfg(vec![beat]);
        let mut s = Scenes::default();
        s.update(&cfg, 0, &input(0, 0, 32), &mut |_| {});
        let mut out = Vec::new();
        s.beat(&cfg, 1000, 2, &mut |o| out.push(o));
        assert_eq!(
            out,
            [Out::NoteOn {
                ch: 9,
                note: 11,
                vel: 127
            }]
        );
        // Half a beat at 120 BPM is 250 ms.
        assert_eq!(s.next_off_ms(), Some(1250));
        out.clear();
        s.update(&cfg, 1249, &input(0, 0, 31), &mut |o| out.push(o));
        assert!(out.is_empty());
        s.update(&cfg, 1250, &input(0, 0, 31), &mut |o| out.push(o));
        assert_eq!(out, [Out::NoteOff { ch: 9, note: 11 }]);
    }

    #[test]
    fn the_most_specific_overlay_wins_on_a_beat() {
        let cfg = cfg(vec![
            row(Event::Beat, &[1]),
            row(Event::Downbeat, &[2]),
            row(Event::PhraseChange, &[3]),
        ]);
        let mut s = Scenes::default();
        let mut out = Vec::new();
        s.update(&cfg, 0, &input(0, 0, 5), &mut |_| {});
        s.beat(&cfg, 500, 2, &mut |o| out.push(o));
        s.beat(&cfg, 1000, 1, &mut |o| out.push(o));
        assert_eq!(notes(&out), [1, 2]);
        // The status shows the last beat of the phrase. The next beat starts the next phrase.
        out.clear();
        s.update(&cfg, 1100, &input(0, 0, 1), &mut |_| {});
        s.beat(&cfg, 1500, 1, &mut |o| out.push(o));
        assert_eq!(notes(&out), [3]);
        // The status then shows the new phrase. The overlay does not fire two times.
        out.clear();
        s.update(&cfg, 1600, &input(1, 0, 32), &mut |o| out.push(o));
        assert!(out.is_empty());
    }

    #[test]
    fn a_phrase_change_from_the_status_suppresses_the_downbeat_of_the_same_beat() {
        let cfg = cfg(vec![
            row(Event::Downbeat, &[2]),
            row(Event::PhraseChange, &[3]),
        ]);
        let mut s = Scenes::default();
        let mut out = Vec::new();
        s.update(&cfg, 0, &input(0, 0, 9), &mut |_| {});
        s.update(&cfg, 990, &input(1, 0, 32), &mut |o| out.push(o));
        s.beat(&cfg, 1000, 1, &mut |o| out.push(o));
        assert_eq!(notes(&out), [3]);
        // The next downbeat fires again.
        s.beat(&cfg, 3000, 1, &mut |o| out.push(o));
        assert_eq!(notes(&out), [3, 2]);
    }

    #[test]
    fn a_row_steps_every_n_bars_while_it_is_the_scene() {
        let mut drop = row(Event::Drop, &[4, 5, 6]);
        drop.advance = 2;
        let cfg = cfg(vec![drop]);
        let mut s = Scenes::default();
        let mut out = Vec::new();
        // The phrase starts on a downbeat. That bar does not count.
        s.update(&cfg, 1000, &input(0, 8, 32), &mut |o| out.push(o));
        s.beat(&cfg, 1050, 1, &mut |o| out.push(o));
        assert_eq!(s.status(&cfg).bars_left, Some(2));
        for bar in 1..=4 {
            s.beat(&cfg, 1000 + bar * 2000, 1, &mut |o| out.push(o));
        }
        assert_eq!(notes(&out), [4, 5, 6]);
        let st = s.status(&cfg);
        assert_eq!(
            (st.row, st.step, st.next, st.bars_left),
            (Some(Event::Drop), Some(2), Some(0), Some(2))
        );
        assert_eq!(st.last[index(Event::Drop)], Some(2));
        assert_eq!(st.phrase, Some(Event::Drop));
    }

    #[test]
    fn the_fill_in_fires_on_its_beat_or_from_the_status() {
        let cfg = cfg(vec![row(Event::Fill, &[8]), row(Event::Beat, &[1])]);
        let mut s = Scenes::default();
        let mut out = Vec::new();
        let fill = |left| SceneInput {
            phrase: Some(ScenePhrase {
                fill_beats_left: Some(left),
                ..input(0, 0, 8).phrase.unwrap()
            }),
            ..input(0, 0, 8)
        };
        s.update(&cfg, 0, &fill(1), &mut |_| {});
        s.beat(&cfg, 500, 1, &mut |o| out.push(o));
        s.update(&cfg, 600, &fill(0), &mut |o| out.push(o));
        assert_eq!(notes(&out), [8]);
    }

    #[test]
    fn transport_and_mixer_edges_fire_their_rows() {
        let cfg = cfg(vec![
            row(Event::Stop, &[14]),
            row(Event::BassCut, &[9]),
            row(Event::TrackLoad, &[3]),
        ]);
        let mut s = Scenes::default();
        let mut out = Vec::new();
        let base = SceneInput {
            device: 1,
            track: 7,
            playing: true,
            bpm: 120.0,
            phrase: None,
            bass_cut: Some(false),
        };
        s.update(&cfg, 0, &base, &mut |_| {});
        s.update(
            &cfg,
            10,
            &SceneInput {
                bass_cut: Some(true),
                ..base
            },
            &mut |o| out.push(o),
        );
        s.update(
            &cfg,
            20,
            &SceneInput {
                playing: false,
                bass_cut: Some(true),
                ..base
            },
            &mut |o| out.push(o),
        );
        s.update(
            &cfg,
            30,
            &SceneInput {
                playing: false,
                track: 8,
                bass_cut: Some(true),
                ..base
            },
            &mut |o| out.push(o),
        );
        assert_eq!(notes(&out), [9, 14, 3]);
    }

    #[test]
    fn each_event_counts_also_without_presets_and_while_off() {
        let mut cfg = cfg(vec![]);
        cfg.enabled = false;
        let mut s = Scenes::default();
        let mut out = Vec::new();
        s.update(&cfg, 0, &input(0, 8, 1), &mut |o| out.push(o));
        s.beat(&cfg, 500, 1, &mut |o| out.push(o));
        s.update(&cfg, 600, &input(1, 10, 32), &mut |o| out.push(o));
        s.beat(&cfg, 1000, 2, &mut |o| out.push(o));
        assert!(out.is_empty());
        let st = s.status(&cfg);
        let n = |e| st.events[index(e)];
        // The phrase change is predicted on the beat, and the status does not count it again.
        assert_eq!(
            (
                n(Event::Beat),
                n(Event::Downbeat),
                n(Event::PhraseChange),
                n(Event::Drop),
                n(Event::Break),
                n(Event::Play)
            ),
            (2, 1, 1, 1, 1, 1)
        );
        assert_eq!(st.phrase, Some(Event::Break));
    }

    #[test]
    fn switching_off_releases_the_notes() {
        let mut held = row(Event::Intro, &[0]);
        held.gate = Gate::Hold;
        let mut cfg = cfg(vec![held]);
        let mut s = Scenes::default();
        s.update(&cfg, 0, &input(0, 0, 32), &mut |_| {});
        cfg.enabled = false;
        let mut out = Vec::new();
        s.update(&cfg, 10, &input(0, 0, 31), &mut |o| out.push(o));
        assert_eq!(out, [Out::NoteOff { ch: 9, note: 0 }]);
    }

    #[test]
    fn sanitize_drops_bad_data_and_parses_the_page_json() {
        let json = r#"{"enabled":true,"channel":20,"first_note":120,"count":15,
            "labels":["Blue wash and more text","Red"],
            "extra":[{"note":200,"label":"Smoke"}],
            "rows":[{"event":"drop","presets":[4,5,99,32,33],"advance":3,"order":"ping_pong","gate":"flash","decay":{"beats":0.25}},
                    {"event":"drop","presets":[1]},
                    {"event":"intro","presets":[]}]}"#;
        let mut c: ScenesConfig = serde_json::from_str(json).unwrap();
        c.sanitize();
        assert_eq!((c.channel, c.first_note, c.count), (15, 113, 15));
        assert_eq!(c.labels, ["Blue wash an", "Red"]);
        assert_eq!(c.rows.len(), 1);
        let r = &c.rows[0];
        assert_eq!(
            (r.presets.as_slice(), r.advance, r.order, r.gate),
            (&[4u8, 5, 32][..], 0, Order::PingPong, Gate::Flash)
        );
        assert_eq!((c.note(4), c.note(32), c.note(33)), (117, 127, 0));
        assert_eq!(r.decay, Decay::Beats(0.25));
        assert_eq!(
            serde_json::from_str::<ScenesConfig>("{}").unwrap(),
            ScenesConfig::default()
        );
    }

    fn hold_cfg(holds: &[f64]) -> ScenesConfig {
        let mut r = row(Event::Beat, &[0, 1]);
        r.holds = holds.to_vec();
        cfg(vec![r])
    }

    #[test]
    fn hold_blocks_and_queues() {
        let c = hold_cfg(&[2.0, 0.0]);
        let mut s = Scenes::default();
        let mut out = Vec::new();
        s.beat(&c, 0, 2, &mut |o| out.push(o));
        assert_eq!(
            out[0],
            Out::NoteOn {
                ch: 9,
                note: 0,
                vel: 127
            }
        );
        out.clear();
        s.beat(&c, 500, 3, &mut |o| out.push(o));
        s.update(&c, 999, &input(0, 0, 30), &mut |o| out.push(o));
        assert!(out.is_empty());
        s.update(&c, 1000, &input(0, 0, 29), &mut |o| out.push(o));
        assert_eq!(out[0], Out::NoteOff { ch: 9, note: 0 });
        assert_eq!(
            out[1],
            Out::NoteOn {
                ch: 9,
                note: 1,
                vel: 127
            }
        );
        let mut fired = Vec::new();
        s.drain_fired(|e, p| fired.push((e, p)));
        assert_eq!(fired, [(Event::Beat, 0), (Event::Beat, 1)]);
    }

    #[test]
    fn hold_without_pending_just_ends() {
        let mut r = row(Event::Beat, &[0]);
        r.holds = vec![1.0];
        let c = cfg(vec![r]);
        let mut s = Scenes::default();
        let mut out = Vec::new();
        s.beat(&c, 0, 2, &mut |o| out.push(o));
        out.clear();
        s.update(&c, 499, &input(0, 0, 30), &mut |o| out.push(o));
        assert!(out.is_empty());
        s.update(&c, 500, &input(0, 0, 30), &mut |o| out.push(o));
        assert_eq!(out, [Out::NoteOff { ch: 9, note: 0 }]);
        assert_eq!(s.next_off_ms(), None);
    }

    #[test]
    fn holds_sanitize() {
        let mut c = ScenesConfig {
            count: 16,
            rows: vec![Row {
                holds: vec![1.0],
                ..row(Event::Beat, &[0, 1, 2])
            }],
            ..ScenesConfig::default()
        };
        c.sanitize();
        assert_eq!(c.rows[0].holds, [1.0, 0.0, 0.0]);
        c.rows[0].holds = vec![99.0, -1.0, f64::NAN, 5.0];
        c.sanitize();
        assert_eq!(c.rows[0].holds, [MAX_DECAY_BEATS, 0.0, 0.0]);
        c.rows[0].holds = vec![0.01, 0.0, 1.0];
        c.sanitize();
        assert_eq!(c.rows[0].holds, [0.0625, 0.0, 1.0]);
    }

    #[test]
    fn next_off_includes_hold() {
        let c = hold_cfg(&[2.0, 0.0]);
        let mut s = Scenes::default();
        s.beat(&c, 0, 2, &mut |_| {});
        assert_eq!(s.next_off_ms(), Some(1000));
    }

    #[test]
    fn old_json_without_holds_parses() {
        let r: Row = serde_json::from_str(r#"{"event":"beat","presets":[1]}"#).unwrap();
        assert!(r.holds.is_empty());
        let c = cfg(vec![r]);
        let mut s = Scenes::default();
        let mut out = Vec::new();
        s.beat(&c, 0, 2, &mut |o| out.push(o));
        s.beat(&c, 500, 3, &mut |o| out.push(o));
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn switching_off_clears_holds() {
        let mut c = hold_cfg(&[2.0, 0.0]);
        let mut s = Scenes::default();
        s.beat(&c, 0, 2, &mut |_| {});
        s.beat(&c, 500, 3, &mut |_| {});
        c.enabled = false;
        let mut out = Vec::new();
        s.update(&c, 600, &input(0, 0, 30), &mut |o| out.push(o));
        assert_eq!(out, [Out::NoteOff { ch: 9, note: 0 }]);
        assert_eq!(s.next_off_ms(), None);
        c.enabled = true;
        out.clear();
        s.update(&c, 1000, &input(0, 0, 29), &mut |o| out.push(o));
        assert!(out.is_empty());
    }

    #[test]
    fn two_triggers_in_one_hold_give_one_queued_fire() {
        let c = hold_cfg(&[2.0, 0.0]);
        let mut s = Scenes::default();
        let mut out = Vec::new();
        s.beat(&c, 0, 2, &mut |o| out.push(o));
        s.beat(&c, 500, 3, &mut |o| out.push(o));
        s.beat(&c, 750, 4, &mut |o| out.push(o));
        out.clear();
        s.update(&c, 1000, &input(0, 0, 29), &mut |o| out.push(o));
        assert_eq!(notes(&out), [1]);
        out.clear();
        s.update(&c, 1100, &input(0, 0, 28), &mut |o| out.push(o));
        assert!(out.is_empty());
    }

    #[test]
    fn a_pulse_overlay_restores_the_hold_note() {
        let mut intro = row(Event::Intro, &[3]);
        intro.holds = vec![8.0];
        let c = cfg(vec![intro, row(Event::Downbeat, &[7])]);
        let mut s = Scenes::default();
        s.update(&c, 0, &input(0, 0, 32), &mut |_| {});
        let mut out = Vec::new();
        s.beat(&c, 1000, 1, &mut |o| out.push(o));
        assert_eq!(
            out,
            [
                Out::NoteOn {
                    ch: 9,
                    note: 7,
                    vel: 127
                },
                Out::NoteOff { ch: 9, note: 7 },
                Out::NoteOn {
                    ch: 9,
                    note: 3,
                    vel: 127
                },
            ]
        );
    }

    #[test]
    fn a_queued_fire_of_a_replaced_scene_is_dropped() {
        let mut intro = row(Event::Intro, &[1, 2]);
        intro.holds = vec![32.0, 0.0];
        let c = cfg(vec![intro, row(Event::Groove, &[5])]);
        let mut s = Scenes::default();
        s.update(&c, 0, &input(0, 3, 32), &mut |_| {});
        let mut out = Vec::new();
        s.event(&c, Event::Intro, 0, &mut |o| out.push(o));
        s.event(&c, Event::Groove, 100, &mut |o| out.push(o));
        s.event(&c, Event::Intro, 200, &mut |o| out.push(o));
        out.clear();
        s.update(&c, 16_000, &input(0, 3, 10), &mut |o| out.push(o));
        assert_eq!(notes(&out), Vec::<u8>::new());
        assert_eq!(s.current, Some(Event::Groove));
    }

    #[test]
    fn rows_without_holds_serialize_without_the_key() {
        let json = serde_json::to_string(&row(Event::Beat, &[1])).unwrap();
        assert!(!json.contains("holds"));
        let mut r = row(Event::Beat, &[1]);
        r.holds = vec![2.0];
        assert!(serde_json::to_string(&r)
            .unwrap()
            .contains("\"holds\":[2.0]"));
    }
}
