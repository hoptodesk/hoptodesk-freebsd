use hbb_common::{
    allow_err,
    config::Config,
    log,
    tokio::{self, select, time::Duration},
    ResultType,
};
use sha2::{Sha256, Digest};
use std::sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Mutex};

const DASHBOARD_API_URL: &str = "https://dashboard.hoptodesk.com/api";
const DASHBOARD_WS_URL: &str = "wss://dashboard.hoptodesk.com/socket.io/";
const HEARTBEAT_INTERVAL_SECS: u64 = 30;
const RECONNECT_DELAY_BASE_SECS: u64 = 1;
const RECONNECT_DELAY_MAX_SECS: u64 = 10;
const WS_SEND_TIMEOUT_SECS: u64 = 15;

static DASHBOARD_RUNNING: AtomicBool = AtomicBool::new(false);
static IN_SESSION: AtomicBool = AtomicBool::new(false);
static SESSION_META: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));
static TICKET_REPLY_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn is_linked() -> bool {
    !Config::get_option("dashboard_user_id").is_empty()
}

pub fn set_pending_quick_connect_token(token: &str) {
    Config::set_option("pending_quick_connect_token".to_owned(), token.to_owned());
}

pub fn has_pending_quick_connect_token() -> bool {
    !Config::get_option("pending_quick_connect_token").is_empty()
}

pub fn take_pending_quick_connect_token() -> String {
    let token = Config::get_option("pending_quick_connect_token");
    if !token.is_empty() {
        Config::set_option("pending_quick_connect_token".to_owned(), String::new());
    }
    token
}

pub fn get_invite_code() -> String {
    let code = Config::get_option("invite_code");
    if !code.is_empty() {
        return code;
    }
    if let Ok(code) = std::fs::read_to_string(Config::path("InviteCode.toml")) {
        let code = code.trim().to_string();
        if !code.is_empty() {
            return code;
        }
    }
    if let Ok(code) = std::fs::read_to_string(Config::shared_path("InviteCode.toml")) {
        let code = code.trim().to_string();
        if !code.is_empty() {
            return code;
        }
    }
    String::new()
}

pub fn get_dashboard_user_id() -> String {
    Config::get_option("dashboard_user_id")
}

fn dashboard_ws_url() -> String {
    let v = Config::get_option("dashboard-ws-url");
    if v.is_empty() { DASHBOARD_WS_URL.to_string() } else { v }
}

fn dashboard_api_base() -> String {
    let v = Config::get_option("dashboard-api-url");
    if v.is_empty() { DASHBOARD_API_URL.to_string() } else { v }
}

fn apply_dashboard_endpoint(key: &str, field: &serde_json::Value, scheme: &str, path: &str) {
    let desired = match (field["host"].as_str(), field["port"].as_str()) {
        (Some(host), Some(port)) => format!("{}://{}:{}{}", scheme, host, port, path),
        _ => String::new(),
    };
    if Config::get_option(key) != desired {
        Config::set_option(key.to_owned(), desired);
    }
}

async fn refresh_dashboard_endpoints() {
    let map = match hbb_common::api::call_api().await {
        Ok(m) => m,
        Err(_) => return,
    };
    apply_dashboard_endpoint("dashboard-ws-url", &map["dashboardws"], "wss", "/socket.io/");
    apply_dashboard_endpoint("dashboard-api-url", &map["dashboardapi"], "https", "/api");
}

pub async fn validate_invite(invite_code: &str) -> ResultType<(String, String, String, String)> {
    refresh_dashboard_endpoints().await;
    let client = crate::common::make_http_client();
    let url = format!(
        "{}?action=validateInvite&invite_code={}",
        dashboard_api_base(), invite_code
    );
    let resp: serde_json::Value = client.get(&url).send().await?.json().await?;
    if resp["success"].as_bool() != Some(true) {
        hbb_common::bail!("validateInvite failed: {}", resp);
    }
    let invite = &resp["invite"];
    let enrollment_token = invite["enrollment_token"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let dashboard_user_id = invite["dashboard_user_id"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let invite_type = invite["invite_type"].as_str().unwrap_or("standard").to_string();
    let account_name = invite["account_name"]
        .as_str()
        .unwrap_or("")
        .to_string();
    if dashboard_user_id.is_empty() || dashboard_user_id.starts_with("DASH-") {
        hbb_common::bail!("Invalid dashboard_user_id: {}", dashboard_user_id);
    }
    Ok((enrollment_token, dashboard_user_id, invite_type, account_name))
}

pub async fn get_share_invite() -> ResultType<String> {
    refresh_dashboard_endpoints().await;
    let cached = Config::get_option("share_invite_code");
    if !cached.is_empty() {
        return Ok(cached);
    }
    let dashboard_user_id = get_dashboard_user_id();
    if dashboard_user_id.is_empty() {
        hbb_common::bail!("Not linked to a dashboard");
    }
    let device_id = Config::get_id();
    if device_id.is_empty() {
        hbb_common::bail!("Device ID not available");
    }
    let client = crate::common::make_http_client();
    let url = format!("{}?action=getShareInvite", dashboard_api_base());
    let params: Vec<(&str, &str)> = vec![
        ("device_id", &device_id),
        ("dashboard_user_id", &dashboard_user_id),
    ];
    let resp: serde_json::Value = client.post(&url).form(&params).send().await?.json().await?;
    if resp["success"].as_bool() != Some(true) {
        hbb_common::bail!("getShareInvite failed: {}", resp);
    }
    let code = resp["invite_code"].as_str().unwrap_or("").to_string();
    if code.is_empty() {
        hbb_common::bail!("getShareInvite returned empty code");
    }
    Config::set_option("share_invite_code".to_owned(), code.clone());
    Ok(code)
}

pub async fn register_device(
    enrollment_token: &str,
    invite_code: &str,
    device_id: &str,
    device_name: &str,
    os_name: &str,
    mac: &str,
) -> ResultType<String> {
    let client = crate::common::make_http_client();
    let url = format!("{}?action=registerDevice", dashboard_api_base());
    let mut params: Vec<(&str, &str)> = vec![
        ("device_id", device_id),
        ("device_name", device_name),
        ("computer_name", device_name),
        ("os", os_name),
        ("mac_address", mac),
    ];
    if !enrollment_token.is_empty() {
        params.push(("enrollment_token", enrollment_token));
    }
    if !invite_code.is_empty() {
        params.push(("invite_code", invite_code));
    }
    let resp: serde_json::Value = client.post(&url).form(&params).send().await?.json().await?;
    if resp["success"].as_bool() != Some(true) {
        hbb_common::bail!("registerDevice failed: {}", resp);
    }
    let dashboard_user_id = resp["dashboard_user_id"]
        .as_str()
        .unwrap_or("")
        .to_string();
    log::info!("Device registered with dashboard successfully (user_id={})", dashboard_user_id);

    // Apply deployment profile settings if provided
    if let Some(settings) = resp.get("deployment_settings") {
        if settings["enable_unattended_access"].as_bool() == Some(true) {
            Config::set_option("unattended-access".to_owned(), "true".to_owned());
            log::info!("Deployment profile: enabled unattended access");
        }
        if let Some(password) = settings["default_password"].as_str() {
            if !password.is_empty() {
                Config::set_permanent_password(password);
                log::info!("Deployment profile: set default password");
            }
        }
    }

    Ok(dashboard_user_id)
}

pub async fn get_network_settings(invite_code: &str) -> ResultType<()> {
    let client = crate::common::make_http_client();
    let url = format!(
        "{}?action=getNetworkSettingsByInvite&invite_code={}",
        dashboard_api_base(), invite_code
    );
    let resp: serde_json::Value = client.get(&url).send().await?.json().await?;
    if resp["success"].as_bool() != Some(true) {
        log::warn!("getNetworkSettingsByInvite: not successful or not available");
        return Ok(());
    }
    let network_type = resp["network_type"].as_str().unwrap_or("hoptodesk");
    if network_type == "custom" {
        let api_json = if resp["api_json"].is_object() {
            resp["api_json"].clone()
        } else {
            let mut api_map = serde_json::Map::new();
            api_map.insert("none".to_owned(), serde_json::json!("none"));
            let turn_host = resp["turn_host"].as_str().unwrap_or("");
            if !turn_host.is_empty() {
                let turn_protocol = resp["turn_protocol"].as_str().filter(|s| !s.is_empty()).unwrap_or("turn");
                api_map.insert("turnservers".to_owned(), serde_json::json!([{
                    "protocol": turn_protocol,
                    "host": turn_host,
                    "port": resp["turn_port"].as_str().unwrap_or(""),
                    "username": resp["turn_username"].as_str().unwrap_or(""),
                    "password": resp["turn_password"].as_str().unwrap_or("")
                }]));
            }
            let rendezvous_host = resp["rendezvous_host"].as_str().unwrap_or("");
            if !rendezvous_host.is_empty() {
                api_map.insert("rendezvous".to_owned(), serde_json::json!({
                    "host": rendezvous_host,
                    "port": resp["rendezvous_port"].as_str().unwrap_or("")
                }));
            }
            let rendezvousssl_host = resp["rendezvousssl_host"].as_str().unwrap_or("");
            if !rendezvousssl_host.is_empty() {
                api_map.insert("rendezvousssl".to_owned(), serde_json::json!({
                    "host": rendezvousssl_host,
                    "port": resp["rendezvousssl_port"].as_str().unwrap_or("")
                }));
            }
            serde_json::Value::Object(api_map)
        };
        let api_json_path = Config::path("api.json");
        if let Some(parent) = api_json_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(&api_json_path, serde_json::to_string_pretty(&api_json)?)?;
        log::info!("Wrote custom network config to {:?}", api_json_path);
        Config::set_option("api-cache".to_owned(), String::new());
        hbb_common::api::erase_api().await;
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        crate::rendezvous_mediator::RendezvousMediator::restart();
    }
    Ok(())
}

fn get_prefilled_enrollment_token() -> String {
    std::fs::read_to_string(Config::shared_path("EnrollmentToken.toml"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

pub async fn link_device() -> ResultType<()> {
    let invite_code = get_invite_code();
    let prefilled_token = get_prefilled_enrollment_token();

    if invite_code.is_empty() && prefilled_token.is_empty() {
        hbb_common::bail!("No invite code or enrollment token set");
    }

    let (enrollment_token, dashboard_user_id) = if !prefilled_token.is_empty() {
        log::info!("Using pre-supplied enrollment token");
        (prefilled_token, String::new())
    } else {
        log::info!("Linking device with invite code: {}...", &invite_code[..invite_code.len().min(8)]);
        let (t, u, _, name) = validate_invite(&invite_code).await?;
        if !name.is_empty() {
            Config::set_option("dashboard_account_name".to_owned(), name);
        }
        (t, u)
    };
    if !enrollment_token.is_empty() {
        log::info!("Got enrollment token for secure registration");
    }

    let mut device_id = Config::get_id();
    for _ in 0..15 {
        if !device_id.is_empty() {
            break;
        }
        hbb_common::sleep(1.0).await;
        device_id = Config::get_id();
    }
    if device_id.is_empty() {
        hbb_common::bail!("Device ID not available after waiting");
    }

    let device_name = crate::common::hostname();
    let os_name = std::env::consts::OS;
    let mac = get_mac_address();
    let resolved_user_id = register_device(&enrollment_token, &invite_code, &device_id, &device_name, os_name, &mac).await?;

    let final_user_id = if !resolved_user_id.is_empty() {
        resolved_user_id
    } else {
        dashboard_user_id
    };
    if !final_user_id.is_empty() {
        Config::set_option("dashboard_user_id".to_owned(), final_user_id.clone());
        Config::set_option("dashboard_device_id".to_owned(), device_id.clone());
        log::info!("Dashboard user ID stored: {}", final_user_id);
    }

    if !invite_code.is_empty() {
        if let Err(e) = get_network_settings(&invite_code).await {
            log::warn!("Failed to get network settings: {}", e);
        }
    }

    Ok(())
}

pub fn percent_decode_path(s: &str) -> String {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
                result.push(byte);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).to_string()
}

const ATTACHMENT_MAGIC: &[u8; 4] = b"HTDE";

fn encrypt_attachment(data: &[u8], dashboard_user_id: &str) -> Vec<u8> {
    let key: [u8; 32] = Sha256::digest(dashboard_user_id.as_bytes()).into();
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(ATTACHMENT_MAGIC);
    for (i, &b) in data.iter().enumerate() {
        out.push(b ^ key[i % 32]);
    }
    out
}

pub fn decrypt_attachment(data: &[u8], dashboard_user_id: &str) -> Vec<u8> {
    if data.len() < 4 || &data[..4] != ATTACHMENT_MAGIC {
        return data.to_vec();
    }
    let key: [u8; 32] = Sha256::digest(dashboard_user_id.as_bytes()).into();
    let encoded = &data[4..];
    encoded.iter().enumerate().map(|(i, &b)| b ^ key[i % 32]).collect()
}

fn get_mac_address() -> String {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        mac_address::get_mac_address()
            .ok()
            .flatten()
            .map(|m| m.to_string())
            .unwrap_or_default()
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    "".to_string()
}

fn apply_pending_proxy() {
    let path = Config::shared_path("Proxy.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return,
    };
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
        let url = v["url"].as_str().unwrap_or("").trim().to_string();
        if !url.is_empty() {
            let username = v["username"].as_str().unwrap_or("").to_string();
            let password = v["password"].as_str().unwrap_or("").to_string();
            let (proxy_type, proxy) = if let Some(idx) = url.find("://") {
                let scheme = url[..idx].to_lowercase();
                let host = url[idx + 3..].to_string();
                let pt = match scheme.as_str() {
                    "http" | "https" => hbb_common::config::ProxyType::Http,
                    "socks5" | "socks" | "socks5h" => hbb_common::config::ProxyType::Socks5,
                    _ => hbb_common::config::ProxyType::Auto,
                };
                (pt, host)
            } else {
                (hbb_common::config::ProxyType::Auto, url.clone())
            };
            Config::set_socks(Some(hbb_common::config::Socks5Server {
                proxy,
                username,
                password,
                proxy_type,
            }));
            log::info!("Applied deployment proxy settings");
        }
    }
    let _ = std::fs::remove_file(&path);
}

#[tokio::main(flavor = "current_thread")]
pub async fn start() {
    if DASHBOARD_RUNNING.swap(true, Ordering::SeqCst) {
        log::info!("Dashboard connection already running");
        return;
    }

    apply_pending_proxy();

    refresh_dashboard_endpoints().await;

    if !get_invite_code().is_empty() || !get_prefilled_enrollment_token().is_empty() {
        let used_code = get_invite_code();
        if let Err(e) = link_device().await {
            log::error!("Failed to link device: {}", e);
            DASHBOARD_RUNNING.store(false, Ordering::SeqCst);
            return;
        }
        Config::set_option("invite_code".to_owned(), String::new());
        Config::set_option("last_enrolled_invite_code".to_owned(), used_code);
        std::fs::remove_file(Config::path("InviteCode.toml")).ok();
        std::fs::remove_file(Config::shared_path("InviteCode.toml")).ok();
        std::fs::remove_file(Config::shared_path("EnrollmentToken.toml")).ok();
        let sentinel = Config::shared_path("Enrolled.toml");
        if let Some(parent) = sentinel.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let body = format!(
            "user_id={}\ndevice_id={}\n",
            get_dashboard_user_id(),
            Config::get_id()
        );
        std::fs::write(&sentinel, body).ok();
    }

    let dashboard_user_id = get_dashboard_user_id();
    if dashboard_user_id.is_empty() {
        log::info!("No dashboard_user_id, not starting WebSocket");
        DASHBOARD_RUNNING.store(false, Ordering::SeqCst);
        return;
    }

    // Restore registered device ID if config was reset
    let stored_device_id = Config::get_option("dashboard_device_id");
    if !stored_device_id.is_empty() && Config::get_id() != stored_device_id {
        log::warn!("Device ID changed from {} to {}, restoring registered ID", Config::get_id(), stored_device_id);
        Config::set_id(&stored_device_id);
    }

    log::info!("Starting dashboard WebSocket connection");
    let mut reconnect_delay = RECONNECT_DELAY_BASE_SECS;

    loop {
        match dashboard_ws_loop(&dashboard_user_id).await {
            Ok(()) => {
                log::info!("Dashboard WebSocket loop ended normally");
                reconnect_delay = RECONNECT_DELAY_BASE_SECS;
            }
            Err(e) => {
                log::error!("Dashboard WebSocket error: {}", e);
            }
        }

        if get_dashboard_user_id().is_empty() {
            log::info!("Device unlinked from dashboard, stopping reconnection");
            DASHBOARD_RUNNING.store(false, Ordering::SeqCst);
            break;
        }

        log::info!("Reconnecting dashboard WebSocket in {}s...", reconnect_delay);
        hbb_common::sleep(reconnect_delay as _).await;
        reconnect_delay = (reconnect_delay * 2).min(RECONNECT_DELAY_MAX_SECS);
    }
}

async fn dashboard_ws_loop(dashboard_user_id: &str) -> ResultType<()> {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let url = format!(
        "{}?dashboard_user_id={}&EIO=4&transport=websocket",
        dashboard_ws_url(), dashboard_user_id
    );
    log::info!("Connecting to dashboard WebSocket");

    let tls_opts = Some(tokio_tungstenite::Connector::NativeTls(
        native_tls::TlsConnector::builder()
            .request_alpns(&["http/1.1"])
            .build()?,
    ));

    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::header::{HeaderValue, ACCEPT_LANGUAGE, USER_AGENT};
    let mut request = url.as_str().into_client_request()?;
    {
        let headers = request.headers_mut();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
            ),
        );
        headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
    }

    let (ws_stream, _) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, tls_opts)
            .await?;

    log::info!("Dashboard WebSocket connected");
    let (mut sender, mut receiver) = ws_stream.split();

    let open_msg = receiver
        .next()
        .await
        .ok_or_else(|| hbb_common::anyhow::anyhow!("WS closed before open packet"))??;
    let open_text = open_msg.to_text()?;
    if !open_text.starts_with('0') {
        hbb_common::bail!("Expected Engine.IO open packet, got: {}", open_text);
    }

    send_ws_message(&mut sender, WsMessage::Text("40/device,".to_string())).await?;

    let ack_msg = receiver
        .next()
        .await
        .ok_or_else(|| hbb_common::anyhow::anyhow!("WS closed before namespace ACK"))??;
    let ack_text = ack_msg.to_text()?;
    if !ack_text.starts_with("40/device") {
        hbb_common::bail!("Expected namespace ACK, got: {}", ack_text);
    }

    let device_id = Config::get_id();
    let computer_name = crate::common::hostname();
    let os_name = std::env::consts::OS;
    let timezone = get_timezone();
    let mac = get_mac_address();
    let wol_enabled = !crate::ui_interface::get_option("wol-enabled").is_empty();

    let register_data = serde_json::json!({
        "device_id": device_id,
        "dashboard_user_id": dashboard_user_id,
        "timezone": timezone,
        "computer_name": computer_name,
        "os": os_name,
        "mac_address": mac,
        "wol_enabled": wol_enabled
    });

    hbb_common::sleep(0.5).await;
    send_socketio_event(&mut sender, "register", &register_data).await?;

    let mut heartbeat_timer =
        crate::rustdesk_interval(tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS)));
    let mut was_in_session = false;
    let mut last_ws_data = tokio::time::Instant::now();
    const WS_READ_TIMEOUT_SECS: u64 = 90;

    loop {
        select! {
            msg = receiver.next() => {
                last_ws_data = tokio::time::Instant::now();
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        if text == "2" {
                            send_ws_message(&mut sender, WsMessage::Text("3".to_string())).await?;
                            continue;
                        }
                        if text == "3" {
                            continue;
                        }
                        if let Some((resp_event, resp_data)) = handle_incoming_message(&text)? {
                            send_socketio_event(&mut sender, &resp_event, &resp_data).await?;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) => {
                        log::info!("Dashboard WebSocket closed by server");
                        break;
                    }
                    Some(Err(e)) => {
                        hbb_common::bail!("Dashboard WebSocket error: {}", e);
                    }
                    None => {
                        log::info!("Dashboard WebSocket stream ended");
                        break;
                    }
                    _ => {}
                }
            }
            _ = heartbeat_timer.tick() => {
                if last_ws_data.elapsed() > Duration::from_secs(WS_READ_TIMEOUT_SECS) {
                    log::warn!("Dashboard WebSocket: no data received for {}s, reconnecting", WS_READ_TIMEOUT_SECS);
                    break;
                }
                let current_in_session = IN_SESSION.load(Ordering::Relaxed);

                if current_in_session && !was_in_session {
                    let (stype, rip) = SESSION_META.lock().unwrap().clone();
                    let session_start = serde_json::json!({
                        "device_id": device_id,
                        "session_type": if stype.is_empty() { "screen".to_string() } else { stype },
                        "remote_ip": rip,
                        "timestamp": chrono::Utc::now().timestamp() as u64
                    });
                    send_socketio_event(&mut sender, "remote_session_start", &session_start).await?;
                } else if !current_in_session && was_in_session {
                    let (stype, rip) = SESSION_META.lock().unwrap().clone();
                    let session_end = serde_json::json!({
                        "device_id": device_id,
                        "session_type": if stype.is_empty() { "screen".to_string() } else { stype },
                        "remote_ip": rip,
                        "timestamp": chrono::Utc::now().timestamp() as u64
                    });
                    send_socketio_event(&mut sender, "remote_session_end", &session_end).await?;
                }
                was_in_session = current_in_session;

                let wol_now = !crate::ui_interface::get_option("wol-enabled").is_empty();
                let heartbeat = serde_json::json!({
                    "device_id": device_id,
                    "timezone": timezone,
                    "in_session": current_in_session,
                    "wol_enabled": wol_now
                });
                send_socketio_event(&mut sender, "heartbeat", &heartbeat).await?;
            }
        }
    }

    Ok(())
}

async fn send_ws_message<S>(
    sender: &mut S,
    msg: tokio_tungstenite::tungstenite::Message,
) -> ResultType<()>
where
    S: futures::Sink<tokio_tungstenite::tungstenite::Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    use futures::SinkExt;
    match tokio::time::timeout(Duration::from_secs(WS_SEND_TIMEOUT_SECS), sender.send(msg)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => hbb_common::bail!("WS send timed out after {}s", WS_SEND_TIMEOUT_SECS),
    }
}

async fn send_socketio_event<S>(
    sender: &mut S,
    event: &str,
    data: &serde_json::Value,
) -> ResultType<()>
where
    S: futures::Sink<tokio_tungstenite::tungstenite::Message, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    let payload = format!("42/device,[{},{}]", serde_json::json!(event), data);
    send_ws_message(sender, tokio_tungstenite::tungstenite::Message::Text(payload)).await
}

fn handle_incoming_message(text: &str) -> ResultType<Option<(String, serde_json::Value)>> {
    if let Some(json_str) = text.strip_prefix("42/device,") {
        if let Ok(arr) = serde_json::from_str::<serde_json::Value>(json_str) {
            if let Some(event) = arr.get(0).and_then(|v| v.as_str()) {
                match event {
                    "registered" => {
                        if let Some(data) = arr.get(1) {
                            if let Some(uid) = data["dashboard_user_id"].as_str() {
                                if !uid.is_empty() && get_dashboard_user_id().is_empty() {
                                    Config::set_option("dashboard_user_id".to_owned(), uid.to_string());
                                    log::info!("Dashboard: stored dashboard_user_id from WS register ACK");
                                }
                            }
                        }
                    }
                    "heartbeat_ack" => {}
                    "unlinked" => {
                        log::warn!("Dashboard: device has been permanently deleted, unlinking");
                        Config::set_option("dashboard_user_id".to_owned(), String::new());
                        Config::set_option("invite_code".to_owned(), String::new());
                        hbb_common::bail!("Device unlinked from dashboard");
                    }
                    "ticket:reply" => {
                        if let Some(data) = arr.get(1) {
                            log::info!("Dashboard: ticket reply notification: {}", data);
                            TICKET_REPLY_COUNTER.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    "wol:send" => {
                        if let Some(data) = arr.get(1) {
                            if let Some(target_mac) = data["target_mac"].as_str() {
                                log::info!("Dashboard: WoL request for MAC {}", target_mac);
                                #[cfg(not(target_os = "ios"))]
                                {
                                    if let Ok(mac_addr) = target_mac.parse() {
                                        let interfaces = default_net::get_interfaces();
                                        for iface in &interfaces {
                                            for ipv4 in &iface.ipv4 {
                                                log::info!("Sending WoL magic packet via {}", ipv4.addr);
                                                allow_err!(wol::send_wol(mac_addr, None, Some(std::net::IpAddr::V4(ipv4.addr))));
                                            }
                                        }
                                    } else {
                                        log::error!("Dashboard: invalid MAC address for WoL: {}", target_mac);
                                    }
                                }
                            }
                        }
                    }
                    "mcp:request" => {
                        if let Some(data) = arr.get(1) {
                            let request_id = data["request_id"].as_str().unwrap_or("").to_string();
                            let payload = &data["payload"];
                            let payload_str = payload.to_string();
                            log::info!("Dashboard: MCP request (id={})", request_id);
                            let mcp_resp = crate::mcp_server::handle_mcp_request(&payload_str)
                                .unwrap_or_else(|| r#"{"error":"no response"}"#.to_string());
                            let resp_val: serde_json::Value = serde_json::from_str(&mcp_resp).unwrap_or_default();
                            return Ok(Some(("mcp:response".to_string(), serde_json::json!({
                                "request_id": request_id,
                                "response": resp_val
                            }))));
                        }
                    }
                    _ => {
                        log::debug!("Dashboard: unknown event '{}': {}", event, json_str);
                    }
                }
            }
        }
    }

    Ok(None)
}

fn get_timezone() -> String {
    #[cfg(target_os = "windows")]
    {
        if let Ok(tz) = std::env::var("TZ") {
            return tz;
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(tz) = std::env::var("TZ") {
            return tz;
        }
        if let Ok(tz) = std::fs::read_to_string("/etc/timezone") {
            let tz = tz.trim().to_string();
            if !tz.is_empty() {
                return tz;
            }
        }
    }
    let offset = chrono::Local::now().format("%:z").to_string();
    format!("UTC{}", offset)
}

pub fn set_in_session(active: bool, session_type: &str, remote_ip: &str) {
    if active {
        *SESSION_META.lock().unwrap() = (session_type.to_string(), remote_ip.to_string());
    }
    IN_SESSION.store(active, Ordering::Relaxed);
}

pub fn submit_ticket(
    email: &str,
    subject: &str,
    description: &str,
    priority: &str,
) -> ResultType<i64> {
    let device_id = Config::get_id();
    let dashboard_user_id = get_dashboard_user_id();
    let device_name = crate::common::hostname();
    let user_name = crate::username();

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let url = format!("{}?action=submitTicket", dashboard_api_base());
    let resp: serde_json::Value = client
        .post(&url)
        .form(&[
            ("device_id", device_id.as_str()),
            ("dashboard_user_id", dashboard_user_id.as_str()),
            ("device_name", device_name.as_str()),
            ("user_name", user_name.as_str()),
            ("user_email", email),
            ("subject", subject),
            ("description", description),
            ("priority", priority),
        ])
        .send()?
        .json()?;

    if resp["success"].as_bool() != Some(true) {
        hbb_common::bail!(
            "submitTicket failed: {}",
            resp["error"].as_str().unwrap_or("unknown error")
        );
    }
    let ticket_id = resp["ticket_id"].as_i64().unwrap_or(0);
    Ok(ticket_id)
}

pub fn fetch_support_contact() -> ResultType<serde_json::Value> {
    let dashboard_user_id = get_dashboard_user_id();
    if dashboard_user_id.is_empty() {
        return Ok(serde_json::json!({}));
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let url = format!(
        "{}?action=getBranding&dashboard_user_id={}",
        dashboard_api_base(), dashboard_user_id
    );
    let resp: serde_json::Value = client.get(&url).send()?.json()?;
    let b = &resp["branding"];
    Ok(serde_json::json!({
        "name": b["company_name"].as_str().unwrap_or(""),
        "support_email": b["support_email"].as_str().unwrap_or(""),
        "phone": b["phone_number"].as_str().unwrap_or(""),
    }))
}

pub fn get_my_tickets() -> ResultType<serde_json::Value> {
    let device_id = Config::get_id();
    let dashboard_user_id = get_dashboard_user_id();

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let url = format!(
        "{}?action=getMyTickets&device_id={}&dashboard_user_id={}",
        dashboard_api_base(), device_id, dashboard_user_id
    );
    let resp: serde_json::Value = client.get(&url).send()?.json()?;
    if resp["success"].as_bool() != Some(true) {
        hbb_common::bail!(
            "getMyTickets failed: {}",
            resp["error"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(resp["tickets"].clone())
}

pub fn get_conversation(ticket_id: i64) -> ResultType<serde_json::Value> {
    let device_id = Config::get_id();

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let url = format!("{}?action=getCustomerConversation", dashboard_api_base());
    let resp: serde_json::Value = client
        .post(&url)
        .form(&[
            ("ticket_id", &ticket_id.to_string()),
            ("device_id", &device_id),
        ])
        .send()?
        .json()?;
    if resp["success"].as_bool() != Some(true) {
        hbb_common::bail!(
            "getCustomerConversation failed: {}",
            resp["error"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(resp["messages"].clone())
}

pub fn get_attachments(ticket_id: i64) -> ResultType<serde_json::Value> {
    let device_id = Config::get_id();

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let url = format!(
        "{}?action=getAttachments&ticket_id={}&device_id={}",
        dashboard_api_base(), ticket_id, device_id
    );
    let resp: serde_json::Value = client.get(&url).send()?.json()?;
    if resp["success"].as_bool() != Some(true) {
        hbb_common::bail!(
            "getAttachments failed: {}",
            resp["error"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(resp["attachments"].clone())
}

pub fn add_reply(ticket_id: i64, message: &str) -> ResultType<bool> {
    let device_id = Config::get_id();

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let url = format!("{}?action=addCustomerReply", dashboard_api_base());
    let resp: serde_json::Value = client
        .post(&url)
        .form(&[
            ("ticket_id", &ticket_id.to_string()),
            ("device_id", &device_id),
            ("message", &message.to_string()),
        ])
        .send()?
        .json()?;
    if resp["success"].as_bool() != Some(true) {
        hbb_common::bail!(
            "addCustomerReply failed: {}",
            resp["error"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(resp["reopened"].as_bool().unwrap_or(false))
}

pub fn upload_attachment(ticket_id: i64, file_path: &str) -> ResultType<()> {
    let device_id = Config::get_id();
    let dashboard_user_id = get_dashboard_user_id();
    let file_path = percent_decode_path(file_path);
    log::info!("upload_attachment: ticket_id={}, file_path={}, device_id={}, dashboard_user_id={}",
        ticket_id, file_path, device_id, if dashboard_user_id.is_empty() { "(empty)" } else { &dashboard_user_id });
    let path = std::path::Path::new(&file_path);
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let file_content = match std::fs::read(&file_path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("Failed to read file '{}': {}", file_path, e);
            hbb_common::bail!("Cannot read file '{}': {}", file_path, e);
        }
    };
    log::info!("Read {} bytes from {}", file_content.len(), file_name);

    let encrypted = if !dashboard_user_id.is_empty() {
        encrypt_attachment(&file_content, &dashboard_user_id)
    } else {
        file_content
    };

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let url = format!("{}?action=customerUploadAttachment", dashboard_api_base());

    let part = reqwest::blocking::multipart::Part::bytes(encrypted)
        .file_name(file_name.clone())
        .mime_str("application/octet-stream")?;

    let form = reqwest::blocking::multipart::Form::new()
        .text("action", "customerUploadAttachment")
        .text("ticket_id", ticket_id.to_string())
        .text("device_id", device_id)
        .text("customer_name", "Customer")
        .part("file", part);

    log::info!("Uploading attachment to {}", url);
    let resp = client.post(&url).multipart(form).send()?;
    let status = resp.status();
    let body_text = resp.text().unwrap_or_default();
    log::info!("Upload response: HTTP {} body={}", status, body_text);
    let body: serde_json::Value = serde_json::from_str(&body_text).unwrap_or_default();
    if !status.is_success() {
        hbb_common::bail!("HTTP {}: {}", status, body.get("message").and_then(|m| m.as_str()).unwrap_or(&body_text));
    }
    if body["success"].as_bool() == Some(false) {
        hbb_common::bail!("{}", body["message"].as_str().unwrap_or("upload failed"));
    }
    log::info!("Attachment uploaded successfully: {}", file_name);
    Ok(())
}

pub fn get_ticket_reply_counter() -> u64 {
    TICKET_REPLY_COUNTER.load(Ordering::Relaxed)
}

pub fn get_attachment_download_url(download_url: &str) -> String {
    if download_url.starts_with("http") {
        download_url.to_string()
    } else {
        format!("{}/{}", dashboard_api_base(), download_url)
    }
}
