//! Streams pushed by a server: WebSockets (`ws://`, `wss://`) and HTTP responses that
//! keep coming (`http://`, `https://`): server-sent events (each event's `data:` is one
//! record) or plain lines (NDJSON, CSV…). Messages to send after connecting (a
//! WebSocket subscription, or an HTTP request body) come from `--send`. A dropped
//! connection is retried, waiting longer each time (up to 30 s).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use crate::data::LineMeta;
use crate::source::Event;
use crate::time::now_secs;

/// Connect, read until the connection ends, and connect again, until the viewer stops
/// listening.
pub fn run(url: &str, send: &[String], tx: &Sender<Event>) -> Result<(), String> {
    let mut wait = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        let result = if is_websocket(url) {
            websocket(url, send, tx)
        } else {
            http(url, send, tx)
        };
        match result {
            Ok(Flow::Stopped) => return Ok(()),
            Ok(Flow::Ended) => {}
            Err(e) => {
                if tx
                    .send(Event::Error(format!(
                        "{}: {e} (trying again in {}s)",
                        short(url),
                        wait.as_secs()
                    )))
                    .is_err()
                {
                    return Ok(());
                }
            }
        }
        // A connection that lasted a while starts the waiting over.
        if started.elapsed() > Duration::from_secs(60) {
            wait = Duration::from_secs(1);
        }
        let until = Instant::now() + wait;
        while Instant::now() < until {
            thread::sleep(Duration::from_millis(200));
            if tx.send(Event::SampleDone).is_err() {
                return Ok(());
            }
        }
        wait = (wait * 2).min(Duration::from_secs(30));
    }
}

/// The longest line (or WebSocket message) read, and the most an event may gather:
/// a server can't make graft hold more than this in memory.
pub const MAX_LINE: usize = 1 << 20;
pub const MAX_EVENT: usize = 4 << 20;
const MAX_MESSAGE: usize = 8 << 20;

pub fn is_websocket(url: &str) -> bool {
    let u = url.trim_start().to_ascii_lowercase();
    u.starts_with("ws://") || u.starts_with("wss://")
}

/// `feed.example.com/v1/stream` (no scheme or query): for titles.
pub fn short(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['?', '#']).next().unwrap_or(rest).to_string()
}

enum Flow {
    /// The connection closed (connect again).
    Ended,
    /// Nobody is listening any more.
    Stopped,
}

/// Send a record's lines; `false` when the viewer has gone.
fn emit(text: &str, tx: &Sender<Event>) -> bool {
    let meta = LineMeta {
        arrival: now_secs(),
        sample: None,
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .all(|l| tx.send(Event::Line(l.to_string(), meta)).is_ok())
}

/// Reads a stream's body into records: server-sent events (only the `data:` fields,
/// one record per event) or, for anything else, its lines.
pub struct SseReader {
    sse: bool,
    data: Vec<String>,
    /// Bytes gathered for the event so far; past `MAX_EVENT` it's skipped.
    size: usize,
}

impl SseReader {
    pub fn new(sse: bool) -> Self {
        Self {
            sse,
            data: Vec::new(),
            size: 0,
        }
    }

    /// One line of the body in; a finished record out (if this line ended one).
    pub fn push(&mut self, line: &str) -> Option<String> {
        if !self.sse {
            return Some(line.to_string());
        }
        if line.is_empty() {
            let too_big = std::mem::take(&mut self.size) > MAX_EVENT;
            let data = std::mem::take(&mut self.data);
            return (!data.is_empty() && !too_big).then(|| data.join("\n"));
        }
        // `field: value` (one space after the colon is dropped); `:` starts a comment.
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field == "data" {
            let value = value.strip_prefix(' ').unwrap_or(value);
            self.size += value.len() + 1;
            // An event too big to keep: dropped (what's gathered goes) when it ends.
            if self.size > MAX_EVENT {
                self.data.clear();
            } else {
                self.data.push(value.to_string());
            }
        }
        None
    }
}

fn http(url: &str, send: &[String], tx: &Sender<Event>) -> Result<Flow, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let response = if send.is_empty() {
        agent
            .get(url)
            .header("Accept", "text/event-stream, */*")
            .call()
    } else {
        agent
            .post(url)
            .header("Accept", "text/event-stream, */*")
            .header("Content-Type", "application/json")
            .send(send.join("\n"))
    }
    .map_err(|e| e.to_string())?;
    let status = response.status();
    let sse = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|t| t.contains("event-stream"));
    let mut body = BufReader::new(response.into_body().into_reader());
    if !status.is_success() {
        let mut text = String::new();
        let _ = body.take(300).read_to_string(&mut text);
        return Err(format!(
            "{status}: {}",
            text.lines().next().unwrap_or("").trim()
        ));
    }
    let mut reader = SseReader::new(sse);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // At most MAX_LINE bytes, whether or not a newline came.
        match (&mut body)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut buf)
        {
            Ok(0) => return Ok(Flow::Ended),
            Ok(_) if buf.len() > MAX_LINE && !buf.ends_with(b"\n") => {
                return Err(format!("a line longer than {} MB", MAX_LINE >> 20));
            }
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim_end_matches(['\n', '\r']);
                match reader.push(line) {
                    Some(record) if !emit(&record, tx) => return Ok(Flow::Stopped),
                    Some(_) => {}
                    // Comments and keep-alives: check someone's still listening.
                    None if line.starts_with(':') && tx.send(Event::SampleDone).is_err() => {
                        return Ok(Flow::Stopped);
                    }
                    None => {}
                }
            }
            Err(e) => return Err(format!("read error: {e}")),
        }
    }
}

/// A plain or TLS connection, for the WebSocket.
enum Conn {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Conn {
    fn tcp(&self) -> &TcpStream {
        match self {
            Conn::Plain(t) => t,
            Conn::Tls(s) => s.get_ref(),
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(t) => t.read(buf),
            Conn::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(t) => t.write(buf),
            Conn::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Plain(t) => t.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring supports the default TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

fn websocket(url: &str, send: &[String], tx: &Sender<Event>) -> Result<Flow, String> {
    use tungstenite::Message;
    use tungstenite::client::IntoClientRequest;
    let request = url
        .trim()
        .into_client_request()
        .map_err(|e| format!("bad URL: {e}"))?;
    let uri = request.uri().clone();
    let tls = uri.scheme_str() == Some("wss");
    let host = uri.host().ok_or("the URL has no host")?.to_string();
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });
    let tcp = TcpStream::connect((host.as_str(), port)).map_err(|e| format!("{host}: {e}"))?;
    tcp.set_read_timeout(Some(Duration::from_secs(15)))
        .map_err(|e| e.to_string())?;
    let conn = if tls {
        let name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|e| format!("{host}: {e}"))?;
        let client =
            rustls::ClientConnection::new(tls_config(), name).map_err(|e| e.to_string())?;
        Conn::Tls(Box::new(rustls::StreamOwned::new(client, tcp)))
    } else {
        Conn::Plain(tcp)
    };
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE))
        .max_frame_size(Some(MAX_MESSAGE));
    let (mut socket, _) = tungstenite::client::client_with_config(request, conn, Some(config))
        .map_err(|e| match e {
            tungstenite::HandshakeError::Failure(e) => e.to_string(),
            tungstenite::HandshakeError::Interrupted(_) => "the handshake timed out".into(),
        })?;
    for m in send {
        socket
            .send(Message::text(m.clone()))
            .map_err(|e| format!("sending {m}: {e}"))?;
    }
    // Wake up now and then, to notice the viewer has gone even when it's quiet.
    socket
        .get_ref()
        .tcp()
        .set_read_timeout(Some(Duration::from_secs(1)))
        .map_err(|e| e.to_string())?;
    let mut quiet_since = Instant::now();
    loop {
        match socket.read() {
            Ok(Message::Text(t)) => {
                quiet_since = Instant::now();
                if !emit(t.as_str(), tx) {
                    return Ok(Flow::Stopped);
                }
            }
            Ok(Message::Binary(b)) => {
                quiet_since = Instant::now();
                if !emit(&String::from_utf8_lossy(&b), tx) {
                    return Ok(Flow::Stopped);
                }
            }
            Ok(Message::Close(_)) => return Ok(Flow::Ended),
            Ok(_) => {} // pings are answered by tungstenite
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if tx.send(Event::SampleDone).is_err() {
                    return Ok(Flow::Stopped);
                }
                // Nothing at all for a long time: the connection is probably dead.
                if quiet_since.elapsed() > Duration::from_secs(120) {
                    return Err("nothing received for 2 minutes".into());
                }
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(Flow::Ended);
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    #[test]
    fn server_sent_events_are_one_record_per_event() {
        let mut r = SseReader::new(true);
        let lines = [
            "event: price_update",
            "data: {\"a\":1}",
            "",
            ": keep-alive",
            "",
            "data:{\"a\":",
            "data: 2}",
            "id: 7",
            "",
        ];
        let out: Vec<String> = lines.iter().filter_map(|l| r.push(l)).collect();
        assert_eq!(out, ["{\"a\":1}", "{\"a\":\n2}"]);
        // An event too big is dropped; the next one is read as usual.
        let mut r = SseReader::new(true);
        let big = format!("data: {}", "x".repeat(MAX_LINE));
        for _ in 0..5 {
            assert_eq!(r.push(&big), None);
        }
        assert_eq!(r.push(""), None);
        assert_eq!(r.push("data: 1"), None);
        assert_eq!(r.push("").as_deref(), Some("1"));
        // Not event-stream: every line is a record.
        let mut plain = SseReader::new(false);
        assert_eq!(plain.push("x=1").as_deref(), Some("x=1"));
    }

    #[test]
    fn names_and_kinds() {
        assert!(is_websocket("wss://api.hyperliquid.xyz/ws"));
        assert!(!is_websocket("https://feed.example.com/v1/stream?ids=1"));
        assert_eq!(
            short("https://feed.example.com/v1/stream?ids=1"),
            "feed.example.com/v1/stream"
        );
    }

    /// A tiny HTTP server that answers once with `body`.
    fn serve_once(head: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\n{head}\r\nConnection: close\r\n\r\n{body}"
                );
            }
        });
        format!("http://{addr}/stream")
    }

    #[test]
    fn a_line_without_end_is_cut_off() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let _ = write!(s, "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n");
                let chunk = vec![b'x'; 64 * 1024];
                // Far more than MAX_LINE, never a newline.
                for _ in 0..40 {
                    if s.write_all(&chunk).is_err() {
                        break;
                    }
                }
            }
        });
        let (tx, _rx) = mpsc::channel();
        let result = http(&format!("http://{addr}/"), &[], &tx);
        assert!(
            matches!(&result, Err(e) if e.contains("longer than")),
            "{:?}",
            result.err()
        );
    }

    #[test]
    fn reads_an_event_stream_over_http() {
        let url = serve_once(
            "Content-Type: text/event-stream",
            "event: tick\ndata: {\"p\":1}\n\ndata: {\"p\":2}\n\n",
        );
        let (tx, rx) = mpsc::channel();
        assert!(matches!(http(&url, &[], &tx), Ok(Flow::Ended)));
        drop(tx);
        let lines: Vec<String> = rx
            .iter()
            .filter_map(|e| match e {
                Event::Line(l, _) => Some(l),
                _ => None,
            })
            .collect();
        assert_eq!(lines, ["{\"p\":1}", "{\"p\":2}"]);
    }

    #[test]
    fn reads_a_websocket_and_sends_the_subscription() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(s).unwrap();
            let got = ws.read().unwrap().into_text().unwrap().to_string();
            ws.send(tungstenite::Message::text(format!("{{\"echo\":{got}}}")))
                .unwrap();
            ws.send(tungstenite::Message::text("x=1\nx=2")).unwrap();
            let _ = ws.close(None);
            let _ = ws.flush();
            // Let the client read the close.
            let _ = ws.read();
        });
        let (tx, rx) = mpsc::channel();
        let flow = websocket(&format!("ws://{addr}/"), &["{\"sub\":1}".into()], &tx);
        assert!(matches!(flow, Ok(Flow::Ended)), "{:?}", flow.err());
        server.join().unwrap();
        drop(tx);
        let lines: Vec<String> = rx
            .iter()
            .filter_map(|e| match e {
                Event::Line(l, _) => Some(l),
                _ => None,
            })
            .collect();
        assert_eq!(lines, ["{\"echo\":{\"sub\":1}}", "x=1", "x=2"]);
    }
}
