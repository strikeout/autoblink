//! The two LEDs.
//!
//! IO32 pulses for 30 ms on each beat of the master deck. IO33 has the label 485_EN on the board.
//! It shows the state of the box. While the master deck plays, IO33 pulses for 120 ms on beat 1
//! and for 25 ms on the other beats. When no beat arrives for 2 s, IO33 shows a blink code:
//!
//! - 1 blink: Ethernet has no address.
//! - 2 blinks: the virtual CDJ is not on the Pro DJ Link network.
//! - 3 blinks: the firmware finds no player.
//! - 4 blinks: the firmware cannot join the WiFi network of the settings.
//! - 1 s on, 1 s off: the box is ready.
//!
//! After a panic, a watchdog or a brownout restart, an 800 ms flash comes before each code for
//! 10 minutes.

use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use esp_idf_svc::hal::gpio::{Output, PinDriver};

use crate::State;

/// The time without a beat after which IO33 shows the blink code.
const IDLE_AFTER: Duration = Duration::from_secs(2);
/// The time after an abnormal restart during which the 800 ms flash comes before the code.
const SHOW_RESTART_MS: u64 = 10 * 60 * 1000;
/// The time of one repetition of a blink code, in milliseconds.
const CODE_MS: u64 = 2000;

/// Starts the LED thread. Send the beat in the bar (1 to 4) to the returned channel.
pub fn start(
    mut beat_led: PinDriver<'static, Output>,
    mut status_led: PinDriver<'static, Output>,
    state: Arc<State>,
) -> anyhow::Result<Sender<u8>> {
    let abnormal = matches!(
        state.reset_reason,
        "panic" | "interrupt watchdog" | "task watchdog" | "other watchdog" | "brownout"
    );
    let (tx, rx) = mpsc::channel::<u8>();
    thread::Builder::new()
        .name("leds".into())
        .stack_size(3 * 1024)
        .spawn(move || {
            let mut last_beat: Option<Instant> = None;
            let mut code = Vec::new();
            let mut step = 0;
            loop {
                let since = last_beat.map_or(IDLE_AFTER, |t| t.elapsed());
                let idle = since >= IDLE_AFTER;
                let timeout = if idle {
                    // Read the state again at the start of each repetition of the code.
                    if step == 0 {
                        let restart = abnormal && state.now_ms() < SHOW_RESTART_MS;
                        code = pattern(blinks(&state), restart);
                    }
                    let (on, ms) = code[step];
                    let _ = status_led.set_level(on.into());
                    Duration::from_millis(ms)
                } else {
                    IDLE_AFTER - since
                };
                let beat = match rx.recv_timeout(timeout) {
                    Ok(beat) => beat,
                    Err(RecvTimeoutError::Timeout) => {
                        if idle {
                            step = (step + 1) % code.len();
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                last_beat = Some(Instant::now());
                step = 0;
                let _ = beat_led.set_high();
                let _ = status_led.set_high();
                thread::sleep(Duration::from_millis(25));
                if beat != 1 {
                    let _ = status_led.set_low();
                }
                thread::sleep(Duration::from_millis(5));
                let _ = beat_led.set_low();
                if beat == 1 {
                    thread::sleep(Duration::from_millis(90));
                    let _ = status_led.set_low();
                }
            }
        })?;
    Ok(tx)
}

/// Returns the number of blinks for the first check that fails. Returns `None` when the box is
/// ready.
fn blinks(state: &State) -> Option<u64> {
    if state.eth.lock().unwrap().is_none() {
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

/// Returns one repetition of the code: the level of IO33 and its time in milliseconds.
fn pattern(blinks: Option<u64>, restart: bool) -> Vec<(bool, u64)> {
    let mut p = Vec::new();
    if restart {
        p.extend([(true, 800), (false, 400)]);
    }
    match blinks {
        Some(n) => {
            for _ in 0..n {
                p.extend([(true, 100), (false, 150)]);
            }
            // The last pause fills the code to CODE_MS.
            if let Some(last) = p.last_mut() {
                last.1 += CODE_MS - n * 250;
            }
        }
        None => p.extend([(true, 1000), (false, 1000)]),
    }
    p
}
