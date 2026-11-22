// The engine's commands. Each takes the request's JSON and returns the result's JSON.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use idevice::IdeviceService;
use idevice::installation_proxy::InstallationProxyClient;
use idevice::lockdown::LockdownClient;
use idevice::pairing_file::PairingFile;
use idevice::provider::IdeviceProvider;
use idevice::usbmuxd::{Connection, UsbmuxdAddr, UsbmuxdConnection, UsbmuxdDevice};
use plume_core::auth::{Account, TwoFactorAction, TwoFactorMethod, TwoFactorRequest};
use plume_core::developer::DeveloperSession;
use plume_core::{AnisetteConfiguration, CertificateIdentity, SettingsScope, SigningSettings, UnifiedSigner};
use plume_store::AccountStore;
use plume_utils::{Bundle, Package, PlistInfoTrait, Signer, SignerFeatures, SignerInstallMode, SignerMode, SignerOptions, Tweak};
use serde_json::{Value, json};

use crate::{direct, event, ipa};

const LABEL: &str = "Sideport";

// ---------------------------------------------------------------- paths and arguments

/// ~/Library/Application Support/Sideport (or $SIDEPORT_DATA): accounts, keys, icon cache.
pub fn data_dir() -> PathBuf {
    let dir = std::env::var_os("SIDEPORT_DATA").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Library/Application Support/Sideport")
    });
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn accounts_path() -> PathBuf {
    data_dir().join("accounts.json")
}

fn anisette() -> AnisetteConfiguration {
    AnisetteConfiguration::default().set_configuration_path(data_dir().join("anisette"))
}

fn arg<'a>(req: &'a Value, key: &str) -> Result<&'a str> {
    req.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("missing \"{key}\""))
}

fn opt<'a>(req: &'a Value, key: &str) -> Option<&'a str> {
    req.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------- prompts the app answers

static PROMPTS: OnceLock<Mutex<HashMap<String, mpsc::Sender<Value>>>> = OnceLock::new();

fn prompts() -> &'static Mutex<HashMap<String, mpsc::Sender<Value>>> {
    PROMPTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Sends a "prompt" event and blocks until the app answers it with {"cmd": "answer", "prompt": id, ...}.
/// Called from Impactor's synchronous callbacks; the request has its own thread, so blocking it is fine.
fn ask(kind: &str, job: &str, mut details: Value) -> Result<Value> {
    let id = format!("{job}:{kind}:{}", chrono::Utc::now().timestamp_millis());
    let (tx, rx) = mpsc::channel();
    prompts().lock().unwrap().insert(id.clone(), tx);
    if let Value::Object(ref mut m) = details {
        m.insert("prompt".into(), json!(id));
        m.insert("kind".into(), json!(kind));
        m.insert("job".into(), json!(job));
    }
    event("prompt", details);
    let answer = rx.recv_timeout(Duration::from_secs(600));
    prompts().lock().unwrap().remove(&id);
    let answer = answer.map_err(|_| anyhow!("no answer within 10 minutes"))?;
    if answer.get("cancel").and_then(Value::as_bool).unwrap_or(false) {
        bail!("cancelled");
    }
    Ok(answer)
}

pub fn answer(req: &Value) {
    if let Some(id) = req.get("prompt").and_then(Value::as_str) {
        if let Some(tx) = prompts().lock().unwrap().get(id) {
            let _ = tx.send(req.clone());
        }
    }
}

fn progress(job: &str, stage: &str, percent: i32) {
    event("progress", json!({ "job": job, "stage": stage, "percent": percent }));
}

// ---------------------------------------------------------------- dispatch

pub async fn handle(cmd: &str, req: &Value) -> Result<Value> {
    match cmd {
        "ping" => Ok(json!("pong")),
        "inspect" => {
            let path = PathBuf::from(arg(req, "path")?);
            tokio::task::spawn_blocking(move || ipa::inspect(&path, &data_dir().join("icons"))).await?
        }
        "devices" => devices().await,
        "installed_apps" => installed_apps(arg(req, "udid")?).await,
        "uninstall" => uninstall(arg(req, "udid")?, arg(req, "bundle_id")?).await,
        "accounts" => accounts().await,
        "login" => login(req).await,
        "logout" => logout(arg(req, "email")?).await,
        "select_team" => select_team(arg(req, "email")?, arg(req, "team")?).await,
        "teams" => teams(arg(req, "email")?).await,
        "app_ids" => app_ids(req).await,
        "app_id_quota" => app_id_quota(req).await,
        "delete_app_id" => delete_app_id(req).await,
        "certificates" => certificates(req).await,
        "revoke_certificate" => revoke_certificate(req).await,
        "install" => install(req).await,
        "enable_wifi" => enable_wifi(arg(req, "udid")?).await,
        "forget_pairing" => forget_pairing(arg(req, "udid")?).await,
        _ => bail!("unknown command \"{cmd}\""),
    }
}

// ---------------------------------------------------------------- devices

/// How the engine reaches a phone: through usbmuxd (USB, or Wi-Fi when macOS sees it), or straight to its IP
/// with the pairing saved earlier (see direct.rs).
enum Route {
    Mux(UsbmuxdDevice),
    Direct { udid: String, addr: IpAddr, pairing: PairingFile },
}

impl Route {
    fn provider(&self) -> Box<dyn IdeviceProvider> {
        match self {
            Route::Mux(d) => Box::new(d.to_provider(UsbmuxdAddr::default(), LABEL)),
            Route::Direct { addr, pairing, .. } => Box::new(direct::provider(*addr, pairing.clone(), LABEL)),
        }
    }

    fn udid(&self) -> &str {
        match self {
            Route::Mux(d) => &d.udid,
            Route::Direct { udid, .. } => udid,
        }
    }

    /// "usb", "wifi" (through macOS) or "network" (direct).
    fn connection(&self) -> &'static str {
        match self {
            Route::Mux(d) if matches!(d.connection_type, Connection::Usb) => "usb",
            Route::Mux(_) => "wifi",
            Route::Direct { .. } => "network",
        }
    }
}

async fn usbmuxd_devices() -> Result<Vec<UsbmuxdDevice>> {
    let mut mux = UsbmuxdConnection::default().await.context("can't reach usbmuxd (the Mac's iPhone service)")?;
    Ok(mux.get_devices().await?)
}

/// What we've already learned about each phone since the engine started, so the device list (asked for every
/// few seconds) doesn't redo it.
#[derive(Default, Clone)]
struct Known {
    /// The pairing record was saved this run.
    saved: bool,
    /// "Show this iPhone when on Wi-Fi" is on (EnableWifiConnections). Only read over USB.
    wifi_enabled: Option<bool>,
}

static KNOWN: OnceLock<Mutex<HashMap<String, Known>>> = OnceLock::new();

fn known() -> &'static Mutex<HashMap<String, Known>> {
    KNOWN.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Saves what's needed to reach a phone directly later: its pairing record, Wi-Fi MAC address and IP.
/// Best effort; a failure here only means the direct route won't be available.
async fn remember(d: &UsbmuxdDevice, values: &plist::Value) {
    let udid = d.udid.clone();
    let dict = values.as_dictionary();
    let text = |k: &str| dict.and_then(|d| d.get(k)).and_then(plist::Value::as_string).map(String::from);
    let wifi_mac = text("WiFiAddress");
    let name = text("DeviceName");
    let ip = match d.connection_type {
        Connection::Network(ip) => Some(ip),
        _ => None,
    };
    direct::update_meta(&udid, |m| {
        if wifi_mac.is_some() {
            m.wifi_mac = wifi_mac;
        }
        if ip.is_some() {
            m.ip = ip;
        }
        if name.is_some() {
            m.name = name;
        }
    });

    let state = known().lock().unwrap().get(&udid).cloned().unwrap_or_default();
    if !state.saved {
        match save_pairing_from_mux(&udid).await {
            Ok(()) => known().lock().unwrap().entry(udid.clone()).or_default().saved = true,
            Err(e) => log::warn!("couldn't save the pairing for {udid}: {e:#}"),
        }
    }
    if state.wifi_enabled.is_none() && matches!(d.connection_type, Connection::Usb) {
        if let Ok(on) = wifi_connections_enabled(d).await {
            known().lock().unwrap().entry(udid).or_default().wifi_enabled = Some(on);
        }
    }
}

async fn save_pairing_from_mux(udid: &str) -> Result<()> {
    let mut mux = UsbmuxdConnection::default().await?;
    let pairing = mux.get_pair_record(udid).await?;
    direct::save_pairing(udid, pairing)
}

/// A lockdown session over usbmuxd, needed to read or change the wireless settings.
async fn mux_session(d: &UsbmuxdDevice) -> Result<LockdownClient> {
    let p = d.to_provider(UsbmuxdAddr::default(), LABEL);
    let mut lockdown = LockdownClient::connect(&p).await?;
    lockdown.start_session(&p.get_pairing_file().await?).await?;
    Ok(lockdown)
}

async fn wifi_connections_enabled(d: &UsbmuxdDevice) -> Result<bool> {
    let mut lockdown = mux_session(d).await?;
    let v = lockdown.get_value(Some("EnableWifiConnections"), Some(WIRELESS_DOMAIN)).await?;
    Ok(v.as_boolean().unwrap_or(false))
}

const WIRELESS_DOMAIN: &str = "com.apple.mobile.wireless_lockdown";

/// Turns on "Show this iPhone when on Wi-Fi" for a phone plugged in by USB, so it can be used without the cable.
async fn enable_wifi(udid: &str) -> Result<Value> {
    let all = usbmuxd_devices().await?;
    let d = all
        .iter()
        .find(|d| d.udid == udid && matches!(d.connection_type, Connection::Usb))
        .ok_or_else(|| anyhow!("plug the phone in with a cable to turn on Wi-Fi"))?;
    let mut lockdown = mux_session(d).await?;
    lockdown.set_value("EnableWifiConnections", plist::Value::Boolean(true), Some(WIRELESS_DOMAIN)).await?;
    let on = lockdown
        .get_value(Some("EnableWifiConnections"), Some(WIRELESS_DOMAIN))
        .await
        .ok()
        .and_then(|v| v.as_boolean())
        .unwrap_or(false);
    if !on {
        bail!("the phone didn't turn on Wi-Fi connections");
    }
    save_pairing_from_mux(udid).await.context("Wi-Fi is on, but the pairing couldn't be saved")?;
    let mut k = known().lock().unwrap();
    let e = k.entry(udid.to_string()).or_default();
    e.wifi_enabled = Some(true);
    e.saved = true;
    Ok(json!({ "wifi_enabled": true }))
}

async fn forget_pairing(udid: &str) -> Result<Value> {
    direct::forget(udid)?;
    known().lock().unwrap().remove(udid);
    Ok(Value::Null)
}

fn device_json(udid: &str, values: &plist::Value, connection: &str) -> Value {
    let dict = values.as_dictionary();
    let text = |k: &str| dict.and_then(|d| d.get(k)).and_then(plist::Value::as_string).unwrap_or("").to_string();
    let name = text("DeviceName");
    let k = known().lock().unwrap().get(udid).cloned().unwrap_or_default();
    json!({
        "udid": udid,
        "name": if name.is_empty() { "iPhone".to_string() } else { name },
        "model": text("ProductType"),
        "ios": text("ProductVersion"),
        "class": text("DeviceClass"),
        "connection": connection,
        "wifi_enabled": k.wifi_enabled,
        "paired": direct::has_pairing(udid),
    })
}

/// One entry per device; a phone seen over both USB and Wi-Fi is listed once, as USB. Phones macOS can't see
/// but that answer at their saved address are listed with connection "network".
async fn devices() -> Result<Value> {
    let mut by_udid: HashMap<String, Value> = HashMap::new();
    let mux = usbmuxd_devices().await;
    let saved = direct::saved_udids();
    if let Err(e) = &mux {
        if saved.is_empty() {
            bail!("{e:#}");
        }
        log::warn!("{e:#}");
    }
    for d in mux.unwrap_or_default() {
        let wifi = matches!(d.connection_type, Connection::Network(_));
        if wifi && by_udid.get(&d.udid).is_some_and(|v| v["connection"] == "usb") {
            continue;
        }
        let route = Route::Mux(d.clone());
        let values = device_values(&*route.provider()).await.unwrap_or(plist::Value::Dictionary(Default::default()));
        remember(&d, &values).await;
        by_udid.insert(d.udid.clone(), device_json(&d.udid, &values, route.connection()));
    }
    for udid in saved {
        if by_udid.contains_key(&udid) {
            continue;
        }
        if let Some((addr, pairing)) = direct::reach(&udid, LABEL, false).await {
            let route = Route::Direct { udid: udid.clone(), addr, pairing };
            if let Ok(values) = direct::device_values(&*route.provider()).await {
                by_udid.insert(udid.clone(), device_json(&udid, &values, route.connection()));
            }
        }
    }
    let mut list: Vec<Value> = by_udid.into_values().collect();
    list.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(json!(list))
}

async fn device_values(p: &dyn IdeviceProvider) -> Result<plist::Value> {
    let mut lockdown = LockdownClient::connect(p).await?;
    Ok(lockdown.get_value(None, None).await?)
}

async fn device_name(p: &dyn IdeviceProvider) -> String {
    device_values(p)
        .await
        .ok()
        .and_then(|v| v.as_dictionary().and_then(|d| d.get("DeviceName")).and_then(plist::Value::as_string).map(String::from))
        .unwrap_or_else(|| "iPhone".into())
}

/// The best way to reach a phone right now: USB, then Wi-Fi through macOS, then directly.
async fn find_device(udid: &str) -> Result<Route> {
    let all = usbmuxd_devices().await.unwrap_or_default();
    // Prefer USB: it's faster for big installs.
    if let Some(d) = all
        .iter()
        .find(|d| d.udid == udid && matches!(d.connection_type, Connection::Usb))
        .or_else(|| all.iter().find(|d| d.udid == udid))
    {
        return Ok(Route::Mux(d.clone()));
    }
    if let Some((addr, pairing)) = direct::reach(udid, LABEL, true).await {
        return Ok(Route::Direct { udid: udid.to_string(), addr, pairing });
    }
    Err(anyhow!(
        "the device isn't connected (plug it in, or make sure it's on the same Wi-Fi with Wi-Fi turned on in Sideport's Devices page)"
    ))
}

async fn installed_apps(udid: &str) -> Result<Value> {
    let d = find_device(udid).await?;
    let mut proxy = InstallationProxyClient::connect(&*d.provider()).await?;
    let apps = proxy.get_apps(Some("User"), None).await?;
    let mut list = Vec::new();
    for (bundle_id, info) in apps {
        let dict = info.as_dictionary();
        let text = |k: &str| dict.and_then(|d| d.get(k)).and_then(plist::Value::as_string).unwrap_or("").to_string();
        list.push(json!({
            "bundle_id": bundle_id,
            "name": if text("CFBundleDisplayName").is_empty() { text("CFBundleName") } else { text("CFBundleDisplayName") },
            "version": text("CFBundleShortVersionString"),
            "signer": text("SignerIdentity"),
        }));
    }
    list.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(json!(list))
}

async fn uninstall(udid: &str, bundle_id: &str) -> Result<Value> {
    let d = find_device(udid).await?;
    let mut proxy = InstallationProxyClient::connect(&*d.provider()).await?;
    proxy.uninstall(bundle_id, None).await?;
    Ok(Value::Null)
}

// ---------------------------------------------------------------- accounts

async fn store() -> Result<AccountStore> {
    Ok(AccountStore::load(&Some(accounts_path())).await?)
}

async fn accounts() -> Result<Value> {
    let store = store().await?;
    let selected = store.selected_account().map(|a| a.email().clone());
    let mut list: Vec<Value> = store
        .accounts()
        .values()
        .map(|a| {
            json!({
                "email": a.email(),
                "first_name": a.first_name(),
                "team": a.team_id(),
                "selected": Some(a.email()) == selected.as_ref(),
            })
        })
        .collect();
    list.sort_by(|a, b| a["email"].as_str().cmp(&b["email"].as_str()));
    Ok(json!(list))
}

async fn login(req: &Value) -> Result<Value> {
    let email = arg(req, "email")?.to_string();
    let password = arg(req, "password")?.to_string();
    let job = opt(req, "job").unwrap_or("login").to_string();

    let creds = {
        let (email, password) = (email.clone(), password.clone());
        move || Ok((email.clone(), password.clone()))
    };
    let two_factor = {
        let job = job.clone();
        move |request: TwoFactorRequest| -> std::result::Result<TwoFactorAction, String> {
            let phones: Vec<Value> = request
                .trusted_phone_numbers
                .iter()
                .map(|p| json!({ "id": p.id, "number": p.number_with_dial_code, "last_two": p.last_two_digits }))
                .collect();
            let method = if request.method == TwoFactorMethod::Sms { "sms" } else { "device" };
            let answer = ask("two_factor", &job, json!({ "method": method, "phones": phones })).map_err(|e| e.to_string())?;
            if let Some(phone) = answer.get("sms_phone").and_then(Value::as_u64) {
                return Ok(TwoFactorAction::SendSms(phone as u32));
            }
            let code = answer.get("code").and_then(Value::as_str).unwrap_or("").trim().to_string();
            Ok(TwoFactorAction::SubmitCode(code))
        }
    };

    progress(&job, "Signing in to Apple", 10);
    let account = Account::login(creds, two_factor, anisette()).await.map_err(|e| anyhow!("Apple sign-in failed: {e}"))?;
    let mut store = store().await?;
    store.accounts_add_from_session(email.clone(), account).await?;
    store.account_select(&email).await?;

    // A free account has one team; pick it now so installs don't have to ask.
    if let Ok(session) = session(&email).await {
        if let Ok(teams) = session.qh_list_teams().await {
            if let Some(team) = teams.teams.first() {
                store.update_account_team(&email, team.team_id.clone()).await?;
            }
        }
    }
    accounts().await
}

async fn logout(email: &str) -> Result<Value> {
    let mut store = store().await?;
    store.accounts_remove(email).await?;
    accounts().await
}

async fn select_team(email: &str, team: &str) -> Result<Value> {
    let mut store = store().await?;
    store.update_account_team(email, team.to_string()).await?;
    accounts().await
}

async fn session(email: &str) -> Result<DeveloperSession> {
    let store = store().await?;
    let account = store.get_account(email).ok_or_else(|| anyhow!("{email} isn't signed in"))?.clone();
    DeveloperSession::new(account.adsid().clone(), account.xcode_gs_token().clone(), anisette())
        .await
        .map_err(|e| anyhow!("Apple session for {email} expired or failed ({e}); sign in again"))
}

async fn team_for(email: &str, session: &DeveloperSession, requested: Option<&str>) -> Result<String> {
    if let Some(t) = requested {
        return Ok(t.to_string());
    }
    let store = store().await?;
    let saved = store.get_account(email).map(|a| a.team_id().clone()).unwrap_or_default();
    if !saved.is_empty() {
        return Ok(saved);
    }
    let teams = session.qh_list_teams().await?;
    teams.teams.first().map(|t| t.team_id.clone()).ok_or_else(|| anyhow!("this Apple ID has no developer team"))
}

async fn teams(email: &str) -> Result<Value> {
    let s = session(email).await?;
    let teams = s.qh_list_teams().await?;
    Ok(json!(teams.teams.iter().map(|t| json!({ "id": t.team_id, "name": t.name, "type": t._type, "status": t.status })).collect::<Vec<_>>()))
}

async fn app_ids(req: &Value) -> Result<Value> {
    let email = arg(req, "email")?;
    let s = session(email).await?;
    let team = team_for(email, &s, opt(req, "team")).await?;
    let ids = s.qh_list_app_ids(&team).await?;
    Ok(json!(ids.app_ids.iter().map(|a| json!({ "id": a.app_id_id, "identifier": a.identifier })).collect::<Vec<_>>()))
}

/// How many App IDs the account has, how many more it can make right now, and when the next one comes back.
/// Apple sends the limits for free accounts only (10 a week); a paid account gets `null` for them.
async fn app_id_quota(req: &Value) -> Result<Value> {
    let email = arg(req, "email")?;
    let s = session(email).await?;
    let team = team_for(email, &s, opt(req, "team")).await?;
    let mut body = plist::Dictionary::new();
    body.insert("teamId".to_string(), plist::Value::String(team));
    let r = s.qh_send_request("https://developerservices2.apple.com/services/QH65B2/ios/listAppIds.action", Some(body)).await?;
    let int = |key: &str| r.get(key).and_then(|v| v.as_signed_integer().or_else(|| v.as_unsigned_integer().map(|u| u as i64)));
    let count = r.get("appIds").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    // Each App ID of a free account counts against the limit until its expirationDate (7 days after it was made).
    let next_free = r.get("appIds").and_then(|v| v.as_array()).into_iter().flatten()
        .filter_map(|a| a.as_dictionary()?.get("expirationDate")?.as_date())
        .map(|d| d.to_xml_format())
        .min();
    Ok(json!({ "count": count, "available": int("availableQuantity"), "max": int("maxQuantity"), "next_free": next_free }))
}

async fn delete_app_id(req: &Value) -> Result<Value> {
    let email = arg(req, "email")?;
    let s = session(email).await?;
    let team = team_for(email, &s, opt(req, "team")).await?;
    s.qh_delete_app_id(&team, &arg(req, "app_id")?.to_string()).await?;
    Ok(Value::Null)
}

async fn certificates(req: &Value) -> Result<Value> {
    let email = arg(req, "email")?;
    let s = session(email).await?;
    let team = team_for(email, &s, opt(req, "team")).await?;
    let certs = s.qh_list_certs(&team).await?;
    Ok(json!(certs
        .certificates
        .iter()
        .map(|c| json!({
            "serial": c.serial_number,
            "name": c.name,
            "status": c.status,
            "machine": c.machine_name,
            "expires": c.expiration_date.to_xml_format(),
        }))
        .collect::<Vec<_>>()))
}

async fn revoke_certificate(req: &Value) -> Result<Value> {
    let email = arg(req, "email")?;
    let s = session(email).await?;
    let team = team_for(email, &s, opt(req, "team")).await?;
    s.qh_revoke_cert(&team, &arg(req, "serial")?.to_string()).await?;
    Ok(Value::Null)
}

// ---------------------------------------------------------------- install

/// Sideloadly keeps its certificate and key in ~/Library/Application Support/sideloadly. Signing with the
/// same certificate means Apple never has to revoke it, so apps Sideloadly installed keep opening.
fn sideloadly_identity_files(email: &str) -> Option<Vec<PathBuf>> {
    let dir = PathBuf::from(std::env::var("HOME").ok()?).join("Library/Application Support/sideloadly");
    let cert = dir.join(format!("cert-{email}.pem"));
    let key = dir.join("key.pem");
    (cert.exists() && key.exists()).then(|| vec![cert, key])
}

async fn identity(job: &str, email: &str, session: &DeveloperSession, team: &String, req: &Value) -> Result<(CertificateIdentity, &'static str)> {
    let mode = opt(req, "certificate").unwrap_or("auto");
    if mode != "own" {
        if let Some(files) = sideloadly_identity_files(email) {
            let identity = CertificateIdentity::new_with_paths(Some(files)).await?;
            // Only use it while Apple still lists it as valid for this team.
            let serial = identity.serial_number.clone().unwrap_or_default().to_uppercase();
            let certs = session.qh_list_certs(team).await?;
            if certs.certificates.iter().any(|c| c.serial_number.to_uppercase() == serial) {
                return Ok((identity, "sideloadly"));
            }
            log::warn!("Sideloadly's certificate ({serial}) is no longer on the account; using Sideport's own");
        }
    }
    let job = job.to_string();
    let mut on_reset = move || -> bool {
        ask(
            "revoke_certificate",
            &job,
            json!({ "message": "This Apple ID already has the most signing certificates it's allowed. Revoke the oldest to make a new one? Apps signed with it will stop opening until they're refreshed." }),
        )
        .map(|a| a.get("confirm").and_then(Value::as_bool).unwrap_or(false))
        .unwrap_or(false)
    };
    let identity = CertificateIdentity::new_with_session(session, data_dir(), Some(LABEL.to_string()), team, false, Some(&mut on_reset)).await?;
    Ok((identity, "own"))
}

fn features(o: &Value) -> SignerFeatures {
    let f = |k: &str| o.get(k).and_then(Value::as_bool).unwrap_or(false);
    SignerFeatures {
        support_minimum_os_version: f("lower_min_os"),
        support_file_sharing: f("file_sharing"),
        support_ipad_fullscreen: f("ipad_fullscreen"),
        support_game_mode: f("game_mode"),
        support_pro_motion: f("pro_motion"),
        support_liquid_glass: f("liquid_glass"),
        // Sideport injects the tweak loader itself (inject_tweaks), so the user can pick which one.
        support_ellekit: false,
        remove_url_schemes: f("remove_url_schemes"),
    }
}

/// Injects the tweak loader the user picked, then the tweaks. Impactor would always use ElleKit; doing it here
/// lets "tweak_framework" choose: "ellekit" (bundled), "substrate" (your own CydiaSubstrate.framework, at
/// "substrate_path") or "none" (tweaks go in as they are, for ones that bring their own hooking).
async fn inject_tweaks(bundle: &Bundle, o: &Value) -> Result<()> {
    let tweaks: Vec<PathBuf> =
        o.get("tweaks").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(PathBuf::from).collect()).unwrap_or_default();
    let always_loader = o.get("ellekit").and_then(Value::as_bool).unwrap_or(false);
    if tweaks.is_empty() && !always_loader {
        return Ok(());
    }
    match opt(o, "tweak_framework").unwrap_or("ellekit") {
        "ellekit" => Tweak::install_ellekit(bundle).await.context("couldn't add ElleKit")?,
        "substrate" => {
            let path = PathBuf::from(
                opt(o, "substrate_path").ok_or_else(|| anyhow!("choose your CydiaSubstrate.framework under Tweaks, or pick another tweak loader"))?,
            );
            if path.file_name().and_then(|n| n.to_str()) != Some("CydiaSubstrate.framework") || !path.join("CydiaSubstrate").is_file() {
                bail!("{} isn't a CydiaSubstrate.framework", path.display());
            }
            // Goes in as Frameworks/CydiaSubstrate.framework, which is where tweaks are pointed to look for it.
            Tweak::new(&path, bundle).await?.apply().await.context("couldn't add Cydia Substrate")?;
        }
        "none" => {}
        other => bail!("unknown tweak loader \"{other}\""),
    }
    for t in &tweaks {
        Tweak::new(t, bundle).await?.apply().await.with_context(|| format!("couldn't inject {}", t.display()))?;
    }
    Ok(())
}

/// Adds get-task-allow to the app's own executable, so debuggers can attach and emulators can use JIT.
/// Used for ad-hoc signing only: with an Apple ID, the development profile already grants it. Impactor's ad-hoc
/// signing keeps the entitlements a binary already has, so signing the executable with them first is enough.
fn add_get_task_allow(bundle: &Bundle) -> Result<()> {
    let exe = bundle.get_executable().ok_or_else(|| anyhow!("the app has no executable"))?;
    let path = bundle.bundle_dir().join(exe);
    let mut entitlements = plume_core::MachO::new(&path)?.entitlements().clone().unwrap_or_default();
    entitlements.insert("get-task-allow".into(), plist::Value::Boolean(true));
    let mut xml = Vec::new();
    plist::Value::Dictionary(entitlements).to_writer_xml(&mut xml)?;
    let mut settings = SigningSettings::default();
    settings.set_entitlements_xml(SettingsScope::Main, String::from_utf8(xml)?)?;
    UnifiedSigner::new(settings).sign_path_in_place(&path)?;
    Ok(())
}

/// Changes Impactor doesn't make itself. Done before App IDs are registered, so removed extensions don't use
/// up any of a free account's 10 App IDs.
fn extra_changes(app: &Path, o: &Value) -> Result<()> {
    let f = |k: &str| o.get(k).and_then(Value::as_bool).unwrap_or(false);
    let info_path = app.join("Info.plist");
    let mut info = plist::Value::from_file(&info_path)?;
    let dict = info.as_dictionary_mut().ok_or_else(|| anyhow!("Info.plist isn't a dictionary"))?;
    let mut changed = false;
    if f("remove_device_limits") && dict.remove("UISupportedDevices").is_some() {
        changed = true;
    }
    if let Some(min) = o.get("min_os").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        dict.insert("MinimumOSVersion".into(), plist::Value::String(min.into()));
        changed = true;
    }
    if changed {
        info.to_file_binary(&info_path)?;
    }
    if f("remove_extensions") {
        for dir in ["PlugIns", "Extensions"] {
            let p = app.join(dir);
            if p.exists() {
                std::fs::remove_dir_all(&p)?;
            }
        }
    }
    if f("remove_watch") {
        let p = app.join("Watch");
        if p.exists() {
            std::fs::remove_dir_all(&p)?;
        }
    }
    // Swap files inside the app (Sideloadly's "replace files"): {"relative/path": "/local/file"}.
    if let Some(map) = o.get("replace_files").and_then(Value::as_object) {
        for (rel, src) in map {
            let Some(src) = src.as_str() else { continue };
            if rel.split('/').any(|p| p == "..") {
                bail!("can't replace {rel}: it points outside the app");
            }
            let dest = app.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(src, &dest).with_context(|| format!("can't copy {src} into the app"))?;
        }
    }
    Ok(())
}

async fn install(req: &Value) -> Result<Value> {
    let job = arg(req, "job")?.to_string();
    let ipa_path = PathBuf::from(arg(req, "ipa")?);
    let o = req.get("options").cloned().unwrap_or(json!({}));
    let install_to = opt(req, "udid").map(String::from);
    let export_to = opt(req, "export").map(PathBuf::from);
    let signing = opt(req, "signing").unwrap_or("apple_id");
    if install_to.is_none() && export_to.is_none() {
        bail!("nothing to do: give a device to install to, or a path to export to");
    }

    progress(&job, "Opening the IPA", 3);
    let device = match &install_to {
        Some(udid) => {
            let route = find_device(udid).await?;
            let name = device_name(&*route.provider()).await;
            Some((route, name))
        }
        None => None,
    };
    let package = {
        let p = ipa_path.clone();
        tokio::task::spawn_blocking(move || Package::new(p)).await??
    };
    progress(&job, "Unpacking", 8);
    let bundle = {
        let pkg = package.clone();
        tokio::task::spawn_blocking(move || pkg.get_package_bundle()).await??
    };

    let mut options = SignerOptions {
        custom_name: opt(&o, "name").map(String::from),
        custom_identifier: opt(&o, "bundle_id").map(String::from),
        custom_version: opt(&o, "version").map(String::from),
        custom_icon: opt(&o, "icon").map(PathBuf::from),
        features: features(&o),
        // Injected by inject_tweaks instead, with the loader the user chose.
        tweaks: None,
        install_mode: if install_to.is_some() { SignerInstallMode::Install } else { SignerInstallMode::Export },
        ..Default::default()
    };
    // "Automatic bundle ID" off: keep the app's own ID instead of adding the team ID to it.
    let auto_bundle_id = o.get("auto_bundle_id").and_then(Value::as_bool).unwrap_or(true);
    if !auto_bundle_id && options.custom_identifier.is_none() {
        options.custom_identifier = bundle.get_bundle_identifier();
    }
    let debugging = o.get("debugging").and_then(Value::as_bool).unwrap_or(false);

    let mut result = json!({});
    let mut expires: Option<String> = None;
    let mut certificate = "";
    match signing {
        "apple_id" => {
            let email = arg(req, "email")?;
            options.mode = SignerMode::Pem;
            progress(&job, "Checking the Apple ID", 14);
            let session = session(email).await?;
            let team = team_for(email, &session, opt(req, "team")).await?;
            progress(&job, "Getting the signing certificate", 22);
            let (identity, which) = identity(&job, email, &session, &team, req).await?;
            certificate = which;
            if let Some((route, name)) = &device {
                progress(&job, "Registering the device", 30);
                session.qh_ensure_device(&team, name, &route.udid().to_string()).await?;
            }
            let mut signer = Signer::new(Some(identity), options);
            progress(&job, "Customising the app", 36);
            signer.modify_bundle(&bundle, &Some(team.clone())).await?;
            inject_tweaks(&bundle, &o).await?;
            extra_changes(bundle.bundle_dir(), &o)?;
            progress(&job, "Registering App IDs", 44);
            signer.register_bundle(&bundle, &session, &team, false).await?;
            progress(&job, "Signing", 55);
            signer.sign_bundle(&bundle).await?;
            expires = signer.provisioning_files.first().map(|p| p.expiration_date().to_xml_format());
            result["bundle_id"] = json!(signer.options.custom_identifier);
            result["team"] = json!(team);
        }
        "certificate" => {
            // A .p12/.pem certificate and .mobileprovision of your own (paid developer accounts).
            options.mode = SignerMode::Pem;
            let files: Vec<PathBuf> = req.get("cert_files").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(PathBuf::from).collect()).unwrap_or_default();
            let identity = CertificateIdentity::new_with_paths(Some(files)).await?;
            let mut signer = Signer::new(Some(identity), options);
            if let Some(p) = opt(req, "profile") {
                signer.provisioning_files.push(plume_core::MobileProvision::load_with_path(&PathBuf::from(p))?);
            }
            // Debugging comes from the profile here (development profiles allow it), since the entitlements have to
            // match it; "debugging" is only acted on for ad-hoc signing.
            progress(&job, "Customising the app", 36);
            signer.modify_bundle(&bundle, &None).await?;
            inject_tweaks(&bundle, &o).await?;
            extra_changes(bundle.bundle_dir(), &o)?;
            progress(&job, "Signing", 55);
            signer.sign_bundle(&bundle).await?;
            expires = signer.provisioning_files.first().map(|p| p.expiration_date().to_xml_format());
            result["bundle_id"] = json!(signer.options.custom_identifier);
        }
        "adhoc" => {
            options.mode = SignerMode::Adhoc;
            let mut signer = Signer::new(None, options);
            signer.modify_bundle(&bundle, &None).await?;
            inject_tweaks(&bundle, &o).await?;
            extra_changes(bundle.bundle_dir(), &o)?;
            if debugging {
                add_get_task_allow(&bundle).context("couldn't turn on debugging")?;
            }
            progress(&job, "Signing", 55);
            signer.sign_bundle(&bundle).await?;
        }
        other => bail!("unknown signing mode \"{other}\""),
    }

    if let Some((route, _)) = &device {
        progress(&job, if route.connection() == "usb" { "Sending to the device" } else { "Sending to the device over Wi-Fi" }, 60);
        let job2 = job.clone();
        idevice::utils::installation::install_package_with_callback(
            &*route.provider(),
            bundle.bundle_dir(),
            None,
            move |(p, _): (u64, ())| {
                let job = job2.clone();
                async move { progress(&job, "Installing", 60 + p as i32 * 38 / 100) }
            },
            (),
        )
        .await
        .map_err(|e| anyhow!("install failed: {e}"))?;
    }
    if let Some(out) = &export_to {
        progress(&job, "Saving the signed IPA", 95);
        let archived = package.get_archive_based_on_path(bundle.bundle_dir())?;
        tokio::fs::copy(&archived, out).await?;
        result["exported"] = json!(out);
    }

    let info = plist::Value::from_file(bundle.bundle_dir().join("Info.plist")).ok();
    let text = |k: &str| info.as_ref().and_then(|v| v.as_dictionary()).and_then(|d| d.get(k)).and_then(plist::Value::as_string).map(String::from);
    if result["bundle_id"].is_null() {
        result["bundle_id"] = json!(text("CFBundleIdentifier"));
    }
    result["name"] = json!(text("CFBundleDisplayName").or_else(|| text("CFBundleName")));
    result["version"] = json!(text("CFBundleShortVersionString"));
    result["expires"] = json!(expires);
    result["certificate"] = json!(certificate);

    package.remove_package_stage();
    progress(&job, "Done", 100);
    Ok(result)
}
