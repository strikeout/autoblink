//! The pages of the OLED and the settings menu. The page keeps all the settings. The menu has
//! only the settings that a user changes at the booth.
//!
//! Left and right flip the pages. On the Settings page, up and down move the cursor, and a push
//! starts an edit. In an edit, up and down change the shown value, a push applies it, and left or
//! right cancel it. A push of the stick never changes a setting by accident.
//!
//! The menu gives an `Action` for each change. The firmware sends the action through the same
//! settings functions as the page, so the side effects stay the same.

use crate::joystick::Input;
use crate::screen::{pair, COLS};

/// The lines of the display (6 lines of 10 pixels).
pub const ROWS: usize = 6;
/// Without an input for this time, an edit stops and the Settings page goes back to Main.
const TIMEOUT_MS: u64 = 15_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Page {
    /// The status screen with the large BPM.
    #[default]
    Main,
    Decks,
    Track,
    Dmx,
    /// The DMX scene player: a push turns it on or off.
    Scenes,
    Network,
    Settings,
}

impl Page {
    /// The pages in the order of the right flips.
    pub const ALL: [Page; 7] = [
        Page::Main,
        Page::Decks,
        Page::Track,
        Page::Dmx,
        Page::Scenes,
        Page::Network,
        Page::Settings,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Page::Main => "Main",
            Page::Decks => "Decks",
            Page::Track => "Track",
            Page::Dmx => "DMX",
            Page::Scenes => "Scenes",
            Page::Network => "Network",
            Page::Settings => "Settings",
        }
    }

    /// The header line of the page: "< Decks         2/6 >". The arrows show the flips.
    pub fn header(self) -> String {
        let n = Page::ALL.iter().position(|&p| p == self).unwrap_or(0) + 1;
        let count = format!("{n}/{} >", Page::ALL.len());
        format!("< {}", pair(self.title(), &count, COLS - 2))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A number from `lo` to `hi`. Up and down step it by `step`.
    Number { lo: i64, hi: i64, step: i64 },
    /// One of `n` values (0 to n - 1). Up and down wrap around.
    Choice { n: i64 },
    /// A push asks for a confirmation. The second push restarts the box.
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item {
    pub label: &'static str,
    /// The index of the setting. 0 for an item without a setting.
    pub idx: usize,
    pub kind: Kind,
}

const fn number(label: &'static str, idx: usize, lo: i64, hi: i64, step: i64) -> Item {
    Item {
        label,
        idx,
        kind: Kind::Number { lo, hi, step },
    }
}

/// The items of the Settings page. Each value applies only on a push, because a change of the
/// device number makes the virtual CDJ join the network again.
pub const ITEMS: [Item; 7] = [
    number("Device Number", 2, 1, 15, 1),
    number("Note Channel", 9, 1, 16, 1),
    Item {
        label: "DMX Mode",
        idx: 110,
        kind: Kind::Choice { n: 4 },
    },
    // ponytail: the beat channel steps by 1 (150 ms per step while held). Add an acceleration
    // if the users set high channels from the box.
    number("DMX Beat Ch", 111, 1, 512, 1),
    number("Brightness", 121, 1, 255, 16),
    Item {
        label: "Rotate 180",
        idx: 120,
        kind: Kind::Choice { n: 2 },
    },
    Item {
        label: "Restart",
        idx: 0,
        kind: Kind::Restart,
    },
];

/// A change that the firmware must do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Set setting `idx` to `value`.
    Set {
        idx: usize,
        value: i64,
    },
    /// Turn the DMX scene player on or off.
    ToggleScenes,
    Restart,
}

#[derive(Debug, Default)]
pub struct Menu {
    page: usize,
    sel: usize,
    /// The shown value of the edit of `ITEMS[sel]`.
    edit: Option<i64>,
    confirm: bool,
    last_ms: u64,
}

impl Menu {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn page(&self) -> Page {
        Page::ALL[self.page]
    }

    /// Stops an edit or a confirmation, and leaves the Settings page, after `TIMEOUT_MS` without
    /// an input. The other pages stay, so that a user can keep the Decks page open.
    pub fn tick(&mut self, now_ms: u64) {
        if now_ms.saturating_sub(self.last_ms) < TIMEOUT_MS {
            return;
        }
        self.edit = None;
        self.confirm = false;
        if self.page() == Page::Settings {
            self.page = 0;
        }
    }

    /// Takes one input. `current` gives the present value of a setting.
    pub fn input(
        &mut self,
        input: Input,
        now_ms: u64,
        current: impl Fn(usize) -> i64,
    ) -> Option<Action> {
        self.last_ms = now_ms;
        let item = ITEMS[self.sel];
        if self.confirm {
            self.confirm = false;
            return (input == Input::Select).then_some(Action::Restart);
        }
        if let Some(v) = self.edit {
            match input {
                Input::Select => {
                    self.edit = None;
                    return Some(Action::Set {
                        idx: item.idx,
                        value: v,
                    });
                }
                Input::Left | Input::Right => self.edit = None,
                Input::Up | Input::Down => {
                    let up = input == Input::Up;
                    self.edit = Some(match item.kind {
                        Kind::Number { lo, hi, step } => {
                            (if up { v + step } else { v - step }).clamp(lo, hi)
                        }
                        Kind::Choice { n } => (v + if up { 1 } else { n - 1 }) % n,
                        Kind::Restart => v,
                    });
                }
            }
            return None;
        }
        let n = Page::ALL.len();
        match input {
            Input::Left => self.page = (self.page + n - 1) % n,
            Input::Right => self.page = (self.page + 1) % n,
            Input::Select if self.page() == Page::Scenes => return Some(Action::ToggleScenes),
            _ if self.page() != Page::Settings => {}
            Input::Up => self.sel = (self.sel + ITEMS.len() - 1) % ITEMS.len(),
            Input::Down => self.sel = (self.sel + 1) % ITEMS.len(),
            Input::Select => match item.kind {
                Kind::Restart => self.confirm = true,
                _ => self.edit = Some(current(item.idx)),
            },
        }
        None
    }

    /// Makes the lines of the Settings page. `text` gives the shown text of a value of a setting.
    pub fn lines(
        &self,
        text: impl Fn(usize, i64) -> String,
        current: impl Fn(usize) -> i64,
    ) -> Vec<String> {
        let mut out = vec![Page::Settings.header()];
        if self.confirm {
            out.extend(
                [
                    "Restart the box?",
                    "",
                    "Push: restart",
                    "Left/right: cancel",
                ]
                .map(String::from),
            );
            return out;
        }
        let shown = ROWS - 1;
        let first = self.sel.saturating_sub(shown - 1).min(ITEMS.len() - shown);
        for (i, item) in ITEMS.iter().enumerate().skip(first).take(shown) {
            let mark = if i == self.sel { ">" } else { " " };
            let value = match (i == self.sel, self.edit, item.kind) {
                (_, _, Kind::Restart) => String::new(),
                (true, Some(v), _) => format!("[{}]", text(item.idx, v)),
                _ => text(item.idx, current(item.idx)),
            };
            let line = format!("{mark}{}", pair(item.label, &value, COLS - 1));
            out.push(line.trim_end().into());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(_: usize, v: i64) -> String {
        v.to_string()
    }

    /// A menu on the Settings page.
    fn settings() -> Menu {
        let mut m = Menu::new();
        assert_eq!(m.input(Input::Left, 0, |_| 7), None);
        assert_eq!(m.page(), Page::Settings);
        m
    }

    #[test]
    fn left_and_right_flip_the_pages_in_a_ring() {
        let mut m = Menu::new();
        for p in &Page::ALL[1..] {
            m.input(Input::Right, 0, |_| 0);
            assert_eq!(m.page(), *p);
        }
        m.input(Input::Right, 0, |_| 0);
        assert_eq!(m.page(), Page::Main);
        // Up, down and a push do nothing on an info page.
        m.input(Input::Right, 0, |_| 0);
        for i in [Input::Up, Input::Down, Input::Select] {
            assert_eq!(m.input(i, 0, |_| 0), None);
            assert_eq!(m.page(), Page::Decks);
        }
    }

    #[test]
    fn a_push_on_the_scenes_page_toggles_the_player() {
        let mut m = Menu::new();
        for _ in 0..4 {
            m.input(Input::Right, 0, |_| 0);
        }
        assert_eq!(m.page(), Page::Scenes);
        assert_eq!(m.input(Input::Select, 1, |_| 0), Some(Action::ToggleScenes));
        assert_eq!(m.page(), Page::Scenes);
    }

    #[test]
    fn a_value_applies_only_on_the_push() {
        let mut m = settings();
        // The cursor starts on Device Number.
        assert_eq!(m.input(Input::Select, 1, |_| 7), None);
        assert_eq!(m.input(Input::Up, 2, |_| 7), None);
        assert_eq!(m.input(Input::Up, 3, |_| 7), None);
        let lines = m.lines(text, |_| 7);
        assert!(lines[1].starts_with(">Device Number"));
        assert!(lines[1].ends_with("[9]"));
        assert_eq!(
            m.input(Input::Select, 4, |_| 7),
            Some(Action::Set { idx: 2, value: 9 })
        );
        // The value stays in its range.
        m.input(Input::Select, 5, |_| 14);
        for t in 0..5 {
            m.input(Input::Up, 6 + t, |_| 14);
        }
        assert_eq!(
            m.input(Input::Select, 20, |_| 14),
            Some(Action::Set { idx: 2, value: 15 })
        );
    }

    #[test]
    fn left_or_right_cancels_an_edit_and_stays_on_the_page() {
        let mut m = settings();
        m.input(Input::Down, 1, |_| 0);
        m.input(Input::Down, 2, |_| 0);
        m.input(Input::Select, 3, |_| 2);
        m.input(Input::Down, 4, |_| 2);
        assert!(m.lines(text, |_| 2)[3].ends_with("[1]"));
        assert_eq!(m.input(Input::Left, 5, |_| 2), None);
        assert_eq!(m.page(), Page::Settings);
        assert!(m.lines(text, |_| 2)[3].ends_with('2'));
    }

    #[test]
    fn a_choice_wraps_around() {
        let mut m = settings();
        m.input(Input::Down, 1, |_| 0);
        m.input(Input::Down, 2, |_| 0);
        m.input(Input::Select, 3, |_| 3);
        m.input(Input::Up, 4, |_| 3);
        assert_eq!(
            m.input(Input::Select, 5, |_| 3),
            Some(Action::Set { idx: 110, value: 0 })
        );
    }

    #[test]
    fn restart_needs_a_confirmation() {
        let mut m = settings();
        m.input(Input::Up, 1, |_| 0);
        assert_eq!(m.input(Input::Select, 2, |_| 0), None);
        assert_eq!(m.lines(text, |_| 0)[1], "Restart the box?");
        assert_eq!(m.input(Input::Right, 3, |_| 0), None);
        assert_eq!(m.page(), Page::Settings);
        m.input(Input::Select, 4, |_| 0);
        assert_eq!(m.input(Input::Select, 5, |_| 0), Some(Action::Restart));
    }

    #[test]
    fn the_timeout_stops_an_edit_and_leaves_the_settings() {
        let mut m = settings();
        m.input(Input::Select, 1, |_| 7);
        m.tick(15_000);
        assert_eq!(m.page(), Page::Settings);
        m.tick(15_001);
        assert_eq!(m.page(), Page::Main);
        // An info page stays.
        let mut m = Menu::new();
        m.input(Input::Right, 1, |_| 0);
        m.tick(60_000);
        assert_eq!(m.page(), Page::Decks);
    }

    #[test]
    fn the_list_scrolls_to_the_selected_item() {
        let mut m = settings();
        m.input(Input::Up, 1, |_| 0);
        let lines = m.lines(text, |_| 0);
        assert_eq!(lines.len(), ROWS);
        assert_eq!(lines[ROWS - 1], ">Restart");
        assert!(lines.iter().all(|l| l.chars().count() <= COLS));
    }

    #[test]
    fn the_header_shows_the_page_and_the_arrows() {
        assert_eq!(Page::Decks.header(), "< Decks         2/7 >");
        assert_eq!(Page::Decks.header().len(), COLS);
    }
}
