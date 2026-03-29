use std::io::{self, BufRead, Write};
use std::time::Instant;
use std::collections::HashMap;
use std::sync::Mutex;
use serde_json::{json, Value};
use std::time::Duration;
use lazy_static::lazy_static;
use hbb_common::tokio;
use hbb_common::config::PeerConfig;

// Workflow recording state
struct WorkflowStep {
    tool_name: String,
    arguments: Value,
    timestamp_ms: u64,
}

struct WorkflowState {
    recording: bool,
    steps: Vec<WorkflowStep>,
    start_time: Option<Instant>,
}

lazy_static! {
    // Static storage for screen_diff_summary references (reference_id -> (rgba, width, height, created))
    static ref DIFF_REFS: Mutex<HashMap<String, (Vec<u8>, u32, u32, Instant)>> =
        Mutex::new(HashMap::new());

    static ref WORKFLOW_STATE: Mutex<WorkflowState> =
        Mutex::new(WorkflowState {
            recording: false,
            steps: Vec::new(),
            start_time: None,
        });
}

const SERVER_NAME: &str = "hoptodesk-mcp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Stdio mode: reads JSON-RPC from stdin, writes to stdout
pub fn run() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => {
                break;
            }
        };
        if let Some(resp) = process_line(&line) {
            if let Err(_) = writeln!(stdout, "{}", resp) {
                break;
            }
            let _ = stdout.flush();
        }
    }
}

/// TCP mode: listens on localhost with token authentication.
/// First line from each connection must be: {"auth":"TOKEN"}
pub fn run_tcp(port: u16) {
    use std::net::TcpListener;

    let addr = format!("127.0.0.1:{}", port);
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(_) => {
            return;
        }
    };

    let token = generate_auth_token();

    write_discovery_file(port, &token, "tcp");
    let _guard = McpFileGuard;

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(_) => {
                continue;
            }
        };
        let reader = io::BufReader::new(stream.try_clone().unwrap_or_else(|_| {
            std::process::exit(1);
        }));
        let mut writer = stream;
        let mut authenticated = false;

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => {
                    break;
                }
            };

            if !authenticated {
                // First line must be {"auth": "TOKEN"}
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if v["auth"].as_str() == Some(&token) {
                        authenticated = true;
                        let _ = writeln!(writer, r#"{{"authenticated":true}}"#);
                        let _ = writer.flush();
                        continue;
                    }
                }
                let _ = writeln!(writer, r#"{{"error":"authentication failed"}}"#);
                let _ = writer.flush();
                break;
            }

            if let Some(resp) = process_line(&line) {
                if let Err(_) = writeln!(writer, "{}", resp) {
                    break;
                }
                let _ = writer.flush();
            }
        }
    }
}

/// Public entry point for processing MCP requests from other modules (e.g. dashboard relay).
/// Takes a JSON-RPC string, returns the response JSON string or None for notifications.
pub fn handle_mcp_request(json_str: &str) -> Option<String> {
    process_line(json_str)
}

/// Process a single JSON-RPC line, return response string or None for notifications
fn process_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let request: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            return None;
        }
    };

    let method = request["method"].as_str().unwrap_or("");
    let id = request.get("id").cloned();

    if id.is_none() {
        return None;
    }

    let response = match method {
        "initialize" => handle_initialize(&request),
        "tools/list" => handle_tools_list(),
        "tools/call" => handle_tools_call(&request),
        "prompts/list" => handle_prompts_list(),
        "prompts/get" => handle_prompts_get(&request),
        "resources/list" => handle_resources_list(),
        "resources/read" => handle_resources_read(&request),
        "roots/list" => handle_roots_list(),
        "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": format!("Unknown method: {}", method) }
        }),
    };

    let mut resp = response;
    if let Some(id_val) = id {
        resp["id"] = id_val;
    }

    Some(serde_json::to_string(&resp).unwrap_or_default())
}

/// Local WebSocket MCP server (localhost-only, auth token required).
/// Generates a random auth token printed to stderr. AI agents connect via
/// ws://127.0.0.1:PORT, send {"auth":"TOKEN"} as first message, then JSON-RPC.
#[tokio::main(flavor = "current_thread")]
pub async fn run_ws_local(port: u16) {
    use hbb_common::tokio::net::TcpListener;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let addr = format!("127.0.0.1:{}", port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(_) => {
            return;
        }
    };

    // Generate random auth token
    let token = generate_auth_token();

    write_discovery_file(port, &token, "websocket");
    let _guard = McpFileGuard;

    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(s) => s,
            Err(_) => {
                continue;
            }
        };
        let ws_stream = match tokio_tungstenite::accept_async(stream).await {
            Ok(ws) => ws,
            Err(_) => {
                continue;
            }
        };

        let (mut ws_sender, mut ws_receiver) = ws_stream.split();
        let mut authenticated = false;
        let token_clone = token.clone();

        while let Some(msg) = ws_receiver.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(_) => {
                    break;
                }
            };

            let text = match msg {
                WsMessage::Text(t) => t,
                WsMessage::Close(_) => break,
                WsMessage::Ping(d) => {
                    let _ = ws_sender.send(WsMessage::Pong(d)).await;
                    continue;
                }
                _ => continue,
            };

            if !authenticated {
                // First message must be {"auth": "TOKEN"}
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    if v["auth"].as_str() == Some(&token_clone) {
                        authenticated = true;
                        let _ = ws_sender
                            .send(WsMessage::Text(r#"{"authenticated":true}"#.to_string()))
                            .await;
                        continue;
                    }
                }
                let _ = ws_sender
                    .send(WsMessage::Text(r#"{"error":"authentication failed"}"#.to_string()))
                    .await;
                let _ = ws_sender.close().await;
                break;
            }

            // Authenticated — process MCP JSON-RPC
            if let Some(resp) = process_line(&text) {
                if let Err(_) = ws_sender.send(WsMessage::Text(resp)).await {
                    break;
                }
            }
        }
    }
}

/// Generate a random 32-character alphanumeric auth token
fn generate_auth_token() -> String {
    use hbb_common::rand::Rng;
    let mut rng = hbb_common::rand::thread_rng();
    (0..32)
        .map(|_| {
            let idx: u8 = rng.gen_range(0..62);
            match idx {
                0..=9 => (b'0' + idx) as char,
                10..=35 => (b'a' + idx - 10) as char,
                _ => (b'A' + idx - 36) as char,
            }
        })
        .collect()
}

/// Guard that deletes mcp.json when dropped (server shutdown).
struct McpFileGuard;

impl Drop for McpFileGuard {
    fn drop(&mut self) {
        let config_dir = hbb_common::config::Config::path("");
        let path = config_dir.join("mcp.json");
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Write a JSON discovery file (mcp.json) to the HopToDesk config directory
/// (same dir as hoptodesk.toml). Contains connection info + auth token.
/// Sets owner-only permissions (0600) on Unix.
fn write_discovery_file(port: u16, token: &str, transport: &str) {
    let url = match transport {
        "websocket" => format!("ws://127.0.0.1:{}", port),
        _ => format!("tcp://127.0.0.1:{}", port),
    };
    let discovery = json!({
        "mcp_version": PROTOCOL_VERSION,
        "server": SERVER_NAME,
        "version": SERVER_VERSION,
        "transport": transport,
        "url": url,
        "port": port,
        "token": token,
        "pid": std::process::id(),
    });
    let content = serde_json::to_string_pretty(&discovery).unwrap_or_default();

    let config_dir = hbb_common::config::Config::path("");
    if config_dir.exists() || std::fs::create_dir_all(&config_dir).is_ok() {
        let path = config_dir.join("mcp.json");
        if std::fs::write(&path, &content).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
}

fn handle_initialize(_request: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "result": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {
                "tools": {},
                "prompts": {},
                "resources": {},
                "roots": { "listChanged": false }
            },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": SERVER_VERSION
            }
        }
    })
}

fn handle_prompts_list() -> Value {
    json!({
        "jsonrpc": "2.0",
        "result": {
            "prompts": [
                { "name": "device_health_check", "description": "Check device health: CPU, memory, disk, and connectivity status" },
                { "name": "automate_ui_task", "description": "Step-by-step UI automation using screenshot, find, click, and verify pattern" },
                { "name": "monitor_screen", "description": "Watch a screen region for changes and report when something happens" },
                { "name": "connect_and_verify", "description": "Connect to a remote peer and verify the session is working properly" },
                { "name": "diagnose_connection", "description": "Diagnose why a remote connection is failing or performing poorly" },
                { "name": "file_operations", "description": "Transfer and manage files between local and remote devices" },
                { "name": "run_maintenance", "description": "Run standard maintenance tasks: clear temp files, check updates, restart services" },
                { "name": "screen_interaction", "description": "Best practice pattern for screen interaction: screenshot → identify → click → verify" },
                { "name": "batch_device_check", "description": "Check status of multiple devices via dashboard API" }
            ]
        }
    })
}

fn handle_prompts_get(request: &Value) -> Value {
    let name = request["params"]["name"].as_str().unwrap_or("");
    let messages = match name {
        "device_health_check" => json!([{ "role": "user", "content": { "type": "text", "text": "Check this device's health. Use get_device_info for basic status, then exec_operation with 'get_system_info' for CPU/memory, 'disk_usage' for storage, and 'network_info' for connectivity. Summarize any issues found." } }]),
        "automate_ui_task" => json!([{ "role": "user", "content": { "type": "text", "text": "To automate a UI task: 1) Take a screenshot to understand the current state. 2) Identify the target element's coordinates. 3) Use mouse_click or type_text to interact. 4) Use verify_action_result to confirm the action worked. 5) Repeat for each step." } }]),
        "monitor_screen" => json!([{ "role": "user", "content": { "type": "text", "text": "Monitor a screen region for changes. Use screen_diff_summary to capture a baseline, then periodically call it again with the reference_id to check for changes." } }]),
        "connect_and_verify" => json!([{ "role": "user", "content": { "type": "text", "text": "Connect to a remote peer: 1) Use list_peers to find the target device. 2) Use connect_to_peer with the peer_id. 3) Use wait_for_event with 'connection_ready'. 4) Use get_ui_state to verify. 5) Take a screenshot to confirm." } }]),
        "diagnose_connection" => json!([{ "role": "user", "content": { "type": "text", "text": "Diagnose a connection issue: 1) Use get_device_info. 2) Use list_active_connections. 3) Use get_ui_state for quality metrics. 4) Use exec_operation with 'ping_test'. 5) Use exec_operation with 'network_info'. Report findings." } }]),
        "file_operations" => json!([{ "role": "user", "content": { "type": "text", "text": "For file operations: 1) Use list_local_files to browse. 2) Use read_local_file for contents. 3) Use connect_to_peer with 'file-transfer'. 4) Use get_clipboard/set_clipboard for small transfers." } }]),
        "run_maintenance" => json!([{ "role": "user", "content": { "type": "text", "text": "Run maintenance: 1) exec_operation 'get_system_info'. 2) exec_operation 'clear_temp_files'. 3) exec_operation 'run_update_check'. 4) exec_operation 'flush_dns'. 5) exec_operation 'restart_hoptodesk'." } }]),
        "screen_interaction" => json!([{ "role": "user", "content": { "type": "text", "text": "Screen interaction: 1) screenshot. 2) Identify coordinates. 3) mouse_click or type_text. 4) verify_action_result. 5) screenshot again if needed." } }]),
        "batch_device_check" => json!([{ "role": "user", "content": { "type": "text", "text": "Check multiple devices: 1) list_peers. 2) For each: connect_to_peer, wait_for_event, exec_operation 'get_system_info'. 3) disconnect_peer. 4) Compile summary." } }]),
        _ => {
            return json!({ "jsonrpc": "2.0", "error": { "code": -32602, "message": format!("Unknown prompt: {}", name) } });
        }
    };
    json!({ "jsonrpc": "2.0", "result": { "description": format!("Prompt: {}", name), "messages": messages } })
}

fn handle_resources_list() -> Value {
    json!({
        "jsonrpc": "2.0",
        "result": {
            "resources": [
                { "uri": "hoptodesk://device/info", "name": "Device Info", "description": "Device ID, version, platform, hostname, and signal server status", "mimeType": "application/json" },
                { "uri": "hoptodesk://device/config", "name": "Device Configuration", "description": "HopToDesk configuration: device ID, dashboard enrollment, options", "mimeType": "application/json" },
                { "uri": "hoptodesk://device/peers", "name": "Known Peers", "description": "List of known peers from recent sessions, favorites, and LAN discovery", "mimeType": "application/json" },
                { "uri": "hoptodesk://device/connections", "name": "Active Connections", "description": "Currently active remote desktop connections", "mimeType": "application/json" }
            ]
        }
    })
}

fn handle_resources_read(request: &Value) -> Value {
    let uri = request["params"]["uri"].as_str().unwrap_or("");
    let (content_text, mime) = match uri {
        "hoptodesk://device/info" => {
            let id = hbb_common::config::Config::get_id();
            let version = env!("CARGO_PKG_VERSION");
            let platform = std::env::consts::OS;
            let hostname = crate::common::hostname();
            let status = crate::ui_interface::get_connect_status();
            let status_text = match status.status_num { 1 => "online", 0 => "connecting", _ => "offline" };
            let info = json!({ "device_id": id, "version": version, "platform": platform, "hostname": hostname, "status": status_text, "status_num": status.status_num });
            (serde_json::to_string_pretty(&info).unwrap_or_default(), "application/json")
        },
        "hoptodesk://device/config" => {
            let id = hbb_common::config::Config::get_id();
            let dashboard_user_id = hbb_common::config::Config::get_option("dashboard_user_id");
            let config_dir = hbb_common::config::Config::path("").to_string_lossy().to_string();
            let config = json!({ "device_id": id, "dashboard_user_id": if dashboard_user_id.is_empty() { Value::Null } else { Value::String(dashboard_user_id) }, "config_directory": config_dir, "version": env!("CARGO_PKG_VERSION"), "platform": std::env::consts::OS });
            (serde_json::to_string_pretty(&config).unwrap_or_default(), "application/json")
        },
        "hoptodesk://device/peers" => {
            let peers: Vec<Value> = PeerConfig::peers(None).into_iter().map(|(id, modified, config)| {
                let last_seen = modified.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                json!({ "id": id, "username": config.info.username, "hostname": config.info.hostname, "platform": config.info.platform, "last_seen_epoch": last_seen })
            }).collect();
            (serde_json::to_string_pretty(&peers).unwrap_or_default(), "application/json")
        },
        "hoptodesk://device/connections" => {
            match tool_list_active_connections() {
                Ok(content) => {
                    let text = content.as_array().and_then(|arr| arr.first()).and_then(|c| c["text"].as_str()).unwrap_or("[]").to_string();
                    (text, "application/json")
                },
                Err(e) => (format!("{{\"error\": \"{}\"}}", e), "application/json"),
            }
        },
        _ => {
            return json!({ "jsonrpc": "2.0", "error": { "code": -32602, "message": format!("Unknown resource URI: {}", uri) } });
        }
    };
    json!({ "jsonrpc": "2.0", "result": { "contents": [{ "uri": uri, "mimeType": mime, "text": content_text }] } })
}

fn handle_roots_list() -> Value {
    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let path = std::path::PathBuf::from(home);
        roots.push(json!({ "uri": format!("file://{}", path.to_string_lossy()), "name": "Home Directory" }));
    }
    let config_dir = hbb_common::config::Config::path("");
    roots.push(json!({ "uri": format!("file://{}", config_dir.to_string_lossy()), "name": "HopToDesk Config" }));
    let tmp = std::env::temp_dir();
    roots.push(json!({ "uri": format!("file://{}", tmp.to_string_lossy()), "name": "Temp Directory" }));
    json!({ "jsonrpc": "2.0", "result": { "roots": roots } })
}

fn handle_tools_list() -> Value {
    json!({
        "jsonrpc": "2.0",
        "result": {
            "tools": [
                {
                    "name": "screenshot",
                    "description": "Capture the primary display (or a cropped region) and return as a PNG image. Use x/y/width/height to crop a specific area — useful for inspecting toolbar buttons, dialog text, or other small UI elements.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "display": {
                                "type": "integer",
                                "description": "Display index (0 = primary). Default: 0"
                            },
                            "x": { "type": "integer", "description": "Crop region left X coordinate in pixels" },
                            "y": { "type": "integer", "description": "Crop region top Y coordinate in pixels" },
                            "width": { "type": "integer", "description": "Crop region width in pixels" },
                            "height": { "type": "integer", "description": "Crop region height in pixels" }
                        }
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "get_window_list",
                    "description": "List all visible windows with their titles, positions, and sizes",
                    "inputSchema": {
                        "type": "object",
                        "properties": {}
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "mouse_click",
                    "description": "Move the mouse to (x, y) and click",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "x": { "type": "integer", "description": "X coordinate in pixels" },
                            "y": { "type": "integer", "description": "Y coordinate in pixels" },
                            "button": {
                                "type": "string",
                                "enum": ["left", "right", "middle"],
                                "description": "Mouse button. Default: left"
                            },
                            "double_click": {
                                "type": "boolean",
                                "description": "Double-click instead of single click. Default: false"
                            }
                        },
                        "required": ["x", "y"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "type_text",
                    "description": "Type text using keyboard input. Supports unicode characters.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string", "description": "Text to type" }
                        },
                        "required": ["text"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "key_press",
                    "description": "Press a keyboard key or combination (e.g. 'Return', 'ctrl+a', 'alt+F4')",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "key": { "type": "string", "description": "Key name (e.g. 'Return', 'Escape', 'Tab', 'BackSpace', 'Delete', 'F1'-'F12', 'Up', 'Down', 'Left', 'Right', 'Home', 'End', 'PageUp', 'PageDown')" },
                            "modifiers": {
                                "type": "array",
                                "items": { "type": "string", "enum": ["ctrl", "alt", "shift", "meta"] },
                                "description": "Modifier keys to hold while pressing"
                            }
                        },
                        "required": ["key"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "mouse_move",
                    "description": "Move the mouse to (x, y) without clicking",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "x": { "type": "integer", "description": "X coordinate in pixels" },
                            "y": { "type": "integer", "description": "Y coordinate in pixels" }
                        },
                        "required": ["x", "y"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "get_device_info",
                    "description": "Get this device's ID, version, platform, hostname, and signal server connection status",
                    "inputSchema": { "type": "object", "properties": {} },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "list_peers",
                    "description": "List known peers (recent sessions, favorites, LAN discovered). Returns peer ID, username, hostname, platform.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "filter": {
                                "type": "string",
                                "enum": ["all", "recent", "favorites", "lan"],
                                "description": "Which peers to list. Default: all"
                            }
                        }
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "connect_to_peer",
                    "description": "Initiate a remote desktop connection to a peer by device ID. Spawns a new connection window. Optionally pass a password to skip the password dialog.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "peer_id": { "type": "string", "description": "The peer device ID to connect to" },
                            "connection_type": {
                                "type": "string",
                                "enum": ["connect", "file-transfer", "port-forward"],
                                "description": "Type of connection. Default: connect"
                            },
                            "password": {
                                "type": "string",
                                "description": "Optional password for the remote peer. If provided, skips the password dialog."
                            }
                        },
                        "required": ["peer_id"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "disconnect_peer",
                    "description": "Close the remote connection to a peer by device ID",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "peer_id": { "type": "string", "description": "The peer device ID to disconnect" }
                        },
                        "required": ["peer_id"]
                    },
                    "annotations": { "readOnlyHint": false, "destructiveHint": true }
                },
                {
                    "name": "list_active_connections",
                    "description": "List all currently active remote connections with their type and alive status",
                    "inputSchema": { "type": "object", "properties": {} },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "get_ui_state",
                    "description": "Get rich structured UI state for HopToDesk. Returns per-connection: toolbar buttons, connection quality (speed/fps/delay/codec), display info (resolution, current monitor), permissions (keyboard/clipboard/audio/file), dialog/popup state, video dimensions, view settings, recording/privacy mode. No screenshot needed — use this to assess UI health programmatically.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "peer_id": {
                                "type": "string",
                                "description": "Optional: only return UI state for this specific connection. Default: return all."
                            }
                        }
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "dismiss_dialog",
                    "description": "Dismiss the currently visible modal dialog on a connection window (e.g. security code prompt, password prompt). Sends a signal to the child process via stdin pipe.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "peer_id": {
                                "type": "string",
                                "description": "The peer ID of the connection whose dialog to dismiss. Use 'all' to dismiss dialogs on all connections."
                            }
                        },
                        "required": ["peer_id"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "click_toolbar_button",
                    "description": "Click a toolbar button or submenu item on a remote connection window. Supports top-level buttons and submenu items using 'menu:item' format. Use get_ui_state to see available buttons and submenu items.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "peer_id": {
                                "type": "string",
                                "description": "The peer ID of the connection whose toolbar button to click."
                            },
                            "button_id": {
                                "type": "string",
                                "description": "Button ID or 'menu:item' for submenu. Top-level: fullscreen, chat, transfer-file, remote-print, recording, privacy-mode. Submenus: 'action:ctrl-alt-del', 'action:lock-screen', 'action:restart_remote_device', 'action:take-screenshot', 'action:refresh', 'action:block-input', 'action:tunnel', 'display:original', 'display:shrink', 'display:stretch', 'display:best', 'display:balanced', 'display:low', 'display:show-remote-cursor', 'display:disable-audio', 'display:disable-clipboard', 'display:lock-after-session-end', 'keyboard:legacy', 'keyboard:map', 'keyboard:translate'"
                            }
                        },
                        "required": ["peer_id", "button_id"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "test_print",
                    "description": "Send a test print job to the configured remote printer. This simulates receiving a print job from the remote machine — useful for verifying the local print pipeline works. Requires set_remote_printer to be configured first.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "text": {
                                "type": "string",
                                "description": "Text content to print. Default: 'HopToDesk Remote Print Test'"
                            }
                        }
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "set_remote_printer",
                    "description": "Set the printer for remote print jobs and enable auto-print mode. When enabled, print jobs from the remote machine are automatically sent to the selected printer without showing a dialog. Use 'auto' for printer_name to auto-select the only available printer. Returns the list of available printers if no name is given.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "printer_name": {
                                "type": "string",
                                "description": "Printer name to use, or 'auto' to select the only available printer. Omit to list available printers."
                            },
                            "auto_print": {
                                "type": "boolean",
                                "description": "Enable auto-print mode (default: true). When true, incoming print jobs are sent directly to the printer without prompting."
                            }
                        }
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "wait_for_event",
                    "description": "Block until a specified event occurs or timeout. Eliminates polling — use this instead of looping get_ui_state. Events: 'connection_ready' (peer connects and video starts), 'connection_closed' (peer disconnects), 'dialog_appeared' (dialog/prompt shows up), 'dialog_dismissed' (dialog closes), 'quality_stable' (fps > 0 and delay < 500ms). Returns the current state when the event fires.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "event": {
                                "type": "string",
                                "enum": ["connection_ready", "connection_closed", "dialog_appeared", "dialog_dismissed", "quality_stable"],
                                "description": "The event to wait for"
                            },
                            "peer_id": {
                                "type": "string",
                                "description": "Peer ID to monitor. Required for connection_ready, connection_closed, dialog_appeared, dialog_dismissed, quality_stable."
                            },
                            "timeout_ms": {
                                "type": "integer",
                                "description": "Maximum time to wait in milliseconds. Default: 30000 (30s). Max: 60000 (60s)."
                            }
                        },
                        "required": ["event", "peer_id"]
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "exec_operation",
                    "description": "Execute a predefined system operation on this device. Operations are hardcoded — only the operation name is accepted, no raw shell commands. Device must be enrolled with a dashboard.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "operation": {
                                "type": "string",
                                "enum": ["get_system_info", "disk_usage", "list_processes", "network_info", "get_service_logs", "ping_test", "installed_software", "restart_hoptodesk", "kill_process", "flush_dns", "reboot_device", "clear_temp_files", "run_update_check"],
                                "description": "The operation to execute"
                            },
                            "sort_by": { "type": "string", "enum": ["cpu", "memory"], "description": "For list_processes: sort by cpu or memory" },
                            "limit": { "type": "integer", "description": "For list_processes: max number of processes" },
                            "lines": { "type": "integer", "description": "For get_service_logs: number of log lines (max 200)" },
                            "host": { "type": "string", "description": "For ping_test: hostname or IP to ping" },
                            "count": { "type": "integer", "description": "For ping_test: number of pings (max 10)" },
                            "process_name": { "type": "string", "description": "For kill_process: name of process to kill" },
                            "pid": { "type": "integer", "description": "For kill_process: PID to kill" }
                        },
                        "required": ["operation"]
                    },
                    "annotations": { "readOnlyHint": false, "destructiveHint": true, "confirmationHint": true }
                },
                {
                    "name": "get_clipboard",
                    "description": "Read the current text content of the local clipboard",
                    "inputSchema": { "type": "object", "properties": {} },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "set_clipboard",
                    "description": "Set the local clipboard to the specified text content",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string", "description": "Text to copy to the clipboard" }
                        },
                        "required": ["text"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "send_chat_message",
                    "description": "Send a chat message to a connected remote peer. Requires an active remote desktop connection to the peer.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "peer_id": { "type": "string", "description": "The peer ID of the active connection to send the message to" },
                            "text": { "type": "string", "description": "The chat message text to send" }
                        },
                        "required": ["peer_id", "text"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "switch_permission",
                    "description": "Toggle a permission on an incoming remote connection. Only works for connections where a remote peer is connected TO this device (not outgoing connections you initiated). Permissions: keyboard, clipboard, audio, file, restart, recording.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "conn_id": { "type": "integer", "description": "The incoming connection ID (from list_incoming_connections)" },
                            "permission": {
                                "type": "string",
                                "enum": ["keyboard", "clipboard", "audio", "file", "restart", "recording"],
                                "description": "The permission to toggle"
                            },
                            "enabled": { "type": "boolean", "description": "true to enable, false to disable" }
                        },
                        "required": ["conn_id", "permission", "enabled"]
                    },
                    "annotations": { "readOnlyHint": false }
                },
                {
                    "name": "list_incoming_connections",
                    "description": "List all incoming remote connections (peers connected TO this device). Returns connection ID, peer info, authorization status, and permissions. Use conn_id with switch_permission to manage permissions.",
                    "inputSchema": { "type": "object", "properties": {} },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "list_local_files",
                    "description": "List files and directories at a given path on this device. Returns name, size, type (file/directory), and modified timestamp.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "Directory path to list. Default: home directory" },
                            "include_hidden": { "type": "boolean", "description": "Include hidden files (starting with '.'). Default: false" }
                        }
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "read_local_file",
                    "description": "Read the text content of a local file. For binary files, returns base64-encoded content. Max 1MB.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "Absolute path to the file to read" }
                        },
                        "required": ["path"]
                    },
                    "annotations": { "readOnlyHint": true }
                },
                {
                    "name": "run_command",
                    "description": "Execute a shell command on this device. Runs via cmd.exe on Windows or /bin/sh on Unix. Device must be enrolled with a dashboard. Output is truncated to 64KB.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "command": { "type": "string", "description": "The shell command to execute" },
                            "timeout": { "type": "integer", "description": "Timeout in seconds (default: 30, max: 120)" }
                        },
                        "required": ["command"]
                    },
                    "annotations": { "readOnlyHint": false, "destructiveHint": true, "confirmationHint": true }
                }
            ]
        }
    })
}

fn handle_tools_call(request: &Value) -> Value {
    let tool_name = request["params"]["name"].as_str().unwrap_or("");
    let arguments = &request["params"]["arguments"];

    let result = match tool_name {
        "screenshot" => tool_screenshot(arguments),
        "get_window_list" => tool_get_window_list(),
        "mouse_click" => tool_mouse_click(arguments),
        "type_text" => tool_type_text(arguments),
        "key_press" => tool_key_press(arguments),
        "mouse_move" => tool_mouse_move(arguments),
        "get_device_info" => tool_get_device_info(),
        "list_peers" => tool_list_peers(arguments),
        "connect_to_peer" => tool_connect_to_peer(arguments),
        "disconnect_peer" => tool_disconnect_peer(arguments),
        "list_active_connections" => tool_list_active_connections(),
        "get_ui_state" => tool_get_ui_state(arguments),
        "dismiss_dialog" => tool_dismiss_dialog(arguments),
        "click_toolbar_button" => tool_click_toolbar_button(arguments),
        "set_remote_printer" => tool_set_remote_printer(arguments),
        "test_print" => tool_test_print(arguments),
        "wait_for_event" => tool_wait_for_event(arguments),
        "exec_operation" => tool_exec_operation(arguments),
        "get_clipboard" => tool_get_clipboard(),
        "set_clipboard" => tool_set_clipboard(arguments),
        "send_chat_message" => tool_send_chat_message(arguments),
        "switch_permission" => tool_switch_permission(arguments),
        "list_incoming_connections" => tool_list_incoming_connections(),
        "list_local_files" => tool_list_local_files(arguments),
        "read_local_file" => tool_read_local_file(arguments),
        "run_command" => tool_run_command(arguments),
        _ => Err(format!("Unknown tool: {}", tool_name)),
    };

    match result {
        Ok(content) => json!({
            "jsonrpc": "2.0",
            "result": { "content": content }
        }),
        Err(msg) => json!({
            "jsonrpc": "2.0",
            "result": {
                "content": [{ "type": "text", "text": format!("Error: {}", msg) }],
                "isError": true
            }
        }),
    }
}

fn tool_screenshot(args: &Value) -> Result<Value, String> {
    // Try platform-specific capture first, fall back to scrap
    #[cfg(windows)]
    {
        if let Ok(result) = screenshot_gdi(args) {
            return Ok(result);
        }
    }
    screenshot_scrap(args)
}

#[cfg(windows)]
fn screenshot_gdi(args: &Value) -> Result<Value, String> {
    use hbb_common::base64::Engine;

    #[allow(clashing_extern_declarations)]
    extern "system" {
        fn GetDC(hwnd: isize) -> isize;
        fn ReleaseDC(hwnd: isize, hdc: isize) -> i32;
        fn CreateCompatibleDC(hdc: isize) -> isize;
        fn CreateCompatibleBitmap(hdc: isize, w: i32, h: i32) -> isize;
        fn SelectObject(hdc: isize, obj: isize) -> isize;
        fn BitBlt(dst: isize, x: i32, y: i32, w: i32, h: i32, src: isize, sx: i32, sy: i32, rop: u32) -> i32;
        fn DeleteDC(hdc: isize) -> i32;
        fn DeleteObject(obj: isize) -> i32;
        fn GetSystemMetrics(idx: i32) -> i32;
        fn GetDIBits(hdc: isize, hbm: isize, start: u32, lines: u32, bits: *mut u8, bi: *mut u8, usage: u32) -> i32;
    }

    const SM_CXSCREEN: i32 = 0;
    const SM_CYSCREEN: i32 = 1;
    const SRCCOPY: u32 = 0x00CC0020;

    unsafe {
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        if screen_w <= 0 || screen_h <= 0 {
            return Err("No display available".to_string());
        }

        // Check for crop region
        let crop = args["x"].as_i64().is_some() && args["y"].as_i64().is_some()
            && args["width"].as_i64().is_some() && args["height"].as_i64().is_some();
        let (src_x, src_y, cap_w, cap_h) = if crop {
            let cx = args["x"].as_i64().unwrap() as i32;
            let cy = args["y"].as_i64().unwrap() as i32;
            let cw = args["width"].as_i64().unwrap() as i32;
            let ch = args["height"].as_i64().unwrap() as i32;
            // Clamp to screen bounds
            let cx = cx.max(0).min(screen_w);
            let cy = cy.max(0).min(screen_h);
            let cw = cw.min(screen_w - cx).max(1);
            let ch = ch.min(screen_h - cy).max(1);
            (cx, cy, cw, ch)
        } else {
            (0, 0, screen_w, screen_h)
        };

        let screen_dc = GetDC(0);
        if screen_dc == 0 {
            return Err("GetDC failed".to_string());
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        let bitmap = CreateCompatibleBitmap(screen_dc, cap_w, cap_h);
        let old = SelectObject(mem_dc, bitmap);
        BitBlt(mem_dc, 0, 0, cap_w, cap_h, screen_dc, src_x, src_y, SRCCOPY);

        // BITMAPINFOHEADER (40 bytes)
        let mut bmi = [0u8; 44];
        bmi[0] = 40; // biSize = 40
        bmi[4..8].copy_from_slice(&cap_w.to_le_bytes());
        let neg_h = (-cap_h).to_le_bytes(); // top-down
        bmi[8..12].copy_from_slice(&neg_h);
        bmi[12..14].copy_from_slice(&1u16.to_le_bytes()); // biPlanes
        bmi[14..16].copy_from_slice(&32u16.to_le_bytes()); // biBitCount

        let mut pixels = vec![0u8; (cap_w * cap_h * 4) as usize];
        let ret = GetDIBits(mem_dc, bitmap, 0, cap_h as u32, pixels.as_mut_ptr(), bmi.as_mut_ptr(), 0);

        SelectObject(mem_dc, old);
        DeleteObject(bitmap);
        DeleteDC(mem_dc);
        ReleaseDC(0, screen_dc);

        if ret == 0 {
            return Err("GetDIBits failed".to_string());
        }

        // Convert BGRA to RGBA
        let mut rgba = Vec::with_capacity(pixels.len());
        for chunk in pixels.chunks(4) {
            rgba.push(chunk[2]); // R
            rgba.push(chunk[1]); // G
            rgba.push(chunk[0]); // B
            rgba.push(255);      // A
        }

        let mut png = Vec::new();
        repng::encode(&mut png, cap_w as _, cap_h as _, &rgba)
            .map_err(|e| format!("PNG encode error: {}", e))?;

        let b64 = hbb_common::base64::engine::general_purpose::STANDARD.encode(&png);

        Ok(json!([{
            "type": "image",
            "data": b64,
            "mimeType": "image/png"
        }]))
    }
}

fn screenshot_scrap(args: &Value) -> Result<Value, String> {
    use scrap::{Capturer, Display, TraitCapturer, TraitPixelBuffer};
    use hbb_common::base64::Engine;

    let display_idx = args["display"].as_u64().unwrap_or(0) as usize;

    let displays = Display::all().map_err(|e| format!("Failed to get displays: {}", e))?;
    if display_idx >= displays.len() {
        return Err(format!("Display {} not found (have {})", display_idx, displays.len()));
    }
    let display = displays.into_iter().nth(display_idx).unwrap();
    let w = display.width();
    let h = display.height();

    let mut capturer = Capturer::new(display).map_err(|e| format!("Failed to create capturer: {}", e))?;

    // Try to capture a frame (may need several attempts for DXGI init)
    let mut frame_data = None;
    for _attempt in 0..30 {
        match capturer.frame(Duration::from_millis(200)) {
            Ok(frame) => {
                let pixbuf = match &frame {
                    scrap::Frame::PixelBuffer(pb) => pb,
                    _ => return Err("GPU texture frames not supported in MCP mode".to_string()),
                };
                // Convert BGRA to RGBA
                let bgra = pixbuf.data();
                let stride = pixbuf.stride()[0];
                let mut rgba = Vec::with_capacity(w * h * 4);
                for y in 0..h {
                    for x in 0..w {
                        let i = stride * y + 4 * x;
                        if i + 3 < bgra.len() {
                            rgba.push(bgra[i + 2]); // R
                            rgba.push(bgra[i + 1]); // G
                            rgba.push(bgra[i]);     // B
                            rgba.push(bgra[i + 3]); // A
                        }
                    }
                }
                frame_data = Some(rgba);
                break;
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(format!("Capture error: {}", e)),
        }
    }

    let rgba = frame_data.ok_or("Failed to capture frame after retries. The desktop may be locked or inaccessible.")?;

    // Encode as PNG
    let mut png = Vec::new();
    repng::encode(&mut png, w as _, h as _, &rgba)
        .map_err(|e| format!("PNG encode error: {}", e))?;

    // Base64 encode
    let b64 = hbb_common::base64::engine::general_purpose::STANDARD.encode(&png);

    Ok(json!([{
        "type": "image",
        "data": b64,
        "mimeType": "image/png"
    }]))
}

fn tool_get_window_list() -> Result<Value, String> {
    let windows = get_window_list_platform()?;
    let text = serde_json::to_string_pretty(&windows).unwrap_or_default();
    Ok(json!([{ "type": "text", "text": text }]))
}

fn tool_mouse_click(args: &Value) -> Result<Value, String> {
    use enigo::{Enigo, MouseButton, MouseControllable};

    let x = args["x"].as_i64().ok_or("Missing x")? as i32;
    let y = args["y"].as_i64().ok_or("Missing y")? as i32;
    let button_str = args["button"].as_str().unwrap_or("left");
    let double = args["double_click"].as_bool().unwrap_or(false);

    let button = match button_str {
        "right" => MouseButton::Right,
        "middle" => MouseButton::Middle,
        _ => MouseButton::Left,
    };

    let mut enigo = Enigo::new();
    enigo.mouse_move_to(x, y);
    std::thread::sleep(Duration::from_millis(10));
    enigo.mouse_click(button);
    if double {
        std::thread::sleep(Duration::from_millis(50));
        enigo.mouse_click(button);
    }

    Ok(json!([{ "type": "text", "text": format!("Clicked ({}, {}) {}{}", x, y, button_str, if double { " (double)" } else { "" }) }]))
}

fn tool_mouse_move(args: &Value) -> Result<Value, String> {
    use enigo::{Enigo, MouseControllable};

    let x = args["x"].as_i64().ok_or("Missing x")? as i32;
    let y = args["y"].as_i64().ok_or("Missing y")? as i32;

    let mut enigo = Enigo::new();
    enigo.mouse_move_to(x, y);

    Ok(json!([{ "type": "text", "text": format!("Moved mouse to ({}, {})", x, y) }]))
}

fn tool_type_text(args: &Value) -> Result<Value, String> {
    use enigo::{Enigo, KeyboardControllable};

    let text = args["text"].as_str().ok_or("Missing text")?;
    let mut enigo = Enigo::new();
    enigo.key_sequence(text);

    Ok(json!([{ "type": "text", "text": format!("Typed {} characters", text.len()) }]))
}

fn tool_key_press(args: &Value) -> Result<Value, String> {
    use enigo::{Enigo, Key, KeyboardControllable};

    let key_str = args["key"].as_str().ok_or("Missing key")?;
    let modifiers = args["modifiers"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();

    let key = match key_str.to_lowercase().as_str() {
        "return" | "enter" => Key::Return,
        "escape" | "esc" => Key::Escape,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "delete" => Key::Delete,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "space" => Key::Space,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        s if s.len() == 1 => Key::Layout(s.chars().next().unwrap()),
        _ => return Err(format!("Unknown key: {}", key_str)),
    };

    let mut enigo = Enigo::new();

    // Press modifiers
    for m in &modifiers {
        let _ = match m.to_lowercase().as_str() {
            "ctrl" | "control" => enigo.key_down(Key::Control),
            "alt" => enigo.key_down(Key::Alt),
            "shift" => enigo.key_down(Key::Shift),
            "meta" | "super" | "win" | "cmd" => enigo.key_down(Key::Meta),
            _ => Ok(()),
        };
    }

    enigo.key_click(key);

    // Release modifiers in reverse
    for m in modifiers.iter().rev() {
        match m.to_lowercase().as_str() {
            "ctrl" | "control" => enigo.key_up(Key::Control),
            "alt" => enigo.key_up(Key::Alt),
            "shift" => enigo.key_up(Key::Shift),
            "meta" | "super" | "win" | "cmd" => enigo.key_up(Key::Meta),
            _ => {}
        }
    }

    Ok(json!([{ "type": "text", "text": format!("Pressed {}{}", if modifiers.is_empty() { String::new() } else { format!("{}+", modifiers.join("+")) }, key_str) }]))
}

// --- Agent-friendly tools ---

fn tool_get_device_info() -> Result<Value, String> {
    let id = hbb_common::config::Config::get_id();
    let version = env!("CARGO_PKG_VERSION");
    let platform = std::env::consts::OS;
    let hostname = crate::common::hostname();
    let status = crate::ui_interface::get_connect_status();

    let status_text = match status.status_num {
        1 => "online",
        0 => "connecting",
        _ => "offline",
    };

    let info = json!({
        "device_id": id,
        "version": version,
        "platform": platform,
        "hostname": hostname,
        "status": status_text,
        "status_num": status.status_num
    });

    Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&info).unwrap_or_default() }]))
}

fn tool_list_peers(args: &Value) -> Result<Value, String> {
    let filter = args["filter"].as_str().unwrap_or("all");
    let mut result = json!({});

    if filter == "recent" || filter == "all" {
        let peers: Vec<Value> = PeerConfig::peers(None)
            .into_iter()
            .map(|(id, modified, config)| {
                let last_seen = modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                json!({
                    "id": id,
                    "username": config.info.username,
                    "hostname": config.info.hostname,
                    "platform": config.info.platform,
                    "alias": config.options.get("alias").unwrap_or(&String::new()).clone(),
                    "last_seen_epoch": last_seen
                })
            })
            .collect();
        result["recent"] = json!(peers);
    }

    if filter == "favorites" || filter == "all" {
        let favs = crate::ui_interface::get_fav();
        result["favorites"] = json!(favs);
    }

    if filter == "lan" || filter == "all" {
        let lan = crate::ui_interface::get_lan_peers();
        let lan_json: Vec<Value> = lan.into_iter().map(|p| {
            json!({
                "id": p.get("id").cloned().unwrap_or_default(),
                "username": p.get("username").cloned().unwrap_or_default(),
                "hostname": p.get("hostname").cloned().unwrap_or_default(),
                "platform": p.get("platform").cloned().unwrap_or_default(),
            })
        }).collect();
        result["lan"] = json!(lan_json);
    }

    Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]))
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn tool_connect_to_peer(args: &Value) -> Result<Value, String> {
    let peer_id = args["peer_id"].as_str().ok_or("Missing peer_id")?;
    let conn_type = args["connection_type"].as_str().unwrap_or("connect");
    let password = args["password"].as_str().map(|s| s.to_string());

    match conn_type {
        "connect" | "file-transfer" | "port-forward" => {}
        _ => return Err(format!("Invalid connection_type: {}. Use: connect, file-transfer, port-forward", conn_type)),
    }

    crate::ui::new_remote(
        peer_id.to_string(),
        conn_type.to_string(),
        false,
        None,
        password,
    );

    Ok(json!([{ "type": "text", "text": format!("Connection initiated to {} (type: {})", peer_id, conn_type) }]))
}

#[cfg(any(feature = "flutter", feature = "cli"))]
fn tool_connect_to_peer(_args: &Value) -> Result<Value, String> {
    Err("connect_to_peer not available in this build".to_string())
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn tool_disconnect_peer(args: &Value) -> Result<Value, String> {
    let peer_id = args["peer_id"].as_str().ok_or("Missing peer_id")?;
    crate::ui::close_remote_connection(peer_id);
    Ok(json!([{ "type": "text", "text": format!("Disconnected peer {}", peer_id) }]))
}

#[cfg(any(feature = "flutter", feature = "cli"))]
fn tool_disconnect_peer(_args: &Value) -> Result<Value, String> {
    Err("disconnect_peer not available in this build".to_string())
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn tool_list_active_connections() -> Result<Value, String> {
    let connections: Vec<Value> = crate::ui::list_active_connections()
        .into_iter()
        .map(|(id, conn_type, alive)| {
            // Check if remote connection is actually established (not just process alive)
            let connected = if alive {
                query_child_state(&id)
                    .map(|s| s["video"]["width"].as_i64().unwrap_or(0) > 0)
                    .unwrap_or(false)
            } else {
                false
            };
            json!({
                "peer_id": id,
                "connection_type": conn_type,
                "alive": alive,
                "connected": connected
            })
        })
        .collect();

    Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&connections).unwrap_or_default() }]))
}

#[cfg(any(feature = "flutter", feature = "cli"))]
fn tool_list_active_connections() -> Result<Value, String> {
    Err("list_active_connections not available in this build".to_string())
}

// --- UI State tool ---

#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn tool_get_ui_state(args: &Value) -> Result<Value, String> {
    let filter_peer = args["peer_id"].as_str();
    let connections = crate::ui::list_active_connections();
    let windows = get_window_list_platform()?;
    let windows_arr = windows.as_array().cloned().unwrap_or_default();

    // Find main HopToDesk window
    let main_win = windows_arr.iter().find(|w| {
        w["title"].as_str() == Some("HopToDesk")
    }).cloned();

    // Query child processes for rich state data
    let mut state_files: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    for (pid, _, alive) in &connections {
        if !alive { continue; }
        if let Some(filter) = filter_peer {
            if pid != filter { continue; }
        }
        if let Some(state) = query_child_state(pid) {
            state_files.insert(pid.clone(), state);
        }
    }

    // Build state for each active connection
    let mut conn_states = Vec::new();
    for (peer_id, conn_type, alive) in &connections {
        if let Some(filter) = filter_peer {
            if peer_id != filter { continue; }
        }

        let win = windows_arr.iter().find(|w| {
            w["title"].as_str() == Some(peer_id.as_str())
        }).cloned();

        let toolbar = get_toolbar_buttons(conn_type);

        // Merge the rich state from child process if available
        // Determine if remote connection is actually established (not just process alive)
        let connected = if let Some(rich_state) = state_files.get(peer_id) {
            // Connection is established if we have video dimensions from the remote
            rich_state["video"]["width"].as_i64().unwrap_or(0) > 0
        } else {
            false
        };

        let mut conn_state = json!({
            "peer_id": peer_id,
            "connection_type": conn_type,
            "alive": alive,
            "connected": connected,
            "window": win,
            "toolbar": toolbar,
        });

        if let Some(rich_state) = state_files.get(peer_id) {
            conn_state["state"] = rich_state.clone();
        }

        conn_states.push(conn_state);
    }

    let result = json!({
        "main_window": main_win,
        "connections": conn_states,
    });

    Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]))
}

#[cfg(any(feature = "flutter", feature = "cli"))]
fn tool_get_ui_state(_args: &Value) -> Result<Value, String> {
    Err("get_ui_state not available in this build".to_string())
}

// --- Wait for Event tool ---

fn tool_wait_for_event(args: &Value) -> Result<Value, String> {
    let event = args["event"].as_str().ok_or("Missing event")?;
    let peer_id = args["peer_id"].as_str().ok_or("Missing peer_id")?;
    let timeout_ms = args["timeout_ms"].as_u64().unwrap_or(30000).min(60000);

    let start = std::time::Instant::now();
    let poll_interval = std::time::Duration::from_millis(500);

    loop {
        if start.elapsed().as_millis() as u64 >= timeout_ms {
            return Ok(json!([{ "type": "text", "text": json!({
                "event": event,
                "peer_id": peer_id,
                "status": "timeout",
                "elapsed_ms": start.elapsed().as_millis() as u64,
                "message": format!("Timed out waiting for '{}' after {}ms", event, timeout_ms)
            }).to_string() }]));
        }

        #[cfg(not(any(feature = "flutter", feature = "cli")))]
        {
            let connections = crate::ui::list_active_connections();
            let conn = connections.iter().find(|(pid, _, _)| pid == peer_id);

            match event {
                "connection_ready" => {
                    if let Some((_, _, alive)) = conn {
                        if *alive {
                            // Connection exists and is alive — query state to check if video is active
                            if let Some(state) = query_child_state(peer_id) {
                                let video_w = state["video"]["width"].as_i64().unwrap_or(0);
                                if video_w > 0 {
                                    return Ok(json!([{ "type": "text", "text": json!({
                                        "event": "connection_ready",
                                        "peer_id": peer_id,
                                        "status": "fired",
                                        "elapsed_ms": start.elapsed().as_millis() as u64,
                                        "state": state,
                                    }).to_string() }]));
                                }
                            }
                        }
                    }
                }
                "connection_closed" => {
                    if conn.is_none() || conn.map_or(false, |(_, _, alive)| !alive) {
                        return Ok(json!([{ "type": "text", "text": json!({
                            "event": "connection_closed",
                            "peer_id": peer_id,
                            "status": "fired",
                            "elapsed_ms": start.elapsed().as_millis() as u64,
                        }).to_string() }]));
                    }
                }
                "dialog_appeared" => {
                    if conn.is_some() {
                        if let Some(state) = query_child_state(peer_id) {
                            if state["ui"]["dialog_open"].as_bool() == Some(true) {
                                return Ok(json!([{ "type": "text", "text": json!({
                                    "event": "dialog_appeared",
                                    "peer_id": peer_id,
                                    "status": "fired",
                                    "elapsed_ms": start.elapsed().as_millis() as u64,
                                    "dialog_type": state["ui"]["dialog_type"],
                                    "state": state,
                                }).to_string() }]));
                            }
                        }
                    }
                }
                "dialog_dismissed" => {
                    if conn.is_some() {
                        if let Some(state) = query_child_state(peer_id) {
                            if state["ui"]["dialog_open"].as_bool() != Some(true) {
                                return Ok(json!([{ "type": "text", "text": json!({
                                    "event": "dialog_dismissed",
                                    "peer_id": peer_id,
                                    "status": "fired",
                                    "elapsed_ms": start.elapsed().as_millis() as u64,
                                    "state": state,
                                }).to_string() }]));
                            }
                        }
                    }
                }
                "quality_stable" => {
                    if conn.is_some() {
                        if let Some(state) = query_child_state(peer_id) {
                            let fps = state["connection"]["quality"]["fps"].as_i64().unwrap_or(0);
                            let delay = state["connection"]["quality"]["delay_ms"].as_i64().unwrap_or(9999);
                            if fps > 0 && delay < 500 {
                                return Ok(json!([{ "type": "text", "text": json!({
                                    "event": "quality_stable",
                                    "peer_id": peer_id,
                                    "status": "fired",
                                    "elapsed_ms": start.elapsed().as_millis() as u64,
                                    "fps": fps,
                                    "delay_ms": delay,
                                    "state": state,
                                }).to_string() }]));
                            }
                        }
                    }
                }
                _ => return Err(format!("Unknown event: {}", event)),
            }
        }

        #[cfg(any(feature = "flutter", feature = "cli"))]
        return Err("wait_for_event not available in this build".to_string());

        std::thread::sleep(poll_interval);
    }
}

/// Query a child connection's UI state via the temp file mechanism.
/// Sends "query_state" to child, waits up to 1s for the state file.
#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn query_child_state(peer_id: &str) -> Option<Value> {
    let path = std::env::temp_dir().join(format!("hoptodesk-mcp-state-{}.json", peer_id));
    let _ = std::fs::remove_file(&path);
    crate::ui::send_to_child(peer_id, "query_state");

    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < 1000 {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(parsed) = serde_json::from_str::<Value>(&content) {
                let _ = std::fs::remove_file(&path);
                return Some(parsed);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    None
}

// --- Dismiss Dialog tool ---

fn tool_dismiss_dialog(args: &Value) -> Result<Value, String> {
    let peer_id = args["peer_id"].as_str().ok_or("Missing peer_id")?;

    #[cfg(not(any(feature = "flutter", feature = "cli")))]
    {
        if peer_id == "all" {
            let count = crate::ui::send_dismiss_to_all_children();
            return Ok(json!([{ "type": "text", "text": format!("Dismiss signal sent to {} connections", count) }]));
        }
        if crate::ui::send_to_child(peer_id, "dismiss") {
            return Ok(json!([{ "type": "text", "text": format!("Dismiss signal sent for peer {}", peer_id) }]));
        }
        return Err(format!("No active connection found for peer {}", peer_id));
    }
    #[cfg(any(feature = "flutter", feature = "cli"))]
    return Err("dismiss_dialog not available in this build".to_string());
}

// --- Click Toolbar Button tool ---

fn tool_click_toolbar_button(args: &Value) -> Result<Value, String> {
    let peer_id = args["peer_id"].as_str().ok_or("Missing peer_id")?;
    let button_id = args["button_id"].as_str().ok_or("Missing button_id")?;

    // Validate: either a top-level button ID or "menu:item" format for submenu items
    let valid_buttons = ["fullscreen", "chat", "action", "display", "keyboard",
                         "recording", "securitycode", "transfer-file", "remote-print",
                         "screenshot", "switch-sides", "privacy-mode"];
    let valid_action_items = ["request-elevation", "os-password", "tunnel", "ctrl-alt-del",
                              "restart_remote_device", "lock-screen", "block-input",
                              "take-screenshot", "refresh"];
    let valid_display_items = ["original", "shrink", "stretch", "adaptive", "best", "balanced",
                               "low", "custom", "show-remote-cursor", "disable-audio",
                               "disable-clipboard", "lock-after-session-end", "privacy-mode",
                               "show-quality-monitor"];
    let valid_keyboard_items = ["auto", "map", "translate"];

    let is_valid = if let Some(colon) = button_id.find(':') {
        let menu = &button_id[..colon];
        let item = &button_id[colon + 1..];
        match menu {
            "action" => valid_action_items.contains(&item),
            "display" => valid_display_items.contains(&item),
            "keyboard" => valid_keyboard_items.contains(&item),
            _ => false,
        }
    } else {
        valid_buttons.contains(&button_id)
    };

    if !is_valid {
        return Err(format!(
            "Invalid button_id '{}'. Use a button ID (e.g. 'chat') or 'menu:item' for submenus (e.g. 'display:shrink', 'action:ctrl-alt-del', 'keyboard:map')",
            button_id
        ));
    }

    #[cfg(not(any(feature = "flutter", feature = "cli")))]
    {
        let cmd = format!("click_button:{}", button_id);
        if crate::ui::send_to_child(peer_id, &cmd) {
            return Ok(json!([{ "type": "text", "text": format!("Clicked '{}' on peer {}", button_id, peer_id) }]));
        }
        return Err(format!("No active connection found for peer {}", peer_id));
    }
    #[cfg(any(feature = "flutter", feature = "cli"))]
    return Err("click_toolbar_button not available in this build".to_string());
}

// --- Test Print tool ---

fn tool_test_print(args: &Value) -> Result<Value, String> {
    #[cfg(target_os = "windows")]
    {
        use hbb_common::config::{keys, LocalConfig};

        let text = args["text"].as_str().unwrap_or("HopToDesk Remote Print Test\n\nThis is a test page to verify remote printing works.\nTimestamp: ");
        let printer = LocalConfig::get_option(keys::OPTION_PRINTER_SELECTED_NAME);
        if printer.is_empty() {
            return Err("No printer configured. Use set_remote_printer first.".to_string());
        }

        // Use GDI printing to send a text page to the printer
        let result = print_text_to_printer(&printer, text);
        match result {
            Ok(path) => Ok(json!([{ "type": "text", "text": format!("Test print job sent to '{}'. Output: {}", printer, path) }])),
            Err(e) => Err(format!("Print failed: {}", e)),
        }
    }
    #[cfg(not(target_os = "windows"))]
    Err("test_print is only available on Windows".to_string())
}

#[cfg(target_os = "windows")]
fn print_text_to_printer(printer_name: &str, text: &str) -> Result<String, String> {
    use std::ptr;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    type HANDLE = *mut std::ffi::c_void;

    #[repr(C)]
    #[allow(non_snake_case)]
    struct DOCINFOW {
        cbSize: i32,
        lpszDocName: *const u16,
        lpszOutput: *const u16,
        lpszDatatype: *const u16,
        fwType: u32,
    }

    #[repr(C)]
    struct RECT { left: i32, top: i32, right: i32, bottom: i32 }

    #[allow(clashing_extern_declarations)]
    extern "system" {
        fn CreateDCW(driver: *const u16, device: *const u16, port: *const u16, devmode: *const u8) -> HANDLE;
        fn DeleteDC(hdc: HANDLE) -> i32;
        fn StartDocW(hdc: HANDLE, lpdi: *const DOCINFOW) -> i32;
        fn EndDoc(hdc: HANDLE) -> i32;
        fn StartPage(hdc: HANDLE) -> i32;
        fn EndPage(hdc: HANDLE) -> i32;
        fn DrawTextW(hdc: HANDLE, text: *const u16, count: i32, rect: *mut RECT, format: u32) -> i32;
        fn GetDeviceCaps(hdc: HANDLE, index: i32) -> i32;
    }

    fn to_wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    // Determine output path for PDF printers
    let output_path = if printer_name.to_lowercase().contains("print to pdf") {
        let docs = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Public".into());
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Some(format!("{}\\Documents\\HopToDesk_Print_{}.pdf", docs, ts))
    } else {
        None
    };

    unsafe {
        let printer_wide = to_wide(printer_name);
        let hdc = CreateDCW(ptr::null(), printer_wide.as_ptr(), ptr::null(), ptr::null());
        if hdc.is_null() {
            return Err("Failed to create printer DC".to_string());
        }

        let doc_name = to_wide("HopToDesk Test Print");
        let output_wide;
        let output_ptr = if let Some(ref path) = output_path {
            output_wide = to_wide(path);
            output_wide.as_ptr()
        } else {
            ptr::null()
        };

        let di = DOCINFOW {
            cbSize: std::mem::size_of::<DOCINFOW>() as i32,
            lpszDocName: doc_name.as_ptr(),
            lpszOutput: output_ptr,
            lpszDatatype: ptr::null(),
            fwType: 0,
        };

        if StartDocW(hdc, &di) <= 0 {
            DeleteDC(hdc);
            return Err("StartDoc failed".to_string());
        }

        if StartPage(hdc) <= 0 {
            EndDoc(hdc);
            DeleteDC(hdc);
            return Err("StartPage failed".to_string());
        }

        // Get page dimensions
        let page_w = GetDeviceCaps(hdc, 8); // HORZRES
        let page_h = GetDeviceCaps(hdc, 10); // VERTRES

        // Build the full text with timestamp
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let full_text = format!("{}{}", text, ts);
        let text_wide = to_wide(&full_text);

        let mut rect = RECT {
            left: 100,
            top: 100,
            right: page_w - 100,
            bottom: page_h - 100,
        };

        // DT_WORDBREAK = 0x10
        DrawTextW(hdc, text_wide.as_ptr(), -1, &mut rect, 0x10);

        EndPage(hdc);
        EndDoc(hdc);
        DeleteDC(hdc);
    }

    Ok(output_path.unwrap_or_else(|| "sent to printer spool".to_string()))
}

// --- Set Remote Printer tool ---

fn tool_set_remote_printer(args: &Value) -> Result<Value, String> {
    #[cfg(target_os = "windows")]
    {
        use hbb_common::config::{keys, LocalConfig};

        let printers = crate::platform::get_printers();
        let printer_name = args["printer_name"].as_str().unwrap_or("");
        let auto_print = args["auto_print"].as_bool().unwrap_or(true);

        // If no printer_name, just list available printers
        if printer_name.is_empty() {
            let current = LocalConfig::get_option(keys::OPTION_PRINTER_SELECTED_NAME);
            let auto_enabled = LocalConfig::get_option(keys::OPTION_PRINTER_ALLOW_AUTO_PRINT) == "Y";
            let result = json!({
                "printers": printers,
                "current_printer": current,
                "auto_print_enabled": auto_enabled,
            });
            return Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]));
        }

        // Handle "auto" — select the only printer
        let selected = if printer_name == "auto" {
            if printers.is_empty() {
                return Err("No printers available".to_string());
            }
            if printers.len() > 1 {
                return Err(format!("Multiple printers found ({}), specify one: {:?}", printers.len(), printers));
            }
            printers[0].clone()
        } else {
            // Validate printer exists
            if !printers.iter().any(|p| p == printer_name) {
                return Err(format!("Printer '{}' not found. Available: {:?}", printer_name, printers));
            }
            printer_name.to_string()
        };

        // Save settings
        LocalConfig::set_option(keys::OPTION_PRINTER_SELECTED_NAME.to_string(), selected.clone());
        if auto_print {
            LocalConfig::set_option(keys::OPTION_PRINTER_ALLOW_AUTO_PRINT.to_string(), "Y".to_string());
        } else {
            LocalConfig::set_option(keys::OPTION_PRINTER_ALLOW_AUTO_PRINT.to_string(), "".to_string());
        }

        let msg = format!("Printer set to '{}', auto-print: {}. Remote print jobs will now be sent directly to this printer.", selected, if auto_print { "enabled" } else { "disabled" });
        Ok(json!([{ "type": "text", "text": msg }]))
    }
    #[cfg(not(target_os = "windows"))]
    Err("set_remote_printer is only available on Windows".to_string())
}

/// Get toolbar button list for a connection. Use click_toolbar_button tool to interact.
fn get_toolbar_buttons(conn_type: &str) -> Value {
    let is_file_transfer = conn_type == "file-transfer";
    let is_view_camera = conn_type == "view-camera";

    let mut buttons = Vec::new();
    // Standard buttons for a remote desktop session
    if !is_file_transfer {
        buttons.push("fullscreen");
    }
    if !is_view_camera && !is_file_transfer {
        buttons.push("chat");
        buttons.push("action");
    }
    if !is_file_transfer {
        buttons.push("display");
    }
    if !is_view_camera && !is_file_transfer {
        buttons.push("keyboard");
        buttons.push("transfer-file");
        buttons.push("remote-print");
        buttons.push("screenshot");
    }

    let items: Vec<Value> = buttons.iter().map(|id| {
        let mut item = json!({
            "id": id,
            "label": get_button_label(id),
        });
        add_submenu_info(&mut item, id);
        item
    }).collect();

    json!({
        "buttons": items,
        "how_to_click": "Use click_toolbar_button with button_id for top-level buttons, or 'menu:item' for submenus (e.g. 'display:shrink', 'action:ctrl-alt-del', 'keyboard:map').",
        "keyboard_shortcut_print": "Ctrl+P opens Remote Print dialog",
    })
}

fn get_button_label(id: &str) -> &str {
    match id {
        "fullscreen" => "Full Screen",
        "screens" => "Display Selector (security + remote ID + monitor buttons)",
        "chat" => "Chat",
        "action" => "Control Actions (lightning bolt)",
        "display" => "Display Settings (monitor icon)",
        "keyboard" => "Keyboard mode",
        "recording" => "Recording (red dot toggle)",
        "securitycode" => "Security Code (shield)",
        "transfer-file" => "Transfer File",
        "remote-print" => "Remote Print (printer icon)",
        "screenshot" => "Screenshot (camera icon, copies to clipboard)",
        "switch-sides" => "Switch Sides",
        "privacy-mode" => "Privacy Mode (eye icon toggle)",
        _ => id,
    }
}

fn add_submenu_info(item: &mut Value, id: &str) {
    match id {
        "action" => {
            item["submenu"] = json!([
                {"id": "request-elevation", "label": "Request Elevation (Windows remote only)"},
                {"id": "os-password", "label": "OS Password"},
                {"id": "tunnel", "label": "TCP Tunneling"},
                {"id": "ctrl-alt-del", "label": "Insert Ctrl+Alt+Del"},
                {"id": "restart_remote_device", "label": "Restart Remote Device"},
                {"id": "lock-screen", "label": "Lock Screen"},
                {"id": "block-input", "label": "Block user input (Windows remote only)"},
                {"id": "take-screenshot", "label": "Take screenshot (saves to file, shows dialog)"},
                {"id": "refresh", "label": "Refresh video"}
            ]);
            item["tip"] = json!("Use click_toolbar_button with button_id 'menu:item' format, e.g. 'action:ctrl-alt-del'");
        }
        "display" => {
            item["submenu"] = json!([
                {"id": "original", "label": "Original", "type": "view-style"},
                {"id": "shrink", "label": "Shrink", "type": "view-style"},
                {"id": "stretch", "label": "Stretch", "type": "view-style"},
                {"id": "best", "label": "Good image quality", "type": "image-quality"},
                {"id": "balanced", "label": "Balanced", "type": "image-quality"},
                {"id": "low", "label": "Optimize reaction time", "type": "image-quality"},
                {"id": "custom", "label": "Custom quality", "type": "image-quality"},
                {"id": "show-remote-cursor", "label": "Show remote cursor", "type": "toggle"},
                {"id": "disable-audio", "label": "Mute", "type": "toggle"},
                {"id": "disable-clipboard", "label": "Disable clipboard", "type": "toggle"},
                {"id": "lock-after-session-end", "label": "Lock after session end", "type": "toggle"}
            ]);
            item["tip"] = json!("Use click_toolbar_button with button_id 'menu:item' format, e.g. 'display:shrink'");
        }
        "keyboard" => {
            item["submenu"] = json!([
                {"id": "auto", "label": "Auto (default)"},
                {"id": "map", "label": "Same layout"},
                {"id": "translate", "label": "Different layout"}
            ]);
            item["tip"] = json!("Use click_toolbar_button with button_id 'menu:item' format, e.g. 'keyboard:auto'");
        }
        _ => {}
    }
}

// Platform-specific window list
#[cfg(windows)]
fn get_window_list_platform() -> Result<Value, String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    extern "system" {
        fn EnumWindows(cb: extern "system" fn(isize, isize) -> i32, lparam: isize) -> i32;
        fn GetWindowTextW(hwnd: isize, buf: *mut u16, max: i32) -> i32;
        fn GetWindowRect(hwnd: isize, rect: *mut [i32; 4]) -> i32;
        fn IsWindowVisible(hwnd: isize) -> i32;
    }

    static mut WINDOWS: Vec<Value> = Vec::new();

    extern "system" fn enum_cb(hwnd: isize, _: isize) -> i32 {
        unsafe {
            if IsWindowVisible(hwnd) == 0 {
                return 1;
            }
            let mut buf = [0u16; 512];
            let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), 512);
            if len <= 0 {
                return 1;
            }
            let title = OsString::from_wide(&buf[..len as usize])
                .to_string_lossy()
                .to_string();
            if title.is_empty() {
                return 1;
            }
            let mut rect = [0i32; 4];
            GetWindowRect(hwnd, &mut rect);
            WINDOWS.push(json!({
                "title": title,
                "x": rect[0],
                "y": rect[1],
                "width": rect[2] - rect[0],
                "height": rect[3] - rect[1]
            }));
        }
        1
    }

    unsafe {
        WINDOWS = Vec::new();
        EnumWindows(enum_cb, 0);
        Ok(json!(WINDOWS.clone()))
    }
}

#[cfg(target_os = "macos")]
fn get_window_list_platform() -> Result<Value, String> {
    use std::process::Command;
    let output = Command::new("osascript")
        .arg("-e")
        .arg("tell application \"System Events\" to get {name, position, size} of every window of every process whose visible is true")
        .output()
        .map_err(|e| format!("osascript error: {}", e))?;
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(json!([{ "type": "text", "text": text }]))
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn get_window_list_platform() -> Result<Value, String> {
    use std::process::Command;
    let output = Command::new("wmctrl")
        .arg("-lG")
        .output()
        .map_err(|e| format!("wmctrl error (install with: pkg install wmctrl): {}", e))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut windows = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.splitn(8, char::is_whitespace).collect();
        if parts.len() >= 8 {
            windows.push(json!({
                "title": parts[7].trim(),
                "x": parts[2].trim().parse::<i32>().unwrap_or(0),
                "y": parts[3].trim().parse::<i32>().unwrap_or(0),
                "width": parts[4].trim().parse::<i32>().unwrap_or(0),
                "height": parts[5].trim().parse::<i32>().unwrap_or(0)
            }));
        }
    }
    Ok(json!(windows))
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux", target_os = "freebsd")))]
fn get_window_list_platform() -> Result<Value, String> {
    Err("Window listing not supported on this platform".to_string())
}

// ─── Remote Exec Operations ─────────────────────────────────────────────────

fn tool_exec_operation(args: &Value) -> Result<Value, String> {
    let operation = args["operation"].as_str().unwrap_or("");
    if operation.is_empty() {
        return Err("operation is required".to_string());
    }

    // Check if device is enrolled with a dashboard (authorization is enforced dashboard-side)
    let dashboard_user_id = hbb_common::config::Config::get_option("dashboard_user_id");
    if dashboard_user_id.is_empty() {
        return Err("Remote execution requires dashboard enrollment".to_string());
    }

    eprintln!("[exec] Running operation: {}", operation);

    let result = match operation {
        "get_system_info" => exec_get_system_info(),
        "disk_usage" => exec_disk_usage(),
        "list_processes" => exec_list_processes(args),
        "network_info" => exec_network_info(),
        "get_service_logs" => exec_get_service_logs(args),
        "ping_test" => exec_ping_test(args),
        "installed_software" => exec_installed_software(),
        "restart_hoptodesk" => exec_restart_hoptodesk(),
        "kill_process" => exec_kill_process(args),
        "flush_dns" => exec_flush_dns(),
        "reboot_device" => exec_reboot_device(),
        "clear_temp_files" => exec_clear_temp_files(),
        "run_update_check" => exec_run_update_check(),
        _ => Err(format!("Unknown operation: {}", operation)),
    };

    match result {
        Ok(output) => {
            let response = json!({
                "operation": operation,
                "exit_code": 0,
                "output": output
            });
            Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&response).unwrap_or_default() }]))
        }
        Err(msg) => {
            let response = json!({
                "operation": operation,
                "exit_code": 1,
                "output": msg
            });
            Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&response).unwrap_or_default() }]))
        }
    }
}

/// Run a shell command with timeout and capture output
fn run_command(cmd: &str, args: &[&str], _timeout_secs: u64) -> Result<String, String> {
    use std::process::Command;

    let output = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("Failed to execute {}: {}", cmd, e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        Ok(if stdout.is_empty() { stderr } else { stdout })
    } else {
        if !stderr.is_empty() {
            Err(format!("Exit code {}: {}", output.status.code().unwrap_or(-1), stderr.trim()))
        } else {
            Err(format!("Exit code {}", output.status.code().unwrap_or(-1)))
        }
    }
}

fn exec_get_system_info() -> Result<String, String> {
    #[cfg(windows)]
    {
        let mut info = String::new();
        if let Ok(o) = run_command("cmd", &["/C", "systeminfo"], 15) {
            info.push_str(&o);
        }
        Ok(info)
    }
    #[cfg(target_os = "macos")]
    {
        let mut info = String::new();
        if let Ok(o) = run_command("sw_vers", &[], 5) { info.push_str(&o); info.push('\n'); }
        if let Ok(o) = run_command("sysctl", &["-n", "hw.memsize"], 5) { info.push_str(&format!("Memory: {} bytes\n", o.trim())); }
        if let Ok(o) = run_command("uptime", &[], 5) { info.push_str(&o); }
        Ok(info)
    }
    #[cfg(target_os = "freebsd")]
    {
        let mut info = String::new();
        if let Ok(o) = run_command("uname", &["-a"], 5) { info.push_str(&o); }
        if let Ok(o) = run_command("sh", &["-c", "sysctl hw.physmem hw.usermem hw.ncpu 2>/dev/null"], 5) { info.push_str(&o); }
        if let Ok(o) = run_command("uptime", &[], 5) { info.push_str(&o); }
        Ok(info)
    }
    #[cfg(target_os = "linux")]
    {
        let mut info = String::new();
        if let Ok(o) = run_command("uname", &["-a"], 5) { info.push_str(&o); }
        if let Ok(o) = run_command("free", &["-h"], 5) { info.push_str(&o); }
        if let Ok(o) = run_command("uptime", &[], 5) { info.push_str(&o); }
        Ok(info)
    }
}

fn exec_disk_usage() -> Result<String, String> {
    #[cfg(windows)]
    { run_command("cmd", &["/C", "wmic logicaldisk get size,freespace,caption"], 15) }
    #[cfg(not(windows))]
    { run_command("df", &["-h"], 10) }
}

#[allow(unused_variables)]
fn exec_list_processes(args: &Value) -> Result<String, String> {
    let limit = args["limit"].as_u64().unwrap_or(20) as usize;
    let sort_by = args["sort_by"].as_str().unwrap_or("cpu");

    #[cfg(windows)]
    {
        run_command("cmd", &["/C", "tasklist /FO TABLE /NH | sort /R | more +1"], 15)
    }
    #[cfg(target_os = "macos")]
    {
        let sort_flag = if sort_by == "memory" { "-m" } else { "-r" };
        let cmd = format!("ps aux {} | head -n {}", sort_flag, limit + 1);
        run_command("sh", &["-c", &cmd], 10)
    }
    #[cfg(target_os = "freebsd")]
    {
        // FreeBSD ps doesn't support GNU --sort flag; use -o with sort
        let sort_col = if sort_by == "memory" { "rss" } else { "pcpu" };
        let cmd = format!("ps aux -O {} | sort -nrk 3 | head -n {}", sort_col, limit + 1);
        run_command("sh", &["-c", &cmd], 10)
    }
    #[cfg(target_os = "linux")]
    {
        let sort_key = if sort_by == "memory" { "--sort=-%mem" } else { "--sort=-%cpu" };
        let cmd = format!("ps aux {} | head -n {}", sort_key, limit + 1);
        run_command("sh", &["-c", &cmd], 10)
    }
}

fn exec_network_info() -> Result<String, String> {
    #[cfg(windows)]
    { run_command("cmd", &["/C", "ipconfig /all"], 15) }
    #[cfg(target_os = "macos")]
    { run_command("sh", &["-c", "ifconfig | grep -E 'flags|inet'"], 10) }
    #[cfg(target_os = "freebsd")]
    { run_command("ifconfig", &[], 10) }
    #[cfg(target_os = "linux")]
    { run_command("sh", &["-c", "ip addr show 2>/dev/null || ifconfig"], 10) }
}

#[allow(unused_variables)]
fn exec_get_service_logs(args: &Value) -> Result<String, String> {
    let lines = args["lines"].as_u64().unwrap_or(50).min(200);

    #[cfg(windows)]
    {
        // Try reading the log file
        let log_path = format!("{}\\HopToDesk\\hoptodesk.log",
            std::env::var("PROGRAMDATA").unwrap_or_else(|_| "C:\\ProgramData".to_string()));
        run_command("cmd", &["/C", &format!("type \"{}\" 2>nul | more", log_path)], 10)
            .or_else(|_| Ok("No log file found".to_string()))
    }
    #[cfg(target_os = "macos")]
    {
        let cmd = format!("tail -n {} ~/Library/Logs/hoptodesk/hoptodesk.log 2>/dev/null || echo 'No log file found'", lines);
        run_command("sh", &["-c", &cmd], 10)
    }
    #[cfg(target_os = "freebsd")]
    {
        let cmd = format!("tail -n {} /var/log/hoptodesk/hoptodesk.log 2>/dev/null || tail -n {} ~/.config/log/server/hoptodesk_rCURRENT.log 2>/dev/null || echo 'No log file found'", lines, lines);
        run_command("sh", &["-c", &cmd], 10)
    }
    #[cfg(target_os = "linux")]
    {
        let cmd = format!("journalctl -u hoptodesk -n {} --no-pager 2>/dev/null || tail -n {} /var/log/hoptodesk/hoptodesk.log 2>/dev/null || echo 'No log file found'", lines, lines);
        run_command("sh", &["-c", &cmd], 10)
    }
}

fn exec_ping_test(args: &Value) -> Result<String, String> {
    let host = args["host"].as_str().unwrap_or("").trim();
    if host.is_empty() {
        return Err("host parameter is required".to_string());
    }
    // Validate host — no shell metacharacters
    if host.chars().any(|c| ";|&`$(){}[]\\!#~<>\"'".contains(c)) {
        return Err("Invalid host".to_string());
    }
    let count = args["count"].as_u64().unwrap_or(4).min(10);

    #[cfg(windows)]
    { run_command("ping", &["-n", &count.to_string(), host], 20) }
    #[cfg(not(windows))]
    { run_command("ping", &["-c", &count.to_string(), host], 20) }
}

fn exec_installed_software() -> Result<String, String> {
    #[cfg(windows)]
    { run_command("cmd", &["/C", "wmic product get name,version /format:list"], 30) }
    #[cfg(target_os = "macos")]
    { run_command("sh", &["-c", "ls /Applications/ && echo '---' && brew list --versions 2>/dev/null || true"], 15) }
    #[cfg(target_os = "freebsd")]
    { run_command("sh", &["-c", "pkg info 2>/dev/null | head -100 || echo 'Package manager not found'"], 15) }
    #[cfg(target_os = "linux")]
    { run_command("sh", &["-c", "dpkg -l 2>/dev/null | head -100 || rpm -qa 2>/dev/null | head -100 || echo 'Package manager not found'"], 15) }
}

fn exec_restart_hoptodesk() -> Result<String, String> {
    eprintln!("[exec] Restarting HopToDesk service");
    #[cfg(windows)]
    { run_command("cmd", &["/C", "net stop hoptodesk & net start hoptodesk"], 30) }
    #[cfg(target_os = "macos")]
    { run_command("sh", &["-c", "launchctl kickstart -kp system/com.hoptodesk.agent 2>/dev/null || echo 'Service restart requested'"], 15) }
    #[cfg(target_os = "freebsd")]
    { run_command("sh", &["-c", "service hoptodesk restart 2>/dev/null || echo 'Service restart requested'"], 15) }
    #[cfg(target_os = "linux")]
    { run_command("sh", &["-c", "systemctl restart hoptodesk 2>/dev/null || echo 'Service restart requested'"], 15) }
}

fn exec_kill_process(args: &Value) -> Result<String, String> {
    let process_name = args["process_name"].as_str().unwrap_or("");
    let pid = args["pid"].as_u64();

    if process_name.is_empty() && pid.is_none() {
        return Err("process_name or pid is required".to_string());
    }

    // Validate process name — no shell metacharacters
    if !process_name.is_empty() && process_name.chars().any(|c| ";|&`$(){}[]\\!#~<>\"'".contains(c)) {
        return Err("Invalid process name".to_string());
    }

    if let Some(pid) = pid {
        #[cfg(windows)]
        { run_command("taskkill", &["/PID", &pid.to_string(), "/F"], 10) }
        #[cfg(not(windows))]
        { run_command("kill", &["-9", &pid.to_string()], 10) }
    } else {
        #[cfg(windows)]
        { run_command("taskkill", &["/IM", &format!("{}.exe", process_name), "/F"], 10) }
        #[cfg(not(windows))]
        { run_command("sh", &["-c", &format!("pkill -f '{}'", process_name)], 10) }
    }
}

fn exec_flush_dns() -> Result<String, String> {
    #[cfg(windows)]
    { run_command("cmd", &["/C", "ipconfig /flushdns"], 10) }
    #[cfg(target_os = "macos")]
    { run_command("sh", &["-c", "sudo dscacheutil -flushcache && sudo killall -HUP mDNSResponder && echo 'DNS cache flushed'"], 10) }
    #[cfg(target_os = "freebsd")]
    { run_command("sh", &["-c", "service local_unbound restart 2>/dev/null && echo 'DNS cache flushed' || echo 'DNS flush not available'"], 10) }
    #[cfg(target_os = "linux")]
    { run_command("sh", &["-c", "systemd-resolve --flush-caches 2>/dev/null && echo 'DNS cache flushed' || echo 'DNS flush not supported'"], 10) }
}

fn exec_reboot_device() -> Result<String, String> {
    eprintln!("[exec] REBOOT requested");
    #[cfg(windows)]
    { run_command("shutdown", &["/r", "/t", "5", "/c", "Reboot requested from HopToDesk Dashboard"], 5) }
    #[cfg(not(windows))]
    { run_command("sh", &["-c", "sudo shutdown -r +1 'Reboot requested from HopToDesk Dashboard' 2>/dev/null || echo 'Reboot scheduled'"], 5) }
}

fn exec_clear_temp_files() -> Result<String, String> {
    #[cfg(windows)]
    {
        let temp = std::env::var("TEMP").unwrap_or_else(|_| "C:\\Windows\\Temp".to_string());
        run_command("cmd", &["/C", &format!("del /q /s \"{}\\*\" 2>nul & echo Temp files cleared", temp)], 60)
    }
    #[cfg(target_os = "macos")]
    { run_command("sh", &["-c", "rm -rf /tmp/com.apple.* ~/Library/Caches/com.apple.* 2>/dev/null; du -sh /tmp/ ~/Library/Caches/ 2>/dev/null || echo 'Temp files cleared'"], 30) }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    { run_command("sh", &["-c", "rm -rf /tmp/hoptodesk-* 2>/dev/null; du -sh /tmp/ 2>/dev/null || echo 'Temp files cleared'"], 30) }
}

fn exec_run_update_check() -> Result<String, String> {
    #[cfg(windows)]
    { run_command("cmd", &["/C", "wuauclt /detectnow & echo 'Windows Update check triggered'"], 15) }
    #[cfg(target_os = "macos")]
    { run_command("sh", &["-c", "softwareupdate -l 2>&1 | head -20"], 60) }
    #[cfg(target_os = "freebsd")]
    { run_command("sh", &["-c", "pkg audit 2>/dev/null | head -20; pkg upgrade -n 2>/dev/null | head -20 || echo 'No updates available'"], 60) }
    #[cfg(target_os = "linux")]
    { run_command("sh", &["-c", "apt list --upgradable 2>/dev/null | head -20 || yum check-update 2>/dev/null | head -20 || echo 'Package manager not found'"], 60) }
}

// ─── Run Command Tool ────────────────────────────────────────────────────────

fn tool_run_command(args: &Value) -> Result<Value, String> {
    let command = args["command"].as_str().unwrap_or("");
    if command.is_empty() {
        return Err("command is required".to_string());
    }

    let dashboard_user_id = hbb_common::config::Config::get_option("dashboard_user_id");
    if dashboard_user_id.is_empty() {
        return Err("run_command requires dashboard enrollment".to_string());
    }

    let timeout_secs = args["timeout"].as_u64().unwrap_or(30).min(120);

    eprintln!("[mcp] run_command: {:?} (timeout: {}s)", command, timeout_secs);

    let output = {
        use std::process::Command;

        #[cfg(windows)]
        let child = Command::new("cmd")
            .args(&["/C", command])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        #[cfg(not(windows))]
        let child = Command::new("sh")
            .args(&["-c", command])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();

        match child {
            Ok(child) => {
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(child.wait_with_output());
                });
                match rx.recv_timeout(Duration::from_secs(timeout_secs)) {
                    Ok(Ok(output)) => Ok(output),
                    Ok(Err(e)) => Err(format!("Command failed: {}", e)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        Err(format!("Command timed out after {}s", timeout_secs))
                    }
                    Err(_) => Err("Command thread disconnected".to_string()),
                }
            }
            Err(e) => Err(format!("Failed to spawn command: {}", e)),
        }
    };

    match output {
        Ok(output) => {
            let max_bytes = 64 * 1024;
            let stdout_raw = String::from_utf8_lossy(&output.stdout);
            let stderr_raw = String::from_utf8_lossy(&output.stderr);
            let truncated = stdout_raw.len() > max_bytes || stderr_raw.len() > max_bytes;
            let stdout = if stdout_raw.len() > max_bytes { &stdout_raw[..max_bytes] } else { &stdout_raw };
            let stderr = if stderr_raw.len() > max_bytes { &stderr_raw[..max_bytes] } else { &stderr_raw };
            let exit_code = output.status.code().unwrap_or(-1);

            let response = json!({
                "command": command,
                "exit_code": exit_code,
                "stdout": stdout,
                "stderr": stderr,
                "truncated": truncated
            });
            Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&response).unwrap_or_default() }]))
        }
        Err(msg) => {
            let response = json!({
                "command": command,
                "exit_code": -1,
                "stdout": "",
                "stderr": msg,
                "truncated": false
            });
            Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&response).unwrap_or_default() }]))
        }
    }
}

// ─── Clipboard Tools ────────────────────────────────────────────────────────

fn tool_get_clipboard() -> Result<Value, String> {
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|e| format!("Failed to access clipboard: {}", e))?;
    let text = clipboard.get_text()
        .unwrap_or_default();
    Ok(json!([{ "type": "text", "text": text }]))
}

fn tool_set_clipboard(args: &Value) -> Result<Value, String> {
    let text = args["text"].as_str().ok_or("Missing text")?;
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|e| format!("Failed to access clipboard: {}", e))?;
    clipboard.set_text(text)
        .map_err(|e| format!("Failed to set clipboard: {}", e))?;
    Ok(json!([{ "type": "text", "text": format!("Clipboard set ({} chars)", text.len()) }]))
}

// ─── Chat Tool ──────────────────────────────────────────────────────────────

#[cfg(not(any(feature = "flutter", feature = "cli")))]
fn tool_send_chat_message(args: &Value) -> Result<Value, String> {
    let peer_id = args["peer_id"].as_str().ok_or("Missing peer_id")?;
    let text = args["text"].as_str().ok_or("Missing text")?;
    let cmd = format!("chat:{}", text);
    if crate::ui::send_to_child(peer_id, &cmd) {
        Ok(json!([{ "type": "text", "text": format!("Chat message sent to peer {}", peer_id) }]))
    } else {
        Err(format!("No active connection found for peer {}", peer_id))
    }
}

#[cfg(any(feature = "flutter", feature = "cli"))]
fn tool_send_chat_message(_args: &Value) -> Result<Value, String> {
    Err("send_chat_message not available in this build".to_string())
}

// ─── Permission Tool ────────────────────────────────────────────────────────

fn tool_switch_permission(args: &Value) -> Result<Value, String> {
    let conn_id = args["conn_id"].as_i64().ok_or("Missing conn_id")? as i32;
    let permission = args["permission"].as_str().ok_or("Missing permission")?;
    let enabled = args["enabled"].as_bool().ok_or("Missing enabled")?;

    let valid = ["keyboard", "clipboard", "audio", "file", "restart", "recording"];
    if !valid.contains(&permission) {
        return Err(format!("Invalid permission '{}'. Valid: {:?}", permission, valid));
    }

    crate::ui_cm_interface::switch_permission(conn_id, permission.to_string(), enabled);
    Ok(json!([{ "type": "text", "text": format!("Permission '{}' {} for connection {}", permission, if enabled { "enabled" } else { "disabled" }, conn_id) }]))
}

// ─── Incoming Connections Tool ──────────────────────────────────────────────

fn tool_list_incoming_connections() -> Result<Value, String> {
    let clients = crate::ui_cm_interface::CLIENTS.read().unwrap();
    let connections: Vec<Value> = clients.iter().map(|(id, client)| {
        json!({
            "conn_id": id,
            "peer_id": client.peer_id,
            "name": client.name,
            "authorized": client.authorized,
            "disconnected": client.disconnected,
            "keyboard": client.keyboard,
            "clipboard": client.clipboard,
            "audio": client.audio,
            "file": client.file,
            "restart": client.restart,
            "recording": client.recording,
        })
    }).collect();
    drop(clients);
    Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&connections).unwrap_or_default() }]))
}

// ─── File Tools ─────────────────────────────────────────────────────────────

fn tool_list_local_files(args: &Value) -> Result<Value, String> {
    let path = args["path"].as_str().unwrap_or("");
    let include_hidden = args["include_hidden"].as_bool().unwrap_or(false);

    let dir = if path.is_empty() {
        #[cfg(windows)]
        { std::path::PathBuf::from(std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string())) }
        #[cfg(not(windows))]
        { std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".to_string())) }
    } else {
        std::path::PathBuf::from(path)
    };

    if !dir.exists() {
        return Err(format!("Path does not exist: {}", dir.display()));
    }
    if !dir.is_dir() {
        return Err(format!("Not a directory: {}", dir.display()));
    }

    let mut entries = Vec::new();
    let read_dir = std::fs::read_dir(&dir)
        .map_err(|e| format!("Failed to read directory: {}", e))?;

    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if !include_hidden && name.starts_with('.') {
            continue;
        }
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta.as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);

        entries.push(json!({
            "name": name,
            "type": if is_dir { "directory" } else { "file" },
            "size": size,
            "modified_epoch": modified,
        }));
    }

    // Sort: directories first, then alphabetical
    entries.sort_by(|a, b| {
        let a_dir = a["type"].as_str() == Some("directory");
        let b_dir = b["type"].as_str() == Some("directory");
        b_dir.cmp(&a_dir).then_with(|| {
            a["name"].as_str().unwrap_or("").to_lowercase()
                .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
        })
    });

    let result = json!({
        "path": dir.display().to_string(),
        "count": entries.len(),
        "entries": entries,
    });
    Ok(json!([{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }]))
}

fn tool_read_local_file(args: &Value) -> Result<Value, String> {
    let path = args["path"].as_str().ok_or("Missing path")?;
    let file_path = std::path::Path::new(path);

    if !file_path.exists() {
        return Err(format!("File does not exist: {}", path));
    }
    if !file_path.is_file() {
        return Err(format!("Not a file: {}", path));
    }

    let meta = std::fs::metadata(file_path)
        .map_err(|e| format!("Failed to read metadata: {}", e))?;
    if meta.len() > 1_048_576 {
        return Err(format!("File too large ({} bytes, max 1MB)", meta.len()));
    }

    let bytes = std::fs::read(file_path)
        .map_err(|e| format!("Failed to read file: {}", e))?;

    // Try UTF-8 first, fall back to base64
    match String::from_utf8(bytes.clone()) {
        Ok(text) => Ok(json!([{ "type": "text", "text": text }])),
        Err(_) => {
            use hbb_common::base64::Engine;
            let b64 = hbb_common::base64::engine::general_purpose::STANDARD.encode(&bytes);
            Ok(json!([{ "type": "text", "text": format!("base64:{}", b64) }]))
        }
    }
}
