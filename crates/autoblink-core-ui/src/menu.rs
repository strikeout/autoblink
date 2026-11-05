//! The small menu of the OLED. The page keeps all the settings. The menu has only the settings
//! that a user changes at the booth.
//!
//! The menu gives an `Action` for each change. The firmware sends the action through the same
//! settings functions as the page, so the side effects stay the same.

use crate::joystick::Input;

/// The characters in one line (see `screen::COLS`).
const COLS: usize = crate::screen::COLS;
/// The lines of the display (6 lines of 10 pixels).
const ROWS: usize = 6;
/// Without an input for this time, the menu closes.
const TIMEOUT_MS: u64 = 15_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Left and right step the value at once.
    Step,
    /// Left and right change a shown value. Select applies it.
    Pending { lo: i64, hi: i64 },
    /// Select, left and right flip the value.
    Flip,
    /// Select shows the network page.
    Network,
    /// Select asks for a confirmation. The second select restarts the box.
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item {
    pub label: &'static str,
    /// The index of the setting. 0 for an item without a setting.
    pub idx: usize,
    pub kind: Kind,
}

/// The items of the menu. The device number applies on select, because each change writes NVS
/// and makes the virtual CDJ join the network again.
pub const ITEMS: [Item; 7] = [
    Item {
        label: "Device Number",
        idx: 2,
        kind: Kind::Pending { lo: 1, hi: 15 },
    },
    Item {
        label: "Note Channel",
        idx: 9,
        kind: Kind::Step,
    },
    Item {
        label: "DMX Mode",
        idx: 110,
        kind: Kind::Step,
    },
    Item {
        label: "DMX Beat Ch",
        idx: 111,
        kind: Kind::Step,
    },
    Item {
        label: "Rotate 180",
        idx: 120,
        kind: Kind::Flip,
    },
    Item {
        label: "Network",
        idx: 0,
        kind: Kind::Network,
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
    /// Step setting `idx` up (1) or down (-1), or flip it.
    Step {
        idx: usize,
        dir: i8,
    },
    /// Set setting `idx` to `value`.
    Set {
        idx: usize,
        value: i64,
    },
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum View {
    /// The status screen. The menu is closed.
    #[default]
    Status,
    List,
    Network,
    Confirm,
}

#[derive(Debug, Default)]
pub struct Menu {
    view: View,
    sel: usize,
    pending: Option<i64>,
    last_ms: u64,
}

impl Menu {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn view(&self) -> View {
        self.view
    }

    /// Closes the menu when it had no input for `TIMEOUT_MS`.
    pub fn tick(&mut self, now_ms: u64) {
        if self.view != View::Status && now_ms.saturating_sub(self.last_ms) >= TIMEOUT_MS {
            self.close();
        }
    }

    fn close(&mut self) {
        self.view = View::Status;
        self.pending = None;
    }

    /// Takes one input. `current` gives the present value of a `Pending` setting.
    pub fn input(
        &mut self,
        input: Input,
        now_ms: u64,
        current: impl Fn(usize) -> i64,
    ) -> Option<Action> {
        self.last_ms = now_ms;
        let item = ITEMS[self.sel];
        match (self.view, input) {
            (View::Status, Input::Select) => {
                self.view = View::List;
                None
            }
            (View::Status, _) => None,
            (View::Network, _) => {
                self.view = View::List;
                None
            }
            (View::Confirm, Input::Select) => {
                self.close();
                Some(Action::Restart)
            }
            (View::Confirm, _) => {
                self.view = View::List;
                None
            }
            (View::List, Input::Back) => {
                self.close();
                None
            }
            (View::List, Input::Up | Input::Down) => {
                let n = ITEMS.len();
                self.sel = if input == Input::Up {
                    (self.sel + n - 1) % n
                } else {
                    (self.sel + 1) % n
                };
                self.pending = None;
                None
            }
            (View::List, Input::Left | Input::Right) => {
                let dir: i8 = if input == Input::Left { -1 } else { 1 };
                match item.kind {
                    Kind::Step | Kind::Flip => Some(Action::Step { idx: item.idx, dir }),
                    Kind::Pending { lo, hi } => {
                        let v = self.pending.unwrap_or_else(|| current(item.idx));
                        self.pending = Some((v + i64::from(dir)).clamp(lo, hi));
                        None
                    }
                    Kind::Network | Kind::Restart => None,
                }
            }
            (View::List, Input::Select) => match item.kind {
                Kind::Flip => Some(Action::Step {
                    idx: item.idx,
                    dir: 1,
                }),
                Kind::Pending { .. } => self.pending.take().map(|value| Action::Set {
                    idx: item.idx,
                    value,
                }),
                Kind::Network => {
                    self.view = View::Network;
                    None
                }
                Kind::Restart => {
                    self.view = View::Confirm;
                    None
                }
                Kind::Step => None,
            },
        }
    }

    /// Makes the lines of the open menu. `value` gives the shown value of a setting. `network`
    /// has the lines of the network page.
    pub fn lines(&self, value: impl Fn(usize) -> String, network: &[String]) -> Vec<String> {
        let cut = |t: String| t.chars().take(COLS).collect::<String>();
        match self.view {
            View::Status => Vec::new(),
            View::Network => std::iter::once("Network".to_string())
                .chain(network.iter().cloned())
                .take(ROWS)
                .map(cut)
                .collect(),
            View::Confirm => ["Restart the box?", "", "Push: restart", "Hold: cancel"]
                .map(String::from)
                .to_vec(),
            View::List => {
                let shown = ROWS - 1;
                let first = self.sel.saturating_sub(shown - 1).min(ITEMS.len() - shown);
                let mut out = vec!["Menu   hold: back".to_string()];
                for (i, item) in ITEMS.iter().enumerate().skip(first).take(shown) {
                    let mark = if i == self.sel { '>' } else { ' ' };
                    let v = match (i == self.sel, self.pending, item.idx) {
                        (true, Some(p), _) => format!("{p}*"),
                        (_, _, 0) => String::new(),
                        _ => value(item.idx),
                    };
                    let width = COLS.saturating_sub(1 + item.label.len());
                    let line = format!("{mark}{}{v:>width$}", item.label);
                    out.push(cut(line.trim_end().to_string()));
                }
                out
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> Menu {
        let mut m = Menu::new();
        assert_eq!(m.input(Input::Select, 0, |_| 7), None);
        assert_eq!(m.view(), View::List);
        m
    }

    #[test]
    fn the_device_number_applies_on_select() {
        let mut m = open();
        assert_eq!(m.input(Input::Right, 1, |_| 7), None);
        assert_eq!(m.input(Input::Right, 2, |_| 7), None);
        let lines = m.lines(|_| "7".into(), &[]);
        assert!(lines[1].starts_with(">Device Number"));
        assert!(lines[1].ends_with("9*"));
        assert_eq!(
            m.input(Input::Select, 3, |_| 7),
            Some(Action::Set { idx: 2, value: 9 })
        );
        // The value stays in its range.
        for t in 0..20 {
            m.input(Input::Right, 10 + t, |_| 14);
        }
        assert_eq!(
            m.input(Input::Select, 40, |_| 14),
            Some(Action::Set { idx: 2, value: 15 })
        );
    }

    #[test]
    fn a_step_item_changes_at_once() {
        let mut m = open();
        m.input(Input::Down, 1, |_| 0);
        assert_eq!(
            m.input(Input::Left, 2, |_| 0),
            Some(Action::Step { idx: 9, dir: -1 })
        );
    }

    #[test]
    fn restart_needs_a_confirmation() {
        let mut m = open();
        m.input(Input::Up, 1, |_| 0);
        assert_eq!(m.input(Input::Select, 2, |_| 0), None);
        assert_eq!(m.view(), View::Confirm);
        assert_eq!(m.input(Input::Back, 3, |_| 0), None);
        assert_eq!(m.view(), View::List);
        m.input(Input::Select, 4, |_| 0);
        assert_eq!(m.input(Input::Select, 5, |_| 0), Some(Action::Restart));
        assert_eq!(m.view(), View::Status);
    }

    #[test]
    fn back_and_the_timeout_close_the_menu() {
        let mut m = open();
        m.input(Input::Back, 1, |_| 0);
        assert_eq!(m.view(), View::Status);
        let mut m = open();
        m.tick(14_999);
        assert_eq!(m.view(), View::List);
        m.tick(15_000);
        assert_eq!(m.view(), View::Status);
    }

    #[test]
    fn the_list_scrolls_to_the_selected_item() {
        let mut m = open();
        m.input(Input::Up, 1, |_| 0);
        let lines = m.lines(|_| "x".into(), &[]);
        assert_eq!(lines.len(), ROWS);
        assert_eq!(lines[ROWS - 1], ">Restart");
        assert!(lines.iter().all(|l| l.chars().count() <= COLS));
    }
}
