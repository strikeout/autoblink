//! Gets light definitions from the Open Fixture Library (OFL) for the page.
//!
//! The phone on the WiFi network of the box often has no internet, so the box is the proxy. The
//! firmware passes the JSON of OFL through to the page. It does not parse the JSON.
//!
//! A fetch runs on the `ofl` thread with a 12 KB stack, because the TLS handshake needs a large
//! stack. The HTTP handler waits for the thread. TLS takes about 50 KB of heap. Thus a fetch does
//! not start when the heap is low, and the page gets no status while a fetch runs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use esp_idf_svc::http::Method;
use esp_idf_svc::sys::{self, EspError};

use crate::ota;

const BASE_URL: &str = "https://open-fixture-library.org";

/// The firmware cuts off a longer OFL reply, and the route answers 502.
pub const MAX_BODY: usize = 24 * 1024;
/// A fetch does not start below this free heap.
pub const MIN_FREE: u32 = 80 * 1024;
/// A fetch does not start below this largest free block. The TLS record buffer has 16 KB.
pub const MIN_BLOCK: u32 = 32 * 1024;

/// The longest wait for the connection and for each read.
const TIMEOUT: Duration = Duration::from_secs(10);
/// The search text has 1 to this many characters.
const MAX_QUERY: usize = 64;
/// The reply buffer grows in steps of this size.
const CHUNK: usize = 4096;

/// `true` while a fetch runs.
static BUSY: AtomicBool = AtomicBool::new(false);

/// True while a fetch runs. The status thread sends no status then.
pub fn busy() -> bool {
    BUSY.load(Ordering::Acquire)
}

/// Searches OFL. Returns the reply body, or an HTTP status and a short text.
pub fn search(query: &str) -> Result<Vec<u8>, (u16, String)> {
    if !(1..=MAX_QUERY).contains(&query.chars().count()) {
        return Err((
            400,
            format!("The search text must have 1 to {MAX_QUERY} characters."),
        ));
    }
    // A `str` serializes as one JSON string with escapes. The firmware builds no `Value` tree.
    let text = serde_json::to_string(query).map_err(|e| (500, e.to_string()))?;
    let body = format!(r#"{{"searchQuery":{text},"manufacturersQuery":[],"categoriesQuery":[]}}"#);
    fetch(
        Method::Post,
        format!("{BASE_URL}/api/v1/get-search-results"),
        body.into_bytes(),
    )
}

/// Gets the JSON of one fixture. `key` is "<manufacturer>/<model>".
pub fn fixture(key: &str) -> Result<Vec<u8>, (u16, String)> {
    if !valid_key(key) {
        return Err((
            400,
            "The key must be <manufacturer>/<model> with only a-z, 0-9 and -.".into(),
        ));
    }
    fetch(Method::Get, format!("{BASE_URL}/{key}.json"), Vec::new())
}

/// Returns `true` when `key` matches `^[a-z0-9-]+/[a-z0-9-]+$`.
fn valid_key(key: &str) -> bool {
    let part = |p: &str| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    key.split_once('/').is_some_and(|(m, f)| part(m) && part(f))
}

/// Runs one request on the `ofl` thread and waits for it.
fn fetch(method: Method, url: String, body: Vec<u8>) -> Result<Vec<u8>, (u16, String)> {
    // The HTTP server task runs this function, so a page cannot start an OTA check during a fetch.
    // An OTA check that runs already uses the heap for TLS.
    if ota::busy() {
        return Err((503, "A firmware update check runs. Try again.".into()));
    }
    if BUSY.swap(true, Ordering::AcqRel) {
        return Err((503, "Another library request runs. Try again.".into()));
    }
    let result = spawn_and_join(method, url, body);
    BUSY.store(false, Ordering::Release);
    result
}

fn spawn_and_join(method: Method, url: String, body: Vec<u8>) -> Result<Vec<u8>, (u16, String)> {
    // SAFETY: the heap calls only read counters.
    let (free, block) = unsafe {
        (
            sys::esp_get_free_heap_size(),
            sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_8BIT),
        )
    };
    if free < MIN_FREE || block < MIN_BLOCK as usize {
        log::warn!("OFL request refused: {free} bytes free, largest block {block} bytes");
        return Err((
            503,
            format!(
                "Not enough memory for the library request: {free} bytes free, largest block \
                 {block} bytes. Close other pages of the box, then try again."
            ),
        ));
    }
    // The TLS handshake of mbedTLS needs a large stack.
    let thread =
        crate::spawn(c"ofl", 12 * 1024, move || request(method, &url, &body)).map_err(|e| {
            (
                503,
                format!("The library request cannot start a thread: {e}"),
            )
        })?;
    thread
        .join()
        .unwrap_or_else(|_| Err((500, "The library request stopped after a panic.".into())))
}

/// Sends the request and reads the reply body. Runs on the `ofl` thread.
fn request(method: Method, url: &str, body: &[u8]) -> Result<Vec<u8>, (u16, String)> {
    let start = Instant::now();
    let len = body.len().to_string();
    let headers = [
        ("Content-Type", "application/json"),
        ("Content-Length", len.as_str()),
    ];
    // A GET request has no body and thus no body headers.
    let headers = if body.is_empty() {
        &[][..]
    } else {
        &headers[..]
    };
    let mut conn = ota::request(method, url, headers, body, TIMEOUT)
        .map_err(|e| failed(e.downcast_ref::<EspError>(), &format!("{e:#}"), start))?;
    let status = conn.status();
    if status != 200 {
        log::warn!("OFL request: {url} gave the status {status}");
        return Err((
            502,
            format!("The library answered with the status {status}."),
        ));
    }
    // The reply has no Content-Length (chunked encoding). The buffer grows in small steps, so
    // that it needs no block larger than `MAX_BODY` + 1.
    let mut out = Vec::new();
    loop {
        let len = out.len();
        // A failed allocation must not stop the firmware, so the reserve can fail.
        if len == out.capacity()
            && out
                .try_reserve_exact(CHUNK.min(MAX_BODY + 1 - len))
                .is_err()
        {
            log::warn!("OFL request: no memory for the reply of {url} after {len} bytes");
            return Err((503, "Not enough memory for the library reply.".into()));
        }
        out.resize(out.capacity().min(MAX_BODY + 1), 0);
        let n = match conn.read(&mut out[len..]) {
            Ok(n) => n,
            Err(e) => return Err(failed(Some(&e), &e.to_string(), start)),
        };
        out.truncate(len + n);
        if n == 0 {
            log::info!("OFL request: {url} gave {} bytes", out.len());
            return Ok(out);
        }
        if out.len() > MAX_BODY {
            log::warn!("OFL request: the reply of {url} is larger than {MAX_BODY} bytes");
            return Err((
                502,
                format!("The library reply is larger than {MAX_BODY} bytes."),
            ));
        }
    }
}

/// Gives the status and the text for an error of the HTTPS client.
///
/// A timeout gives 504. Each other error gives 502, for example a DNS error or no route to the
/// internet.
fn failed(e: Option<&EspError>, text: &str, start: Instant) -> (u16, String) {
    log::warn!("OFL request: {text}");
    // The HTTP client gives some errors as negative codes.
    let code = e.map_or(0, |e| e.code().abs());
    // A failed connection gives the same code for a timeout and for a refusal. Only the time
    // shows a timeout.
    let timeout = matches!(
        code,
        sys::ESP_ERR_HTTP_EAGAIN | sys::ESP_ERR_HTTP_READ_TIMEOUT | sys::ESP_ERR_HTTP_CONNECTING
    ) || start.elapsed() >= TIMEOUT;
    if timeout {
        (
            504,
            format!("The library did not answer in {} s.", TIMEOUT.as_secs()),
        )
    } else {
        (
            502,
            format!("The library request failed. The box needs a network with internet. {text}"),
        )
    }
}
