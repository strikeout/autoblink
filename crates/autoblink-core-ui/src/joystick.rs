//! The analog joystick (KY-023 type): two ADC axes and a push switch. The state machine turns the
//! samples into menu inputs.
//!
//! ponytail: the axes have a fixed direction (X up = right, Y up = down). If the module on the box
//! is turned, add a setting that turns or mirrors the axes.

/// One input for the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Up,
    Down,
    Left,
    Right,
    /// A push of the switch.
    Select,
}

/// The highest value of the 12-bit ADC.
const ADC_MAX: i32 = 4095;
/// The samples for the center calibration at the start.
const CALIBRATION_SAMPLES: i32 = 16;
/// A direction starts above this part of the travel from the center (per mille).
const START: i32 = 700;
/// A held direction stops below this part of the travel (per mille).
const STOP: i32 = 400;
/// Another direction takes over a held direction when its travel is larger by this part.
const SWITCH: i32 = 200;
/// A held up or down repeats after this time, then every `REPEAT_MS`. Left and right do not
/// repeat: one push of the stick flips one page.
const FIRST_REPEAT_MS: u64 = 500;
const REPEAT_MS: u64 = 150;
/// The switch must stay in one state for this time.
const DEBOUNCE_MS: u64 = 30;

const DIRECTIONS: [Input; 4] = [Input::Up, Input::Down, Input::Left, Input::Right];

#[derive(Debug, Default)]
pub struct Joystick {
    /// The sums of the calibration samples, then the center.
    sum: (i32, i32),
    samples: i32,
    held: Option<Input>,
    next_repeat: u64,
    /// The last raw switch state and the time of its last change.
    raw: bool,
    raw_since: u64,
    /// The debounced switch state.
    pressed: bool,
}

impl Joystick {
    pub fn new() -> Self {
        Self::default()
    }

    /// The center of the two axes after the calibration. A sample at the center gives no
    /// direction.
    pub fn center(&self) -> Option<(u16, u16)> {
        (self.samples >= CALIBRATION_SAMPLES).then(|| {
            (
                self.sum.0.clamp(0, ADC_MAX) as u16,
                self.sum.1.clamp(0, ADC_MAX) as u16,
            )
        })
    }

    /// Takes one sample. `x` and `y` are raw ADC values (0 to 4095). `pressed` is `true` while
    /// the switch is down. Returns the input of this sample, if any.
    pub fn update(&mut self, x: u16, y: u16, pressed: bool, now_ms: u64) -> Option<Input> {
        let (x, y) = (i32::from(x), i32::from(y));
        if self.samples < CALIBRATION_SAMPLES {
            self.sum.0 += x;
            self.sum.1 += y;
            self.samples += 1;
            if self.samples == CALIBRATION_SAMPLES {
                self.sum = (
                    self.sum.0 / CALIBRATION_SAMPLES,
                    self.sum.1 / CALIBRATION_SAMPLES,
                );
            }
            self.raw = pressed;
            self.raw_since = now_ms;
            self.pressed = pressed;
            return None;
        }
        self.switch(pressed, now_ms)
            .or_else(|| self.stick(x, y, now_ms))
    }

    fn switch(&mut self, raw: bool, now: u64) -> Option<Input> {
        if raw != self.raw {
            self.raw = raw;
            self.raw_since = now;
        }
        if self.raw != self.pressed && now - self.raw_since >= DEBOUNCE_MS {
            self.pressed = self.raw;
            if self.pressed {
                return Some(Input::Select);
            }
        }
        None
    }

    fn stick(&mut self, x: i32, y: i32, now: u64) -> Option<Input> {
        let travel = |dir: Input| -> i32 {
            let (v, c) = match dir {
                Input::Left | Input::Right => (x, self.sum.0),
                _ => (y, self.sum.1),
            };
            let (dist, room) = match dir {
                Input::Right | Input::Down => (v - c, ADC_MAX - c),
                _ => (c - v, c),
            };
            if room <= 0 {
                0
            } else {
                dist * 1000 / room
            }
        };
        if self.held.is_some_and(|d| travel(d) < STOP) {
            self.held = None;
        }
        let best = DIRECTIONS
            .into_iter()
            .map(|d| (travel(d), d))
            .filter(|(t, _)| *t > START)
            .max_by_key(|(t, _)| *t)
            .map(|(_, d)| d);
        // A push that starts on one axis can end on the other: a push to the right can pass the
        // start of "down" first. The stronger direction then takes over. The margin stops a
        // diagonal push from flipping between the two.
        let take = best.filter(|&b| {
            self.held
                .is_none_or(|h| h != b && travel(b) > travel(h) + SWITCH)
        });
        if let Some(d) = take {
            self.held = Some(d);
            self.next_repeat = now + FIRST_REPEAT_MS;
            return Some(d);
        }
        match self.held {
            Some(d @ (Input::Up | Input::Down)) if now >= self.next_repeat => {
                self.next_repeat = now + REPEAT_MS;
                Some(d)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: u16 = 1900;

    /// A joystick after the calibration at the center `C`.
    fn calibrated() -> Joystick {
        let mut j = Joystick::new();
        for t in 0..16 {
            assert_eq!(j.update(C, C, false, t), None);
        }
        j
    }

    #[test]
    fn a_direction_starts_above_70_and_stops_below_40_percent() {
        let mut j = calibrated();
        // 60 % of the travel to the right does not start a direction.
        let x60 = C + (4095 - C) * 6 / 10;
        assert_eq!(j.update(x60, C, false, 100), None);
        assert_eq!(j.update(4095, C, false, 110), Some(Input::Right));
        // Back to 60 %: the direction is still held, so it does not start again.
        assert_eq!(j.update(x60, C, false, 120), None);
        assert_eq!(j.update(C, C, false, 130), None);
        assert_eq!(j.update(0, C, false, 140), Some(Input::Left));
        assert_eq!(j.update(C, C, false, 150), None);
        assert_eq!(j.update(C, 0, false, 160), Some(Input::Up));
        assert_eq!(j.update(C, C, false, 170), None);
        assert_eq!(j.update(C, 4095, false, 180), Some(Input::Down));
    }

    #[test]
    fn the_center_is_known_after_the_calibration() {
        let mut j = Joystick::new();
        j.update(2000, 1800, false, 0);
        assert_eq!(j.center(), None);
        let j = calibrated();
        assert_eq!(j.center(), Some((C, C)));
    }

    #[test]
    fn a_held_direction_repeats() {
        let mut j = calibrated();
        assert_eq!(j.update(C, 0, false, 1000), Some(Input::Up));
        assert_eq!(j.update(C, 0, false, 1499), None);
        assert_eq!(j.update(C, 0, false, 1500), Some(Input::Up));
        assert_eq!(j.update(C, 0, false, 1600), None);
        assert_eq!(j.update(C, 0, false, 1650), Some(Input::Up));
    }

    #[test]
    fn a_held_left_or_right_gives_one_input() {
        let mut j = calibrated();
        assert_eq!(j.update(4095, C, false, 1000), Some(Input::Right));
        for t in (1020..3000).step_by(20) {
            assert_eq!(j.update(4095, C, false, t), None);
        }
        assert_eq!(j.update(C, C, false, 3000), None);
        assert_eq!(j.update(4095, C, false, 3020), Some(Input::Right));
    }

    #[test]
    fn a_push_that_starts_on_the_other_axis_ends_on_the_stronger_direction() {
        let mut j = calibrated();
        let y75 = C + (4095 - C) * 3 / 4;
        assert_eq!(j.update(C, y75, false, 100), Some(Input::Down));
        assert_eq!(j.update(4095, y75, false, 120), Some(Input::Right));
        assert_eq!(j.update(4095, y75, false, 140), None);
        // A diagonal push with two equal directions does not flip.
        let mut j = calibrated();
        let x75 = C + (4095 - C) * 3 / 4;
        assert!(j.update(x75, y75, false, 100).is_some());
        for t in 1..20 {
            assert_eq!(j.update(x75, y75, false, 100 + 10 * t), None);
        }
    }

    #[test]
    fn the_push_selects_at_once() {
        let mut j = calibrated();
        assert_eq!(j.update(C, C, true, 100), None);
        // A bounce shorter than the debounce time is ignored.
        assert_eq!(j.update(C, C, false, 110), None);
        assert_eq!(j.update(C, C, true, 115), None);
        assert_eq!(j.update(C, C, true, 145), Some(Input::Select));
        // A long push gives no second input. The release gives none.
        assert_eq!(j.update(C, C, true, 2000), None);
        assert_eq!(j.update(C, C, false, 2100), None);
        assert_eq!(j.update(C, C, false, 2200), None);
    }
}
