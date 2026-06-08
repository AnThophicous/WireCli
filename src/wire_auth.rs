//! OpenRouter authentication client.
//!
//! This module implements the CLI side of the OpenRouter PKCE flow: create a
//! code challenge, open the browser approval URL, listen for the local callback,
//! and exchange the code for an OpenRouter API key.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::blocking::Client;
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use std::env;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const OPENROUTER_API_BASE_URL: &str = "https://openrouter.ai/api/v1";
const OPENROUTER_AUTH_URL: &str = "https://openrouter.ai/auth";
const OPENROUTER_CALLBACK_PATH: &str = "/wirecli/openrouter/callback";

#[derive(Debug, Clone)]
pub struct OpenRouterLoginResult {
    pub api_key: String,
    pub user_id: Option<String>,
    pub base_url: String,
    pub model: String,
}

#[derive(Debug, Deserialize)]
struct OpenRouterKeyResponse {
    key: Option<String>,
    user_id: Option<String>,
}

pub fn default_openrouter_base_url() -> String {
    OPENROUTER_API_BASE_URL.to_string()
}

pub fn login_with_openrouter() -> Result<OpenRouterLoginResult, String> {
    login_with_openrouter_progress(|_| {})
}

pub fn login_with_openrouter_progress<F>(mut status: F) -> Result<OpenRouterLoginResult, String>
where
    F: FnMut(String),
{
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let verifier = pkce_code_verifier()?;
    let challenge = pkce_code_challenge(&verifier);
    let callback = OpenRouterCallback::bind()?;
    let callback_url = callback.url();
    let auth_url = format!(
        "{}?callback_url={}&code_challenge={}&code_challenge_method=S256",
        OPENROUTER_AUTH_URL,
        percent_encode(&callback_url),
        percent_encode(&challenge)
    );
    status(format!("OpenRouter login URL: {auth_url}"));
    open_browser(&auth_url).map_err(|err| format!("{err}\nOpen manually: {auth_url}"))?;
    status("Waiting for OpenRouter approval...".to_string());
    let code = callback.wait_for_code(Duration::from_secs(300))?;
    status("Finishing OpenRouter connection...".to_string());
    let exchange = exchange_openrouter_code(&client, &code, &verifier)?;
    let api_key = exchange
        .key
        .ok_or_else(|| "OpenRouter exchange returned no key".to_string())?;
    Ok(OpenRouterLoginResult {
        api_key,
        user_id: exchange.user_id,
        base_url: OPENROUTER_API_BASE_URL.to_string(),
        model: String::new(),
    })
}

fn exchange_openrouter_code(
    client: &Client,
    code: &str,
    verifier: &str,
) -> Result<OpenRouterKeyResponse, String> {
    let response = client
        .post(format!("{OPENROUTER_API_BASE_URL}/auth/keys"))
        .header(CONTENT_TYPE, "application/json")
        .json(&serde_json::json!({
            "code": code,
            "code_verifier": verifier,
            "code_challenge_method": "S256"
        }))
        .send()
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let text = response.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "OpenRouter PKCE exchange failed with {status}: {text}"
        ));
    }
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

struct OpenRouterCallback {
    listener: TcpListener,
    port: u16,
}

impl OpenRouterCallback {
    fn bind() -> Result<Self, String> {
        let preferred = std::env::var("WIRECLI_OPENROUTER_CALLBACK_PORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(3000);
        let mut ports = vec![preferred, 39187, 39188, 39189];
        ports.dedup();
        let mut last_err = None;
        for port in ports {
            match TcpListener::bind(("127.0.0.1", port)) {
                Ok(listener) => {
                    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
                    return Ok(Self { listener, port });
                }
                Err(err) => last_err = Some(err.to_string()),
            }
        }
        Err(format!(
            "could not bind OpenRouter callback listener; set WIRECLI_OPENROUTER_CALLBACK_PORT to a free localhost port{}",
            last_err
                .map(|err| format!(" (last error: {err})"))
                .unwrap_or_default()
        ))
    }

    fn url(&self) -> String {
        format!("http://localhost:{}{}", self.port, OPENROUTER_CALLBACK_PATH)
    }

    fn wait_for_code(&self, timeout: Duration) -> Result<String, String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.listener.accept() {
                Ok((mut stream, _addr)) => {
                    if let Some(code) = read_callback_code(&mut stream)? {
                        write_callback_response(&mut stream, true)?;
                        return Ok(code);
                    }
                    write_callback_response(&mut stream, false)?;
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(err) => return Err(err.to_string()),
            }
        }
        Err("OpenRouter PKCE timed out waiting for browser callback".to_string())
    }
}

fn read_callback_code(stream: &mut TcpStream) -> Result<Option<String>, String> {
    let mut buffer = [0u8; 4096];
    let n = stream.read(&mut buffer).map_err(|e| e.to_string())?;
    let request = String::from_utf8_lossy(&buffer[..n]);
    let first = request.lines().next().unwrap_or_default();
    let Some(path) = first
        .strip_prefix("GET ")
        .and_then(|rest| rest.split_whitespace().next())
    else {
        return Ok(None);
    };
    if !path.starts_with(OPENROUTER_CALLBACK_PATH) {
        return Ok(None);
    }
    let query = path.split_once('?').map(|(_, query)| query).unwrap_or("");
    Ok(query_param(query, "code"))
}

fn write_callback_response(stream: &mut TcpStream, ok: bool) -> Result<(), String> {
    let body = if ok {
        "Wire CLI connected to OpenRouter. You can close this tab."
    } else {
        "Wire CLI did not receive an OpenRouter authorization code."
    };
    let status = if ok { "200 OK" } else { "400 Bad Request" };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|e| e.to_string())
}

fn pkce_code_verifier() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn pkce_code_challenge(verifier: &str) -> String {
    let digest = sha256(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn open_browser(url: &str) -> Result<(), String> {
    let mut errors = Vec::new();
    if let Ok(browser) = env::var("BROWSER") {
        let browser = browser.trim();
        if !browser.is_empty() {
            match try_open_browser(browser, &[url]) {
                Ok(()) => return Ok(()),
                Err(err) => errors.push(err),
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        match try_open_browser("cmd", &["/C", "start", "", url]) {
            Ok(()) => return Ok(()),
            Err(err) => errors.push(err),
        }
    }

    let attempts: &[(&str, &[&str])] = &[
        ("xdg-open", &[url]),
        ("gio", &["open", url]),
        ("open", &[url]),
        ("sensible-browser", &[url]),
        ("x-www-browser", &[url]),
        ("firefox", &[url]),
        ("google-chrome", &[url]),
        ("chromium", &[url]),
        ("brave-browser", &[url]),
    ];
    for (command, args) in attempts {
        match try_open_browser(command, args) {
            Ok(()) => return Ok(()),
            Err(err) => errors.push(err),
        }
    }

    let detail = errors
        .into_iter()
        .rev()
        .find(|err| !err.trim().is_empty())
        .map(|err| format!(" Last browser error: {err}"))
        .unwrap_or_default();
    Err(format!("could not open browser for {url}.{detail}"))
}

fn try_open_browser(command: &str, args: &[&str]) -> Result<(), String> {
    let mut child = Command::new(command)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("{command}: {err}"))?;
    thread::sleep(Duration::from_millis(250));
    match child.try_wait() {
        Ok(Some(status)) if !status.success() => Err(format!("{command}: exited with {status}")),
        Ok(_) => Ok(()),
        Err(err) => Err(format!("{command}: {err}")),
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let (left, right) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode(left) == key {
            return Some(percent_decode(right));
        }
    }
    None
}

fn percent_decode(value: &str) -> String {
    let mut out = Vec::new();
    let bytes = value.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn sha256(input: &[u8]) -> [u8; 32] {
    const H0: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut message = input.to_vec();
    let bit_len = (message.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    let mut h = H0;
    for chunk in message.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let offset = i * 4;
            *word = u32::from_be_bytes([
                chunk[offset],
                chunk[offset + 1],
                chunk[offset + 2],
                chunk[offset + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = h[0];
        let mut b = h[1];
        let mut c = h[2];
        let mut d = h[3];
        let mut e = h[4];
        let mut f = h[5];
        let mut g = h[6];
        let mut hh = h[7];

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (index, word) in h.iter().enumerate() {
        out[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}
