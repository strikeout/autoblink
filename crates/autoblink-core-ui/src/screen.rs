//! The text of the status screen of the OLED. The firmware only draws the lines.

/// The characters in one line of the small font (6 pixels wide on a 128-pixel display).
pub const COLS: usize = 21;

/// The values that the status screen shows. The firmware copies them from its state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// The number of the master deck.
    pub master: Option<u8>,
    pub playing: bool,
    /// The BPM of the master deck. 0 when it is not known.
    pub bpm: f64,
    /// The beat in the bar, 1 to 4. 0 when it is not known.
    pub beat: u8,
    /// The phrase of the master deck, for example "Chorus 3 bars left".
    pub phrase: Option<String>,
    /// The blink code of the status LED (IO33). `None` when the box is ready.
    pub blinks: Option<u64>,
    /// The address of the Pro DJ Link interface.
    pub ip: Option<String>,
    /// The DMX input: (signal, good frames each second). `None` without DMX.
    pub dmx: Option<(bool, u16)>,
}

/// The lines of the status screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Lines {
    /// The master deck and its state, for example "Deck 2  PLAY".
    pub deck: String,
    /// The BPM for the large font, for example "128.0".
    pub bpm: String,
    /// The beat box to fill, 1 to 4. 0 fills no box.
    pub beat: u8,
    pub phrase: String,
    /// The address, or the error of the blink code.
    pub net: String,
    pub dmx: String,
}

/// The text of a blink code of the status LED (see the table in AGENTS.md).
pub fn blink_text(blinks: u64) -> &'static str {
    match blinks {
        1 => "No network address",
        2 => "Not on Pro DJ Link",
        3 => "No player found",
        4 => "WiFi join failed",
        _ => "Error",
    }
}

/// Makes the lines of the status screen. Each line has `COLS` characters or less.
pub fn lines(s: &Snapshot) -> Lines {
    let deck = match s.master {
        Some(n) => format!("Deck {n}  {}", if s.playing { "PLAY" } else { "STOP" }),
        None => "No master".into(),
    };
    let bpm = if s.bpm > 0.0 {
        format!("{:.1}", s.bpm)
    } else {
        "---.-".into()
    };
    let net = match (s.blinks, &s.ip) {
        (Some(b), _) => blink_text(b).into(),
        (None, Some(ip)) => ip.clone(),
        (None, None) => String::new(),
    };
    let dmx = match s.dmx {
        Some((true, fps)) => format!("DMX IN {fps} fps"),
        Some((false, _)) => "DMX IN: no signal".into(),
        None => String::new(),
    };
    let cut = |t: String| t.chars().take(COLS).collect::<String>();
    Lines {
        deck: cut(deck),
        bpm,
        beat: if (1..=4).contains(&s.beat) { s.beat } else { 0 },
        phrase: cut(s.phrase.clone().unwrap_or_default()),
        net: cut(net),
        dmx: cut(dmx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_playing_master_shows_its_bpm_and_beat() {
        let l = lines(&Snapshot {
            master: Some(2),
            playing: true,
            bpm: 127.96,
            beat: 3,
            phrase: Some("Chorus 3 bars left".into()),
            ip: Some("169.254.1.2".into()),
            dmx: Some((true, 44)),
            ..Default::default()
        });
        assert_eq!(l.deck, "Deck 2  PLAY");
        assert_eq!(l.bpm, "128.0");
        assert_eq!(l.beat, 3);
        assert_eq!(l.phrase, "Chorus 3 bars left");
        assert_eq!(l.net, "169.254.1.2");
        assert_eq!(l.dmx, "DMX IN 44 fps");
    }

    #[test]
    fn the_blink_code_replaces_the_address() {
        let l = lines(&Snapshot {
            blinks: Some(3),
            ip: Some("10.0.0.2".into()),
            dmx: Some((false, 0)),
            ..Default::default()
        });
        assert_eq!(l.deck, "No master");
        assert_eq!(l.bpm, "---.-");
        assert_eq!(l.net, "No player found");
        assert_eq!(l.dmx, "DMX IN: no signal");
    }

    #[test]
    fn a_long_line_is_cut() {
        let l = lines(&Snapshot {
            phrase: Some("x".repeat(40)),
            ..Default::default()
        });
        assert_eq!(l.phrase.chars().count(), COLS);
        assert_eq!(l.beat, 0);
    }
}
