//! The one-time loopback listener of an OAuth sign-in (RFC 8252, section 7.3).
//!
//! The window asks the server to start a sign-in with a redirect address on this computer. The
//! browser goes there after the sign-in. The listener takes that one request, answers it, and
//! closes. The address with the code then goes to the server, which exchanges it for tokens.

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// How long the listener waits for the browser. The server keeps a sign-in as long.
const WAIT: Duration = Duration::from_secs(10 * 60);
/// The path of the redirect address.
const PATH: &str = "/callback";
/// How long one connection may take to send its request head.
const READ_TIME: Duration = Duration::from_secs(5);
/// The longest request head that the listener reads.
const MAX_HEAD: usize = 16 * 1024;

const DONE_PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>Bunsin</title>\
    <p>Bunsin received the sign-in. You can close this page and go back to the Bunsin window.</p>";

/// Opens a listener on a free port of 127.0.0.1. Returns it with its redirect address.
pub async fn listen() -> Result<(TcpListener, String), String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("the window cannot listen for the sign-in: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("the window cannot listen for the sign-in: {e}"))?
        .port();
    Ok((listener, format!("http://127.0.0.1:{port}{PATH}")))
}

/// Waits for the browser's request to the redirect address and returns its whole address.
/// A request to another path gets 404 and the listener waits on. The listener closes after the
/// first request to the redirect path, or after 10 minutes.
pub async fn wait(listener: TcpListener, redirect_uri: &str) -> Result<String, String> {
    let waiting = async {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            // A connection that sends nothing must not hold up the browser's request.
            let Ok(Some(target)) = tokio::time::timeout(READ_TIME, request_target(&mut socket)).await else {
                continue;
            };
            let path = target.split('?').next().unwrap_or_default();
            if path != PATH {
                let _ = socket
                    .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
                continue;
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n",
                DONE_PAGE.len()
            );
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(DONE_PAGE.as_bytes()).await;
            let query = target.strip_prefix(PATH).unwrap_or_default();
            return format!("{redirect_uri}{query}");
        }
    };
    tokio::time::timeout(WAIT, waiting)
        .await
        .map_err(|_| "nobody signed in within 10 minutes. Sign in again".to_owned())
}

/// The target of a `GET` request (`/callback?code=...`), or `None` for anything else.
async fn request_target(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut buffer = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let read = socket.read(&mut buffer).await.ok()?;
        if read == 0 || head.len() + read > MAX_HEAD {
            return None;
        }
        head.extend_from_slice(&buffer[..read]);
    }
    let head = String::from_utf8(head).ok()?;
    let mut words = head.lines().next()?.split(' ');
    match (words.next(), words.next()) {
        (Some("GET"), Some(target)) if target.starts_with('/') => Some(target.to_owned()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(redirect_uri: &str, path: &str) -> String {
        let address = redirect_uri
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap()
            .to_owned();
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        let request = format!("GET {path} HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n");
        socket.write_all(request.as_bytes()).await.unwrap();
        let mut answer = String::new();
        socket.read_to_string(&mut answer).await.unwrap();
        answer
    }

    #[tokio::test]
    async fn the_listener_takes_the_callback_once_and_ignores_other_paths() {
        let (listener, redirect_uri) = listen().await.unwrap();
        assert!(redirect_uri.starts_with("http://127.0.0.1:") && redirect_uri.ends_with("/callback"));
        let waiting = tokio::spawn({
            let redirect_uri = redirect_uri.clone();
            async move { wait(listener, &redirect_uri).await }
        });
        assert!(get(&redirect_uri, "/favicon.ico").await.starts_with("HTTP/1.1 404"));
        let answer = get(&redirect_uri, "/callback?code=c1&state=s1").await;
        assert!(
            answer.starts_with("HTTP/1.1 200") && answer.contains("close this page"),
            "{answer}"
        );
        assert_eq!(
            waiting.await.unwrap().unwrap(),
            format!("{redirect_uri}?code=c1&state=s1")
        );
        // The listener is closed after the callback.
        let address = redirect_uri
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap()
            .to_owned();
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }
}
