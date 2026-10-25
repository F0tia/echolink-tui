//! How we reach the EchoLink network: directly (UDP 5198-5199 and TCP 5200
//! from this computer) or through an EchoLink proxy.

use crate::config::Config;
use crate::proxy::{self, Proxy, Udp};
use anyhow::{Result, bail};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

const AUTO_TRIES: usize = 10;

pub struct ProxyManager {
    /// None picks a Ready proxy from echolink.org's public list.
    fixed: Option<(String, u16)>,
    password: String,
    callsign: String,
    /// Our state/region from the configured location ("Anytown, AL" -> "AL").
    region: String,
    udp: Sender<Udp>,
    current: Mutex<Option<Proxy>>,
}

#[derive(Clone)]
pub enum Net {
    Direct,
    Proxy(Arc<ProxyManager>),
}

impl Net {
    /// From the config's `proxy` setting: absent/"" or "none" = direct,
    /// "auto" = public list, otherwise "host" or "host:port".
    /// Station traffic from a proxy goes to `udp`.
    pub fn from_config(cfg: &Config, udp: Sender<Udp>) -> Net {
        let setting = cfg.proxy.as_deref().unwrap_or("").trim();
        if setting.is_empty() || setting.eq_ignore_ascii_case("none") {
            return Net::Direct;
        }
        let fixed = (!setting.eq_ignore_ascii_case("auto")).then(|| match setting.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_string(), p.parse().unwrap()),
            _ => (setting.to_string(), proxy::DEFAULT_PORT),
        });
        Net::Proxy(Arc::new(ProxyManager {
            fixed,
            password: cfg.proxy_password.clone().unwrap_or_else(|| "PUBLIC".into()),
            callsign: cfg.callsign.clone(),
            region: cfg.location.rsplit(',').next().unwrap_or("").trim().to_string(),
            udp,
            current: Mutex::new(None),
        }))
    }

    pub fn describe(&self) -> String {
        match self {
            Net::Direct => "direct".into(),
            Net::Proxy(m) => match m.current() {
                Some(p) => format!("via proxy {}", p.name()),
                None => "proxy not connected".into(),
            },
        }
    }
}

impl ProxyManager {
    /// The live proxy connection, if there is one. Never blocks.
    pub fn current(&self) -> Option<Proxy> {
        self.current.lock().unwrap().clone().filter(|p| p.dead().is_none())
    }

    pub fn is_down(&self) -> bool {
        self.current().is_none()
    }

    /// The live connection, connecting (or reconnecting) first if needed.
    pub fn get(&self) -> Result<Proxy> {
        if let Some(p) = self.current() {
            return Ok(p);
        }
        let candidates: Vec<(String, u16, String)> = match &self.fixed {
            Some((h, p)) => vec![(h.clone(), *p, String::new())],
            None => {
                let mut list = proxy::public_list()?;
                proxy::shuffle(&mut list);
                // Nearby proxies add less audio delay: same state first, then North America.
                list.sort_by_key(|l| (!same_region(&l.location, &self.region), !looks_north_american(&l.location)));
                crate::log!("proxy list: {} public proxies ready", list.len());
                list.into_iter().take(AUTO_TRIES).map(|l| (l.host, l.port, l.location)).collect()
            }
        };
        let mut errors = Vec::new();
        for (host, port, location) in candidates {
            match Proxy::connect(&host, port, &self.callsign, &self.password, self.udp.clone()) {
                Ok(p) => {
                    crate::log!("proxy connected: {host}:{port} {location}");
                    *self.current.lock().unwrap() = Some(p.clone());
                    return Ok(p);
                }
                Err(e) => {
                    crate::log!("proxy {host}:{port} failed: {e:#}");
                    errors.push(format!("{e:#}"));
                }
            }
        }
        bail!("couldn't connect to an EchoLink proxy: {}", errors.last().map(String::as_str).unwrap_or("none to try"))
    }
}

/// "Huntsville, AL" is in region "AL".
fn same_region(location: &str, region: &str) -> bool {
    !region.is_empty() && location.split([',', ' ']).any(|w| w.eq_ignore_ascii_case(region))
}

/// Locations like "Lenoir, North Carolina", "Silver Spring, MD" or "..., USA".
fn looks_north_american(location: &str) -> bool {
    const STATES: [&str; 63] = [
        "AL", "AK", "AZ", "AR", "CA", "CO", "CT", "DE", "FL", "GA", "HI", "ID", "IL", "IN", "IA", "KS", "KY", "LA",
        "ME", "MD", "MA", "MI", "MN", "MS", "MO", "MT", "NE", "NV", "NH", "NJ", "NM", "NY", "NC", "ND", "OH", "OK",
        "OR", "PA", "RI", "SC", "SD", "TN", "TX", "UT", "VT", "VA", "WA", "WV", "WI", "WY", "DC", "PR", "USA", "US",
        "ON", "QC", "BC", "AB", "MB", "SK", "NS", "NB", "Canada",
    ];
    let words: Vec<&str> = location.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    words.iter().any(|w| STATES.contains(w))
        || ["Carolina", "Virginia", "Alabama", "Georgia", "Florida", "Texas", "Tennessee", "Ohio", "California", "Maryland"]
            .iter()
            .any(|n| location.contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_preference() {
        assert!(same_region("Huntsville, AL", "AL"));
        assert!(!same_region("Albany, NY", "AL"));
        assert!(looks_north_american("Silver Spring, MD FM19LB"));
        assert!(looks_north_american("Lenoir, North Carolina"));
        assert!(!looks_north_american("Hilversum, Netherlands"));
        assert!(!looks_north_american("Amsterdam, NL, no timeout, no restrictions"));
    }
}
