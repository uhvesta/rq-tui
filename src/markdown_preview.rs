use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use uuid::Uuid;

const INDEX: &str = include_str!("../vendor/markdown-preview.nvim/index.html");
const PREVIEW_JS: &str = include_str!("../vendor/markdown-preview.nvim/rev-preview.js");
const PREVIEW_CSS: &str = include_str!("../vendor/markdown-preview.nvim/rev-preview.css");
const PAGE_CSS: &str = include_str!("../vendor/markdown-preview.nvim/static/page.css");
const MARKDOWN_CSS: &str = include_str!("../vendor/markdown-preview.nvim/static/markdown.css");
const HIGHLIGHT_CSS: &str = include_str!("../vendor/markdown-preview.nvim/static/highlight.css");
const HIGHLIGHT_JS: &[u8] =
    include_bytes!("../vendor/markdown-preview.nvim/static/highlight.min.js");
const MARKDOWN_IT: &[u8] =
    include_bytes!("../vendor/markdown-preview.nvim/markdown-it/markdown-it.min.js");
const MERMAID: &[u8] = include_bytes!("../vendor/markdown-preview.nvim/static/mermaid.min.js");

pub(crate) struct MarkdownPreviewServer {
    address: SocketAddr,
    token: String,
    document: Arc<RwLock<String>>,
    focus: Arc<RwLock<String>>,
    last_seen: Arc<Mutex<Option<Instant>>>,
    started: Instant,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl MarkdownPreviewServer {
    pub(crate) fn start(initial_document: String, initial_focus: String) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .context("cannot bind the local Markdown preview server")?;
        listener
            .set_nonblocking(true)
            .context("cannot configure the local Markdown preview server")?;
        let address = listener.local_addr()?;
        let token = Uuid::new_v4().simple().to_string();
        let document = Arc::new(RwLock::new(initial_document));
        let focus = Arc::new(RwLock::new(initial_focus));
        let last_seen = Arc::new(Mutex::new(None));
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_document = Arc::clone(&document);
        let worker_focus = Arc::clone(&focus);
        let worker_last_seen = Arc::clone(&last_seen);
        let worker_shutdown = Arc::clone(&shutdown);
        let worker_token = token.clone();
        let worker = thread::spawn(move || {
            while !worker_shutdown.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        serve(
                            stream,
                            &worker_token,
                            &worker_document,
                            &worker_focus,
                            &worker_last_seen,
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(15));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            address,
            token,
            document,
            focus,
            last_seen,
            started: Instant::now(),
            shutdown,
            worker: Some(worker),
        })
    }

    pub(crate) fn url(&self) -> String {
        format!("http://{}/review/{}", self.address, self.token)
    }

    pub(crate) fn update_document(&self, document: String) {
        if let Ok(mut current) = self.document.write() {
            *current = document;
        }
    }

    pub(crate) fn update_focus(&self, focus: String) {
        if let Ok(mut current) = self.focus.write() {
            *current = focus;
        }
    }

    pub(crate) fn client_age(&self) -> Option<Duration> {
        self.last_seen
            .lock()
            .ok()
            .and_then(|seen| seen.as_ref().map(Instant::elapsed))
    }

    pub(crate) fn waiting_for(&self) -> Duration {
        self.started.elapsed()
    }
}

impl Drop for MarkdownPreviewServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        TcpStream::connect(self.address).ok();
        if let Some(worker) = self.worker.take() {
            worker.join().ok();
        }
    }
}

fn serve(
    mut stream: TcpStream,
    token: &str,
    document: &RwLock<String>,
    focus: &RwLock<String>,
    last_seen: &Mutex<Option<Instant>>,
) {
    stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .ok();
    let mut request = [0u8; 8_192];
    let Ok(read) = stream.read(&mut request) else {
        return;
    };
    let request = String::from_utf8_lossy(&request[..read]);
    let Some(target) = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
    else {
        return;
    };
    let review_path = format!("/review/{token}");
    let (status, content_type, cache, body): (&str, &str, &str, Vec<u8>) =
        if target == review_path || target == format!("{review_path}/") {
            let index = INDEX.replacen(
                "<body>",
                &format!("<body data-preview-token=\"{token}\">"),
                1,
            );
            (
                "200 OK",
                "text/html; charset=utf-8",
                "no-store",
                index.into_bytes(),
            )
        } else if target == format!("/document.json?token={token}") {
            let body = document
                .read()
                .map(|document| document.as_bytes().to_vec())
                .unwrap_or_else(|_| b"{}".to_vec());
            ("200 OK", "application/json", "no-store", body)
        } else if target == format!("/focus.json?token={token}") {
            let body = focus
                .read()
                .map(|focus| focus.as_bytes().to_vec())
                .unwrap_or_else(|_| b"{}".to_vec());
            ("200 OK", "application/json", "no-store", body)
        } else if target == format!("/ready.json?token={token}") {
            mark_seen(last_seen);
            (
                "200 OK",
                "application/json",
                "no-store",
                b"{\"ready\":true}".to_vec(),
            )
        } else if let Some(asset) = asset(target) {
            (
                "200 OK",
                asset.0,
                "public, max-age=31536000, immutable",
                asset.1.to_vec(),
            )
        } else {
            (
                "404 Not Found",
                "text/plain; charset=utf-8",
                "no-store",
                b"not found".to_vec(),
            )
        };
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         Cache-Control: {cache}\r\nX-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes()).ok();
    stream.write_all(&body).ok();
}

fn mark_seen(last_seen: &Mutex<Option<Instant>>) {
    if let Ok(mut seen) = last_seen.lock() {
        *seen = Some(Instant::now());
    }
}

fn asset(path: &str) -> Option<(&'static str, &'static [u8])> {
    match path {
        "/rev-preview.js" => Some(("text/javascript; charset=utf-8", PREVIEW_JS.as_bytes())),
        "/rev-preview.css" => Some(("text/css; charset=utf-8", PREVIEW_CSS.as_bytes())),
        "/vendor/page.css" => Some(("text/css; charset=utf-8", PAGE_CSS.as_bytes())),
        "/vendor/markdown.css" => Some(("text/css; charset=utf-8", MARKDOWN_CSS.as_bytes())),
        "/vendor/highlight.css" => Some(("text/css; charset=utf-8", HIGHLIGHT_CSS.as_bytes())),
        "/vendor/markdown-it.min.js" => Some(("text/javascript; charset=utf-8", MARKDOWN_IT)),
        "/vendor/highlight.min.js" => Some(("text/javascript; charset=utf-8", HIGHLIGHT_JS)),
        "/vendor/mermaid.min.js" => Some(("text/javascript; charset=utf-8", MERMAID)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    use super::MarkdownPreviewServer;

    fn get(url: &str, path: &str) -> String {
        let address = url
            .strip_prefix("http://")
            .unwrap()
            .split('/')
            .next()
            .unwrap();
        let Ok(mut stream) = TcpStream::connect(address) else {
            return String::new();
        };
        if write!(stream, "GET {path} HTTP/1.1\r\nHost: {address}\r\n\r\n").is_err() {
            return String::new();
        }
        let mut response = String::new();
        stream.read_to_string(&mut response).ok();
        response
    }

    fn get_matching(url: &str, path: &str, expected: &str) -> String {
        for _ in 0..20 {
            let response = get(url, path);
            if response.starts_with(expected) {
                return response;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        get(url, path)
    }

    #[test]
    fn serves_a_token_protected_live_state_and_vendored_renderer() {
        let server = MarkdownPreviewServer::start(
            r#"{"revision":1,"current":"document"}"#.into(),
            r#"{"revision":1,"focus_line":1}"#.into(),
        )
        .unwrap();
        let url = server.url();
        let authority = address(&url);
        let review_path = &url["http://".len() + authority.len()..];
        let index = get_matching(&server.url(), review_path, "HTTP/1.1 200 OK");
        assert!(index.starts_with("HTTP/1.1 200 OK"), "{index:?}");
        assert!(index.contains("data-preview-token="));
        assert!(
            get_matching(&server.url(), "/rev-preview.js", "HTTP/1.1 200 OK")
                .contains("window.markdownit")
        );
        assert!(
            get_matching(&server.url(), "/focus.json?token=wrong", "HTTP/1.1 404")
                .starts_with("HTTP/1.1 404")
        );

        server.update_document(r#"{"revision":2}"#.into());
        server.update_focus(r#"{"revision":2,"focus_line":8}"#.into());
        let updated_url = server.url();
        let token = updated_url.rsplit('/').next().unwrap();
        assert!(get_matching(
            &updated_url,
            &format!("/document.json?token={token}"),
            "HTTP/1.1 200 OK"
        )
        .contains(r#"{"revision":2}"#));
        assert!(get_matching(
            &updated_url,
            &format!("/focus.json?token={token}"),
            "HTTP/1.1 200 OK"
        )
        .contains(r#""focus_line":8"#));
        assert!(server.client_age().is_none());
        assert!(get_matching(
            &updated_url,
            &format!("/ready.json?token={token}"),
            "HTTP/1.1 200 OK"
        )
        .contains(r#""ready":true"#));
        assert!(server.client_age().is_some());
    }

    fn address(url: &str) -> &str {
        url.strip_prefix("http://")
            .unwrap()
            .split('/')
            .next()
            .unwrap()
    }
}
