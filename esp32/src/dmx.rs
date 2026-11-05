//! DMX512 on UART1: output on IO4, input on IO36. The feature `dmx` is on by default.
//!
//! Two RS-485 modules (MAX3485 or SP3485) turn the UART signals into DMX. Channel 1 flashes to
//! full on each master beat and fades out in `FLASH_MS`. The other channels stay at 0.
//!
//! The input receives the frames of a console. One frame is a break, the start code and up to 512
//! channels. The thread puts the channels of each good frame into the input buffer.
//!
//! ponytail: one fixed effect on channel 1. Settings for channels and effects come when the DMX
//! behaviour is decided.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use esp_idf_svc::hal::delay::TickType;
use esp_idf_svc::hal::gpio::{Gpio36, Gpio4};
use esp_idf_svc::hal::uart::{config, UartDriver, UartEventPayload, UART1};
use esp_idf_svc::hal::units::Hertz;

use crate::{DmxStatus, State};

/// The fade time of the beat flash.
const FLASH_MS: f32 = 200.0;

/// The time between the starts of two output frames. The UART sends one frame (512 channels) in
/// about 23 ms. The idle time after the break is the mark after break of the next frame. Thus the
/// rate is about 41 Hz.
const FRAME_PERIOD: Duration = Duration::from_millis(24);

/// The break length in bit times. At 250 kbaud, one bit is 4 µs, so 25 bits give 100 µs.
/// DMX512 needs at least 88 µs.
const BREAK_BITS: i32 = 25;

/// The start code and the 512 channels.
const FRAME_LEN: usize = 513;

/// The input has no signal when no good frame arrives for this time.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(1);

/// The ESP32 UART receives a break as one 0x00 byte. That byte is the last byte before the
/// data event that closes the frame.
///
/// ponytail: the loopback test (DMX OUT to DMX IN) must show 512 channels. If it shows 511, the
/// UART does not store the break byte. Then set this to `false`.
const BREAK_BYTE: bool = true;

#[derive(Clone, Default)]
pub struct Dmx {
    last_beat: Arc<Mutex<Option<Instant>>>,
}

impl Dmx {
    /// Starts the thread that sends and receives the frames.
    pub fn start(
        uart: UART1<'static>,
        tx: Gpio4<'static>,
        rx: Gpio36<'static>,
        state: Arc<State>,
    ) -> anyhow::Result<Self> {
        // A receive timeout of one character time puts the bytes before a break into the receive
        // buffer while the break is on the line. Then the data event that follows a break event
        // closes the frame.
        let cfg = config::Config {
            event_config: config::EventConfig {
                receive_timeout: Some(1),
                ..Default::default()
            },
            ..config::Config::new()
                .baudrate(Hertz(250_000))
                .stop_bits(config::StopBits::STOP2)
                .rx_fifo_size(2048)
                .tx_fifo_size(1024)
                .queue_size(32)
        };
        let uart = UartDriver::new(uart, tx, rx, None::<Gpio36>, None::<Gpio4>, &cfg)?;
        let dmx = Self::default();
        let last_beat = dmx.last_beat.clone();
        *state.dmx.lock().unwrap() = Some(DmxStatus::default());
        crate::spawn(c"dmx", 4 * 1024, move || run(&uart, &state, &last_beat))?;
        log::info!("DMX output is on IO4, DMX input is on IO36");
        Ok(dmx)
    }

    pub fn beat(&self) {
        *self.last_beat.lock().unwrap() = Some(Instant::now());
    }
}

fn run(uart: &UartDriver, state: &State, last_beat: &Mutex<Option<Instant>>) {
    let Some(events) = uart.event_queue() else {
        log::error!("DMX: the UART has no event queue");
        return;
    };
    // The buffers are on the heap, not on the stack of the thread. The receive buffer has space
    // for a frame, the break byte and one byte that shows a frame that is too long.
    let mut out: Box<[u8; FRAME_LEN]> = Box::new([0; FRAME_LEN]);
    let mut input: Box<[u8; 512]> = Box::new([0; 512]);
    let mut rx: Box<[u8; FRAME_LEN + 2]> = Box::new([0; FRAME_LEN + 2]);
    let mut rx_len = 0;
    let mut rx_bad = false;
    let mut break_seen = false;
    let mut last_good: Option<Instant> = None;
    let mut frames = 0u16;
    let mut rejected = 0u32;
    let mut next_count = Instant::now() + Duration::from_secs(1);
    let mut next_send = Instant::now();
    loop {
        let wait = next_send.saturating_duration_since(Instant::now());
        if let Some((event, _)) = events.recv_front(TickType::from(wait).ticks()) {
            match event.payload() {
                UartEventPayload::Data { size, timeout } => {
                    // Read exactly the bytes of this event. Thus a frame ends at the event that
                    // closes it, also when the thread is late and the next frame is in the buffer.
                    let mut left = size;
                    while left > 0 {
                        let room = rx.len() - rx_len;
                        let n = if room == 0 {
                            rx_bad = true;
                            let mut skip = [0u8; 64];
                            uart.read(&mut skip[..left.min(64)], 0).unwrap_or(0)
                        } else {
                            let n = uart
                                .read(&mut rx[rx_len..rx_len + left.min(room)], 0)
                                .unwrap_or(0);
                            rx_len += n;
                            n
                        };
                        if n == 0 {
                            break;
                        }
                        left -= n;
                    }
                    if timeout && break_seen {
                        match close_frame(&rx[..rx_len], rx_bad) {
                            Some(channels) => {
                                input[..channels.len()].copy_from_slice(channels);
                                last_good = Some(Instant::now());
                                frames = frames.saturating_add(1);
                            }
                            None => rejected = rejected.wrapping_add(1),
                        }
                        rx_len = 0;
                        rx_bad = false;
                        break_seen = false;
                    }
                }
                UartEventPayload::Break => break_seen = true,
                UartEventPayload::RxFifoOverflow | UartEventPayload::RxBufferFull => {
                    // Drop the frame and the events of the bytes that are gone.
                    let _ = uart.clear_rx();
                    while events.recv_front(0).is_some() {}
                    rx_len = 0;
                    rx_bad = false;
                    break_seen = false;
                    rejected = rejected.wrapping_add(1);
                }
                _ => {}
            }
        }

        let signal = last_good.is_some_and(|t| t.elapsed() < SIGNAL_TIMEOUT);
        if !signal {
            input.fill(0);
        }
        if Instant::now() >= next_count {
            next_count += Duration::from_secs(1);
            let mut status = state.dmx.lock().unwrap();
            let s = status.get_or_insert_with(DmxStatus::default);
            if s.signal != signal {
                if signal {
                    log::info!("DMX input: signal, {frames} frames/s");
                } else {
                    log::info!("DMX input: no signal. {rejected} frames rejected");
                }
            }
            s.signal = signal;
            s.in_fps = frames;
            frames = 0;
        }

        if Instant::now() >= next_send {
            next_send += FRAME_PERIOD;
            // After a long stop, do not send the missed frames.
            if next_send < Instant::now() {
                next_send = Instant::now() + FRAME_PERIOD;
            }
            let level = last_beat
                .lock()
                .unwrap()
                .map_or(0.0, |t| 1.0 - t.elapsed().as_secs_f32() * 1000.0 / FLASH_MS);
            // Byte 0 is the start code (0 for dimmer data). Bytes 1 to 512 are the channels.
            out[1] = (level.clamp(0.0, 1.0) * 255.0) as u8;
            // The UART sends the frame, then the break. The break before the next frame and the
            // idle time after it make the reset sequence of DMX512.
            if let Err(e) = uart.write_with_break(&out[..], BREAK_BITS) {
                log::warn!("DMX frame not sent: {e}");
            }
        }
    }
}

/// Returns the channels of a received frame, or `None` when the frame is not good.
fn close_frame(rx: &[u8], bad: bool) -> Option<&[u8]> {
    let frame = if BREAK_BYTE {
        rx.split_last().filter(|(b, _)| **b == 0)?.1
    } else {
        rx
    };
    // Byte 0 is the start code. Only start code 0 has dimmer data.
    match frame.split_first() {
        Some((0, channels)) if !bad && !channels.is_empty() && frame.len() <= FRAME_LEN => {
            Some(channels)
        }
        _ => None,
    }
}
