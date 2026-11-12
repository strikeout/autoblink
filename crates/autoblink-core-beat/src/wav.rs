//! A 16-bit PCM WAV header for `GET /mic`, and a reader for the eval example.

/// The header of a 16-bit PCM WAV file with `frames` frames.
pub fn header(frames: u32, channels: u16, rate: u32) -> [u8; 44] {
    let data = frames * u32::from(channels) * 2;
    let mut h = [0u8; 44];
    let mut put = |at: usize, b: &[u8]| h[at..at + b.len()].copy_from_slice(b);
    put(0, b"RIFF");
    put(4, &(36 + data).to_le_bytes());
    put(8, b"WAVEfmt ");
    put(16, &16u32.to_le_bytes());
    put(20, &1u16.to_le_bytes());
    put(22, &channels.to_le_bytes());
    put(24, &rate.to_le_bytes());
    put(28, &(rate * u32::from(channels) * 2).to_le_bytes());
    put(32, &(channels * 2).to_le_bytes());
    put(34, &16u16.to_le_bytes());
    put(36, b"data");
    put(40, &data.to_le_bytes());
    h
}

/// A 16-bit PCM WAV file. The samples of the channels alternate.
#[derive(Debug, Clone, PartialEq)]
pub struct Wav {
    pub rate: u32,
    pub channels: u16,
    pub samples: Vec<i16>,
}

/// Reads a 16-bit PCM WAV file. Skips the chunks that it does not know.
pub fn read(bytes: &[u8]) -> Result<Wav, &'static str> {
    let u32_at = |at: usize| -> Option<u32> {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err("not a WAV file");
    }
    let (mut at, mut fmt) = (12, None);
    while let (Some(id), Some(len)) = (bytes.get(at..at + 4), u32_at(at + 4)) {
        let body = at + 8;
        let len = len as usize;
        match id {
            b"fmt " => {
                let f = bytes.get(body..body + 16).ok_or("short fmt chunk")?;
                if u16::from_le_bytes([f[0], f[1]]) != 1 || u16::from_le_bytes([f[14], f[15]]) != 16
                {
                    return Err("not 16-bit PCM");
                }
                fmt = Some((
                    u16::from_le_bytes([f[2], f[3]]),
                    u32::from_le_bytes([f[4], f[5], f[6], f[7]]),
                ));
            }
            b"data" => {
                let (channels, rate) = fmt.ok_or("data before fmt")?;
                let end = (body + len).min(bytes.len());
                let samples = bytes[body..end]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes(*b))
                    .collect();
                return Ok(Wav {
                    rate,
                    channels,
                    samples,
                });
            }
            _ => {}
        }
        at = body + len + (len & 1);
    }
    Err("no data chunk")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_round_trips() {
        let mut bytes = header(16_000, 2, 16_000).to_vec();
        bytes.extend((0..32_000i32).flat_map(|i| ((i % 100) as i16).to_le_bytes()));
        let w = read(&bytes).unwrap();
        assert_eq!(w.rate, 16_000);
        assert_eq!(w.channels, 2);
        assert_eq!(w.samples.len(), 32_000);
        assert_eq!(w.samples[99], 99);
    }

    #[test]
    fn read_skips_an_unknown_chunk() {
        let h = header(1, 1, 16_000);
        let mut bytes = h[..36].to_vec();
        bytes.extend(b"LIST");
        bytes.extend(4u32.to_le_bytes());
        bytes.extend(b"abcd");
        bytes.extend(&h[36..]);
        bytes.extend(7i16.to_le_bytes());
        assert_eq!(read(&bytes).unwrap().samples, vec![7]);
    }

    #[test]
    fn read_refuses_other_files() {
        assert!(read(b"not a wav file at all, but long enough to have a header").is_err());
    }
}
