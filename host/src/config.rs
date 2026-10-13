use std::sync::Arc;

use parking_lot::RwLock;
use serde::Deserialize;
use std::path::Path;

// ── Beat source selector ──────────────────────────────────────────────────────

/// Which source provides beat/tempo/phase data.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Use Pioneer Pro DJ Link (Ethernet UDP, hardware CDJs/XDJs).
    ProLink,
    /// Use Ableton Link (works with rekordbox Performance mode on same machine).
    Link,
    /// Auto: prefer Ableton Link when peers are present, fall back to Pro DJ Link.
    #[default]
    Auto,
}

// ── Top-level config ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub source: Source,
    pub interface: String,
    pub device_number: u8,
    pub device_name: String,
    pub midi: MidiConfig,
    pub link: LinkConfig,
}

pub type SharedConfig = Arc<RwLock<Config>>;

pub fn new_shared(cfg: Config) -> SharedConfig {
    Arc::new(RwLock::new(cfg))
}

impl Default for Config {
    fn default() -> Self {
        Self {
            source: Source::default(),
            interface: "auto".into(),
            device_number: 5,
            device_name: "xdj-clock".into(),
            midi: MidiConfig::default(),
            link: LinkConfig::default(),
        }
    }
}

// ── MIDI config ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MidiConfig {
    pub output: String,
    pub clock_enabled: bool,
    pub clock_loop_enabled: bool,
    pub smoothing_ms: u64,
    pub latency_compensation_ms: i64,
    /// Bar interval for phrase-lock full resync (1 bar = 4 beats).
    pub phrase_lock_stable_beats: u8,
    pub notes: NoteConfig,
    pub cc: CcConfig,
    pub program: ProgramConfig,
    pub mtc: MtcConfig,
}

impl Default for MidiConfig {
    fn default() -> Self {
        Self {
            output: "auto".into(),
            clock_enabled: true,
            clock_loop_enabled: true,
            smoothing_ms: 30,
            latency_compensation_ms: 0,
            phrase_lock_stable_beats: 4,
            notes: NoteConfig::default(),
            cc: CcConfig::default(),
            program: ProgramConfig::default(),
            mtc: MtcConfig::default(),
        }
    }
}

// ── Note config ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NoteConfig {
    /// MIDI channel (0-indexed, 0-15).
    pub channel: u8,
    /// Note number fired on every beat.
    pub beat: u8,
    /// Note number fired on beat 1 of each bar (downbeat).
    pub downbeat: u8,
    /// Note number fired on phrase change (song structure transition).
    pub phrase_change: u8,
    /// First note of the phrase-kind range. The note is `phrase_base` + the kind code (0–11).
    pub phrase_base: u8,
}

impl Default for NoteConfig {
    fn default() -> Self {
        Self {
            channel: 9,
            beat: 36,
            downbeat: 37,
            phrase_change: 38,
            phrase_base: 40,
        }
    }
}

// ── CC config ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CcConfig {
    pub channel: u8,
    pub bpm_coarse: u8,
    pub bpm_fine: u8,
    pub pitch: u8,
    pub bar_phase: u8,
    pub beat_phase: u8,
    pub playing: u8,
    pub master_deck: u8,
    pub phrase_16: u8,
    /// Phrase kind of the master deck. The value is the kind code times 10.
    pub phrase_kind: u8,
    /// Progress through the current phrase of the master deck (0.0–1.0 maps to 0–127).
    pub phrase_progress: u8,
    /// Beats left in the current phrase of the master deck (capped at 127).
    pub phrase_beats_left: u8,
}

impl Default for CcConfig {
    fn default() -> Self {
        Self {
            channel: 0,
            bpm_coarse: 1,
            bpm_fine: 33,
            pitch: 2,
            bar_phase: 3,
            beat_phase: 4,
            playing: 5,
            master_deck: 6,
            phrase_16: 7,
            phrase_kind: 20,
            phrase_progress: 21,
            phrase_beats_left: 22,
        }
    }
}

// ── Program Change config ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProgramConfig {
    /// Send a Program Change with the phrase kind code when the master phrase changes.
    /// Off by default, because a Program Change switches the patch of a synth.
    pub enabled: bool,
    /// MIDI channel (0-indexed, 0-15).
    pub channel: u8,
}

// ── MTC (MIDI Timecode) config ────────────────────────────────────────────────

/// MTC frame rate variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MtcFrameRate {
    /// 24 fps (film/cinema).
    #[serde(alias = "24")]
    Fps24,
    /// 25 fps (PAL video).
    #[serde(alias = "25")]
    Fps25,
    /// 30 fps non-drop (NTSC).
    #[serde(alias = "30")]
    Fps30,
}

impl Default for MtcFrameRate {
    fn default() -> Self {
        Self::Fps25
    }
}

impl MtcFrameRate {
    /// Frames per second as an integer.
    pub fn fps(self) -> u8 {
        match self {
            Self::Fps24 => 24,
            Self::Fps25 => 25,
            Self::Fps30 => 30,
        }
    }

    /// Rate code for MTC quarter-frame piece 7 (bits 5-6).
    pub fn rate_code(self) -> u8 {
        match self {
            Self::Fps24 => 0x00,
            Self::Fps25 => 0x01,
            Self::Fps30 => 0x03,
        }
    }

    /// Cycle through frame rates.
    pub fn next(self) -> Self {
        match self {
            Self::Fps24 => Self::Fps25,
            Self::Fps25 => Self::Fps30,
            Self::Fps30 => Self::Fps24,
        }
    }

    /// Display label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Fps24 => "24 fps",
            Self::Fps25 => "25 fps",
            Self::Fps30 => "30 fps",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MtcConfig {
    /// Enable MIDI Timecode output.
    pub enabled: bool,
    /// Frame rate for MTC generation.
    pub frame_rate: MtcFrameRate,
}

impl Default for MtcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            frame_rate: MtcFrameRate::default(),
        }
    }
}

// ── Ableton Link config ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LinkConfig {
    /// Enable Ableton Link session participation.
    pub enabled: bool,
    /// Beat quantum (number of beats per bar/loop cycle, typically 4).
    /// Controls how phase is interpreted — phase = beat_position_within_quantum.
    pub quantum: f64,
    /// Polling interval in microseconds for the Link timeline query.
    /// Lower = tighter MIDI clock timing but more CPU.  Default 500µs.
    pub poll_interval_us: u64,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            quantum: 4.0,
            poll_interval_us: 500,
        }
    }
}

// ── Loader ────────────────────────────────────────────────────────────────────

pub fn load(path: &Path) -> anyhow::Result<Config> {
    if !path.exists() {
        tracing::info!("No config file at {path:?}, using defaults");
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(path)?;
    let cfg: Config = toml::from_str(&text)?;
    validate(&cfg)?;
    Ok(cfg)
}

pub(crate) fn validate(cfg: &Config) -> anyhow::Result<()> {
    anyhow::ensure!(
        cfg.device_number >= 1 && cfg.device_number <= 15,
        "device_number must be 1–15, got {}",
        cfg.device_number
    );
    anyhow::ensure!(
        cfg.device_name.len() <= 16,
        "device_name must be ≤ 16 ASCII bytes, got {:?}",
        cfg.device_name
    );
    anyhow::ensure!(
        cfg.midi.notes.channel <= 15,
        "midi.notes.channel must be 0–15"
    );
    anyhow::ensure!(cfg.midi.cc.channel <= 15, "midi.cc.channel must be 0–15");
    anyhow::ensure!(
        cfg.midi.program.channel <= 15,
        "midi.program.channel must be 0–15"
    );
    anyhow::ensure!(
        cfg.midi.notes.phrase_base <= 116,
        "midi.notes.phrase_base must be 0–116, got {}",
        cfg.midi.notes.phrase_base
    );
    for (name, num) in [
        ("phrase_kind", cfg.midi.cc.phrase_kind),
        ("phrase_progress", cfg.midi.cc.phrase_progress),
        ("phrase_beats_left", cfg.midi.cc.phrase_beats_left),
    ] {
        anyhow::ensure!(num <= 127, "midi.cc.{name} must be 0–127, got {num}");
    }
    anyhow::ensure!(
        cfg.midi.latency_compensation_ms >= -1000 && cfg.midi.latency_compensation_ms <= 1000,
        "midi.latency_compensation_ms must be -1000 to 1000, got {}",
        cfg.midi.latency_compensation_ms
    );
    anyhow::ensure!(
        (1..=32).contains(&cfg.midi.phrase_lock_stable_beats),
        "midi.phrase_lock_stable_beats (bar resync interval) must be 1 to 32, got {}",
        cfg.midi.phrase_lock_stable_beats
    );
    anyhow::ensure!(
        cfg.link.quantum >= 1.0 && cfg.link.quantum <= 16.0,
        "link.quantum must be 1–16, got {}",
        cfg.link.quantum
    );
    anyhow::ensure!(
        cfg.link.poll_interval_us >= 100 && cfg.link.poll_interval_us <= 10_000,
        "link.poll_interval_us must be 100–10000µs, got {}",
        cfg.link.poll_interval_us
    );
    Ok(())
}
