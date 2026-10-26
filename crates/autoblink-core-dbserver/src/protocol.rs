//! The dbserver wire format: constants, fields and messages.
//!
//! The functions are pure or generic over `std::io::Read`. They do not open a socket.
//! An error is a text string that names the fault.

use std::io::{BufReader, Read};

/// TCP port used to discover the dbserver port of a player.
pub const DB_SERVER_QUERY_PORT: u16 = 12523;

/// The number that starts every dbserver message. It is sent as a u32 field.
pub const DB_MAGIC: u32 = 0x8723_49ae;
/// The argument type tag of a number, a string and a blob.
pub const ARG_TAG_NUMBER: u8 = 0x06;
pub const ARG_TAG_STRING: u8 = 0x02;
pub const ARG_TAG_BLOB: u8 = 0x03;
/// A dbserver message has at most 12 arguments. A larger count is a bad peer.
pub const MAX_ARG_COUNT: usize = 64;
/// The largest dbserver field that the reader accepts. Waveform blobs are smaller.
#[cfg(not(target_os = "espidf"))]
pub const MAX_FIELD_LEN: usize = 16 * 1024 * 1024;
/// The largest dbserver field that the reader accepts on the ESP32. The client streams the PWV4
/// preview and the PQTZ beat grid (see `MAX_STREAM_LEN`). This limit applies to the other fields.
/// Without a limit, one large field can use all the free heap. The firmware then restarts each
/// time that it fetches the track.
#[cfg(target_os = "espidf")]
pub const MAX_FIELD_LEN: usize = 48 * 1024;

/// The largest blob that [`read_message_with`] streams. A streamed blob does not use
/// `MAX_FIELD_LEN`, because the reader keeps only a part of it. Thus a long track gets a beat
/// grid too.
pub const MAX_STREAM_LEN: usize = 1024 * 1024;

/// The largest number of waveform preview columns that the client keeps. A player sends 1200
/// columns of 6 bytes. On the ESP32, 600 columns keep 3600 bytes of heap for each deck, and the
/// page draws them across the full width.
#[cfg(target_os = "espidf")]
pub const PREVIEW_COLUMNS: usize = 600;
/// The largest number of waveform preview columns that the client keeps. The host keeps all.
#[cfg(not(target_os = "espidf"))]
pub const PREVIEW_COLUMNS: usize = usize::MAX;

// Message types.
pub const MSG_SETUP_REQ: u16 = 0x0000;
pub const MSG_METADATA_REQ: u16 = 0x2002;
pub const MSG_RENDER_MENU: u16 = 0x3000;
pub const MSG_MENU_HEADER: u16 = 0x4001;
pub const MSG_MENU_ITEM: u16 = 0x4101;
pub const MSG_MENU_FOOTER: u16 = 0x4201;
pub const MSG_ANLZ_TAG_REQ: u16 = 0x2c04;
/// The response to the analysis tag request has type 0x4f02, not 0x2c04.
pub const MSG_ANLZ_TAG_RESP: u16 = 0x4f02;
pub const MSG_CUE_LIST_EXT_REQ: u16 = 0x2b04;
pub const MSG_CUE_LIST_EXT_RESP: u16 = 0x4e02;

/// The menu identifier "data" of an RMST argument (Beat Link `MenuIdentifier.DATA`).
pub const MENU_DATA: u8 = 0x08;
/// An analysis tag code or a file extension is sent as its ASCII bytes in reverse order.
pub const TAG_PSSI: u32 = u32::from_le_bytes(*b"PSSI");
pub const TAG_PWV4: u32 = u32::from_le_bytes(*b"PWV4");
pub const TAG_PQTZ: u32 = u32::from_le_bytes(*b"PQTZ");
pub const EXT_EXT: u32 = u32::from_le_bytes(*b"EXT\0");
pub const EXT_DAT: u32 = u32::from_le_bytes(*b"DAT\0");

// Field type tags.
pub const FIELD_U8: u8 = 0x0f;
pub const FIELD_U16: u8 = 0x10;
pub const FIELD_U32: u8 = 0x11;
pub const FIELD_BLOB: u8 = 0x14;
pub const FIELD_STRING: u8 = 0x26;

/// A field value in the dbserver protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    /// Numeric field (1, 2, or 4 bytes).
    Number(u32),
    /// Binary blob.
    Blob(Vec<u8>),
    /// UTF-16BE string (decoded to a Rust string).
    Text(String),
}

/// A parsed dbserver message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbMessage {
    pub msg_type: u16,
    pub args: Vec<Field>,
}

/// Encodes a bare number field of 1, 2 or 4 bytes (used for the greeting exchange).
pub fn encode_number_field(value: u32, size: u8) -> Result<Vec<u8>, String> {
    let mut buf = Vec::with_capacity(5);
    match size {
        1 => {
            buf.push(FIELD_U8);
            buf.push(value as u8);
        }
        2 => {
            buf.push(FIELD_U16);
            buf.extend_from_slice(&(value as u16).to_be_bytes());
        }
        4 => {
            buf.push(FIELD_U32);
            buf.extend_from_slice(&value.to_be_bytes());
        }
        _ => return Err(format!("Invalid number field size {size}")),
    }
    Ok(buf)
}

/// Encodes a dbserver message. `type_tags` gives the field size of each number argument.
/// A number without an entry in `type_tags` is a u32 field.
pub fn encode_message(tx_id: u32, msg_type: u16, args: &[Field], type_tags: &[u8]) -> Vec<u8> {
    let arg_count = args.len() as u8;

    // Build the type-tag area (12 bytes, padded with 0x00).
    // An argument type tag is not the field type tag: a number of any size has tag 0x06.
    let mut tags = [0u8; 12];
    for (tag, arg) in tags.iter_mut().zip(args) {
        *tag = match arg {
            Field::Number(_) => ARG_TAG_NUMBER,
            Field::Text(_) => ARG_TAG_STRING,
            Field::Blob(_) => ARG_TAG_BLOB,
        };
    }

    let mut buf = Vec::with_capacity(64);

    // Magic.
    buf.push(FIELD_U32);
    buf.extend_from_slice(&DB_MAGIC.to_be_bytes());
    // Transaction ID (as a u32 field: type tag + value).
    buf.push(FIELD_U32);
    buf.extend_from_slice(&tx_id.to_be_bytes());
    // Message type (as a u16 field).
    buf.push(FIELD_U16);
    buf.extend_from_slice(&msg_type.to_be_bytes());
    // Argument count (as a u8 field).
    buf.push(FIELD_U8);
    buf.push(arg_count);
    // Type tags (as a blob: 4-byte length + 12 bytes).
    buf.push(FIELD_BLOB);
    buf.extend_from_slice(&12u32.to_be_bytes());
    buf.extend_from_slice(&tags);

    // Arguments.
    for (i, arg) in args.iter().enumerate() {
        let tag = type_tags.get(i).copied().unwrap_or(FIELD_U32);
        match arg {
            Field::Number(v) => match tag {
                FIELD_U8 => {
                    buf.push(FIELD_U8);
                    buf.push(*v as u8);
                }
                FIELD_U16 => {
                    buf.push(FIELD_U16);
                    buf.extend_from_slice(&(*v as u16).to_be_bytes());
                }
                _ => {
                    buf.push(FIELD_U32);
                    buf.extend_from_slice(&v.to_be_bytes());
                }
            },
            Field::Blob(data) => {
                buf.push(FIELD_BLOB);
                buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
                buf.extend_from_slice(data);
            }
            Field::Text(s) => {
                let encoded = encode_utf16be(s);
                buf.push(FIELD_STRING);
                // The length counts UTF-16 code units, not bytes.
                buf.extend_from_slice(&((encoded.len() / 2) as u32).to_be_bytes());
                buf.extend_from_slice(&encoded);
            }
        }
    }
    buf
}

fn read_exact<R: Read + ?Sized>(r: &mut R, buf: &mut [u8]) -> Result<(), String> {
    r.read_exact(buf)
        .map_err(|e| format!("dbserver read failed: {e}"))
}

/// Reads `len` bytes after a length check. A bad peer can send any length.
pub(crate) fn read_sized<R: Read + ?Sized>(r: &mut R, len: usize) -> Result<Vec<u8>, String> {
    if len > MAX_FIELD_LEN {
        return Err(format!(
            "dbserver field length {len} is larger than {MAX_FIELD_LEN}"
        ));
    }
    // On the ESP32, a large length can be more than the free heap. A failed reservation gives
    // an error, not an abort.
    let mut data = Vec::new();
    data.try_reserve_exact(len)
        .map_err(|_| format!("dbserver field length {len} does not fit in memory"))?;
    data.resize(len, 0);
    if len > 0 {
        read_exact(r, &mut data)?;
    }
    Ok(data)
}

/// Reads and discards `n` bytes.
///
/// `std::io::copy` puts an 8 KB buffer on the stack. The fetch thread of the ESP32 has a smaller
/// stack. Thus this function uses a buffer of 64 bytes.
pub(crate) fn skip<R: Read + ?Sized>(r: &mut R, mut n: usize) -> Result<(), String> {
    let mut buf = [0u8; 64];
    while n > 0 {
        let k = n.min(buf.len());
        read_exact(r, &mut buf[..k])?;
        n -= k;
    }
    Ok(())
}

/// Reads a single field from a stream.
pub fn read_field<R: Read>(r: &mut R) -> Result<Field, String> {
    let mut tag = [0u8; 1];
    read_exact(r, &mut tag)?;
    read_value(r, tag[0])
}

/// Reads the value of a field after its type tag.
fn read_value<R: Read>(r: &mut R, tag: u8) -> Result<Field, String> {
    match tag {
        FIELD_U8 => {
            let mut b = [0u8; 1];
            read_exact(r, &mut b)?;
            Ok(Field::Number(u32::from(b[0])))
        }
        FIELD_U16 => {
            let mut b = [0u8; 2];
            read_exact(r, &mut b)?;
            Ok(Field::Number(u32::from(u16::from_be_bytes(b))))
        }
        FIELD_U32 => {
            let mut b = [0u8; 4];
            read_exact(r, &mut b)?;
            Ok(Field::Number(u32::from_be_bytes(b)))
        }
        FIELD_BLOB => {
            let mut len_buf = [0u8; 4];
            read_exact(r, &mut len_buf)?;
            let len = u32::from_be_bytes(len_buf) as usize;
            Ok(Field::Blob(read_sized(r, len)?))
        }
        FIELD_STRING => {
            let mut len_buf = [0u8; 4];
            read_exact(r, &mut len_buf)?;
            // The length counts UTF-16 code units, including the null terminator. It does not count bytes.
            let len = (u32::from_be_bytes(len_buf) as usize).saturating_mul(2);
            Ok(Field::Text(decode_utf16be(&read_sized(r, len)?)))
        }
        other => Err(format!("Unknown field type tag 0x{other:02x}")),
    }
}

/// Reads a dbserver message from a stream.
pub fn read_message<R: Read>(r: &mut R) -> Result<DbMessage, String> {
    type NoStream = fn(usize, &mut dyn Read) -> Result<(), String>;
    read_message_inner(r, None::<NoStream>).map(|(msg, _)| msg)
}

/// Reads a dbserver message like [`read_message`], but streams the first blob argument that is
/// longer than 20 bytes.
///
/// `blob` gets the length of the blob and a reader for its bytes. It can stop before the end.
/// The function then discards the rest, so that the stream stays at the next argument. The
/// message holds an empty blob in place of the streamed blob. The result of `blob` comes back
/// with the message, also when it is an error. Thus the caller can examine the message type
/// first.
pub fn read_message_with<R: Read, T>(
    r: &mut R,
    blob: impl FnOnce(usize, &mut dyn Read) -> Result<T, String>,
) -> Result<(DbMessage, Option<Result<T, String>>), String> {
    read_message_inner(r, Some(blob))
}

fn read_message_inner<R: Read, T>(
    r: &mut R,
    mut blob: Option<impl FnOnce(usize, &mut dyn Read) -> Result<T, String>>,
) -> Result<(DbMessage, Option<Result<T, String>>), String> {
    let mut streamed = None;
    // The magic is a u32 field.
    let magic = read_field(r)?;
    if magic != Field::Number(DB_MAGIC) {
        return Err(format!("Bad dbserver magic: {magic:?}"));
    }
    let _tx_id = read_field(r)?;
    let msg_type = match read_field(r)? {
        Field::Number(v) => v as u16,
        _ => return Err("Expected numeric message type".into()),
    };
    let arg_count = match read_field(r)? {
        Field::Number(v) => v as usize,
        _ => return Err("Expected numeric argument count".into()),
    };
    // A bad peer can send any count. Refuse it before the allocation below.
    if arg_count > MAX_ARG_COUNT {
        return Err(format!(
            "dbserver argument count {arg_count} is larger than {MAX_ARG_COUNT}"
        ));
    }
    let _tags = read_field(r)?;
    let mut args = Vec::with_capacity(arg_count);
    for _ in 0..arg_count {
        let mut tag = [0u8; 1];
        read_exact(r, &mut tag)?;
        if tag[0] != FIELD_BLOB {
            args.push(read_value(r, tag[0])?);
            continue;
        }
        let mut len_buf = [0u8; 4];
        read_exact(r, &mut len_buf)?;
        let len = u32::from_be_bytes(len_buf) as usize;
        match blob.take_if(|_| len > 20) {
            Some(f) => {
                if len > MAX_STREAM_LEN {
                    return Err(format!(
                        "dbserver blob length {len} is larger than {MAX_STREAM_LEN}"
                    ));
                }
                // The small buffer gives fewer socket reads than the 6-byte and 8-byte entries.
                let mut part = BufReader::with_capacity(256, (&mut *r).take(len as u64));
                streamed = Some(f(len, &mut part));
                let rest = part.buffer().len() + part.get_ref().limit() as usize;
                skip(&mut part, rest)?;
                args.push(Field::Blob(Vec::new()));
            }
            None => args.push(Field::Blob(read_sized(r, len)?)),
        }
    }
    Ok((DbMessage { msg_type, args }, streamed))
}

// The chunk size is constant on purpose. `as_chunks` is not in older toolchains (the esp one).
#[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
/// Decodes UTF-16 big-endian bytes to a string and strips the null terminators.
pub fn decode_utf16be(data: &[u8]) -> String {
    let u16s: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&u16s)
        .trim_end_matches('\0')
        .to_string()
}

/// Encodes a string as UTF-16BE with a null terminator.
pub fn encode_utf16be(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for c in s.encode_utf16() {
        out.extend_from_slice(&c.to_be_bytes());
    }
    out.extend_from_slice(&[0x00, 0x00]); // null terminator
    out
}

/// Summarises dbserver arguments for an error message: a number in hex, the length of a blob.
pub fn describe_args(args: &[Field]) -> String {
    args.iter()
        .map(|a| match a {
            Field::Number(n) => format!("0x{n:x}"),
            Field::Blob(b) => format!("blob {} bytes", b.len()),
            Field::Text(t) => format!("text {t:?}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// An XDJ-AZ accepts these setup bytes from asking player 1.
    #[test]
    fn setup_message_matches_the_wire_format() {
        let got = encode_message(
            0xffff_fffe,
            MSG_SETUP_REQ,
            &[Field::Number(1)],
            &[FIELD_U32],
        );
        let mut want = vec![0x11, 0x87, 0x23, 0x49, 0xae, 0x11, 0xff, 0xff, 0xff, 0xfe];
        want.extend([
            0x10, 0x00, 0x00, 0x0f, 0x01, 0x14, 0x00, 0x00, 0x00, 0x0c, 0x06,
        ]);
        want.extend([0; 11]);
        want.extend([0x11, 0x00, 0x00, 0x00, 0x01]);
        assert_eq!(got.len(), 37);
        assert_eq!(got, want);
    }

    #[test]
    fn read_field_counts_string_length_in_utf16_units() {
        // "5A" and the null terminator are 3 code units, 6 bytes.
        let mut msg = vec![FIELD_STRING, 0, 0, 0, 3, 0, b'5', 0, b'A', 0, 0];
        msg.extend([FIELD_U32, 0, 0, 0, 7]);
        let mut r = Cursor::new(msg);
        assert_eq!(read_field(&mut r).unwrap(), Field::Text("5A".into()));
        assert_eq!(read_field(&mut r).unwrap(), Field::Number(7));
    }

    #[test]
    fn read_message_refuses_a_huge_argument_count() {
        let mut msg = vec![FIELD_U32];
        msg.extend(DB_MAGIC.to_be_bytes());
        msg.extend([FIELD_U32, 0, 0, 0, 1]); // transaction ID
        msg.extend([FIELD_U16, 0x40, 0x00]); // message type
        msg.extend([FIELD_U32, 0xFF, 0xFF, 0xFF, 0xFF]); // argument count
        msg.extend([FIELD_BLOB, 0, 0, 0, 0]); // type tags
        let err = read_message(&mut Cursor::new(msg)).unwrap_err();
        assert!(err.contains("argument count"), "{err}");
    }

    #[test]
    fn read_field_refuses_a_huge_blob() {
        let msg = vec![FIELD_BLOB, 0xFF, 0xFF, 0xFF, 0xFF];
        let err = read_field(&mut Cursor::new(msg)).unwrap_err();
        assert!(err.contains("larger than"), "{err}");
    }

    #[test]
    fn a_message_survives_a_round_trip() {
        let args = [
            Field::Number(0x0108_0301),
            Field::Text("Träck".into()),
            Field::Blob(vec![1, 2, 3]),
        ];
        let bytes = encode_message(5, MSG_MENU_ITEM, &args, &[]);
        let msg = read_message(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(msg.msg_type, MSG_MENU_ITEM);
        assert_eq!(msg.args, args);
    }

    #[test]
    fn a_streamed_blob_keeps_the_stream_at_the_next_message() {
        let blob: Vec<u8> = (0..100).collect();
        let args = [Field::Blob(blob), Field::Number(7)];
        let mut bytes = encode_message(5, MSG_ANLZ_TAG_RESP, &args, &[]);
        bytes.extend(encode_message(6, MSG_MENU_FOOTER, &[Field::Number(1)], &[]));
        let mut r = Cursor::new(bytes);
        let (msg, first) = read_message_with(&mut r, |len, r| {
            let mut b = [0u8; 2];
            r.read_exact(&mut b).map_err(|e| e.to_string())?;
            Ok((len, b))
        })
        .unwrap();
        assert_eq!(first, Some(Ok((100, [0, 1]))));
        assert_eq!(msg.args, vec![Field::Blob(Vec::new()), Field::Number(7)]);
        assert_eq!(read_message(&mut r).unwrap().msg_type, MSG_MENU_FOOTER);
    }

    #[test]
    fn a_short_stream_is_an_error() {
        assert!(read_field(&mut Cursor::new(vec![FIELD_U32, 0, 0])).is_err());
        assert!(read_field(&mut Cursor::new(vec![0x99])).is_err());
    }

    #[test]
    fn number_fields_have_the_requested_size() {
        assert_eq!(encode_number_field(1, 4).unwrap(), [0x11, 0, 0, 0, 1]);
        assert_eq!(encode_number_field(7, 2).unwrap(), [0x10, 0, 7]);
        assert_eq!(encode_number_field(7, 1).unwrap(), [0x0f, 7]);
        assert!(encode_number_field(7, 3).is_err());
    }

    #[test]
    fn tag_codes_are_the_reversed_ascii_bytes() {
        assert_eq!(TAG_PSSI, 0x4953_5350);
        assert_eq!(TAG_PWV4, 0x3456_5750);
        assert_eq!(EXT_EXT, 0x0054_5845);
    }
}
