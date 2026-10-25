use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    /// Your validated EchoLink callsign, without -L/-R for a personal station.
    pub callsign: String,
    pub password: String,
    pub name: String,
    /// Shown in the directory, at most 27 characters.
    pub location: String,
    #[serde(default = "default_servers")]
    pub servers: Vec<String>,
    /// Let other stations connect to you while you're idle.
    #[serde(default = "yes")]
    pub accept_incoming: bool,
    /// EchoLink proxy for networks where UDP 5198-5199 can't reach this
    /// computer (Starlink, cellular, carrier-grade NAT): "auto" picks a public
    /// proxy, or give "host" / "host:port". Leave out to connect directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Password for a private proxy; public proxies use "PUBLIC".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_password: Option<String>,
}

fn default_servers() -> Vec<String> {
    ["servers.echolink.org", "naeast.echolink.org", "nasouth.echolink.org", "backup.echolink.org"]
        .map(String::from)
        .to_vec()
}

fn yes() -> bool {
    true
}

pub fn path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".config")).join("echolink-tui/config.toml")
}

pub fn load() -> Result<Config> {
    let p = path();
    let text = std::fs::read_to_string(&p)
        .with_context(|| format!("no settings at {}; run `echolink-tui setup` first", p.display()))?;
    let mode = std::fs::metadata(&p)?.permissions().mode();
    if mode & 0o077 != 0 {
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    }
    let mut c: Config = toml::from_str(&text).with_context(|| format!("reading {}", p.display()))?;
    if let Ok(pw) = std::env::var("ECHOLINK_PASSWORD") {
        c.password = pw;
    }
    c.callsign = c.callsign.trim().to_uppercase();
    if c.callsign.is_empty() || c.password.is_empty() {
        bail!("callsign and password must be set in {}", p.display());
    }
    Ok(c)
}

fn ask(prompt: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{prompt}: ");
    } else {
        print!("{prompt} [{default}]: ");
    }
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    let s = s.trim();
    Ok(if s.is_empty() { default.to_string() } else { s.to_string() })
}

/// Interactive first-time setup. The password is read without echo and
/// stored in a file only you can read.
pub fn setup() -> Result<()> {
    let old = load().ok();
    let get = |f: fn(&Config) -> &str| old.as_ref().map(f).unwrap_or("").to_string();
    println!("EchoLink station setup. Your callsign must be validated at echolink.org.");
    println!("Use your personal callsign (e.g. N0CALL), not your repeater's -R node.\n");
    let callsign = ask("Callsign", &get(|c| &c.callsign))?.to_uppercase();
    let name = ask("Your name", &get(|c| &c.name))?;
    let location = ask("Location (max 27 chars)", &get(|c| &c.location))?;
    println!("\nOn Starlink, cellular or other shared-IP internet, EchoLink needs a proxy.");
    let proxy = ask("Proxy: auto, host[:port], or none", old.as_ref().and_then(|c| c.proxy.as_deref()).unwrap_or("none"))?;
    let proxy = (!proxy.eq_ignore_ascii_case("none") && !proxy.is_empty()).then_some(proxy);
    let keep = old.is_some();
    let password = loop {
        let prompt = if keep { "EchoLink password (Enter keeps current): " } else { "EchoLink password: " };
        let pw = rpassword::prompt_password(prompt)?;
        if pw.is_empty() && keep {
            break old.as_ref().unwrap().password.clone();
        }
        if pw.is_empty() {
            continue;
        }
        if rpassword::prompt_password("Again: ")? == pw {
            break pw;
        }
        println!("They don't match, try again.");
    };
    let cfg = Config {
        callsign,
        password,
        name,
        location: location.chars().take(27).collect(),
        servers: old.as_ref().map(|c| c.servers.clone()).unwrap_or_else(default_servers),
        accept_incoming: old.as_ref().map(|c| c.accept_incoming).unwrap_or(true),
        proxy,
        proxy_password: old.as_ref().and_then(|c| c.proxy_password.clone()),
    };
    let p = path();
    std::fs::create_dir_all(p.parent().unwrap())?;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&p)?;
    f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    f.write_all(toml::to_string(&cfg)?.as_bytes())?;
    println!("\nSaved to {} (readable only by you).", p.display());
    Ok(())
}
