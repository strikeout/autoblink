//! The text of the pages of the OLED. The firmware only draws the lines.

use crate::menu::{Page, ROWS};

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
        2 => "Joining Pro DJ Link",
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
        // The virtual CDJ joins for about 4 s. The beats arrive already, so this is no error.
        (Some(2), Some(ip)) => format!("{ip} joining"),
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

/// Cuts a text to one line.
fn cut(t: String) -> String {
    t.chars().take(COLS).collect()
}

/// Puts `label` on the left and `value` on the right of a line of `width` characters. A long
/// label is cut, so that one space stays between the two.
pub fn pair(label: &str, value: &str, width: usize) -> String {
    let value: String = value.chars().take(width).collect();
    let room = width - value.chars().count();
    let label: String = label.chars().take(room.saturating_sub(1)).collect();
    format!("{label:<room$}{value}")
}

/// Replaces each character that the ASCII fonts of the OLED cannot show with "?".
pub fn ascii(t: &str) -> String {
    t.chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() {
                c
            } else {
                '?'
            }
        })
        .collect()
}

/// One deck of the Decks page.
#[derive(Debug, Clone, PartialEq)]
pub struct DeckRow {
    pub num: u8,
    pub master: bool,
    pub playing: bool,
    pub bpm: f64,
    /// The pitch in percent.
    pub pitch: f64,
}

/// The Decks page: one line for each deck, for example "*2 PLAY 128.0  +1.2%".
pub fn decks_lines(rows: &[DeckRow]) -> Vec<String> {
    let mut out = vec![Page::Decks.header()];
    if rows.is_empty() {
        out.push(blink_text(3).into());
    }
    for r in rows.iter().take(ROWS - 1) {
        let bpm = if r.bpm > 0.0 {
            format!("{:5.1}", r.bpm)
        } else {
            "  ---".into()
        };
        out.push(cut(format!(
            "{}{} {} {bpm} {:+5.1}%",
            if r.master { '*' } else { ' ' },
            r.num,
            if r.playing { "PLAY" } else { "STOP" },
            r.pitch
        )));
    }
    out
}

/// The track of the master deck for the Track page.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackInfo {
    pub deck: u8,
    pub title: String,
    pub artist: String,
    pub key: String,
    /// The time to the end of the track.
    pub left_s: Option<u32>,
    pub phrase: Option<String>,
}

/// Splits a title into two lines at a space, when it is longer than one line.
fn wrap2(t: &str) -> (String, String) {
    if t.len() <= COLS {
        return (t.into(), String::new());
    }
    // `t` is ASCII, so a byte index is a character index.
    let at = t[..=COLS].rfind(' ').filter(|&i| i > 0).unwrap_or(COLS);
    (t[..at].trim_end().into(), cut(t[at..].trim_start().into()))
}

/// The Track page: the title on two lines, the artist, the deck with the key and the time left,
/// and the phrase. The firmware draws the phrase bar under the last line.
pub fn track_lines(t: Option<&TrackInfo>) -> Vec<String> {
    let mut out = vec![Page::Track.header()];
    let Some(t) = t else {
        out.push("No track data".into());
        return out;
    };
    let (a, b) = wrap2(&ascii(&t.title));
    let left = t
        .left_s
        .map_or(String::new(), |s| format!("-{}:{:02}", s / 60, s % 60));
    let deck = format!("Deck {}  {}", t.deck, ascii(&t.key));
    out.extend([
        a,
        b,
        cut(ascii(&t.artist)),
        pair(&deck, &left, COLS),
        cut(t.phrase.clone().unwrap_or_default()),
    ]);
    out
}

/// The DMX values for the DMX page. `input` is `None` without the feature `dmx`.
#[derive(Debug, Clone, PartialEq)]
pub struct DmxInfo {
    pub mode: &'static str,
    pub beat_channel: u16,
    pub input: Option<(bool, u16)>,
}

/// The DMX page.
pub fn dmx_lines(d: &DmxInfo) -> Vec<String> {
    let mut out = vec![Page::Dmx.header()];
    let Some((signal, fps)) = d.input else {
        out.push("No DMX in this build".into());
        return out;
    };
    let input = if signal {
        format!("{fps} fps")
    } else {
        "no signal".into()
    };
    out.extend([
        pair("Mode", d.mode, COLS),
        pair("Beat channel", &d.beat_channel.to_string(), COLS),
        pair("DMX IN", &input, COLS),
    ]);
    out
}

/// The Network page: the state of the Pro DJ Link, the addresses of the interfaces (`iface` has
/// the parts with " · " between them), then the fixed lines of the own WiFi network.
pub fn network_lines(fixed: &[String], iface: &str, blinks: Option<u64>) -> Vec<String> {
    let status = blinks.map_or("Ready", blink_text).to_string();
    std::iter::once(Page::Network.header())
        .chain(std::iter::once(status))
        .chain(
            iface
                .split(" · ")
                .filter(|p| !p.is_empty())
                .map(String::from),
        )
        .chain(fixed.iter().cloned())
        .take(ROWS)
        .map(cut)
        .collect()
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
    fn the_address_shows_while_the_virtual_cdj_joins() {
        let l = lines(&Snapshot {
            blinks: Some(2),
            ip: Some("192.168.1.104".into()),
            ..Default::default()
        });
        assert_eq!(l.net, "192.168.1.104 joining");
    }

    #[test]
    fn pair_puts_the_value_on_the_right_and_cuts_the_label() {
        assert_eq!(pair("Mode", "Merge", 12), "Mode   Merge");
        assert_eq!(pair("A long label", "1234", 10), "A lon 1234");
        assert_eq!(pair("A long label", "1234", 10).len(), 10);
    }

    #[test]
    fn the_decks_page_marks_the_master() {
        let l = decks_lines(&[
            DeckRow {
                num: 1,
                master: false,
                playing: false,
                bpm: 0.0,
                pitch: 0.0,
            },
            DeckRow {
                num: 2,
                master: true,
                playing: true,
                bpm: 127.96,
                pitch: 1.24,
            },
        ]);
        assert_eq!(l[1], " 1 STOP   ---  +0.0%");
        assert_eq!(l[2], "*2 PLAY 128.0  +1.2%");
        assert!(l.iter().all(|t| t.len() <= COLS));
    }

    #[test]
    fn the_track_page_wraps_the_title_and_shows_ascii_only() {
        let l = track_lines(Some(&TrackInfo {
            deck: 2,
            title: "Blackbird (Ian Pooley's Main Remix)".into(),
            artist: "Zoë Xenia".into(),
            key: "8A".into(),
            left_s: Some(201),
            phrase: Some("DROP 3 bars left".into()),
        }));
        assert_eq!(l[1], "Blackbird (Ian");
        assert_eq!(l[2], "Pooley's Main Remix)");
        assert_eq!(l[3], "Zo? Xenia");
        assert_eq!(l[4], "Deck 2  8A      -3:21");
        assert_eq!(l.len(), ROWS);
        assert!(l.iter().all(|t| t.len() <= COLS));
        assert_eq!(track_lines(None)[1], "No track data");
    }

    #[test]
    fn the_dmx_page_shows_the_input() {
        let l = dmx_lines(&DmxInfo {
            mode: "Merge",
            beat_channel: 1,
            input: Some((true, 41)),
        });
        assert_eq!(l[1], "Mode            Merge");
        assert_eq!(l[3], "DMX IN         41 fps");
    }

    #[test]
    fn the_network_page_starts_with_the_state() {
        let l = network_lines(
            &["AP autoblink-D06D".into(), "Page 192.168.1.1".into()],
            "Ethernet: no address · WiFi 192.168.1.104",
            None,
        );
        assert_eq!(l[1], "Ready");
        assert_eq!(l[3], "WiFi 192.168.1.104");
        assert_eq!(l.len(), ROWS);
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
