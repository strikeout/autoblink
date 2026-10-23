//! DMX512 output on UART1, TX on IO4. Build with `--features dmx`.
//!
//! An RS-485 module (MAX3485 or SP3485) turns the UART signal into DMX. Channel 1 flashes to
//! full on each master beat and fades out in `FLASH_MS`. The other channels stay at 0.
//!
//! ponytail: one fixed effect on channel 1. Settings for channels and effects come when the DMX
//! behaviour is decided.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use esp_idf_svc::hal::gpio::{Gpio36, Gpio4};
use esp_idf_svc::hal::uart::{config, UartDriver, UART1};
use esp_idf_svc::hal::units::Hertz;

/// The fade time of the beat flash.
const FLASH_MS: f32 = 200.0;

/// The idle time between two frames. The UART sends one frame (break, start code and 512
/// channels) in about 23 ms, so the rate is about 42 Hz.
const FRAME_GAP: Duration = Duration::from_millis(1);

/// The break length in bit times. At 250 kbaud, one bit is 4 µs, so 25 bits give 100 µs.
/// DMX512 needs at least 88 µs.
const BREAK_BITS: i32 = 25;

#[derive(Clone, Default)]
pub struct Dmx {
    last_beat: Arc<Mutex<Option<Instant>>>,
}

impl Dmx {
    /// Starts the frame thread. `rx` is not used. The UART driver of ESP-IDF 5.5 does not install
    /// without a receive buffer, so the driver gets the input-only pin IO36.
    pub fn start(uart: UART1<'static>, tx: Gpio4<'static>, rx: Gpio36<'static>) -> anyhow::Result<Self> {
        let cfg = config::Config::new()
            .baudrate(Hertz(250_000))
            .stop_bits(config::StopBits::STOP2);
        let uart = UartDriver::new(uart, tx, rx, None::<Gpio36>, None::<Gpio4>, &cfg)?;
        let dmx = Self::default();
        let last_beat = dmx.last_beat.clone();
        crate::spawn(c"dmx", 4 * 1024, move || {
            // Byte 0 is the start code (0 for dimmer data). Bytes 1 to 512 are the channels.
            let mut frame = [0u8; 513];
            loop {
                let level = last_beat.lock().unwrap().map_or(0.0, |t| {
                    1.0 - t.elapsed().as_secs_f32() * 1000.0 / FLASH_MS
                });
                frame[1] = (level.clamp(0.0, 1.0) * 255.0) as u8;
                // The UART sends the frame, then the break. The break before the next frame
                // and the idle time after it make the reset sequence of DMX512.
                if let Err(e) = uart.write_with_break(&frame, BREAK_BITS) {
                    log::warn!("DMX frame not sent: {e}");
                }
                let _ = uart.wait_tx_done(100);
                thread::sleep(FRAME_GAP);
            }
        })?;
        log::info!("DMX output is on IO4");
        Ok(dmx)
    }

    pub fn beat(&self) {
        *self.last_beat.lock().unwrap() = Some(Instant::now());
    }
}
