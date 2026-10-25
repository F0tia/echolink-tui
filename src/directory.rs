//! EchoLink directory servers (TCP 5200), following SvxLink's EchoLinkDirectory.
//!
//! Each command uses its own connection. A station has to log in (and refresh
//! the login every few minutes) to be listed, to get the full station list, and
//! for other nodes to accept its connections: they check the caller's IP
//! against the directory.

use crate::net::Net;
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream, ToSocketAddrs};
use std::time::Duration;

const PORT: u16 = 5200;
const TIMEOUT: Duration = Duration::from_secs(20);
pub const MAX_DESCRIPTION: usize = 27;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Online,
    Busy,
    Off,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Repeater,
    Link,
    Conference,
    User,
}

#[derive(Clone, Debug)]
pub struct Station {
    pub callsign: String,
    pub description: String,
    pub busy: bool,
    pub id: u32,
    pub ip: Ipv4Addr,
}

impl Station {
    pub fn kind(&self) -> Kind {
        let c = &self.callsign;
        if c.starts_with('*') {
            Kind::Conference
        } else if c.ends_with("-R") {
            Kind::Repeater
        } else if c.ends_with("-L") {
            Kind::Link
        } else {
            Kind::User
        }
    }
}

fn connect(servers: &[String]) -> Result<TcpStream> {
    let mut last = None;
    for host in servers {
        let addrs = match (host.as_str(), PORT).to_socket_addrs() {
            Ok(a) => a,
            Err(e) => {
                last = Some(anyhow::Error::from(e).context(format!("looking up {host}")));
                continue;
            }
        };
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, TIMEOUT) {
                Ok(s) => {
                    s.set_read_timeout(Some(TIMEOUT))?;
                    s.set_write_timeout(Some(TIMEOUT))?;
                    return Ok(s);
                }
                Err(e) => last = Some(anyhow::Error::from(e).context(format!("connecting to {host}"))),
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no directory servers configured")))
}

fn local_hhmm() -> String {
    // libc-free local time: ask the system's `date`, falling back to UTC.
    std::process::Command::new("date")
        .arg("+%H:%M")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| s.len() == 5)
        .unwrap_or_else(|| {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("{:02}:{:02}", secs / 3600 % 24, secs / 60 % 60)
        })
}

/// Register (or refresh) our status with the directory.
pub fn login(net: &Net, servers: &[String], callsign: &str, password: &str, description: &str, status: Status) -> Result<()> {
    let desc: String = description.chars().take(MAX_DESCRIPTION).collect();
    let state = match status {
        Status::Online => format!("ONLINE3.38({})", local_hhmm()),
        Status::Busy => format!("BUSY3.40({})", local_hhmm()),
        Status::Off => "OFF-V3.40".to_string(),
    };
    let mut cmd = Vec::new();
    cmd.push(b'l');
    cmd.extend_from_slice(callsign.to_uppercase().as_bytes());
    cmd.extend_from_slice(&[0xAC, 0xAC]);
    cmd.extend_from_slice(password.as_bytes());
    cmd.extend_from_slice(format!("\r{state}\r{desc}\r").as_bytes());

    let raw = exchange(net, servers, &cmd, &|r| r.len() >= 2)?;
    let reply = String::from_utf8_lossy(&raw);
    if !reply.starts_with("OK") {
        return Err(Refused(reply.trim().to_string()).into());
    }
    Ok(())
}

/// The directory answered a login with something other than OK (bad
/// password, unvalidated callsign, ...). Retrying soon won't help.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "directory refused login: {}", self.0)
    }
}

impl std::error::Error for Refused {}

/// Fetch the station list, as AllStarLink's chan_echolink does ("F" plus an
/// empty snapshot id). An IP whose login isn't validated gets only a short
/// stub list instead.
pub fn stations(net: &Net, servers: &[String]) -> Result<Vec<Station>> {
    let raw = exchange(net, servers, b"F\r", &|_| false)?;
    parse_list(&decompress(&raw)?)
}

/// Send one command to a directory server and collect the reply, until
/// `done` says it's complete or the server closes the connection.
fn exchange(net: &Net, servers: &[String], request: &[u8], done: &dyn Fn(&[u8]) -> bool) -> Result<Vec<u8>> {
    let proxy = match net {
        Net::Direct => {
            let mut s = connect(servers)?;
            s.write_all(request)?;
            let mut reply = Vec::new();
            let mut buf = [0u8; 65536];
            loop {
                match s.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        reply.extend_from_slice(&buf[..n]);
                        if done(&reply) {
                            break;
                        }
                    }
                    Err(e) if reply.is_empty() => return Err(e).context("waiting for the directory's reply"),
                    Err(_) => break,
                }
            }
            return Ok(reply);
        }
        Net::Proxy(m) => m.get()?,
    };
    let mut last = None;
    for host in servers {
        let addrs = match (host.as_str(), PORT).to_socket_addrs() {
            Ok(a) => a,
            Err(e) => {
                last = Some(anyhow::Error::from(e).context(format!("looking up {host}")));
                continue;
            }
        };
        for addr in addrs {
            let std::net::SocketAddr::V4(v4) = addr else { continue };
            match proxy.tcp_exchange(*v4.ip(), request, done) {
                // Closed without a word: the proxy isn't really getting through.
                Ok(r) if r.is_empty() => last = Some(anyhow::anyhow!("no reply from directory {host} via proxy {}", proxy.name())),
                Ok(r) => return Ok(r),
                Err(e) => last = Some(e.context(format!("directory {host} via proxy {}", proxy.name()))),
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no directory servers configured")))
}

/// The full list has thousands of entries. A short one means the directory
/// doesn't consider this IP validated; it names the station it thinks is here.
pub fn stub_warning(list: &[Station]) -> Option<String> {
    if list.len() >= 100 {
        return None;
    }
    let culprit = list
        .iter()
        .find(|s| s.id == 0 && !s.ip.is_unspecified())
        .map(|s| format!(" It lists {} at your IP {} with no node number, so that station isn't validated: turn off EchoLink on that device, or validate the callsign at echolink.org.", s.callsign, s.ip))
        .unwrap_or_default();
    Some(format!(
        "The directory sent only {} stations, the short list it gives an IP whose login isn't validated.{culprit}",
        list.len()
    ))
}

/// Plain lists start with "@@@"; otherwise there's a 4-byte little-endian
/// uncompressed length followed by a zlib stream.
fn decompress(raw: &[u8]) -> Result<Vec<u8>> {
    if raw.starts_with(b"@@@") || raw.starts_with(b"DDD") {
        return Ok(raw.to_vec());
    }
    let body = raw.get(4..).context("station list: empty reply")?;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(body)
        .read_to_end(&mut out)
        .context("station list: can't decompress")?;
    Ok(out)
}

pub fn parse_list(raw: &[u8]) -> Result<Vec<Station>> {
    // The directory sends Windows-1252/Latin-1 text ("España" as 0xF1).
    let text: String = match std::str::from_utf8(raw) {
        Ok(t) => t.to_string(),
        Err(_) => raw.iter().map(|&b| b as char).collect(),
    };
    let body = text.strip_prefix("@@@\n").context("station list: missing @@@ header")?;
    let mut lines = body.split('\n');
    // "6617" or, for compressed lists, "6617:snapshot-id".
    let count: usize = lines.next().unwrap_or("").split(':').next().unwrap_or("").trim().parse().context("station list: bad count")?;
    let mut out = Vec::with_capacity(count);
    let mut messages = Vec::new();
    for _ in 0..count {
        let (Some(call), Some(data), Some(id), Some(ip)) = (lines.next(), lines.next(), lines.next(), lines.next())
        else {
            break;
        };
        if call.trim().is_empty() || call == "." {
            // Server messages are sent as entries with a blank callsign.
            if !data.trim().is_empty() {
                messages.push(data.trim().to_string());
            }
            continue;
        }
        let (description, flags) = match data.rfind('[') {
            Some(i) => (&data[..i], &data[i..]),
            None => (data, ""),
        };
        out.push(Station {
            callsign: call.trim().to_string(),
            description: description.trim().to_string(),
            busy: flags.contains("BUSY"),
            id: id.trim().parse().unwrap_or(0),
            ip: ip.trim().parse().unwrap_or(Ipv4Addr::UNSPECIFIED),
        });
    }
    if let Some(m) = messages.iter().find(|m| m.contains("INCORRECT PASSWORD")) {
        bail!("{m}");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_live_sample() {
        let raw = b"@@@\n3\n*ECHOTEST*\nAudio test server          [ON 21:24]\n9999\n13.57.14.183\n \nEchoLink Server v2.6.160             \n    \n127.0.0.1\nN0CALL-R\nExample Repeater           [BUSY 10:00]\n400000\n203.0.113.10\n+++";
        let s = parse_list(raw).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].callsign, "*ECHOTEST*");
        assert_eq!(s[0].id, 9999);
        assert_eq!(s[0].kind(), Kind::Conference);
        assert_eq!(s[0].description, "Audio test server");
        assert!(!s[0].busy);
        assert_eq!(s[1].kind(), Kind::Repeater);
        assert!(s[1].busy);
        assert_eq!(s[1].ip, Ipv4Addr::new(203, 0, 113, 10));
    }

    /// Run with DIR_SAMPLE=<raw reply saved from a validated IP>.
    #[test]
    #[ignore]
    fn parses_saved_reply() {
        let raw = std::fs::read(std::env::var("DIR_SAMPLE").unwrap()).unwrap();
        let s = parse_list(&decompress(&raw).unwrap()).unwrap();
        println!("{} stations; first {:?}", s.len(), s.first());
        assert!(s.len() > 1000);
        assert!(stub_warning(&s).is_none());
        assert!(s.iter().any(|x| x.callsign == "*ECHOTEST*" && x.id == 9999));
    }

    #[test]
    fn warns_on_stub_list() {
        let raw = b"@@@\n2\nN0CALL-L\nAnytown, ST [0/20]       [ON 17:24]\n0\n198.51.100.7\n*ECHOTEST*\nAudio test server          [ON 22:24]\n9999\n13.57.14.183\n+++";
        let w = stub_warning(&parse_list(raw).unwrap()).unwrap();
        assert!(w.contains("N0CALL-L at your IP 198.51.100.7"), "{w}");
    }

    #[test]
    fn parses_compressed_reply() {
        use std::io::Write;
        let plain = b"@@@\n1:12345\n*ECHOTEST*\nAudio test server          [ON 21:24]\n9999\n13.57.14.183\n+++";
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(plain).unwrap();
        let mut raw = (plain.len() as u32).to_le_bytes().to_vec();
        raw.extend(z.finish().unwrap());
        let s = parse_list(&decompress(&raw).unwrap()).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].id, 9999);
    }
}
