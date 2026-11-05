//! The OLED status screen: a 128×64 SSD1306 or SSD1315 on I2C0 (SCL IO14, SDA IO15, address
//! 0x3C or 0x3D). An analog joystick opens a small menu: VRx on IO39, VRy on IO35 (ADC1), the
//! switch on IO5 with the internal pull-up. The joystick is optional.
//!
//! The `ui` thread runs on core 0 with a low priority. The `midi` and `prolink-rx` threads run on
//! core 1, so a slow I2C write cannot delay them. Until a display answers, the thread looks for
//! it every `RETRY`. The box works without a display.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use autoblink_core_dbserver::phrase_status;
use autoblink_core_midi::scenes::Event;
use autoblink_core_midi::MidiConfig;
use autoblink_core_ui::config::{IDX_BEAT_CHANNEL, IDX_DMX_MODE, IDX_ROTATE};
use autoblink_core_ui::joystick;
use autoblink_core_ui::menu::{Action, Menu, View};
use autoblink_core_ui::screen::{lines, Lines, Snapshot};
use autoblink_core_ui::BoxConfig;
use embedded_graphics::mono_font::ascii::{FONT_10X20, FONT_6X10};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::BinaryColor;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::text::{Baseline, Text};
use esp_idf_svc::hal::adc::attenuation::DB_12;
use esp_idf_svc::hal::adc::oneshot::config::AdcChannelConfig;
use esp_idf_svc::hal::adc::oneshot::{AdcChannelDriver, AdcDriver};
use esp_idf_svc::hal::adc::{ADC1, ADCCH3, ADCCH7, ADCU1};
use esp_idf_svc::hal::cpu::Core;
use esp_idf_svc::hal::gpio::{Gpio14, Gpio15, Gpio35, Gpio39, Gpio5, Input, PinDriver, Pull};
use esp_idf_svc::hal::i2c::{I2cConfig, I2cDriver, I2C0};
use esp_idf_svc::hal::units::Hertz;
use ssd1306::mode::BufferedGraphicsMode;
use ssd1306::prelude::*;
use ssd1306::{I2CDisplayInterface, Ssd1306};

use crate::net::AP_IP;
use crate::settings::Settings;
use crate::State;

type Display<'a> = Ssd1306<
    I2CInterface<&'a mut I2cDriver<'static>>,
    DisplaySize128x64,
    BufferedGraphicsMode<DisplaySize128x64>,
>;

/// The thread reads the joystick this often.
const TICK: Duration = Duration::from_millis(20);

/// The screen shows a new frame every this many ticks (200 ms), and at once after an input. One
/// full frame over I2C at 400 kHz takes about 25 ms.
const FRAME_TICKS: u32 = 10;

/// The I2C addresses of an SSD1306 or SSD1315 module.
const ADDRESSES: [u8; 2] = [0x3C, 0x3D];

/// Without a display, the thread looks for it again after this time.
const RETRY: Duration = Duration::from_secs(5);

/// When no deck plays for this time, the screen goes to the minimum brightness against burn-in.
const DIM_AFTER: Duration = Duration::from_secs(600);

/// The pins of the joystick.
pub struct Joystick {
    pub adc: ADC1<'static>,
    pub x: Gpio39<'static>,
    pub y: Gpio35<'static>,
    pub sw: Gpio5<'static>,
}

/// The settings that the menu shows. The thread reads them without the lock of `Settings`.
struct Values {
    device: Arc<AtomicU8>,
    midi: Arc<Mutex<MidiConfig>>,
    box_cfg: Arc<Mutex<BoxConfig>>,
}

impl Values {
    fn number(&self, idx: usize) -> i64 {
        match idx {
            2 => self.device.load(Ordering::Relaxed).into(),
            _ => 0,
        }
    }

    fn text(&self, idx: usize) -> String {
        match idx {
            2 => self.device.load(Ordering::Relaxed).to_string(),
            9 => (self.midi.lock().unwrap().note_channel + 1).to_string(),
            IDX_DMX_MODE => self.box_cfg.lock().unwrap().dmx_mode.label().into(),
            IDX_BEAT_CHANNEL => self.box_cfg.lock().unwrap().beat_channel.to_string(),
            IDX_ROTATE => if self.box_cfg.lock().unwrap().rotate {
                "On"
            } else {
                "Off"
            }
            .into(),
            _ => String::new(),
        }
    }
}

/// Starts the `ui` thread. It finds the display, then draws on it.
pub fn start(
    i2c: I2C0<'static>,
    sda: Gpio15<'static>,
    scl: Gpio14<'static>,
    joystick: Joystick,
    state: Arc<State>,
    settings: Arc<Mutex<Settings>>,
) -> anyhow::Result<()> {
    let mut i2c = I2cDriver::new(i2c, sda, scl, &I2cConfig::new().baudrate(Hertz(400_000)))?;
    let (values, network) = {
        let s = settings.lock().unwrap();
        let values = Values {
            device: s.device_number.clone(),
            midi: s.midi.clone(),
            box_cfg: s.box_cfg.clone(),
        };
        // The WiFi settings apply after a restart, so the start values stay right.
        let mut network = vec![format!("AP {}", s.ssid()), format!("Page {AP_IP}")];
        if let Some((join, _)) = s.join() {
            network.push(format!("Join {join}"));
        }
        (values, network)
    };
    crate::spawn_rt(c"ui", 8 * 1024, 3, Some(Core::Core0), move || {
        let ui = Ui {
            state: &state,
            settings: &settings,
            values: &values,
            network: &network,
        };
        let mut warned = false;
        loop {
            for addr in ADDRESSES {
                // The display struct holds the 1 KB frame buffer. It goes on the heap, not on
                // the stack of the thread. On the stack, it overflowed the 5 KB stack of 1.24.0.
                let mut display = Box::new(
                    Ssd1306::new(
                        I2CDisplayInterface::new_custom_address(&mut i2c, addr),
                        DisplaySize128x64,
                        DisplayRotation::Rotate0,
                    )
                    .into_buffered_graphics_mode(),
                );
                if display.init().is_ok() {
                    log::info!("The OLED answers at I2C address {addr:#04x}");
                    ui.run(display, joystick);
                }
            }
            if !warned {
                warned = true;
                log::warn!(
                    "No OLED at I2C address 0x3C or 0x3D. {} The firmware tries again every {} s.",
                    scan(&mut i2c),
                    RETRY.as_secs()
                );
            }
            thread::sleep(RETRY);
        }
    })?;
    Ok(())
}

/// Returns a text with the I2C addresses that answer, for the log.
fn scan(i2c: &mut I2cDriver<'static>) -> String {
    let found: Vec<String> = (0x08..0x78u8)
        .filter(|&a| i2c.write(a, &[], 10).is_ok())
        .map(|a| format!("{a:#04x}"))
        .collect();
    if found.is_empty() {
        "No I2C device answers. Examine SDA (IO15), SCL (IO14), VCC (3V3) and GND.".into()
    } else {
        format!("Other I2C devices answer: {}.", found.join(", "))
    }
}

struct Ui<'a> {
    state: &'a State,
    settings: &'a Mutex<Settings>,
    values: &'a Values,
    /// The fixed lines of the network page.
    network: &'a [String],
}

impl Ui<'_> {
    fn run(&self, mut display: Box<Display<'_>>, pins: Joystick) -> ! {
        // The ADC driver must live as long as its channel drivers.
        let Joystick { adc, x, y, sw } = pins;
        let adc = AdcDriver::new(adc);
        let mut stick = match adc.as_ref() {
            Ok(adc) => stick(adc, x, y, sw).map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        }
        .inspect_err(|e| {
            log::warn!("The joystick is off: {e}. The status screen works without it.")
        })
        .ok();
        let mut joy = joystick::Joystick::new();
        let mut menu = Menu::new();
        let mut rotate = false;
        // 0 is not a valid setting, so the first frame sets the brightness.
        let mut brightness = 0u8;
        let mut active = Instant::now();
        let mut failed = false;
        let mut tick = 0u32;
        loop {
            let now = self.state.now_ms();
            // ESP32 errata 3.11: ADC1 can pull IO36 and IO39 low for about 80 ns. IO36 is DMX IN.
            // Thus the thread reads the axes only for the calibration and while the menu is open.
            // The switch on IO5 opens the menu.
            let menu_open = menu.view() != View::Status;
            let input = stick.as_mut().and_then(|(x, y, sw)| {
                let (xv, yv) = match joy.center() {
                    Some(c) if !menu_open => c,
                    _ => (x.read_raw().ok()?, y.read_raw().ok()?),
                };
                joy.update(xv, yv, sw.is_low(), now)
            });
            if let Some(input) = input {
                active = Instant::now();
                self.apply(menu.input(input, now, |idx| self.values.number(idx)));
                // Draw the change at once.
                tick = 0;
            }
            menu.tick(now);
            if tick % FRAME_TICKS == 0 {
                let snap = snapshot(self.state);
                if snap.playing {
                    active = Instant::now();
                }
                let (want_rotate, want_brightness) = {
                    let cfg = self.values.box_cfg.lock().unwrap();
                    (cfg.rotate, cfg.brightness)
                };
                let want_brightness = if active.elapsed() >= DIM_AFTER {
                    1
                } else {
                    want_brightness
                };
                let mut result = Ok(());
                // After a write error, the display can have lost its power and its settings.
                // Initialize it again, and send the rotation and the brightness again.
                if failed {
                    result = display.init();
                    rotate = false;
                    brightness = 0;
                }
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
                if menu.view() == View::Status {
                    draw(&mut display, &lines(&snap));
                } else {
                    let iface = self.state.iface.lock().unwrap().clone();
                    let mut network = self.network.to_vec();
                    network.extend(iface.split(" · ").map(String::from));
                    let text = menu.lines(|idx| self.values.text(idx), &network);
                    draw_menu(&mut display, &text);
                }
                result = result.and(display.flush());
                // Log only the change, not each frame. A loose wire can fail each frame.
                if result.is_err() != failed {
                    failed = result.is_err();
                    match result {
                        Err(e) => log::warn!("The OLED does not answer: {e:?}"),
                        Ok(()) => log::info!("The OLED answers again"),
                    }
                }
            }
            tick = tick.wrapping_add(1);
            thread::sleep(TICK);
        }
    }

    /// Does a change of the menu with the functions of the page commands. Then the page gets the
    /// new settings.
    fn apply(&self, action: Option<Action>) {
        let result = match action {
            None => return,
            Some(Action::Restart) => {
                log::info!("Restart from the menu of the OLED");
                crate::restart_soon().map_err(|e| e.to_string())
            }
            Some(Action::Step { idx, dir }) => self.settings.lock().unwrap().step(idx, dir),
            Some(Action::Set { idx, value }) => {
                self.settings.lock().unwrap().set(idx, &value.to_string())
            }
        };
        match result {
            Ok(()) => crate::webui::settings_changed(),
            Err(e) => log::warn!("The menu cannot change the setting: {e}"),
        }
    }
}

/// VRx (IO39) is channel 3 of ADC1, VRy (IO35) is channel 7.
type Stick<'a> = (
    AdcChannelDriver<'static, ADCCH3<ADCU1>, &'a AdcDriver<'static, ADCU1>>,
    AdcChannelDriver<'static, ADCCH7<ADCU1>, &'a AdcDriver<'static, ADCU1>>,
    PinDriver<'static, Input>,
);

/// Opens the two ADC channels at 12 dB (0 to about 3.1 V) and the switch input.
fn stick<'a>(
    adc: &'a AdcDriver<'static, ADCU1>,
    x: Gpio39<'static>,
    y: Gpio35<'static>,
    sw: Gpio5<'static>,
) -> anyhow::Result<Stick<'a>> {
    let cfg = AdcChannelConfig {
        attenuation: DB_12,
        ..Default::default()
    };
    Ok((
        AdcChannelDriver::new(adc, x, &cfg)?,
        AdcChannelDriver::new(adc, y, &cfg)?,
        PinDriver::input(sw, Pull::Up)?,
    ))
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
        // The page shows the buckets of the DMX scenes (DROP, BREAK), not the rekordbox kinds.
        let name = p
            .kind
            .code()
            .and_then(Event::bucket)
            .map_or_else(|| p.kind.to_string(), |e| format!("{e:?}").to_uppercase());
        Some(format!("{name} {} bars left", p.beats_left.div_ceil(4)))
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

fn draw(display: &mut Display<'_>, l: &Lines) {
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    let large = MonoTextStyle::new(&FONT_10X20, BinaryColor::On);
    let text = |d: &mut Display<'_>, t: &str, y: i32, style| {
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

fn draw_menu(display: &mut Display<'_>, text: &[String]) {
    let small = MonoTextStyle::new(&FONT_6X10, BinaryColor::On);
    for (row, line) in text.iter().enumerate() {
        let y = 10 * row as i32;
        let _ = Text::with_baseline(line, Point::new(0, y), small, Baseline::Top).draw(display);
    }
}
