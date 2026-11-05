//! The OLED status screen: a 128×64 SSD1306 or SSD1315 on I2C0 (SCL IO14, SDA IO15, address
//! 0x3C).
//!
//! The `ui` thread runs on core 0 with a low priority. The `midi` and `prolink-rx` threads run on
//! core 1, so a slow I2C write cannot delay them. Without a display, the thread does not start
//! and the box works as before.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use autoblink_core_dbserver::phrase_status;
use autoblink_core_ui::screen::{lines, Lines, Snapshot};
use autoblink_core_ui::BoxConfig;
use embedded_graphics::mono_font::ascii::{FONT_10X20, FONT_6X10};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::text::{Baseline, Text};
use esp_idf_svc::hal::cpu::Core;
use esp_idf_svc::hal::gpio::{Gpio14, Gpio15};
use esp_idf_svc::hal::i2c::{I2cConfig, I2cDriver, I2C0};
use esp_idf_svc::hal::units::Hertz;
use ssd1306::mode::BufferedGraphicsMode;
use ssd1306::prelude::*;
use ssd1306::{I2CDisplayInterface, Ssd1306};

use crate::State;

type Display = Ssd1306<
    I2CInterface<I2cDriver<'static>>,
    DisplaySize128x64,
    BufferedGraphicsMode<DisplaySize128x64>,
>;

/// The screen shows a new frame this often. One full frame over I2C at 400 kHz takes about 25 ms.
const FRAME: Duration = Duration::from_millis(200);

/// When no deck plays for this time, the screen goes to the minimum brightness against burn-in.
const DIM_AFTER: Duration = Duration::from_secs(600);

/// Starts the `ui` thread. Without a display, it logs a warning and starts no thread.
pub fn start(
    i2c: I2C0<'static>,
    sda: Gpio15<'static>,
    scl: Gpio14<'static>,
    state: Arc<State>,
    cfg: Arc<Mutex<BoxConfig>>,
) -> anyhow::Result<()> {
    let i2c = I2cDriver::new(i2c, sda, scl, &I2cConfig::new().baudrate(Hertz(400_000)))?;
    let mut display = Ssd1306::new(
        I2CDisplayInterface::new(i2c),
        DisplaySize128x64,
        DisplayRotation::Rotate0,
    )
    .into_buffered_graphics_mode();
    if let Err(e) = display.init() {
        log::warn!("No OLED at I2C address 0x3C ({e:?}). The box works without it.");
        return Ok(());
    }
    crate::spawn_rt(c"ui", 4 * 1024, 3, Some(Core::Core0), move || {
        run(display, &state, &cfg)
    })?;
    log::info!("The OLED is on I2C0 (SCL IO14, SDA IO15)");
    Ok(())
}

fn run(mut display: Display, state: &State, cfg: &Mutex<BoxConfig>) {
    let mut rotate = false;
    // 0 is not a valid setting, so the first frame sets the brightness.
    let mut brightness = 0u8;
    let mut active = Instant::now();
    let mut failed = false;
    loop {
        let snap = snapshot(state);
        if snap.playing {
            active = Instant::now();
        }
        let (want_rotate, want_brightness) = {
            let cfg = cfg.lock().unwrap();
            (cfg.rotate, cfg.brightness)
        };
        let want_brightness = if active.elapsed() >= DIM_AFTER {
            1
        } else {
            want_brightness
        };
        let mut result = Ok(());
        if want_rotate != rotate {
            rotate = want_rotate;
            let r = if rotate {
                DisplayRotation::Rotate180
            } else {
                DisplayRotation::Rotate0
            };
            result = result.and(display.set_rotation(r));
        }
        if want_brightness != brightness {
            brightness = want_brightness;
            result = result.and(display.set_brightness(Brightness::custom(2, brightness)));
        }
        display.clear_buffer();
        draw(&mut display, &lines(&snap));
        result = result.and(display.flush());
        // Log only the change, not each frame. A loose wire can fail each frame.
        if result.is_err() != failed {
            failed = result.is_err();
            match result {
                Err(e) => log::warn!("The OLED does not answer: {e:?}"),
                Ok(()) => log::info!("The OLED answers again"),
            }
        }
        thread::sleep(FRAME);
    }
}

/// Copies the values of the screen from the state. Each lock is taken alone.
fn snapshot(state: &State) -> Snapshot {
    let master = state.tracker.lock().unwrap().master().cloned();
    let phrase = master.as_ref().and_then(|d| {
        let beat_count = d.beat_count?;
        let tracks = state.tracks.0.lock().unwrap();
        let t = tracks
            .get(&d.num)
            .filter(|t| t.rekordbox_id == d.track.rekordbox_id && t.rekordbox_id != 0)?;
        let Some(Ok(ss)) = &t.structure else {
            return None;
        };
        let p = phrase_status(ss, beat_count)?;
        Some(format!("{} {} bars left", p.kind, p.beats_left.div_ceil(4)))
    });
    let blinks = crate::led::blinks(state);
    let ip = state.link.lock().unwrap().map(|l| l.ip.to_string());
    let dmx = state.dmx.lock().unwrap().map(|d| (d.signal, d.in_fps));
    let now = state.now_ms();
    Snapshot {
        master: master.as_ref().map(|d| d.num),
        playing: master.as_ref().is_some_and(|d| d.is_playing(now)),
        bpm: master.as_ref().map_or(0.0, |d| d.bpm),
        beat: master.as_ref().map_or(0, |d| d.beat),
        phrase,
        blinks,
        ip,
        dmx,
    }
}

fn draw(display: &mut Display, l: &Lines) {
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let large = MonoTextStyle::new(&FONT_10X20, BinaryColor::On);
    let text = |d: &mut Display, t: &str, y: i32, style| {
        let _ = Text::with_baseline(t, Point::new(0, y), style, Baseline::Top).draw(d);
    };
    text(display, &l.deck, 0, small);
    // Four boxes for the beat in the bar. The box of the present beat is full.
    for i in 1..=4u8 {
        let style = if i == l.beat {
            PrimitiveStyle::with_fill(BinaryColor::On)
        } else {
            PrimitiveStyle::with_stroke(BinaryColor::On, 1)
        };
        let x = 88 + 10 * i32::from(i - 1);
        let _ = Rectangle::new(Point::new(x, 0), Size::new(8, 8))
            .into_styled(style)
            .draw(display);
    }
    text(display, &l.bpm, 12, large);
    text(display, &l.phrase, 34, small);
    text(display, &l.net, 44, small);
    text(display, &l.dmx, 54, small);
}
