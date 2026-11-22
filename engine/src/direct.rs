// Reaching a phone over Wi-Fi without macOS's help.
//
// Normally every connection goes through usbmuxd, the Mac's iPhone service, which only sees a phone on Wi-Fi when
// it feels like it. So whenever usbmuxd does show us a phone, we keep its pairing record (the keys made when you
// tapped Trust) and its Wi-Fi details. Later, if usbmuxd can't find it, we connect to the phone's lockdown port
// ourselves with that pairing, the same way usbmuxd would.
//
// Saved in <data>/pairings: <udid>.plist (the pairing record, readable only by you) and <udid>.json (last IP).
// When the IP changes, the phone is found again with Bonjour.

use std::collections::HashMap;
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use idevice::lockdown::LockdownClient;
use idevice::pairing_file::PairingFile;
use idevice::provider::{IdeviceProvider, TcpProvider};
use idevice::IdeviceService;
use serde::{Deserialize, Serialize};

use crate::ops::data_dir;

// lockdownd (port 62078) listens on Wi-Fi once "Show this iPhone when on Wi-Fi" (EnableWifiConnections) is on.

/// Bonjour service a phone with Wi-Fi connections turned on advertises.
const BONJOUR_SERVICE: &str = "_apple-mobdev2._tcp.local.";
/// How long to wait for a phone before deciding it isn't on the network.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
/// After a failed attempt, how long before trying a phone again. The app asks for devices every 3 seconds.
const RETRY_AFTER: Duration = Duration::from_secs(20);
const BONJOUR_RETRY_AFTER: Duration = Duration::from_secs(60);

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct Meta {
    /// Last IP address the phone was reached at.
    pub ip: Option<IpAddr>,
    pub name: Option<String>,
}

fn dir() -> PathBuf {
    let d = data_dir().join("pairings");
    if !d.exists() {
        let _ = std::fs::create_dir_all(&d);
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
    }
    d
}

fn valid_udid(udid: &str) -> bool {
    !udid.is_empty() && udid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn pairing_path(udid: &str) -> PathBuf {
    dir().join(format!("{udid}.plist"))
}

fn meta_path(udid: &str) -> PathBuf {
    dir().join(format!("{udid}.json"))
}

pub fn has_pairing(udid: &str) -> bool {
    valid_udid(udid) && pairing_path(udid).exists()
}

pub fn save_pairing(udid: &str, pairing: PairingFile) -> Result<()> {
    if !valid_udid(udid) {
        return Err(anyhow!("odd device id {udid:?}"));
    }
    let bytes = pairing.serialize().map_err(|e| anyhow!("can't save the pairing: {e}"))?;
    let path = pairing_path(udid);
    let tmp = path.with_extension("plist.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn load_pairing(udid: &str) -> Option<PairingFile> {
    if !has_pairing(udid) {
        return None;
    }
    PairingFile::read_from_file(pairing_path(udid)).ok()
}

pub fn load_meta(udid: &str) -> Meta {
    std::fs::read(meta_path(udid)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Merges what we just learned about a phone into its saved details.
pub fn update_meta(udid: &str, f: impl FnOnce(&mut Meta)) {
    if !valid_udid(udid) {
        return;
    }
    let before = load_meta(udid);
    let mut meta = before.clone();
    f(&mut meta);
    if serde_json::to_value(&meta).ok() != serde_json::to_value(&before).ok() {
        if let Ok(json) = serde_json::to_vec_pretty(&meta) {
            let _ = std::fs::write(meta_path(udid), json);
        }
    }
}

/// Every phone we have a pairing for.
pub fn saved_udids() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir()) else { return vec![] };
    let mut out: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_suffix(".plist").map(String::from)
        })
        .filter(|u| valid_udid(u))
        .collect();
    out.sort();
    out
}

pub fn forget(udid: &str) -> Result<()> {
    if !valid_udid(udid) {
        return Err(anyhow!("odd device id {udid:?}"));
    }
    for p in [pairing_path(udid), meta_path(udid)] {
        if p.exists() {
            std::fs::remove_file(p)?;
        }
    }
    attempts().lock().unwrap().remove(udid);
    Ok(())
}

// ---------------------------------------------------------------- connecting

pub fn provider(addr: IpAddr, pairing: PairingFile, label: &str) -> TcpProvider {
    TcpProvider { addr, scope_id: None, pairing_file: pairing, label: label.to_string() }
}

#[derive(Default)]
struct Attempt {
    failed_at: Option<Instant>,
    bonjour_at: Option<Instant>,
}

static ATTEMPTS: OnceLock<Mutex<HashMap<String, Attempt>>> = OnceLock::new();

fn attempts() -> &'static Mutex<HashMap<String, Attempt>> {
    ATTEMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Checks the phone answers at `addr` and the saved pairing still works (it stops working if the phone's
/// "Trust This Computer" list was reset).
async fn check(addr: IpAddr, pairing: &PairingFile, label: &str) -> Result<()> {
    let p = provider(addr, pairing.clone(), label);
    tokio::time::timeout(CONNECT_TIMEOUT * 2, async {
        let mut lockdown = tokio::time::timeout(CONNECT_TIMEOUT, LockdownClient::connect(&p))
            .await
            .map_err(|_| anyhow!("no answer from {addr}"))??;
        lockdown.start_session(pairing).await.map_err(|e| anyhow!("the phone at {addr} didn't accept the pairing: {e}"))?;
        Ok::<(), anyhow::Error>(())
    })
    .await
    .map_err(|_| anyhow!("{addr} took too long to answer"))?
}

/// Tries to reach a saved phone directly: its last IP first, then wherever Bonjour says it is now.
/// `eager` skips the wait after a recent failure (used when the user asks for that phone specifically).
pub async fn reach(udid: &str, label: &str, eager: bool) -> Option<(IpAddr, PairingFile)> {
    let pairing = load_pairing(udid)?;
    {
        let a = attempts().lock().unwrap();
        if !eager && a.get(udid).and_then(|a| a.failed_at).is_some_and(|t| t.elapsed() < RETRY_AFTER) {
            return None;
        }
    }
    let meta = load_meta(udid);
    if let Some(ip) = meta.ip.filter(usable) {
        match check(ip, &pairing, label).await {
            Ok(()) => {
                attempts().lock().unwrap().remove(udid);
                return Some((ip, pairing));
            }
            Err(e) => log::debug!("{udid} not at {ip}: {e:#}"),
        }
    }
    // The IP may have changed. Ask Bonjour for every phone with Wi-Fi connections on, at most once a minute,
    // and keep the one that accepts this phone's pairing. (Bonjour names phones by their private Wi-Fi
    // address, which iOS changes per network, so the name can't be matched to a phone.)
    let due = {
        let mut a = attempts().lock().unwrap();
        let entry = a.entry(udid.to_string()).or_default();
        let due = eager || entry.bonjour_at.is_none_or(|t| t.elapsed() >= BONJOUR_RETRY_AFTER);
        if due {
            entry.bonjour_at = Some(Instant::now());
        }
        due
    };
    if due {
        for ip in bonjour_phones().await.into_iter().filter(|ip| Some(*ip) != meta.ip) {
            if check(ip, &pairing, label).await.is_ok() {
                log::info!("found {udid} at {ip} with Bonjour");
                update_meta(udid, |m| m.ip = Some(ip));
                attempts().lock().unwrap().remove(udid);
                return Some((ip, pairing));
            }
        }
    }
    attempts().lock().unwrap().entry(udid.to_string()).or_default().failed_at = Some(Instant::now());
    None
}

/// IPv4 addresses of phones on the local network advertising Wi-Fi connections. Waits up to 6 seconds (phones can
/// be slow to answer), but stops half a second after the first one does.
async fn bonjour_phones() -> Vec<IpAddr> {
    tokio::task::spawn_blocking(move || {
        let Ok(daemon) = mdns_sd::ServiceDaemon::new() else { return vec![] };
        let Ok(rx) = daemon.browse(BONJOUR_SERVICE) else { return vec![] };
        let mut deadline = Instant::now() + Duration::from_secs(6);
        let mut found: Vec<IpAddr> = Vec::new();
        while let Ok(event) = rx.recv_deadline(deadline) {
            if let mdns_sd::ServiceEvent::ServiceResolved(info) = event {
                for a in info.get_addresses_v4() {
                    let ip = IpAddr::V4(a);
                    if usable(&ip) && !found.contains(&ip) {
                        found.push(ip);
                    }
                }
                if !found.is_empty() {
                    deadline = deadline.min(Instant::now() + Duration::from_millis(500));
                }
            }
        }
        let _ = daemon.stop_browse(BONJOUR_SERVICE);
        let _ = daemon.shutdown();
        found
    })
    .await
    .unwrap_or_default()
}

/// Whether an address can be connected to without knowing which network interface it's on. IPv6 link-local
/// addresses (fe80::) can't, and 169.254 ones are usually a USB or peer-to-peer link rather than the Wi-Fi.
pub fn usable(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_link_local() && !v4.is_loopback(),
        IpAddr::V6(v6) => !v6.is_loopback() && (v6.segments()[0] & 0xffc0) != 0xfe80,
    }
}

/// The phone's basic details over a direct connection (no session needed), for the device list.
pub async fn device_values(p: &dyn IdeviceProvider) -> Result<plist::Value> {
    let mut lockdown = tokio::time::timeout(CONNECT_TIMEOUT, LockdownClient::connect(p))
        .await
        .map_err(|_| anyhow!("timed out"))??;
    Ok(lockdown.get_value(None, None).await?)
}
