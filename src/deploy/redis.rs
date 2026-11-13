//! Just enough RESP to delete a retired cache prefix's keys: AUTH, SELECT,
//! SCAN and UNLINK, over TCP or a unix socket.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::envphp::RedisCache;

const TIMEOUT: Duration = Duration::from_secs(10);

trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

enum Reply {
    Simple,
    Int(i64),
    Bulk(Option<Vec<u8>>),
    Array(Vec<Reply>),
}

struct Conn {
    r: BufReader<Box<dyn Stream>>,
}

impl Conn {
    fn open(c: &RedisCache) -> Result<Self> {
        let s: Box<dyn Stream> = if c.server.starts_with('/') || c.server.starts_with("unix://") {
            let path = c.server.trim_start_matches("unix://");
            let s = UnixStream::connect(path)
                .with_context(|| format!("connecting to redis at {path}"))?;
            s.set_read_timeout(Some(TIMEOUT))?;
            s.set_write_timeout(Some(TIMEOUT))?;
            Box::new(s)
        } else {
            let host = c.server.trim_start_matches("tcp://");
            let s = TcpStream::connect((host, c.port))
                .with_context(|| format!("connecting to redis at {host}:{}", c.port))?;
            s.set_read_timeout(Some(TIMEOUT))?;
            s.set_write_timeout(Some(TIMEOUT))?;
            Box::new(s)
        };
        let mut conn = Conn {
            r: BufReader::new(s),
        };
        if let Some(pw) = &c.password {
            conn.call(&[b"AUTH", pw.as_bytes()])?;
        }
        conn.call(&[b"SELECT", c.database.to_string().as_bytes()])?;
        Ok(conn)
    }

    fn call(&mut self, args: &[&[u8]]) -> Result<Reply> {
        let mut buf = format!("*{}\r\n", args.len()).into_bytes();
        for a in args {
            buf.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
            buf.extend_from_slice(a);
            buf.extend_from_slice(b"\r\n");
        }
        self.r.get_mut().write_all(&buf)?;
        self.read()
    }

    fn read(&mut self) -> Result<Reply> {
        let mut line = String::new();
        if self.r.read_line(&mut line)? == 0 {
            bail!("redis closed the connection");
        }
        let line = line.trim_end_matches("\r\n");
        let (kind, rest) = line.split_at(1);
        Ok(match kind {
            "+" => Reply::Simple,
            "-" => bail!("redis: {rest}"),
            ":" => Reply::Int(rest.parse()?),
            "$" => {
                let n: i64 = rest.parse()?;
                if n < 0 {
                    Reply::Bulk(None)
                } else {
                    let mut b = vec![0; n as usize + 2];
                    self.r.read_exact(&mut b)?;
                    b.truncate(n as usize);
                    Reply::Bulk(Some(b))
                }
            }
            "*" => {
                let n: i64 = rest.parse()?;
                let mut items = Vec::with_capacity(n.max(0) as usize);
                for _ in 0..n.max(0) {
                    items.push(self.read()?);
                }
                Reply::Array(items)
            }
            _ => bail!("unexpected redis reply `{line}`"),
        })
    }
}

/// The key patterns a cache prefix appears in. Zend/Cm (Magento's default Redis
/// backend) stores `zc:k:<prefix><id>` and `zc:ti:<prefix><tag>`; Symfony's
/// tag-aware adapter stores `<prefix>…` items and `cache:id_tags:<prefix>…`
/// reverse-index keys. Magento upper-cases cache ids and tags, so a lower-case
/// `r<N>_` after a colon only ever is the prefix itself.
pub fn patterns(prefix: &str) -> [String; 2] {
    let p = glob_escape(prefix);
    [format!("{p}*"), format!("*:{p}*")]
}

/// Delete every key of `cache.prefix` in its database. Returns how many went.
pub fn delete_prefix(cache: &RedisCache) -> Result<u64> {
    let mut conn = Conn::open(cache)?;
    let mut deleted = 0;
    for pattern in patterns(&cache.prefix) {
        let mut cursor = b"0".to_vec();
        loop {
            let Reply::Array(mut parts) = conn.call(&[
                b"SCAN",
                &cursor,
                b"MATCH",
                pattern.as_bytes(),
                b"COUNT",
                b"1000",
            ])?
            else {
                bail!("unexpected SCAN reply");
            };
            let (Some(Reply::Array(keys)), Some(Reply::Bulk(Some(next)))) =
                (parts.pop(), parts.pop())
            else {
                bail!("unexpected SCAN reply");
            };
            let keys: Vec<Vec<u8>> = keys
                .into_iter()
                .filter_map(|k| match k {
                    Reply::Bulk(Some(k)) => Some(k),
                    _ => None,
                })
                .collect();
            if !keys.is_empty() {
                let mut args: Vec<&[u8]> = vec![b"UNLINK"];
                args.extend(keys.iter().map(Vec::as_slice));
                if let Reply::Int(n) = conn.call(&args)? {
                    deleted += n as u64;
                }
            }
            if next == b"0" {
                break;
            }
            cursor = next;
        }
    }
    Ok(deleted)
}

fn glob_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_cover_leading_and_namespaced_prefixes() {
        assert_eq!(
            patterns("r35_"),
            ["r35_*".to_string(), "*:r35_*".to_string()]
        );
        assert_eq!(patterns("a*b")[0], "a\\*b*");
    }
}
