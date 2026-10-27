//! Blocking Pro DJ Link dbserver client for the ESP32 firmware.
//!
//! The crate fetches the track metadata, the phrases, the waveform preview, the cue list and
//! the beat grid from the dbserver of a CDJ or XDJ. It uses `std` only and has no dependency.
//! Each network call blocks the calling thread, because the firmware has no async runtime.
//!
//! The host app keeps its own async copy of this logic in `host/src/prolink/metadata.rs` and
//! `host/src/state/song_structure.rs`. A change to the protocol code must go to both copies.
//!
//! Protocol reference:
//!   <https://djl-analysis.deepsymmetry.org/djl-analysis/track_metadata.html>

mod client;
mod parse;
mod protocol;
mod types;

pub use client::{discover_db_port, fetch, fetch_at, fetch_with, Connect, FetchError, Fetched};
pub use parse::{
    asking_player, cue_list_args, extract_menu_count, parse_cue_list, parse_menu_item, parse_pqtz,
    parse_pssi_tag, parse_pwv4, phrase_spans, read_pqtz, read_pwv4, tag_start,
};
pub use protocol::{
    decode_utf16be, describe_args, encode_message, encode_number_field, encode_utf16be, read_field,
    read_message, read_message_with, DbMessage, Field, ARG_TAG_BLOB, ARG_TAG_NUMBER,
    ARG_TAG_STRING, DB_MAGIC, DB_SERVER_QUERY_PORT, EXT_DAT, EXT_EXT, FIELD_BLOB, FIELD_STRING,
    FIELD_U16, FIELD_U32, FIELD_U8, MAX_ARG_COUNT, MAX_FIELD_LEN, MAX_STREAM_LEN, MENU_DATA,
    MSG_ANLZ_TAG_REQ, MSG_ANLZ_TAG_RESP, MSG_CUE_LIST_EXT_REQ, MSG_CUE_LIST_EXT_RESP,
    MSG_MENU_FOOTER, MSG_MENU_HEADER, MSG_MENU_ITEM, MSG_METADATA_REQ, MSG_RENDER_MENU,
    MSG_SETUP_REQ, PREVIEW_COLUMNS, TAG_PQTZ, TAG_PSSI, TAG_PWV4,
};
pub use types::{
    current_phrase_idx, phrase_info, phrase_status, Cue, PhraseEntry, PhraseInfo, PhraseKind,
    PhraseSpan, PhraseStatus, SongStructure, TrackMetadata, TrackMood, TrackOverview,
};
