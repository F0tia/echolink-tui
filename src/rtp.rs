//! EchoLink's flavour of RTP/RTCP, following SvxLink's echolib (rtpacket.cpp).
//!
//! Audio (UDP 5198): a 12-byte RTP header with version bits 3 (0xC0), payload
//! type 3 (GSM), then four 33-byte GSM frames (80 ms). Text on the same port
//! starts with "oNDATA": "\r" + station info, or "CALL>message\r\n" for chat.
//!
//! Control (UDP 5199): RTCP with a null receiver report followed by an SDES
//! (sent every 10 s as keepalive) or a BYE (disconnect).

use crate::gsm::FRAME_BYTES;

const VERSION: u8 = 3;
const RR: u8 = 201;
const SDES: u8 = 202;
const BYE: u8 = 203;

const SDES_END: u8 = 0;
const SDES_CNAME: u8 = 1;
const SDES_NAME: u8 = 2;
const SDES_EMAIL: u8 = 3;
const SDES_PHONE: u8 = 4;

pub const PT_GSM: u8 = 0x03;
pub const FRAMES_PER_PACKET: usize = 4;

fn null_rr(p: &mut Vec<u8>) {
    p.extend_from_slice(&[VERSION << 6, RR, 0, 1, 0, 0, 0, 0]);
}

fn add_text(p: &mut Vec<u8>, item: u8, text: &str) {
    let t = &text.as_bytes()[..text.len().min(255)];
    p.push(item);
    p.push(t.len() as u8);
    p.extend_from_slice(t);
}

/// Pad from `start` to a 4-byte boundary and fill in the RTCP length field.
fn finish(p: &mut Vec<u8>, start: usize) {
    while !(p.len() - start).is_multiple_of(4) {
        p.push(0);
    }
    let words = ((p.len() - start) / 4 - 1) as u16;
    p[start + 2..start + 4].copy_from_slice(&words.to_be_bytes());
}

pub fn sdes(callsign: &str, name: &str) -> Vec<u8> {
    let mut p = Vec::with_capacity(128);
    null_rr(&mut p);
    let start = p.len();
    p.extend_from_slice(&[(VERSION << 6) | 1, SDES, 0, 0, 0, 0, 0, 0]);
    add_text(&mut p, SDES_CNAME, "CALLSIGN");
    add_text(&mut p, SDES_NAME, &format!("{callsign:<15}{name}"));
    add_text(&mut p, SDES_EMAIL, "CALLSIGN");
    add_text(&mut p, SDES_PHONE, "08:30");
    p.extend_from_slice(&[SDES_END, 0]);
    finish(&mut p, start);
    p
}

pub fn bye() -> Vec<u8> {
    let mut p = Vec::with_capacity(32);
    null_rr(&mut p);
    let start = p.len();
    p.extend_from_slice(&[(VERSION << 6) | 1, BYE, 0, 0, 0, 0, 0, 0]);
    p.push(7);
    p.extend_from_slice(b"jan2002");
    finish(&mut p, start);
    p
}

/// Iterate over the subpackets of a compound RTCP packet as (type, bytes).
fn subpackets(mut p: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    std::iter::from_fn(move || {
        if p.len() < 4 {
            return None;
        }
        let ver = p[0] >> 6;
        if ver != VERSION && ver != 1 {
            return None;
        }
        let len = (u16::from_be_bytes([p[2], p[3]]) as usize + 1) * 4;
        if len > p.len() {
            return None;
        }
        let (sub, rest) = p.split_at(len);
        p = rest;
        Some((sub[1], sub))
    })
}

fn is_rtcp(p: &[u8]) -> bool {
    p.len() >= 4 && matches!(p[0] >> 6, VERSION | 1) && p[0] & 0x20 == 0 && matches!(p[1], 200 | RR)
}

pub fn is_bye(p: &[u8]) -> bool {
    is_rtcp(p) && subpackets(p).any(|(t, _)| t == BYE)
}

/// The NAME item of an SDES packet, e.g. "W1AW-L         Newington, CT".
pub fn sdes_name(p: &[u8]) -> Option<String> {
    if !is_rtcp(p) {
        return None;
    }
    let (_, sub) = subpackets(p).find(|(t, _)| *t == SDES)?;
    let mut items = sub.get(8..)?;
    while items.len() >= 2 && items[0] != SDES_END {
        let (kind, len) = (items[0], items[1] as usize);
        let text = items.get(2..2 + len)?;
        if kind == SDES_NAME {
            return Some(String::from_utf8_lossy(text).trim().to_string());
        }
        items = &items[2 + len..];
    }
    None
}

/// Split an SDES name into (callsign, rest), keeping only callsign characters.
pub fn split_name(name: &str) -> (String, String) {
    let (call, rest) = name.split_once(char::is_whitespace).unwrap_or((name, ""));
    let call = call
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "-/*_".contains(*c))
        .take(32)
        .collect();
    (call, rest.trim().to_string())
}

pub fn voice(seq: u16, frames: &[[u8; FRAME_BYTES]; FRAMES_PER_PACKET]) -> Vec<u8> {
    let mut p = Vec::with_capacity(12 + FRAMES_PER_PACKET * FRAME_BYTES);
    p.extend_from_slice(&[0xC0, PT_GSM]);
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&[0; 8]); // timestamp and ssrc are always zero
    for f in frames {
        p.extend_from_slice(f);
    }
    p
}

pub enum AudioPort<'a> {
    /// GSM frames of a voice packet.
    Voice(Vec<&'a [u8]>),
    Info(String),
    Chat(String),
    Other,
}

pub fn parse_audio_port(p: &[u8]) -> AudioPort<'_> {
    if p.first() == Some(&0xC0) {
        if p.len() < 12 || p[1] != PT_GSM {
            return AudioPort::Other;
        }
        return AudioPort::Voice(p[12..].as_chunks::<FRAME_BYTES>().0.iter().map(|f| f.as_slice()).collect());
    }
    if p.len() < 7 || &p[1..6] != b"NDATA" {
        return AudioPort::Other;
    }
    let text = |from: usize| {
        let end = p.iter().position(|&b| b == 0).unwrap_or(p.len()).max(from);
        String::from_utf8_lossy(&p[from..end]).replace('\r', "\n").trim_end().to_string()
    };
    if p[6] == b'\r' {
        AudioPort::Info(text(7))
    } else {
        AudioPort::Chat(text(6))
    }
}

pub fn info(text: &str) -> Vec<u8> {
    let mut p = format!("oNDATA\r{}", text.replace('\n', "\r")).into_bytes();
    p.push(0);
    p
}

pub fn chat(callsign: &str, msg: &str) -> Vec<u8> {
    let mut p = format!("oNDATA{callsign}>{msg}\r\n").into_bytes();
    p.push(0);
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdes_round_trip() {
        let p = sdes("N0CALL", "Pat");
        assert_eq!(p.len() % 4, 0);
        assert!(is_rtcp(&p) && !is_bye(&p));
        let name = sdes_name(&p).unwrap();
        assert_eq!(split_name(&name), ("N0CALL".into(), "Pat".into()));
    }

    #[test]
    fn sdes_matches_svxlink_layout() {
        // CNAME, NAME, EMAIL, PHONE, END, 0, padded; length in words minus one.
        let p = sdes("N0CALL", "Test");
        let sdes = &p[8..];
        assert_eq!(&sdes[..2], &[0xC1, 202]);
        let words = u16::from_be_bytes([sdes[2], sdes[3]]) as usize;
        assert_eq!((words + 1) * 4, sdes.len());
        assert_eq!(&sdes[8..18], b"\x01\x08CALLSIGN");
    }

    #[test]
    fn bye_detected() {
        assert!(is_bye(&bye()));
        assert!(sdes_name(&bye()).is_none());
    }

    #[test]
    fn audio_port_text() {
        match parse_audio_port(&chat("N0CALL", "hello")) {
            AudioPort::Chat(t) => assert_eq!(t, "N0CALL>hello"),
            _ => panic!(),
        }
        match parse_audio_port(&info("line1\nline2")) {
            AudioPort::Info(t) => assert_eq!(t, "line1\nline2"),
            _ => panic!(),
        }
    }

    #[test]
    fn voice_layout() {
        let p = voice(7, &[[1; FRAME_BYTES]; 4]);
        assert_eq!(p.len(), 12 + 132);
        match parse_audio_port(&p) {
            AudioPort::Voice(f) => assert_eq!(f.len(), 4),
            _ => panic!(),
        }
    }
}
