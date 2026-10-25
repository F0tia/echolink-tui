//! EchoLink proxy client, following SvxLink's EchoLinkProxy.
//!
//! For networks where UDP 5198-5199 can't reach this computer (carrier-grade
//! NAT such as Starlink or cellular). One outgoing TCP connection to a proxy
//! (port 8100) carries everything: the proxy logs in to the directory and
//! exchanges station traffic from its own public IP on our behalf.
//!
//! After an 8-byte nonce from the proxy, we send "CALLSIGN\n" followed by
//! MD5(PASSWORD + nonce). Then both directions use message blocks:
//! type (1 byte), IPv4 address (4 bytes, network order), length (4 bytes,
//! little-endian), data.

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 8100;
const LIST_URL_HOST: &str = "www.echolink.org";
const LIST_URL_PATH: &str = "/proxylist.jsp";
const TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BLOCK: usize = 1 << 20;

const TCP_OPEN: u8 = 1;
const TCP_DATA: u8 = 2;
const TCP_CLOSE: u8 = 3;
const TCP_STATUS: u8 = 4;
const UDP_DATA: u8 = 5;
const UDP_CONTROL: u8 = 6;
const SYSTEM: u8 = 7;

/// Station traffic received through the proxy.
pub enum Udp {
    Audio(Ipv4Addr, Vec<u8>),
    Ctrl(Ipv4Addr, Vec<u8>),
}

enum Tcp {
    Status(u32),
    Data(Vec<u8>),
    Close,
}

struct Inner {
    writer: Mutex<TcpStream>,
    name: String,
    /// One directory exchange at a time; the proxy tunnels a single TCP stream.
    tcp_lock: Mutex<Receiver<Tcp>>,
    dead: Mutex<Option<String>>,
}

#[derive(Clone)]
pub struct Proxy(Arc<Inner>);

fn block(kind: u8, ip: Ipv4Addr, data: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(9 + data.len());
    b.push(kind);
    b.extend_from_slice(&ip.octets());
    b.extend_from_slice(&(data.len() as u32).to_le_bytes());
    b.extend_from_slice(data);
    b
}

fn auth_reply(callsign: &str, password: &str, nonce: &[u8]) -> Vec<u8> {
    let mut secret = password.to_uppercase().into_bytes();
    secret.extend_from_slice(nonce);
    let mut out = callsign.to_uppercase().into_bytes();
    out.push(b'\n');
    out.extend_from_slice(&md5::compute(&secret).0);
    out
}

fn read_block(s: &mut TcpStream) -> std::io::Result<(u8, Ipv4Addr, Vec<u8>)> {
    let mut h = [0u8; 9];
    s.read_exact(&mut h)?;
    let len = u32::from_le_bytes([h[5], h[6], h[7], h[8]]) as usize;
    if len > MAX_BLOCK {
        return Err(std::io::Error::other(format!("oversized proxy message ({len} bytes)")));
    }
    let mut data = vec![0u8; len];
    s.read_exact(&mut data)?;
    Ok((h[0], Ipv4Addr::new(h[1], h[2], h[3], h[4]), data))
}

fn system_message(data: &[u8]) -> String {
    match data.first() {
        Some(1) => "the proxy rejected the password".into(),
        Some(2) => "the proxy denied access (it may be private or restricted)".into(),
        Some(n) => format!("the proxy sent system message {n}"),
        None => "the proxy sent an empty system message".into(),
    }
}

impl Proxy {
    /// Connect and authenticate. Incoming station traffic goes to `udp`.
    pub fn connect(host: &str, port: u16, callsign: &str, password: &str, udp: Sender<Udp>) -> Result<Proxy> {
        let addr: SocketAddr = (host, port)
            .to_socket_addrs()
            .with_context(|| format!("looking up proxy {host}"))?
            .find(SocketAddr::is_ipv4)
            .with_context(|| format!("proxy {host} has no IPv4 address"))?;
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(8))
            .with_context(|| format!("connecting to proxy {host}:{port}"))?;
        s.set_nodelay(true)?;
        s.set_read_timeout(Some(Duration::from_secs(8)))?;

        let mut nonce = [0u8; 8];
        s.read_exact(&mut nonce).context("waiting for the proxy's greeting")?;
        s.write_all(&auth_reply(callsign, password, &nonce))?;

        // A refusal arrives straight away as a SYSTEM message; silence means we're in.
        s.set_read_timeout(Some(Duration::from_millis(1500)))?;
        let mut first = None;
        match read_block(&mut s) {
            Ok((SYSTEM, _, data)) => bail!("{host}: {}", system_message(&data)),
            Ok(b) => first = Some(b),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => bail!("{host} closed the connection after login: {e}"),
        }
        s.set_read_timeout(None)?;

        let (tcp_tx, tcp_rx) = channel();
        let inner = Arc::new(Inner {
            writer: Mutex::new(s.try_clone()?),
            name: format!("{host}:{port}"),
            tcp_lock: Mutex::new(tcp_rx),
            dead: Mutex::new(None),
        });
        let reader_inner = inner.clone();
        std::thread::spawn(move || {
            let dispatch = |(kind, ip, data): (u8, Ipv4Addr, Vec<u8>)| -> Option<String> {
                match kind {
                    UDP_DATA => drop(udp.send(Udp::Audio(ip, data))),
                    UDP_CONTROL => drop(udp.send(Udp::Ctrl(ip, data))),
                    TCP_DATA => drop(tcp_tx.send(Tcp::Data(data))),
                    TCP_CLOSE => drop(tcp_tx.send(Tcp::Close)),
                    TCP_STATUS if data.len() == 4 => {
                        drop(tcp_tx.send(Tcp::Status(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))))
                    }
                    SYSTEM => return Some(system_message(&data)),
                    other => return Some(format!("unexpected proxy message type {other}")),
                }
                None
            };
            let mut why = first.and_then(dispatch);
            while why.is_none() {
                why = match read_block(&mut s) {
                    Ok(b) => dispatch(b),
                    Err(e) => Some(format!("connection lost: {e}")),
                };
            }
            crate::log!("proxy {}: {}", reader_inner.name, why.as_deref().unwrap_or(""));
            *reader_inner.dead.lock().unwrap() = why;
            let _ = s.shutdown(Shutdown::Both);
            let _ = tcp_tx.send(Tcp::Close);
        });
        Ok(Proxy(inner))
    }

    pub fn name(&self) -> &str {
        &self.0.name
    }

    /// Hang up on the proxy (its reader thread then marks it dead).
    pub fn close(&self, why: &str) {
        let mut dead = self.0.dead.lock().unwrap();
        if dead.is_none() {
            *dead = Some(why.to_string());
        }
        let _ = self.0.writer.lock().unwrap().shutdown(Shutdown::Both);
    }

    /// Why the connection ended, or None while it's up.
    pub fn dead(&self) -> Option<String> {
        self.0.dead.lock().unwrap().clone()
    }

    fn send(&self, b: &[u8]) -> Result<()> {
        if let Some(why) = self.dead() {
            bail!("proxy {}: {why}", self.0.name);
        }
        self.0.writer.lock().unwrap().write_all(b).context("writing to the proxy")
    }

    pub fn udp_audio(&self, ip: Ipv4Addr, data: &[u8]) -> Result<()> {
        self.send(&block(UDP_DATA, ip, data))
    }

    pub fn udp_ctrl(&self, ip: Ipv4Addr, data: &[u8]) -> Result<()> {
        self.send(&block(UDP_CONTROL, ip, data))
    }

    /// Open a TCP connection from the proxy to `server` (port 5200), send
    /// `request`, and collect the reply until `done` says it's complete or
    /// the server closes.
    pub fn tcp_exchange(&self, server: Ipv4Addr, request: &[u8], done: &dyn Fn(&[u8]) -> bool) -> Result<Vec<u8>> {
        let rx = self.0.tcp_lock.lock().unwrap();
        while rx.try_recv().is_ok() {} // leftovers from an earlier exchange
        self.send(&block(TCP_OPEN, server, &[]))?;
        let deadline = Instant::now() + TIMEOUT;
        let next = |rx: &Receiver<Tcp>| {
            let left = deadline.saturating_duration_since(Instant::now());
            rx.recv_timeout(left).map_err(|_| anyhow::anyhow!("timed out talking to {server} through the proxy"))
        };
        match next(&rx)? {
            Tcp::Status(0) => {}
            Tcp::Status(code) => bail!("the proxy couldn't reach {server} (status {code})"),
            Tcp::Close | Tcp::Data(_) => bail!("the proxy couldn't reach {server}"),
        }
        self.send(&block(TCP_DATA, Ipv4Addr::UNSPECIFIED, request))?;
        let mut reply = Vec::new();
        let closed = loop {
            match next(&rx) {
                Ok(Tcp::Data(d)) => {
                    reply.extend_from_slice(&d);
                    if done(&reply) {
                        break false;
                    }
                }
                Ok(Tcp::Close) => break true,
                Ok(Tcp::Status(_)) => {}
                Err(e) if reply.is_empty() => return Err(e),
                Err(_) => break false,
            }
        };
        if !closed {
            let _ = self.send(&block(TCP_CLOSE, Ipv4Addr::UNSPECIFIED, &[]));
            // Wait briefly for the proxy to confirm so the next exchange starts clean.
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
        if let Some(why) = self.dead()
            && reply.is_empty()
        {
            bail!("proxy {}: {why}", self.0.name);
        }
        Ok(reply)
    }
}

/// A public proxy from echolink.org's list.
#[derive(Debug, Clone)]
pub struct Listed {
    pub host: String,
    pub port: u16,
    pub location: String,
}

/// Public proxies currently marked Ready.
pub fn public_list() -> Result<Vec<Listed>> {
    let mut s = TcpStream::connect((LIST_URL_HOST, 80)).context("connecting to echolink.org for the proxy list")?;
    s.set_read_timeout(Some(TIMEOUT))?;
    write!(s, "GET {LIST_URL_PATH} HTTP/1.0\r\nHost: {LIST_URL_HOST}\r\nUser-Agent: echolink-tui\r\n\r\n")?;
    let mut page = Vec::new();
    s.read_to_end(&mut page).context("downloading the proxy list")?;
    let list = parse_list(&String::from_utf8_lossy(&page));
    if list.is_empty() {
        bail!("no public proxies are marked Ready on echolink.org");
    }
    Ok(list)
}

fn parse_list(html: &str) -> Vec<Listed> {
    let cell = |td: &str| -> String {
        let mut out = String::new();
        let mut in_tag = false;
        for c in td.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                _ if !in_tag => out.push(c),
                _ => {}
            }
        }
        out.replace("&nbsp;", " ").trim().to_string()
    };
    html.split("<tr")
        .filter_map(|row| {
            let cells: Vec<String> = row.split("<td").skip(1).map(|td| cell(&format!("<td{td}"))).collect();
            // name, host, port, last report, status, version, location
            if cells.len() < 7 || !cells[4].starts_with("Ready") {
                return None;
            }
            Some(Listed { host: cells[1].clone(), port: cells[2].parse().ok()?, location: cells[6].clone() })
        })
        .collect()
}

/// Shuffle without a random-number crate: xorshift seeded from the clock.
pub fn shuffle<T>(v: &mut [T]) {
    let mut x = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        | 1;
    for i in (1..v.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.swap(i, (x % (i as u64 + 1)) as usize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_matches_svxlink() {
        // "CALL\n" + md5("PUBLIC" + nonce); password is upper-cased first.
        let nonce = *b"ABCDEFGH";
        let r = auth_reply("n0call", "public", &nonce);
        assert_eq!(&r[..7], b"N0CALL\n");
        assert_eq!(&r[7..], &md5::compute(b"PUBLICABCDEFGH").0);
    }

    #[test]
    fn block_layout() {
        let b = block(UDP_CONTROL, Ipv4Addr::new(13, 57, 14, 183), b"hi");
        assert_eq!(b, [6, 13, 57, 14, 183, 2, 0, 0, 0, b'h', b'i']);
    }

    #[test]
    fn parses_list_rows() {
        let html = r#"<tr class="normal-row">
            <td>W3KIT #59&nbsp;</td><td>elp-59.w3kit.net&nbsp;</td><td>8100&nbsp;</td><td>23:37&nbsp;</td>
            <td><font color="Red">Busy&nbsp;</font></td><td>1.2.5c&nbsp;</td><td>Silver Spring, MD&nbsp;</td></tr>
            <tr class="normal-row">
            <td>KK4DDF 01&nbsp;</td><td>proxy2.echolink.kk4ddf.org&nbsp;</td><td>8100&nbsp;</td><td>23:36&nbsp;</td>
            <td><font color="Green">Ready&nbsp;</font></td><td>1.2.5c&nbsp;</td><td>Somewhere, AL&nbsp;
            </td></tr>"#;
        let l = parse_list(html);
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].host, "proxy2.echolink.kk4ddf.org");
        assert_eq!(l[0].port, 8100);
        assert_eq!(l[0].location, "Somewhere, AL");
    }

    #[test]
    fn shuffle_keeps_items() {
        let mut v: Vec<u32> = (0..50).collect();
        shuffle(&mut v);
        let mut s = v.clone();
        s.sort();
        assert_eq!(s, (0..50).collect::<Vec<_>>());
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Against real public proxies: ELPROXY_CALL=<callsign> cargo test live -- --ignored
    #[test]
    #[ignore]
    fn tunnel_to_directory() {
        let call = std::env::var("ELPROXY_CALL").unwrap();
        let mut list = public_list().unwrap();
        println!("{} public proxies ready", list.len());
        shuffle(&mut list);
        let (udp_tx, _udp_rx) = channel();
        let p = list
            .iter()
            .take(10)
            .find_map(|l| match Proxy::connect(&l.host, l.port, &call, "PUBLIC", udp_tx.clone()) {
                Ok(p) => Some(p),
                Err(e) => {
                    println!("{}: {e:#}", l.host);
                    None
                }
            })
            .expect("no proxy accepted us");
        println!("connected to {}", p.name());
        let server = ("servers.echolink.org", 5200).to_socket_addrs().unwrap().find_map(|a| match a {
            SocketAddr::V4(v) => Some(*v.ip()),
            _ => None,
        });
        let reply = p.tcp_exchange(server.unwrap(), b"F\r", &|_| false).unwrap();
        println!("directory reply through proxy: {} bytes, starts {:?}", reply.len(), &reply[..reply.len().min(12)]);
        assert!(reply.len() > 50);
        // A second exchange on the same proxy must also work.
        let again = p.tcp_exchange(server.unwrap(), b"F\r", &|_| false).unwrap();
        println!("second exchange: {} bytes", again.len());
        assert!(again.len() > 50);
        assert!(p.dead().is_none());
    }
}
