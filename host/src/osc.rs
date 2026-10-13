//! OSC output over UDP for lighting desks and media servers.
//!
//! The MIDI mapper calls [`OscOut::send`] at the same events that send MIDI.
//! The `[osc]` section of the config sets the targets and the address prefix.
//!
//! ponytail: the encoder writes single messages with `i`, `f` and `s` arguments only.
//! It writes no bundles and no time tags. Use the `rosc` crate if a desk needs them.

use std::fmt;
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

use crate::config::SharedConfig;
use crate::midi::activity::MidiActivity;

/// One OSC argument.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Arg<'a> {
    I(i32),
    F(f32),
    S(&'a str),
}

impl fmt::Display for Arg<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Arg::I(v) => write!(f, "{v}"),
            Arg::F(v) => write!(f, "{v:.2}"),
            Arg::S(s) => write!(f, "{s:?}"),
        }
    }
}

/// Add a NUL terminator, then pad the buffer to a multiple of 4 bytes.
fn pad(buf: &mut Vec<u8>) {
    buf.push(0);
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}

/// Encode one OSC message.
pub fn encode(addr: &str, args: &[Arg]) -> Vec<u8> {
    let mut buf = addr.as_bytes().to_vec();
    pad(&mut buf);
    buf.push(b',');
    for arg in args {
        buf.push(match arg {
            Arg::I(_) => b'i',
            Arg::F(_) => b'f',
            Arg::S(_) => b's',
        });
    }
    pad(&mut buf);
    for arg in args {
        match arg {
            Arg::I(v) => buf.extend_from_slice(&v.to_be_bytes()),
            Arg::F(v) => buf.extend_from_slice(&v.to_be_bytes()),
            Arg::S(s) => {
                buf.extend(s.bytes().filter(|&b| b != 0));
                pad(&mut buf);
            }
        }
    }
    buf
}

/// Sends OSC messages to the targets in `cfg.osc`.
pub struct OscOut {
    sock: Option<UdpSocket>,
    /// The counters that the web UI shows. `None` in a test that does not read them.
    activity: Option<Arc<Mutex<MidiActivity>>>,
}

impl OscOut {
    /// Opens the socket. Each send updates the OSC counters in `activity`.
    pub fn with_activity(activity: Arc<Mutex<MidiActivity>>) -> Self {
        Self {
            activity: Some(activity),
            ..Self::new()
        }
    }

    pub fn new() -> Self {
        let sock = UdpSocket::bind("0.0.0.0:0").and_then(|s| {
            s.set_nonblocking(true)?;
            s.set_broadcast(true)?;
            Ok(s)
        });
        match sock {
            Ok(s) => Self {
                sock: Some(s),
                activity: None,
            },
            Err(e) => {
                tracing::warn!(error = %e, "The OSC socket did not open. The app sends no OSC.");
                Self {
                    sock: None,
                    activity: None,
                }
            }
        }
    }

    /// Send `prefix + path` with `args` to each target. Do nothing when OSC is off.
    pub fn send(&self, cfg: &SharedConfig, path: &str, args: &[Arg]) {
        let Some(sock) = &self.sock else { return };
        let cfg = cfg.read();
        if !cfg.osc.enabled {
            return;
        }
        let addr = format!("{}{}", cfg.osc.prefix, path);
        let msg = encode(&addr, args);
        let (mut sent, mut errors) = (0, 0);
        for target in &cfg.osc.targets {
            // A parsed address needs no DNS lookup, so the send never blocks.
            let Ok(to) = target.parse::<std::net::SocketAddr>() else {
                tracing::debug!(%target, "The OSC target is not an IP address with a port");
                errors += 1;
                continue;
            };
            match sock.send_to(&msg, to) {
                Ok(_) => sent += 1,
                Err(e) => {
                    tracing::debug!(%target, error = %e, "OSC send failed");
                    errors += 1;
                }
            }
        }
        drop(cfg);
        // The MIDI tasks share the counters. A busy lock skips this update and does not wait.
        if let Some(mut act) = self.activity.as_ref().and_then(|a| a.try_lock()) {
            act.osc_sent += sent;
            act.osc_errors += errors;
            if sent > 0 {
                let text = args.iter().fold(addr, |t, a| format!("{t} {a}"));
                act.last_osc = Some((text, Instant::now()));
            }
        }
    }
}

impl Default for OscOut {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_float_message() {
        let mut want = b"/xdj/bpm\0\0\0\0,f\0\0".to_vec();
        want.extend_from_slice(&128.0f32.to_be_bytes());
        assert_eq!(encode("/xdj/bpm", &[Arg::F(128.0)]), want);
    }

    #[test]
    fn encodes_string_and_int_with_padding() {
        let mut want = b"/xdj/phrase\0,si\0INTRO\0\0\0".to_vec();
        want.extend_from_slice(&0i32.to_be_bytes());
        assert_eq!(encode("/xdj/phrase", &[Arg::S("INTRO"), Arg::I(0)]), want);
    }

    #[test]
    fn encodes_message_without_arguments() {
        assert_eq!(
            encode("/xdj/downbeat", &[]),
            b"/xdj/downbeat\0\0\0,\0\0\0".to_vec()
        );
    }

    #[test]
    fn sends_to_configured_target() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let cfg = crate::config::new_shared(crate::config::Config::default());
        let out = OscOut::new();

        out.send(&cfg, "/beat", &[Arg::I(1)]);
        {
            let mut c = cfg.write();
            c.osc.enabled = true;
            c.osc.targets = vec![rx.local_addr().unwrap().to_string()];
        }
        out.send(&cfg, "/beat", &[Arg::I(2)]);

        let mut buf = [0u8; 64];
        let n = rx.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], encode("/xdj/beat", &[Arg::I(2)]).as_slice());
    }

    #[test]
    fn send_counts_messages_and_bad_targets() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let cfg = crate::config::new_shared(crate::config::Config::default());
        {
            let mut c = cfg.write();
            c.osc.enabled = true;
            c.osc.targets = vec![rx.local_addr().unwrap().to_string(), "desk:8000".into()];
        }
        let activity = Arc::new(Mutex::new(MidiActivity::default()));
        let out = OscOut::with_activity(Arc::clone(&activity));

        out.send(&cfg, "/phrase", &[Arg::S("INTRO"), Arg::F(0.5)]);

        let act = activity.lock();
        assert_eq!((act.osc_sent, act.osc_errors), (1, 1));
        let (text, _) = act.last_osc.as_ref().unwrap();
        assert_eq!(text, "/xdj/phrase \"INTRO\" 0.50");
    }
}
