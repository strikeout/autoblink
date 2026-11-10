//! The two LEDs.
//!
//! IO32 pulses for 30 ms on each beat of the master deck. IO33 has the label 485_EN on the board.
//! It shows the state of the box. While the master deck plays, IO33 pulses for 120 ms on beat 1
//! and for 25 ms on the other beats. When no beat arrives for 2 s, IO33 shows a blink code:
//!
//! - 1 blink: the Ethernet port and the joined WiFi network have no address.
//! - 2 blinks: the virtual CDJ is not on the Pro DJ Link network.
//! - 3 blinks: the firmware finds no player.
//! - 4 blinks: the firmware cannot join the WiFi network of the settings.
//! - 1 s on, 1 s off: the box is ready.
//!
//! After a panic, a watchdog or a brownout restart, an 800 ms flash comes before each code for
//! 10 minutes.
//!
//! One esp_timer of 5 ms sets the two pins. It has no thread and no stack of its own. The receive
//! thread writes the beat into atomics, and the main loop writes the blink code.

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use esp_idf_svc::hal::gpio::{Output, PinDriver};
use esp_idf_svc::timer::{EspTaskTimerService, EspTimer};

use crate::State;

/// The time without a beat after which IO33 shows the blink code, in milliseconds.
const IDLE_AFTER_MS: u32 = 2000;
/// The time after an abnormal restart during which the 800 ms flash comes before the code.
const SHOW_RESTART_MS: u64 = 10 * 60 * 1000;
/// The time of one repetition of a blink code, in milliseconds.
const CODE_MS: u32 = 2000;
/// The period of the LED timer.
const TICK: Duration = Duration::from_millis(5);

/// The time of the last master beat in milliseconds, with the clock of `State::now_ms`.
static BEAT_AT: AtomicU32 = AtomicU32::new(0);
/// The beat in the bar (1 to 4) of the last master beat. 0 before the first beat.
static BEAT_KIND: AtomicU8 = AtomicU8::new(0);
/// The number of blinks from `blinks`. 0 is the code of a box that is ready.
static CODE: AtomicU8 = AtomicU8::new(1);

/// Starts the pulse of a master beat. `beat_in_bar` is 1 to 4, and `at_ms` uses the clock of
/// `State::now_ms`. The receive thread calls this function. It does not wait.
pub fn beat(beat_in_bar: u8, at_ms: u64) {
    BEAT_AT.store(at_ms as u32, Ordering::Relaxed);
    BEAT_KIND.store(beat_in_bar.max(1), Ordering::Release);
}

/// Sets the blink code from the state of the box. The main loop calls this function each second.
pub fn update_code(state: &State) {
    CODE.store(blinks(state).map_or(0, |n| n as u8), Ordering::Relaxed);
}

/// Starts the LED timer. Keep the returned timer, because the LEDs stop when it drops.
///
/// The callback runs every 5 ms in the esp_timer task. It reads only atomics, so it does not wait
/// for a lock.
pub fn start(
    mut beat_led: PinDriver<'static, Output>,
    mut status_led: PinDriver<'static, Output>,
    state: Arc<State>,
) -> anyhow::Result<EspTimer<'static>> {
    let abnormal = matches!(
        state.reset_reason,
        "panic" | "interrupt watchdog" | "task watchdog" | "other watchdog" | "brownout"
    );
    update_code(&state);
    // The start time of the current repetition of the code, the code and the restart flash.
    let mut rep: Option<(u32, u8, bool)> = None;
    let timer = EspTaskTimerService::new()?.timer(move || {
        let now_ms = state.now_ms();
        let now = now_ms as u32;
        let kind = BEAT_KIND.load(Ordering::Acquire);
        let since = now.wrapping_sub(BEAT_AT.load(Ordering::Relaxed));
        if kind != 0 && since < IDLE_AFTER_MS {
            // The beat LED pulses for 30 ms. IO33 pulses for 120 ms on beat 1 and for 25 ms on
            // the other beats. The code starts again after the beats.
            rep = None;
            let _ = beat_led.set_level((since < 30).into());
            let pulse = if kind == 1 { 120 } else { 25 };
            let _ = status_led.set_level((since < pulse).into());
            return;
        }
        let _ = beat_led.set_low();
        // The callback reads the code again at the start of each repetition.
        let level = loop {
            if let Some((start, code, restart)) = rep {
                if let Some(on) = code_level(code, restart, now.wrapping_sub(start)) {
                    break on;
                }
            }
            let restart = abnormal && now_ms < SHOW_RESTART_MS;
            rep = Some((now, CODE.load(Ordering::Relaxed), restart));
        };
        let _ = status_led.set_level(level.into());
    })?;
    timer.every(TICK)?;
    Ok(timer)
}

/// Returns the number of blinks for the first check that fails. Returns `None` when the box is
/// ready.
pub fn blinks(state: &State) -> Option<u64> {
    if state.link.lock().unwrap().is_none() {
        return Some(1);
    }
    if state.own_device.load(Ordering::Relaxed) == 0 {
        return Some(2);
    }
    if state.tracker.lock().unwrap().decks().is_empty() {
        return Some(3);
    }
    if state.wifi_join_down.load(Ordering::Relaxed) {
        return Some(4);
    }
    None
}

/// Returns the level of IO33 at `t` milliseconds into one repetition of the code. Returns `None`
/// after the end of the repetition. A `code` of 0 is the code of a box that is ready.
fn code_level(code: u8, restart: bool, mut t: u32) -> Option<bool> {
    if restart {
        // The 800 ms flash and a pause of 400 ms come first.
        if t < 1200 {
            return Some(t < 800);
        }
        t -= 1200;
    }
    if t >= CODE_MS {
        return None;
    }
    Some(match u32::from(code) {
        // 1 s on, 1 s off.
        0 => t < 1000,
        // Each blink is 100 ms on and 150 ms off. The last pause fills the code to CODE_MS.
        n => t < n * 250 && t % 250 < 100,
    })
}
