//! Browser sign-in the way the Windows apps do it: PKCE, with the site sending the browser
//! back to a listener on 127.0.0.1.

use base64::Engine;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::Duration;

/// A PKCE verifier, its S256 challenge and a `state` value.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

impl Pkce {
    pub fn new() -> Self {
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let random = |n: usize| {
            let mut b = vec![0u8; n];
            rand::RngCore::fill_bytes(&mut rand::rng(), &mut b);
            b64.encode(b)
        };
        let verifier = random(64);
        let challenge = {
            use sha2::Digest;
            b64.encode(sha2::Sha256::digest(verifier.as_bytes()))
        };
        Self { verifier, challenge, state: random(32) }
    }
}

/// This computer's name, as the apps show it in the list of linked devices.
pub fn device_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Linux".into())
}

pub fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                // Work on bytes: slicing the str could split a multi-byte character.
                match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// How the browser tab is answered once the site sent it back.
pub enum Finish<'a> {
    /// A small page saying how it went.
    Page,
    /// A redirect to the site's own result page; gets whether it worked.
    Redirect(&'a dyn Fn(bool) -> String),
}

/// Waits for the browser to come back to `/callback` with `state`. `Ok(None)` = cancelled
/// (by the user in the browser or through `cancelled`).
pub fn wait_for_callback(
    listener: &TcpListener,
    state: &str,
    cancelled: &dyn Fn() -> bool,
    finish: Finish,
) -> anyhow::Result<Option<String>> {
    listener.set_nonblocking(true)?;
    let deadline = super::now_unix() + 10 * 60;
    loop {
        if cancelled() {
            return Ok(None);
        }
        if super::now_unix() > deadline {
            anyhow::bail!("sign-in timed out, please try again");
        }
        let (mut stream, _) = match listener.accept() {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        let target = line.split_whitespace().nth(1).unwrap_or_default().to_string();
        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
        if path != "/callback" {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            continue;
        }
        let params: BTreeMap<String, String> = query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (url_decode(k), url_decode(v)))
            .collect();
        let foreign = params.get("state").map(String::as_str) != Some(state);
        let code = params.get("code").cloned().filter(|c| !c.is_empty());
        let ok = !foreign && !params.contains_key("error") && code.is_some();
        match (&finish, foreign) {
            (Finish::Redirect(to), false) => {
                let _ = write!(
                    stream,
                    "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    to(ok)
                );
            }
            _ => {
                let msg = if foreign {
                    "This sign-in link is old or wasn't started by Hearthlink. Go back to Hearthlink and sign in from there."
                } else if params.contains_key("error") {
                    "Sign-in was cancelled."
                } else if ok {
                    "Signed in. You can close this tab and go back to Hearthlink."
                } else {
                    "Something went wrong. Try signing in again from Hearthlink."
                };
                let page = format!(
                    "<!doctype html><html><head><meta charset=\"utf-8\"><title>Hearthlink</title></head><body style=\"font-family:system-ui;background:#18181b;color:#e4e4e7;display:flex;align-items:center;justify-content:center;height:100vh;margin:0\"><p>{msg}</p></body></html>"
                );
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                    page.len()
                );
            }
        }
        if foreign {
            // A stale tab or another program: keep waiting for the real reply.
            continue;
        }
        if params.contains_key("error") {
            return Ok(None);
        }
        return code.map(Some).ok_or_else(|| anyhow::anyhow!("no sign-in code"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_decode_bytes() {
        assert_eq!(url_decode("a%20b+c%3D"), "a b c=");
        assert_eq!(url_decode("%C3%BC"), "ü");
        // A '%' before a multi-byte character used to slice into it and panic.
        assert_eq!(url_decode("%ü%"), "%ü%");
        assert_eq!(url_decode("%4"), "%4");
    }
}
