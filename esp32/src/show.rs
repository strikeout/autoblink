//! The DMX show in the flash partition `show`.
//!
//! Bytes 0 to 3 hold the length of the JSON as a little-endian `u32`. The JSON follows. An
//! erased length (0xFFFF_FFFF) or a length that is too large for the partition means "no show".
//! `save` writes the length last. Thus a power loss during a save gives "no show", not half a
//! show.

use std::io;

use autoblink_core_dmx::Show;
use esp_idf_svc::partition::EspPartition;

/// The bytes of the length field before the JSON.
const HEADER: usize = 4;
/// The size of the read buffer and of the write buffer. The buffers are on the stack.
const BUF: usize = 256;
/// The size of the chunks that `write_json` sends.
const CHUNK: usize = 1024;

pub struct ShowStore {
    part: EspPartition,
}

impl ShowStore {
    /// `None` when the partition table has no "show" partition (a box flashed before 1.24.0).
    pub fn open() -> Option<ShowStore> {
        // SAFETY: only this function opens the show partition, and `main` calls it one time.
        match unsafe { EspPartition::new("show") } {
            Ok(Some(part)) => Some(ShowStore { part }),
            Ok(None) => {
                log::warn!(
                    "The partition table has no show partition. Flash the box over USB to add it."
                );
                None
            }
            Err(e) => {
                log::warn!("The firmware cannot open the show partition: {e}");
                None
            }
        }
    }

    /// Reads the show. Erased flash or bad data gives `Show::default()`.
    pub fn load(&mut self) -> Show {
        match self.read_show() {
            Ok(Some(mut show)) => {
                show.sanitize();
                show
            }
            Ok(None) => Show::default(),
            Err(e) => {
                log::warn!("The firmware cannot read the show. It uses an empty show: {e:#}");
                Show::default()
            }
        }
    }

    /// Writes the show as JSON. Erases only the sectors that the new data needs.
    pub fn save(&mut self, show: &Show) -> anyhow::Result<()> {
        // The first pass counts the bytes. Thus a show that is too large does not erase the
        // stored show.
        let mut count = Counter(0);
        serde_json::to_writer(&mut count, show)?;
        let len = count.0;
        let max = self.part.size() - HEADER;
        anyhow::ensure!(
            len <= max,
            "The show has {len} bytes of JSON. The show partition holds {max} bytes."
        );
        let sector = self.part.erase_size();
        let end = HEADER + len;
        self.part.erase(0, end.div_ceil(sector) * sector)?;
        let mut w = Writer {
            part: &mut self.part,
            pos: HEADER,
            end,
            buf: [0; BUF],
            len: 0,
        };
        serde_json::to_writer(&mut w, show)?;
        io::Write::flush(&mut w)?;
        self.part.write(0, &(len as u32).to_le_bytes())?;
        Ok(())
    }

    /// Streams the stored JSON to `w` in chunks of 1 KB.
    pub fn write_json(
        &mut self,
        w: &mut impl FnMut(&[u8]) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let Some(len) = self.stored_len()? else {
            return w(&serde_json::to_vec(&Show::default())?);
        };
        let mut chunk = [0u8; CHUNK];
        let mut pos = HEADER;
        let end = HEADER + len;
        while pos < end {
            let n = CHUNK.min(end - pos);
            self.part.read(pos, &mut chunk[..n])?;
            w(&chunk[..n])?;
            pos += n;
        }
        Ok(())
    }

    /// Gives the length of the stored JSON, or `None` when the partition has no show.
    fn stored_len(&mut self) -> anyhow::Result<Option<usize>> {
        let mut header = [0u8; HEADER];
        self.part.read(0, &mut header)?;
        let len = u32::from_le_bytes(header);
        if len == u32::MAX || len as usize > self.part.size() - HEADER {
            return Ok(None);
        }
        Ok(Some(len as usize))
    }

    fn read_show(&mut self) -> anyhow::Result<Option<Show>> {
        let Some(len) = self.stored_len()? else {
            return Ok(None);
        };
        let reader = Reader {
            part: &mut self.part,
            pos: HEADER,
            end: HEADER + len,
            buf: [0; BUF],
            start: 0,
            filled: 0,
        };
        Ok(Some(serde_json::from_reader(reader)?))
    }
}

/// Counts the bytes of the JSON and keeps none of them.
struct Counter(usize);

impl io::Write for Counter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0 += data.len();
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Writes the JSON to the erased flash in blocks of `BUF` bytes.
struct Writer<'a> {
    part: &'a mut EspPartition,
    /// The partition offset of the first byte in `buf`.
    pos: usize,
    /// The end of the erased area. The writer does not write after it.
    end: usize,
    buf: [u8; BUF],
    len: usize,
}

impl io::Write for Writer<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.len == BUF {
            self.flush()?;
        }
        let n = data.len().min(BUF - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&data[..n]);
        self.len += n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.len == 0 {
            return Ok(());
        }
        if self.pos + self.len > self.end {
            return Err(io::Error::other(
                "The show JSON is longer than the erased area.",
            ));
        }
        self.part
            .write(self.pos, &self.buf[..self.len])
            .map_err(io::Error::other)?;
        self.pos += self.len;
        self.len = 0;
        Ok(())
    }
}

/// Reads the stored JSON in blocks of `BUF` bytes, and stops at the stored length.
struct Reader<'a> {
    part: &'a mut EspPartition,
    /// The partition offset of the next block.
    pos: usize,
    end: usize,
    buf: [u8; BUF],
    start: usize,
    filled: usize,
}

impl io::Read for Reader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.start == self.filled {
            let n = BUF.min(self.end - self.pos);
            if n == 0 {
                return Ok(0);
            }
            self.part
                .read(self.pos, &mut self.buf[..n])
                .map_err(io::Error::other)?;
            self.pos += n;
            self.start = 0;
            self.filled = n;
        }
        let n = out.len().min(self.filled - self.start);
        out[..n].copy_from_slice(&self.buf[self.start..self.start + n]);
        self.start += n;
        Ok(n)
    }
}
