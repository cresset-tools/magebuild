//! Reset php-fpm's opcache the way cachetool does: write a one-line script to a
//! directory php-fpm can read, and run it with a single FastCGI request.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

const TIMEOUT: Duration = Duration::from_secs(30);
const BEGIN_REQUEST: u8 = 1;
const END_REQUEST: u8 = 3;
const PARAMS: u8 = 4;
const STDIN: u8 = 5;
const STDOUT: u8 = 6;
const STDERR: u8 = 7;
const RESPONDER: u16 = 1;

/// Run `opcache_reset()` in php-fpm at `addr` (`host:port` or a socket path).
pub fn opcache_reset(addr: &str, tmp_dir: &Path) -> Result<()> {
    let script = tmp_dir.join(format!("magebuild-opcache-{}.php", std::process::id()));
    std::fs::write(
        &script,
        "<?php echo function_exists('opcache_reset') && opcache_reset() ? 'ok' : 'off';",
    )
    .with_context(|| format!("writing {}", script.display()))?;
    let out = request(addr, &script);
    let _ = std::fs::remove_file(&script);
    let (body, stderr) = out?;
    match body.as_str() {
        "ok" => Ok(()),
        "off" => bail!("php-fpm at {addr} has no opcache to reset"),
        other => bail!("unexpected opcache reset response from {addr}: {other} {stderr}"),
    }
}

fn request(addr: &str, script: &Path) -> Result<(String, String)> {
    let mut s: Box<dyn ReadWrite> = if addr.starts_with('/') || addr.starts_with("unix:") {
        let path = addr.trim_start_matches("unix:");
        let s = UnixStream::connect(path)
            .with_context(|| format!("connecting to php-fpm at {path}"))?;
        s.set_read_timeout(Some(TIMEOUT))?;
        Box::new(s)
    } else {
        let s =
            TcpStream::connect(addr).with_context(|| format!("connecting to php-fpm at {addr}"))?;
        s.set_read_timeout(Some(TIMEOUT))?;
        Box::new(s)
    };

    let path = script.to_string_lossy();
    let mut params = Vec::new();
    for (k, v) in [
        ("GATEWAY_INTERFACE", "FastCGI/1.0"),
        ("REQUEST_METHOD", "GET"),
        ("SCRIPT_FILENAME", path.as_ref()),
        ("SCRIPT_NAME", path.as_ref()),
        ("REQUEST_URI", path.as_ref()),
        ("DOCUMENT_ROOT", "/"),
        ("SERVER_SOFTWARE", "magebuild"),
        ("SERVER_PROTOCOL", "HTTP/1.1"),
        ("CONTENT_LENGTH", "0"),
    ] {
        encode_pair(&mut params, k.as_bytes(), v.as_bytes());
    }

    let mut req = Vec::new();
    record(
        &mut req,
        BEGIN_REQUEST,
        &[(RESPONDER >> 8) as u8, RESPONDER as u8, 0, 0, 0, 0, 0, 0],
    );
    record(&mut req, PARAMS, &params);
    record(&mut req, PARAMS, &[]);
    record(&mut req, STDIN, &[]);
    s.write_all(&req)?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        let mut header = [0u8; 8];
        s.read_exact(&mut header)
            .context("reading php-fpm's reply")?;
        let kind = header[1];
        let len = u16::from_be_bytes([header[4], header[5]]) as usize;
        let pad = header[6] as usize;
        let mut content = vec![0; len + pad];
        s.read_exact(&mut content)?;
        content.truncate(len);
        match kind {
            STDOUT => stdout.extend(content),
            STDERR => stderr.extend(content),
            END_REQUEST => break,
            _ => {}
        }
    }
    // Drop the CGI headers.
    let text = String::from_utf8_lossy(&stdout);
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or(&text);
    Ok((
        body.trim().to_string(),
        String::from_utf8_lossy(&stderr).trim().to_string(),
    ))
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

fn record(out: &mut Vec<u8>, kind: u8, content: &[u8]) {
    let len = content.len() as u16;
    out.extend_from_slice(&[1, kind, 0, 1]);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(content);
}

fn encode_pair(out: &mut Vec<u8>, k: &[u8], v: &[u8]) {
    for n in [k.len(), v.len()] {
        if n < 128 {
            out.push(n as u8);
        } else {
            out.extend_from_slice(&((n as u32) | 0x8000_0000).to_be_bytes());
        }
    }
    out.extend_from_slice(k);
    out.extend_from_slice(v);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_use_one_byte_lengths_when_short_and_four_when_long() {
        let mut out = Vec::new();
        encode_pair(&mut out, b"A", &[b'x'; 200]);
        assert_eq!(&out[..5], &[1, 0x80, 0, 0, 200]);
        assert_eq!(out.len(), 5 + 1 + 200);
    }

    #[test]
    fn records_carry_version_type_id_and_length() {
        let mut out = Vec::new();
        record(&mut out, PARAMS, b"abc");
        assert_eq!(out, [1, PARAMS, 0, 1, 0, 3, 0, 0, b'a', b'b', b'c']);
    }
}
