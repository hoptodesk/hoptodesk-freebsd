#[cfg(not(any(target_os = "android", target_os = "ios")))]
use crate::clipboard::{update_clipboard, ClipboardSide};
use hbb_common::message_proto::{Clipboard, ClipboardFormat};

use std::{
    collections::HashMap,
    env,
    iter::FromIterator,
    io::Write,
    process::{Child, Stdio},
    sync::{Arc, Mutex},
};
use hbb_common::rand;
use hbb_common::sodiumoxide::base64;
use hbb_common::{
    allow_err,
    config::{Config, LocalConfig, PeerConfig},
    log,
    tokio::{self},
};

#[cfg(not(any(feature = "flutter", feature = "cli")))]
use crate::ui_session_interface::Session;
use crate::{common::get_app_name, ipc, ui_interface::*};
use crate::client::file_trait::FileManager;
use hbb_common::get_version_number;
use tokio::runtime::Runtime;
use std::net::ToSocketAddrs;

pub mod cm;
#[cfg(feature = "inline")]
pub mod inline;
pub mod remote;

pub type Children = Arc<Mutex<(bool, HashMap<(String, String), Child>)>>;
#[allow(dead_code)]
type Status = (i32, bool, i64, String);

#[cfg(not(any(feature = "flutter", feature = "cli")))]
lazy_static::lazy_static! {
    pub static ref CUR_SESSION: Arc<Mutex<Option<Session<remote::SciterHandler>>>> = Default::default();
    static ref CHILDREN : Children = Default::default();
    static ref CHILD_STDINS: Mutex<HashMap<String, std::process::ChildStdin>> = Mutex::new(HashMap::new());
}

struct UI {}

pub fn start(args: &mut [String]) {
    #[cfg(target_os = "macos")]
    crate::platform::delegate::show_dock();

    let page;
    if args.len() > 1 && args[0] == "--play" {
        args[0] = "--connect".to_owned();
        let path: std::path::PathBuf = (&args[1]).into();
        let id = path
            .file_stem()
            .map(|p| p.to_str().unwrap_or(""))
            .unwrap_or("")
            .to_owned();
        args[1] = id;
    }
    let args_string = args.concat().replace("\"", "").replace("[", "").replace("]", "");

    if args.is_empty()
        || args_string.is_empty()
        || args[0] == "--qs"
        || (args[0] != "--install" && std::env::current_exe().ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().contains("-qs")))
            .unwrap_or(false)) {
        std::thread::spawn(move || check_zombie());
        set_version();
        page = "index.html";
        #[cfg(target_os = "linux")]
        std::thread::spawn(crate::ipc::start_pa);
    } else if args[0] == "--install" {
        page = "install.html";
    } else if args[0] == "--cm" {
        page = "cm.html";
    } else if args[0] == "--ticket" {
        page = "ticket.html";
    } else if args[0] == "--invite" && args.len() >= 3 {
        let peer_id_to_invite = args[1].clone();
        let self_id = args[2].clone();
        let invite_password = args[3].clone();
        log::info!("[UI::start] Received --invite command for peer ID: {}.", peer_id_to_invite);

        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => {
                rt.block_on(async {
                    if let Err(e) = crate::ui_interface::process_invite_request(
                        peer_id_to_invite.clone(), self_id.clone(), invite_password.clone()
                    ).await {
                        log::error!("[UI::start] --invite failed: {}", e);
                    }
                });
            }
            Err(e) => {
                log::error!("[UI::start] Failed to create Tokio runtime: {}", e);
            }
        }
        return;
    } else if (args[0] == "--connect"
        || args[0] == "--file-transfer"
        || args[0] == "--port-forward"
        || args[0] == "--rdp"
        || args[0] == "--view-camera")
        && args.len() > 1
    {
        log::info!("[UI::start] args: {:?}", args);
        let mut iter = args.iter();
        let cmd = iter.next().unwrap().clone();
        let mut id = "".to_owned();
        let mut pass = "".to_owned();
        let mut _teamid = "".to_owned();
        let mut tokenexp = "".to_owned();
        let mut remaining_args_vec: Vec<String> = Vec::new();

        if cmd == "--file-transfer" && args.get(1).map_or(false, |s| s == "--select-for-print") {
            iter.next();
            id = iter.next().unwrap_or(&"".to_owned()).clone();
            pass = iter.next().unwrap_or(&"".to_owned()).clone();
            _teamid = iter.next().unwrap_or(&"".to_owned()).clone();
            tokenexp = iter.next().unwrap_or(&"".to_owned()).clone();
            remaining_args_vec = iter.map(|x| x.clone()).collect();
            remaining_args_vec.insert(0, "--select-for-print".to_string());
        } else {
            let mut id_found = false;
            while let Some(arg) = iter.next() {
                if arg == "--password" {
                    if let Some(p) = iter.next() {
                        pass = p.clone();
                    }
                } else if arg == "--tokenex" {
                    if let Some(t) = iter.next() {
                        tokenexp = t.clone();
                    }
                } else if !id_found && !arg.starts_with("--") {
                    id = arg.clone();
                    id_found = true;
                } else {
                    remaining_args_vec.push(arg.clone());
                }
            }
        }
        if id.contains('.')
            && !hbb_common::is_ipv4_str(&id)
            && !hbb_common::is_ipv6_str(&id)
            && !is_numeric_id(&id)
        {
            if let Some(resolved_ip) = resolve_hostname(&id) {
                id = resolved_ip;
            }
        }

        if id == "hoptodesk:///" || id.is_empty() {
            return;
        }
        if !tokenexp.is_empty() {
            std::fs::write(&Config::path("LastToken.toml"), tokenexp.clone())
                .expect("Failed to write tokenexp to file");
        }

        if args[0] == "--connect" {
            if let Some(full_arg) = args.get(1) {
                if full_arg.starts_with("hoptodesk://sso-login/") || full_arg.starts_with("hoptodesk://file-transfer/") {
                    let parts: Vec<&str> = full_arg.split('/').collect();
                    if parts.len() >= 5 {
                        id = parts[3].to_string();
                        if let Some(token) = parts.iter().find(|s| s.len() == 32) {
                            tokenexp = token.to_string();
                            pass = tokenexp.clone();
                        }
                    }
                } else {
                    tokenexp = args.get(2).cloned().unwrap_or_default();
                }
            }
        }

        *REMOTE_PARAMS.lock().unwrap() = Some(RemoteParams {
            cmd: cmd.clone(),
            id: id.clone(),
            pass: pass.clone(),
            tokenexp: tokenexp.clone(),
            args: remaining_args_vec.clone(),
        });

        if cmd == "--file-transfer" {
            page = "file-transfer.html";
        } else if cmd == "--port-forward" || cmd == "--rdp" {
            page = "port-forward.html";
        } else {
            page = "remote.html";
        }
    } else {
        log::error!("Wrong command: {:?}", args);
        return;
    }

    start_wry_ui(page, &crate::get_app_name());
}

lazy_static::lazy_static! {
    static ref WEBVIEW_SENDER: Mutex<Option<std::sync::mpsc::Sender<String>>> = Mutex::new(None);
    static ref EVENT_LOOP_PROXY: Mutex<Option<tao::event_loop::EventLoopProxy<String>>> = Mutex::new(None);
    static ref REMOTE_PARAMS: Mutex<Option<RemoteParams>> = Mutex::new(None);
    static ref CM_INSTANCE: Mutex<Option<cm::SciterConnectionManager>> = Mutex::new(None);
}

#[derive(Clone)]
struct RemoteParams {
    cmd: String,
    id: String,
    pass: String,
    tokenexp: String,
    args: Vec<String>,
}

fn send_to_webview(method: &str, data: &str) {
    let script = format!("window.onRustResponse && window.onRustResponse('{}', {})", method, data);
    if let Some(ref sender) = *WEBVIEW_SENDER.lock().unwrap() {
        sender.send(script).ok();
    }
}

fn start_wry_ui(page: &str, title: &str) {
    use wry::WebViewBuilder;
    use tao::{
        event::{Event, WindowEvent},
        event_loop::{ControlFlow, EventLoopBuilder},
        window::WindowBuilder,
    };

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        std::env::set_var("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "1");
        gtk::init().expect("Failed to initialize GTK");
    }

    crate::ui_interface::start_option_status_sync();

    let event_loop = EventLoopBuilder::<String>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    *WEBVIEW_SENDER.lock().unwrap() = Some(tx);

    let proxy_for_ipc = proxy.clone();
    *EVENT_LOOP_PROXY.lock().unwrap() = Some(proxy.clone());

    let is_remote = page == "remote.html";
    let is_file_transfer = page == "file-transfer.html";
    let is_port_forward = page == "port-forward.html";
    let is_cm = page == "cm.html";
    let window_size = if is_remote || is_file_transfer {
        tao::dpi::LogicalSize::new(1024.0, 768.0)
    } else if is_port_forward {
        tao::dpi::LogicalSize::new(500.0, 400.0)
    } else if is_cm {
        tao::dpi::LogicalSize::new(300.0, 450.0)
    } else {
        tao::dpi::LogicalSize::new(800.0, 600.0)
    };

    let window_title = if is_remote || is_file_transfer || is_port_forward {
        if let Some(ref params) = *REMOTE_PARAMS.lock().unwrap() {
            let prefix = if is_file_transfer { "File Transfer" }
                else if is_port_forward { "Port Forward" }
                else { &params.id };
            format!("{} - {}", prefix, title)
        } else {
            title.to_string()
        }
    } else {
        title.to_string()
    };

    let window = WindowBuilder::new()
        .with_title(&window_title)
        .with_inner_size(window_size)
        .build(&event_loop)
        .expect("Failed to create window");

    if is_cm {
        if let Some(monitor) = window.current_monitor() {
            let screen_size = monitor.size();
            let win_size = window.outer_size();
            let x = screen_size.width.saturating_sub(win_size.width) as i32;
            window.set_outer_position(tao::dpi::PhysicalPosition::new(x, 0));
        }
    }

    let html_content = get_page_html(page);

    std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");

    use tao::platform::unix::WindowExtUnix;
    use wry::WebViewBuilderExtUnix;
    let vbox = window.default_vbox().expect("No default vbox in tao window");
    let webview = WebViewBuilder::new()
        .with_html(&html_content)
        .with_ipc_handler(move |req: wry::http::Request<String>| {
            handle_ipc_message(req.body());
            proxy_for_ipc.send_event("ipc_response".to_string()).ok();
        })
        .build_gtk(vbox)
        .expect("Failed to create WebView");

    if page == "cm.html" {
        let cm = cm::SciterConnectionManager::new();
        *CM_INSTANCE.lock().unwrap() = Some(cm);
        log::info!("[CM] Connection Manager initialized");
    }

    if is_remote || is_file_transfer || is_port_forward {
        if is_remote {
            remote::start_frame_server();
            let tx_port = WEBVIEW_SENDER.lock().unwrap().clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(2000));
                let port = *remote::FRAME_SERVER_PORT.lock().unwrap();
                if let Some(ref sender) = tx_port {
                    let script = format!("window.onRustResponse && window.onRustResponse('set_frame_port', {})", port);
                    sender.send(script).ok();
                }
            });
        }
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(3000));
            start_remote_session();
        });
    }

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        while let Ok(script) = rx.try_recv() {
            webview.evaluate_script(&script).ok();
        }

        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                    session.close();
                }
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(ref cmd) => {
                if cmd == "cmd:close" {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.close();
                    }
                    *control_flow = ControlFlow::Exit;
                } else if cmd == "cmd:fullscreen" {
                    use tao::window::Fullscreen;
                    if window.fullscreen().is_some() {
                        window.set_fullscreen(None);
                    } else {
                        window.set_fullscreen(Some(Fullscreen::Borderless(None)));
                    }
                }
            }
            _ => {
                #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                while gtk::events_pending() {
                    gtk::main_iteration_do(false);
                }
            }
        }
    });
}

fn start_remote_session() {
    use hbb_common::rendezvous_proto::ConnType;

    let params = match REMOTE_PARAMS.lock().unwrap().take() {
        Some(p) => p,
        None => {
            log::error!("[remote] No connection params available");
            return;
        }
    };

    log::info!("[remote] Starting session: cmd={}, id={}", params.cmd, params.id);

    let force_relay = params.args.contains(&"--relay".to_string());
    let handler = remote::SciterHandler::default();

    let session: Session<remote::SciterHandler> = Session {
        id: params.id.clone(),
        password: params.pass.clone(),
        tokenexp: params.tokenexp.clone(),
        args: params.args.clone(),
        ui_handler: handler,
        server_keyboard_enabled: Arc::new(std::sync::RwLock::new(true)),
        server_file_transfer_enabled: Arc::new(std::sync::RwLock::new(true)),
        server_clipboard_enabled: Arc::new(std::sync::RwLock::new(true)),
        ..Default::default()
    };

    let conn_type = if params.cmd == "--file-transfer" {
        ConnType::FILE_TRANSFER
    } else if params.cmd == "--view-camera" {
        ConnType::VIEW_CAMERA
    } else if params.cmd == "--port-forward" {
        ConnType::PORT_FORWARD
    } else if params.cmd == "--rdp" {
        ConnType::RDP
    } else {
        ConnType::DEFAULT_CONN
    };

    let invoked_switch_uuid = crate::core_main::SWITCH_SIDES_INVOKED_UUID.lock().unwrap().take();

    session.lc.write().unwrap().initialize(
        params.id.clone(),
        conn_type,
        invoked_switch_uuid,
        force_relay,
        None,
        None,
        None,
        params.tokenexp.clone(),
    );

    *CUR_SESSION.lock().unwrap() = Some(session.clone());

    remote::start_stdin_command_reader();
    remote::start_file_command_watcher();

    session.reconnect();
}

fn get_page_html(page: &str) -> String {
    if page == "remote.html" {
        return get_remote_page_html();
    }
    if page == "cm.html" {
        return get_cm_page_html();
    }
    if page == "file-transfer.html" {
        return get_file_transfer_page_html();
    }
    if page == "port-forward.html" {
        return get_port_forward_page_html();
    }

    let title = match page {
        "index.html" => "HopToDesk",
        "install.html" => "Install",
        "ticket.html" => "Tickets",
        _ => "HopToDesk",
    };

    format!(r##"<!DOCTYPE html>
<html>
<head>
    <meta charset="UTF-8">
    <title>{title}</title>
    <style>
        :root {{
            --bg: #F0F4F8; --card-bg: white; --text: #1E293B; --text-dark: #1E3A5F;
            --border: #E2E8F0; --border-light: #F1F5F9; --secondary: #64748B; --muted: #94A3B8;
            --input-bg: white; --input-border: #E2E8F0; --hover-bg: #F8FAFC;
            --sessions-bg: #ECF4FF; --tab-hover: rgba(255,255,255,0.5); --tab-active-bg: white;
            --pill-bg: #E3F2FD; --pill-color: #1565C0; --pill-hover: #BBDEFB;
            --invite-bg: #E8F0FE; --invite-hover: #D0E4FD;
            --modal-bg: white; --modal-overlay: rgba(0,0,0,0.4);
            --ctx-bg: white; --ctx-hover: #F1F5F9;
            --toggle-bg: #CBD5E1; --btn-transfer-bg: white;
            --shadow: 0 1px 3px rgba(0,0,0,0.06), 0 1px 2px rgba(0,0,0,0.04);
            --shadow-hover: 0 2px 8px rgba(0,0,0,0.1);
            --lang-hover: #EFF6FF;
        }}
        html.darktheme {{
            --bg: #0F172A; --card-bg: #1E293B; --text: #F1F5F9; --text-dark: #E2E8F0;
            --border: #334155; --border-light: #334155; --secondary: #94A3B8; --muted: #64748B;
            --input-bg: #0F172A; --input-border: #334155; --hover-bg: #334155;
            --sessions-bg: #1E293B; --tab-hover: rgba(255,255,255,0.08); --tab-active-bg: #334155;
            --pill-bg: #1E3A5F; --pill-color: #93C5FD; --pill-hover: #1E4A7F;
            --invite-bg: #1E3A5F; --invite-hover: #1E4A7F;
            --modal-bg: #1E293B; --modal-overlay: rgba(0,0,0,0.6);
            --ctx-bg: #1E293B; --ctx-hover: #334155;
            --toggle-bg: #475569; --btn-transfer-bg: #0F172A;
            --shadow: 0 1px 3px rgba(0,0,0,0.2), 0 1px 2px rgba(0,0,0,0.15);
            --shadow-hover: 0 2px 8px rgba(0,0,0,0.3);
            --lang-hover: #334155;
        }}
        * {{ margin: 0; padding: 0; box-sizing: border-box; }}
        body {{ font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
               background: var(--bg); color: var(--text); height: 100vh; overflow: hidden; }}
        .top-bar {{ background: var(--card-bg); display: flex; align-items: center; padding: 8px 20px;
                   border-bottom: 1px solid var(--border); height: 48px; }}
        .brand {{ font-size: 18px; font-weight: 700; color: var(--text-dark); }}
        .spacer {{ flex: 1; }}
        .top-right {{ display: flex; align-items: center; gap: 12px; }}
        .status-indicator {{ display: flex; align-items: center; gap: 6px; }}
        .status-dot {{ width: 10px; height: 10px; border-radius: 50%; }}
        .status-dot.online {{ background: #22C55E; }}
        .status-dot.connecting {{ background: #F59E0B; }}
        .status-dot.offline {{ background: var(--muted); }}
        .status-text {{ font-size: 13px; color: var(--secondary); font-weight: 500; }}
        .main-content {{ padding: 16px 20px; height: calc(100vh - 48px); overflow-y: auto; }}
        .cards-row {{ display: flex; gap: 16px; margin-bottom: 16px; }}
        .card {{ background: var(--card-bg); border-radius: 12px;
                box-shadow: var(--shadow); flex: 1; min-width: 0; }}
        .card-header {{ display: flex; align-items: center; padding: 10px 16px;
                       border-bottom: 1px solid var(--border-light); font-size: 11px; font-weight: 700;
                       color: var(--secondary); text-transform: uppercase; letter-spacing: 1.5px; }}
        .card-header .gear {{ margin-left: auto; cursor: pointer; color: var(--muted); font-size: 16px; }}
        .card-header .gear:hover {{ color: var(--secondary); }}
        .card-body {{ padding: 16px; }}
        .id-label {{ font-size: 10px; color: var(--muted); text-transform: uppercase;
                    letter-spacing: 1.5px; font-weight: 600; margin-bottom: 4px; }}
        .id-row {{ display: flex; align-items: center; gap: 8px; margin-bottom: 12px; flex-wrap: wrap; }}
        .idbox {{ font-size: 24px; font-weight: 700; color: var(--text-dark); letter-spacing: 2px; }}
        .pill {{ display: inline-flex; align-items: center; gap: 4px; padding: 3px 10px;
                border-radius: 12px; font-size: 12px; font-weight: 500; cursor: pointer;
                border: none; background: var(--pill-bg); color: var(--pill-color); }}
        .pill:hover {{ background: var(--pill-hover); }}
        .pill.invite {{ background: var(--invite-bg); color: #2C8CFF; }}
        .pill.invite:hover {{ background: var(--invite-hover); }}
        .pill svg {{ width: 13px; height: 13px; }}
        .pw-label {{ font-size: 10px; color: var(--muted); text-transform: uppercase;
                    letter-spacing: 1.5px; font-weight: 600; margin-bottom: 4px; }}
        .pw-row {{ display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }}
        .passwordbox {{ font-size: 22px; font-weight: 700; color: var(--text-dark); font-family: monospace;
                       letter-spacing: 2px; cursor: pointer; }}
        .pw-right {{ display: flex; align-items: center; gap: 12px; margin-left: auto; }}
        .unattended-label {{ font-size: 12px; color: var(--secondary); white-space: nowrap; }}
        .toggle {{ position: relative; display: inline-block; width: 38px; height: 20px; }}
        .toggle input {{ opacity: 0; width: 0; height: 0; }}
        .toggle-slider {{ position: absolute; cursor: pointer; top: 0; left: 0; right: 0; bottom: 0;
                         background: var(--toggle-bg); border-radius: 20px; transition: 0.2s; }}
        .toggle-slider:before {{ content: ""; position: absolute; height: 16px; width: 16px;
                                left: 2px; bottom: 2px; background: white; border-radius: 50%;
                                transition: 0.2s; }}
        .toggle input:checked + .toggle-slider {{ background: #2C8CFF; }}
        .toggle input:checked + .toggle-slider:before {{ transform: translateX(18px); }}
        .partner-input {{ width: 100%; padding: 12px 14px; border: 2px solid var(--input-border);
                         border-radius: 8px; font-size: 20px; font-weight: 500; color: var(--text-dark);
                         letter-spacing: 2px; margin-bottom: 14px; background: var(--input-bg); }}
        .partner-input:focus {{ outline: none; border-color: #2C8CFF;
                               box-shadow: 0 0 0 3px rgba(44,140,255,0.15); }}
        .partner-input::placeholder {{ color: var(--toggle-bg); font-weight: 400; letter-spacing: 0; font-size: 16px; }}
        .connect-buttons {{ display: flex; gap: 10px; }}
        .btn-connect {{ flex: 1; padding: 10px; border: none; border-radius: 8px; font-size: 15px;
                       font-weight: 600; cursor: pointer; background: #2C8CFF; color: white; }}
        .btn-connect:hover {{ background: #1a7ae6; }}
        .btn-transfer {{ flex: 1; padding: 10px; border: 2px solid var(--border); border-radius: 8px;
                        font-size: 15px; font-weight: 600; cursor: pointer; background: var(--btn-transfer-bg);
                        color: var(--text-dark); }}
        .btn-transfer:hover {{ background: var(--hover-bg); border-color: var(--toggle-bg); }}
        .sessions-area {{ background: var(--sessions-bg); border-radius: 12px; padding: 16px; }}
        .sessions-bar {{ display: flex; align-items: center; margin-bottom: 14px; gap: 4px; }}
        .tab {{ padding: 6px 12px; font-size: 13px; font-weight: 500; color: var(--secondary);
               cursor: pointer; border-radius: 6px; white-space: nowrap; }}
        .tab:hover {{ color: var(--text-dark); background: var(--tab-hover); }}
        .tab.active {{ color: var(--text-dark); font-weight: 700; background: var(--tab-active-bg);
                      box-shadow: 0 1px 2px rgba(0,0,0,0.06); }}
        .search-input {{ margin-left: auto; padding: 6px 12px; border: 1px solid var(--border);
                        border-radius: 6px; font-size: 13px; background: var(--input-bg); color: var(--text-dark);
                        width: 140px; }}
        .search-input::placeholder {{ color: var(--muted); }}
        .search-input:focus {{ outline: none; border-color: #2C8CFF; }}
        .sessions-grid {{ display: grid; grid-template-columns: repeat(auto-fill, minmax(140px, 1fr));
                         gap: 10px; }}
        .session-card {{ background: var(--card-bg); border-radius: 10px; padding: 12px; cursor: pointer;
                        position: relative; text-align: center;
                        box-shadow: 0 1px 2px rgba(0,0,0,0.05); }}
        .session-card:hover {{ box-shadow: var(--shadow-hover); }}
        .session-card .os-icon {{ display: flex; justify-content: center; margin-bottom: 6px; }}
        .session-card .os-icon svg {{ width: 32px; height: 32px; }}
        .session-card .peer-name {{ font-size: 11px; color: var(--secondary); white-space: nowrap;
                                   overflow: hidden; text-overflow: ellipsis; margin-bottom: 4px; }}
        .session-card .peer-id {{ font-size: 13px; font-weight: 600; color: var(--text-dark); }}
        .session-card .fav-icon {{ position: absolute; top: 8px; right: 8px; font-size: 14px;
                                  color: var(--toggle-bg); cursor: pointer; opacity: 0.5; }}
        .session-card .fav-icon:hover {{ opacity: 1; color: #2C8CFF; }}
        .session-card .fav-icon.active {{ opacity: 1; color: #2C8CFF; }}
        .session-card .menu-icon {{ position: absolute; bottom: 8px; right: 8px; font-size: 16px;
                                   color: var(--muted); cursor: pointer; }}
        .session-card .menu-icon:hover {{ color: var(--secondary); }}
        .sessions-empty {{ text-align: center; color: var(--muted); padding: 40px 24px; font-size: 13px;
                          display: flex; flex-direction: column; align-items: center; justify-content: center;
                          min-height: 200px; grid-column: 1 / -1; }}
        .os-win {{ color: #0078D4; }}
        .os-mac {{ color: #555; }}
        .os-linux {{ color: #E95420; }}
        .os-android {{ color: #3DDC84; }}

        .modal-overlay {{ display: none; position: fixed; top: 0; left: 0; width: 100%; height: 100%;
                         background: var(--modal-overlay); z-index: 100; align-items: center; justify-content: center; }}
        .modal-overlay.active {{ display: flex; }}
        .modal {{ background: var(--modal-bg); border-radius: 12px; padding: 24px; min-width: 360px; max-width: 500px;
                 max-height: 80vh; overflow-y: auto; box-shadow: 0 8px 32px rgba(0,0,0,0.15); }}
        .modal h2 {{ font-size: 16px; color: var(--text-dark); margin-bottom: 16px; font-weight: 600; }}
        .modal-close {{ float: right; cursor: pointer; color: var(--muted); font-size: 20px; border: none;
                       background: none; }}
        .modal-close:hover {{ color: var(--secondary); }}

        .settings-item {{ display: flex; align-items: center; justify-content: space-between;
                         padding: 10px 0; border-bottom: 1px solid var(--border-light); }}
        .settings-item:last-child {{ border-bottom: none; }}
        .settings-item label {{ font-size: 13px; color: var(--text-dark); flex: 1; }}
        .settings-item label.toggle {{ flex: 0 0 auto; margin-left: auto; }}
        .settings-section {{ font-size: 11px; color: var(--muted); text-transform: uppercase;
                            letter-spacing: 1.5px; font-weight: 700; margin: 16px 0 8px; }}
        .settings-section:first-child {{ margin-top: 0; }}

        .pw-dialog-row {{ margin-bottom: 12px; }}
        .pw-dialog-row label {{ display: block; font-size: 12px; color: var(--secondary); margin-bottom: 4px; }}
        .pw-dialog-row input {{ width: 100%; padding: 8px 12px; border: 1px solid var(--input-border);
                               border-radius: 6px; font-size: 14px; background: var(--input-bg); color: var(--text); }}
        .pw-dialog-row input:focus {{ outline: none; border-color: #2C8CFF; }}
        .pw-length-btns {{ display: flex; gap: 6px; margin-top: 8px; }}
        .pw-length-btn {{ padding: 4px 12px; border: 1px solid var(--border); border-radius: 6px;
                         background: var(--card-bg); cursor: pointer; font-size: 12px; color: var(--secondary); }}
        .pw-length-btn.active {{ background: #2C8CFF; color: white; border-color: #2C8CFF; }}
        .modal-buttons {{ display: flex; gap: 8px; justify-content: flex-end; margin-top: 16px; }}
        .modal-btn {{ padding: 8px 20px; border-radius: 6px; font-size: 13px; font-weight: 500;
                     cursor: pointer; }}
        .modal-btn.primary {{ background: #2C8CFF; color: white; border: none; }}
        .modal-btn.primary:hover {{ background: #1a7ae6; }}
        .modal-btn.secondary {{ background: var(--card-bg); color: var(--secondary); border: 1px solid var(--border); }}
        .modal-btn.secondary:hover {{ background: var(--hover-bg); }}

        .lang-item {{ padding: 8px 14px; cursor: pointer; font-size: 13px; border-radius: 6px; margin: 2px 0; color: var(--text); }}
        .lang-item:hover {{ background: var(--lang-hover); }}
        .lang-item.active {{ background: #2C8CFF; color: #fff; font-weight: 600; }}

        .ctx-menu {{ display: none; position: fixed; background: var(--ctx-bg); border-radius: 8px;
                    box-shadow: 0 4px 16px rgba(0,0,0,0.15); z-index: 200; min-width: 160px;
                    padding: 4px 0; }}
        .ctx-menu.active {{ display: block; }}
        .ctx-item {{ padding: 8px 16px; font-size: 13px; color: var(--text-dark); cursor: pointer; }}
        .ctx-item:hover {{ background: var(--ctx-hover); }}
        .ctx-sep {{ height: 1px; background: var(--border-light); margin: 4px 0; }}

        .about-info {{ text-align: center; }}
        .about-info .app-name {{ font-size: 20px; font-weight: 700; color: var(--text-dark); margin-bottom: 8px; }}
        .about-info .version {{ font-size: 13px; color: var(--secondary); margin-bottom: 4px; }}
        .about-info .fingerprint {{ font-size: 11px; color: var(--muted); word-break: break-all; margin-top: 12px; }}
        .about-info a {{ color: #2C8CFF; text-decoration: none; }}
        .about-info a:hover {{ text-decoration: underline; }}
    </style>
</head>
<body>
    <div class="top-bar">
        <span class="brand">HopToDesk</span>
        <div class="spacer"></div>
        <div class="top-right">
            <div class="status-indicator">
                <span class="status-dot connecting" id="status-dot"></span>
                <span class="status-text" id="status-text" data-t="Ready">Connecting...</span>
            </div>
        </div>
    </div>
    <div class="main-content">
        <div class="cards-row">
            <div class="card">
                <div class="card-header">
                    <span data-t="This Device">THIS DEVICE</span>
                    <span class="gear" onclick="openSettings()" title="Settings">&#9881;</span>
                </div>
                <div class="card-body">
                    <div class="id-label" data-t="Your ID">YOUR ID</div>
                    <div class="id-row">
                        <span class="idbox" id="my-id">...</span>
                        <button class="pill" onclick="copyId()" title="Copy ID">
                            <svg viewBox="0 0 24 24" fill="currentColor"><path d="M16 1H4c-1.1 0-2 .9-2 2v14h2V3h12V1zm3 4H8c-1.1 0-2 .9-2 2v14c0 1.1.9 2 2 2h11c1.1 0 2-.9 2-2V7c0-1.1-.9-2-2-2zm0 16H8V7h11v14z"/></svg>
                            <span data-t="Copy">Copy</span>
                        </button>
                        <button class="pill invite" onclick="invitePeer()" title="Invite">
                            <svg viewBox="0 0 24 24" fill="currentColor"><path d="M15 12c2.21 0 4-1.79 4-4s-1.79-4-4-4-4 1.79-4 4 1.79 4 4 4zm-9-2V7H4v3H1v2h3v3h2v-3h3v-2H6zm9 4c-2.67 0-8 1.34-8 4v2h16v-2c0-2.66-5.33-4-8-4z"/></svg>
                            <span data-t="Invite">Invite</span>
                        </button>
                    </div>
                    <div class="pw-label" data-t="Password">Password</div>
                    <div class="pw-row">
                        <span class="passwordbox" id="my-password" onclick="copyPassword()" title="Click to copy">------</span>
                        <button class="pill" onclick="openPasswordDialog()" title="Set password">
                            <svg viewBox="0 0 24 24" fill="currentColor"><path d="M3 17.25V21h3.75L17.81 9.94l-3.75-3.75L3 17.25zM20.71 7.04c.39-.39.39-1.02 0-1.41l-2.34-2.34c-.39-.39-1.02-.39-1.41 0l-1.83 1.83 3.75 3.75 1.83-1.83z"/></svg>
                            <span data-t="Set">Set</span>
                        </button>
                        <div class="pw-right">
                            <span class="unattended-label" data-t="Unattended Access">Unattended Access</span>
                            <label class="toggle">
                                <input type="checkbox" id="unattended-toggle" onchange="toggleUnattended(this)">
                                <span class="toggle-slider"></span>
                            </label>
                        </div>
                    </div>
                </div>
            </div>
            <div class="card">
                <div class="card-header" data-t="Remote Control">REMOTE CONTROL</div>
                <div class="card-body">
                    <div class="id-label" data-t="Partner ID">PARTNER ID</div>
                    <input class="partner-input" type="text" id="remote-id"
                           placeholder="Enter Remote ID"
                           onkeydown="if(event.key==='Enter')doConnect()">
                    <div class="connect-buttons">
                        <button class="btn-connect" onclick="doConnect()" data-t="Connect">Connect</button>
                        <button class="btn-transfer" onclick="doTransfer()" data-t="Transfer File">Transfer File</button>
                    </div>
                </div>
            </div>
        </div>
        <div class="sessions-area">
            <div class="sessions-bar">
                <span class="tab active" data-tab="recent" onclick="switchTab('recent')" data-t="Recent Sessions">Recent Sessions</span>
                <span class="tab" data-tab="favorites" onclick="switchTab('favorites')" data-t="Favorites">Favorites</span>
                <span class="tab" data-tab="discovered" onclick="switchTab('discovered')" data-t="Discovered">Discovered</span>
                <input class="search-input" type="text" placeholder="Search ID" oninput="filterSessions(this.value)">
            </div>
            <div class="sessions-grid" id="sessions-grid"></div>
        </div>
    </div>

    <div class="modal-overlay" id="settings-modal">
        <div class="modal" style="min-width:400px;max-height:85vh;overflow-y:auto;">
            <button class="modal-close" onclick="closeModal('settings-modal')">&times;</button>
            <h2 data-t="Settings">Settings</h2>
            <div class="settings-section" data-t="Remote Access">Remote Access</div>
            <div class="settings-item">
                <label data-t="Keyboard/Mouse">Keyboard/Mouse</label>
                <label class="toggle"><input type="checkbox" id="opt-enable-keyboard" onchange="setOpt('enable-keyboard',this.checked)"><span class="toggle-slider"></span></label>
            </div>
            <div class="settings-item">
                <label data-t="Clipboard">Clipboard</label>
                <label class="toggle"><input type="checkbox" id="opt-enable-clipboard" onchange="setOpt('enable-clipboard',this.checked)"><span class="toggle-slider"></span></label>
            </div>
            <div class="settings-item">
                <label data-t="File Transfer">File Transfer</label>
                <label class="toggle"><input type="checkbox" id="opt-enable-file-transfer" onchange="setOpt('enable-file-transfer',this.checked)"><span class="toggle-slider"></span></label>
            </div>
            <div class="settings-item">
                <label data-t="Remote Restart">Remote Restart</label>
                <label class="toggle"><input type="checkbox" id="opt-enable-remote-restart" onchange="setOpt('enable-remote-restart',this.checked)"><span class="toggle-slider"></span></label>
            </div>
            <div class="settings-item">
                <label data-t="TCP Tunneling">TCP Tunneling</label>
                <label class="toggle"><input type="checkbox" id="opt-enable-tunnel" onchange="setOpt('enable-tunnel',this.checked)"><span class="toggle-slider"></span></label>
            </div>
            <div class="settings-item">
                <label data-t="Wake On LAN">Wake On LAN</label>
                <label class="toggle"><input type="checkbox" id="opt-enable-wol" onchange="setOpt('enable-wol',this.checked)"><span class="toggle-slider"></span></label>
            </div>
            <div class="settings-section" data-t="Audio Input">Audio Input</div>
            <div class="settings-item">
                <label data-t="Mute">Mute</label>
                <label class="toggle"><input type="checkbox" id="opt-enable-audio" onchange="setOptInverted('enable-audio',this.checked)"><span class="toggle-slider"></span></label>
            </div>
            <div class="settings-section" data-t="Network">Network</div>
            <div class="settings-item">
                <label data-t="Direct IP Access">Direct IP Access</label>
                <label class="toggle"><input type="checkbox" id="opt-direct-server" onchange="setOpt('direct-server',this.checked)"><span class="toggle-slider"></span></label>
            </div>
            <div class="settings-item">
                <label data-t="LAN Discovery">LAN Discovery</label>
                <label class="toggle"><input type="checkbox" id="opt-enable-lan-discovery" onchange="setOpt('enable-lan-discovery',this.checked)"><span class="toggle-slider"></span></label>
            </div>
            <div class="settings-item" style="cursor:pointer" onclick="openSocksDialog()">
                <label style="cursor:pointer" data-t="SOCKS5 Proxy">SOCKS5 Proxy</label>
                <span style="color:#2C8CFF;font-size:13px" id="socks-status"></span>
            </div>
            <div class="settings-item" style="cursor:pointer" onclick="openNetworkDialog()">
                <label style="cursor:pointer" data-t="Choose Network">Choose Network</label>
                <span style="color:#2C8CFF;font-size:13px" id="network-status"></span>
            </div>
            <div class="settings-section" data-t="Recording">Recording</div>
            <div class="settings-item">
                <label data-t="Enable Recording Session">Enable Recording Session</label>
                <label class="toggle"><input type="checkbox" id="opt-enable-record-session" onchange="setOptRecording('enable-record-session',this.checked)"><span class="toggle-slider"></span></label>
            </div>
            <div class="settings-item">
                <label data-t="Automatically record incoming sessions">Auto-record Incoming Sessions</label>
                <label class="toggle"><input type="checkbox" id="opt-allow-auto-record-incoming" onchange="setOpt('allow-auto-record-incoming',this.checked)"><span class="toggle-slider"></span></label>
            </div>
            <div class="settings-section" data-t="Security">Security</div>
            <div class="settings-item">
                <label data-t="Allow Incoming Connections">Allow Incoming Connections</label>
                <label class="toggle"><input type="checkbox" id="opt-stop-service" onchange="setOptInverted('stop-service',this.checked)"><span class="toggle-slider"></span></label>
            </div>
            <div class="settings-item" style="cursor:pointer" onclick="open2FASetup()">
                <label style="cursor:pointer" data-t="Two-Factor Authentication">Two-Factor Authentication</label>
                <span style="font-size:13px;font-weight:600" id="tfa-status">Off</span>
            </div>
            <div class="settings-section" data-t="Appearance">Appearance</div>
            <div class="settings-item">
                <label data-t="Dark Theme">Dark Theme</label>
                <label class="toggle"><input type="checkbox" id="opt-allow-darktheme" onchange="setOpt('allow-darktheme',this.checked)"><span class="toggle-slider"></span></label>
            </div>
            <div class="settings-item" style="cursor:pointer" onclick="openLanguagePicker()">
                <label style="cursor:pointer" id="lbl-language" data-t="Language">Language</label>
                <span style="color:#2C8CFF;font-size:13px" id="current-lang-name">Default</span>
            </div>
            <div class="settings-section" style="margin-top:8px">
                <div class="settings-item" style="cursor:pointer" onclick="openAbout()">
                    <label style="cursor:pointer;color:#2C8CFF" data-t="About">About</label>
                </div>
            </div>
            <div class="modal-buttons">
                <button class="modal-btn primary" onclick="closeModal('settings-modal')" data-t="Close">Close</button>
            </div>
        </div>
    </div>

    <div class="modal-overlay" id="password-modal">
        <div class="modal" style="min-width:320px;max-width:380px">
            <button class="modal-close" onclick="closeModal('password-modal')">&times;</button>
            <h2 data-t="Password Settings">Password Settings</h2>
            <div style="font-size:11px;font-weight:700;text-transform:uppercase;color:var(--text-secondary);margin-bottom:6px" data-t="PERMANENT PASSWORD">PERMANENT PASSWORD</div>
            <div class="pw-dialog-row">
                <label style="font-size:13px" data-t="Password">Password</label>
                <input type="password" id="perm-pw-1" placeholder="">
            </div>
            <div class="pw-dialog-row">
                <label style="font-size:13px" data-t="Confirm">Confirm</label>
                <input type="password" id="perm-pw-2" placeholder="">
            </div>
            <div id="pw-error" style="color:#EF4444;font-size:12px;margin-bottom:4px;display:none"></div>
            <div style="text-align:right;margin-bottom:12px">
                <button class="modal-btn primary" onclick="savePermanentPassword()" data-t="Save">Save</button>
            </div>
            <div style="height:1px;background:var(--border-light);margin:8px 0 12px"></div>
            <div style="font-size:11px;font-weight:700;text-transform:uppercase;color:var(--text-secondary);margin-bottom:6px" data-t="RANDOM PASSWORD">RANDOM PASSWORD</div>
            <div style="display:flex;align-items:center;gap:12px;margin-bottom:10px">
                <span id="pw-random-display" style="font-size:18px;font-weight:600;letter-spacing:1px;flex:1"></span>
                <button class="modal-btn secondary" onclick="refreshTempPassword()" style="min-width:70px" data-t="Refresh">Refresh</button>
            </div>
            <div style="display:flex;align-items:center;gap:8px">
                <span style="font-size:13px" data-t="Length:">Length:</span>
                <div class="pw-length-btns">
                    <button class="pw-length-btn" onclick="setTempPwLen(6)">6</button>
                    <button class="pw-length-btn" onclick="setTempPwLen(8)">8</button>
                    <button class="pw-length-btn" onclick="setTempPwLen(10)">10</button>
                </div>
            </div>
        </div>
    </div>

    <div class="modal-overlay" id="about-modal">
        <div class="modal" style="min-width:380px;max-width:420px;padding:0;overflow:hidden">
            <div style="background:#475569;color:white;padding:14px 20px;text-align:center;font-size:16px;font-weight:700" data-t="About HopToDesk">About HopToDesk</div>
            <div style="padding:20px 24px">
                <div id="about-version" style="font-size:14px;margin-bottom:4px">Version: ...</div>
                <div style="font-size:14px;margin-bottom:12px">TeamID: <span id="about-teamid">(none)</span></div>
                <div style="margin-bottom:4px"><a href="#" onclick="callRust('open_url',['https://www.hoptodesk.com']);return false" style="color:#2C8CFF;font-size:14px" data-t="Website">Website</a></div>
                <div style="margin-bottom:16px"><a href="#" onclick="callRust('open_url',['https://www.hoptodesk.com/privacy']);return false" style="color:#2C8CFF;font-size:14px" data-t="Privacy Statement">Privacy Statement</a></div>
                <div style="font-size:13px;margin-bottom:4px" id="about-license"></div>
                <div style="font-size:13px;margin-bottom:16px" id="about-source"></div>
                <div class="fingerprint" id="about-fingerprint" style="font-size:12px;color:#64748B;margin-bottom:12px"></div>
                <div style="font-size:11px;color:#94A3B8;border-top:1px solid var(--border-light);padding-top:12px">Copyright &copy; HopToDesk. Originally forked from RustDesk.</div>
            </div>
            <div style="display:flex;justify-content:flex-end;padding:0 24px 16px">
                <button class="modal-btn primary" onclick="closeModal('about-modal')" style="min-width:80px">Close</button>
            </div>
        </div>
    </div>

    <div class="modal-overlay" id="tfa-modal">
        <div class="modal" style="min-width:380px;max-width:420px">
            <button class="modal-close" onclick="closeModal('tfa-modal')">&times;</button>
            <h2 id="tfa-modal-title" data-t="Two-Factor Authentication Setup">Two-Factor Authentication Setup</h2>
            <div id="tfa-setup-content">
                <p style="font-size:13px;color:var(--secondary);margin-bottom:12px" data-t="enable-2fa-desc">Open your authenticator app such as Google Authenticator and scan the QR code below.</p>
                <div style="text-align:center;margin:16px 0">
                    <img id="tfa-qr-img" style="width:180px;height:180px" />
                </div>
                <p style="font-size:13px;color:var(--secondary);margin-bottom:8px" data-t="enable-2fa-desc-verify">Enter the 6-digit code generated by your authenticator app:</p>
                <div style="text-align:center;margin-bottom:8px">
                    <input type="text" id="tfa-code-input" maxlength="6" placeholder="000000" style="width:150px;padding:10px;text-align:center;font-size:20px;font-family:monospace;letter-spacing:4px;border:2px solid var(--input-border);border-radius:8px;background:var(--input-bg);color:var(--text-dark)" />
                </div>
                <div id="tfa-error" style="display:none;text-align:center;color:#EF4444;font-size:12px;margin-bottom:8px">Invalid code. Please try again.</div>
            </div>
            <div class="modal-buttons">
                <button class="modal-btn secondary" onclick="closeModal('tfa-modal')" data-t="Cancel">Cancel</button>
                <button class="modal-btn primary" id="tfa-verify-btn" onclick="verify2FACode()" data-t="Verify">Verify</button>
            </div>
        </div>
    </div>

    <div class="modal-overlay" id="lang-modal">
        <div class="modal" style="min-width:320px;max-width:400px;max-height:80vh">
            <button class="modal-close" onclick="closeModal('lang-modal')">&times;</button>
            <h2>Language</h2>
            <div id="lang-list" style="max-height:55vh;overflow-y:auto;margin:8px 0"></div>
            <div class="modal-buttons">
                <button class="modal-btn primary" onclick="closeModal('lang-modal')">Close</button>
            </div>
        </div>
    </div>

    <div class="modal-overlay" id="invite-modal">
        <div class="modal" style="min-width:360px;max-width:420px">
            <h2 style="text-align:center;margin-bottom:12px" data-t="Invite A Device">Invite A Device</h2>
            <p style="font-size:13px;color:#64748B;margin-bottom:16px" data-t="Input an ID to send a connection invitation for this device:">Input an ID to send a connection invitation for this device:</p>
            <input type="text" id="invite-id-input" placeholder="Device ID to invite"
                   style="width:100%;padding:10px 12px;border:1px solid #E2E8F0;border-radius:8px;font-size:14px;outline:none"
                   onfocus="this.style.borderColor='#2C8CFF'" onblur="this.style.borderColor='#E2E8F0'"
                   onkeydown="if(event.key==='Enter')sendInvite()">
            <div class="modal-buttons">
                <button class="modal-btn secondary" onclick="closeModal('invite-modal')" data-t="Cancel">Cancel</button>
                <button class="modal-btn primary" onclick="sendInvite()" data-t="OK">OK</button>
            </div>
        </div>
    </div>

    <div class="modal-overlay" id="socks-modal">
        <div class="modal" style="min-width:380px;max-width:440px">
            <button class="modal-close" onclick="closeModal('socks-modal')">&times;</button>
            <h2 data-t="SOCKS5 Proxy">SOCKS5 Proxy</h2>
            <div class="pw-dialog-row">
                <label data-t="Hostname">Hostname</label>
                <input type="text" id="socks-proxy" placeholder="e.g. 127.0.0.1:1080">
            </div>
            <div class="pw-dialog-row">
                <label data-t="Username">Username</label>
                <input type="text" id="socks-username" placeholder="(optional)">
            </div>
            <div class="pw-dialog-row">
                <label data-t="Password">Password</label>
                <input type="password" id="socks-password" placeholder="(optional)">
            </div>
            <div class="modal-buttons">
                <button class="modal-btn secondary" onclick="closeModal('socks-modal')" data-t="Cancel">Cancel</button>
                <button class="modal-btn primary" onclick="saveSocks()" data-t="OK">OK</button>
            </div>
        </div>
    </div>

    <div class="modal-overlay" id="network-modal">
        <div class="modal" style="min-width:400px;max-width:480px">
            <button class="modal-close" onclick="closeModal('network-modal')">&times;</button>
            <h2 data-t="Choose Network">Choose Network</h2>
            <div style="margin:12px 0">
                <label style="display:flex;align-items:center;gap:8px;cursor:pointer;margin-bottom:10px">
                    <input type="radio" name="network-choice" value="default" id="net-default" checked onchange="toggleNetworkInput()">
                    <span data-t="HopToDesk Network (Default)">HopToDesk Network (Default)</span>
                </label>
                <label style="display:flex;align-items:center;gap:8px;cursor:pointer">
                    <input type="radio" name="network-choice" value="custom" id="net-custom" onchange="toggleNetworkInput()">
                    <span data-t="Custom Network Settings">Custom Network Settings</span>
                </label>
            </div>
            <div id="custom-url-section" style="display:none;margin:8px 0">
                <div class="pw-dialog-row">
                    <label data-t="API URL">API URL</label>
                    <input type="text" id="custom-api-url" placeholder="e.g. https://my-server.com">
                </div>
            </div>
            <div id="network-error" style="color:#EF4444;font-size:12px;margin-bottom:8px;display:none"></div>
            <div class="modal-buttons">
                <button class="modal-btn secondary" onclick="closeModal('network-modal')" data-t="Cancel">Cancel</button>
                <button class="modal-btn primary" onclick="saveNetwork()" data-t="OK">OK</button>
            </div>
        </div>
    </div>

    <div class="ctx-menu" id="ctx-menu">
        <div class="ctx-item" onclick="ctxAction('connect')" data-t="Connect">Connect</div>
        <div class="ctx-item" onclick="ctxAction('transfer')" data-t="Transfer File">Transfer File</div>
        <div class="ctx-item" onclick="ctxAction('tunnel')" data-t="TCP Tunneling">TCP Tunneling</div>
        <div class="ctx-sep"></div>
        <div class="ctx-item" onclick="ctxAction('rename')" data-t="Rename">Rename</div>
        <div class="ctx-item" onclick="ctxAction('copy-id')" data-t="Copy ID">Copy ID</div>
        <div class="ctx-item" id="ctx-fav-item" onclick="ctxAction('toggle-fav')" data-t="Add to Favorites">Add to Favorites</div>
        <div class="ctx-sep"></div>
        <div class="ctx-item" onclick="ctxAction('wol')" data-t="Wake On LAN">Wake On LAN</div>
        <div class="ctx-item" onclick="ctxAction('forget-pw')" data-t="Forget Password">Forget Password</div>
        <div class="ctx-item" style="color:#EF4444" onclick="ctxAction('remove')" data-t="Remove">Remove</div>
    </div>

    <script>
        var currentTab = 'recent';
        var sessionsData = {{ recent: [], favorites: [], discovered: [] }};
        var favSet = new Set();
        var searchQuery = '';
        var ctxPeerId = '';
        var pendingOptionKey = '';
        var optionsCache = {{}};

        function callRust(method, args) {{
            window.ipc.postMessage(JSON.stringify({{ method: method, args: args || [] }}));
        }}

        function getOsIcon(platform) {{
            var p = (platform || '').toLowerCase();
            var svg = '';
            if (p.indexOf('linux') >= 0) {{
                svg = '<svg viewBox="0 0 256 256" width="32" height="32"><g transform="translate(0 256) scale(.1 -.1)" fill="#64748B"><path d="m1215 2537c-140-37-242-135-286-278-23-75-23-131 1-383l18-200-54-60c-203-224-383-615-384-831v-51l-66-43c-113-75-194-199-194-300 0-110 99-234 244-305 103-50 185-69 296-69 100 0 156 14 211 54 26 18 35 19 78 10 86-18 233-24 335-12 85 10 222 38 269 56 9 4 19-7 29-35 20-50 52-64 136-57 98 8 180 52 282 156 124 125 180 244 180 380 0 80-28 142-79 179l-36 26 4 119c5 175-22 292-105 460-74 149-142 246-286 409-43 49-78 92-78 97 0 4-7 52-15 107-8 54-19 140-24 189-13 121-41 192-103 260-95 104-248 154-373 122z"/></g></svg>';
            }} else if (p.indexOf('mac') >= 0 || p.indexOf('darwin') >= 0) {{
                svg = '<svg viewBox="0 0 384 512" width="32" height="32"><path d="M318.7 268.7c-.2-36.7 16.4-64.4 50-84.8-18.8-26.9-47.2-41.7-84.7-44.6-35.5-2.8-74.3 20.7-88.5 20.7-15 0-49.4-19.7-76.4-19.7C63.3 141.2 4 184.8 4 273.5q0 39.3 14.4 81.2c12.8 36.7 59 126.7 107.2 125.2 25.2-.6 43-17.9 75.8-17.9 31.8 0 48.3 17.9 76.4 17.9 48.6-.7 90.4-82.5 102.6-119.3-65.2-30.7-61.7-90-61.7-91.9zm-56.6-164.2c27.3-32.4 24.8-61.9 24-72.5-24.1 1.4-52 16.4-67.9 34.9-17.5 19.8-27.8 44.3-25.6 71.9 26.1 2 49.9-11.4 69.5-34.3z" fill="#64748B"/></svg>';
            }} else if (p.indexOf('android') >= 0) {{
                svg = '<svg viewBox="0 0 553 553" width="32" height="32"><path d="M77 179a33 33 0 0 0-25 10 33 33 0 0 0-9 24v143a33 33 0 0 0 10 24 33 33 0 0 0 24 10c9 0 17-3 24-10a33 33 0 0 0 10-24V213c0-9-4-17-10-24a33 33 0 0 0-24-10zM352 51l24-44c1-3 1-5-2-6-3-2-5-1-7 2l-24 43a163 163 0 0 0-133 0L186 3c-2-3-4-4-7-2-2 1-3 3-1 6l23 44c-24 12-43 29-57 51a129 129 0 0 0-21 72h307c0-26-7-50-21-72a146 146 0 0 0-57-51zM124 407c0 10 4 19 11 26s15 10 26 10h24v76c0 9 4 17 10 24s15 10 24 10c10 0 18-3 25-10s10-15 10-24v-76h45v76c0 9 4 17 10 24s15 10 25 10c9 0 17-3 24-10s10-15 10-24v-76h25a35 35 0 0 0 25-10c7-7 11-16 11-26V185H124v222zm352-228a33 33 0 0 0-24 10 33 33 0 0 0-10 24v143a34 34 0 0 0 34 34c10 0 18-3 25-10s10-15 10-24V213c0-9-4-17-10-24a33 33 0 0 0-25-10z" fill="#64748B"/></svg>';
            }} else if (p.indexOf('bsd') >= 0 || p.indexOf('freebsd') >= 0) {{
                svg = '<svg viewBox="0 0 24 24" width="32" height="32"><rect x="2" y="3" width="20" height="14" rx="2" fill="none" stroke="#64748B" stroke-width="1.5"/><line x1="8" y1="21" x2="16" y2="21" stroke="#64748B" stroke-width="1.5" stroke-linecap="round"/><line x1="12" y1="17" x2="12" y2="21" stroke="#64748B" stroke-width="1.5"/><text x="12" y="13" text-anchor="middle" font-size="6" font-weight="bold" fill="#64748B">BSD</text></svg>';
            }} else if (p.indexOf('windows') >= 0 || p.indexOf('win') >= 0) {{
                svg = '<svg viewBox="0 0 448 512" width="32" height="32"><path d="M0 93.7l183.6-25.3v177.4H0V93.7zm0 324.6l183.6 25.3V268.4H0v149.9zm203.8 28L448 480V268.4H203.8v177.9zm0-380.6v180.1H448V32L203.8 65.7z" fill="#64748B"/></svg>';
            }} else {{
                svg = '<svg viewBox="0 0 24 24" width="32" height="32"><rect x="2" y="3" width="20" height="14" rx="2" fill="none" stroke="#64748B" stroke-width="1.5"/><line x1="8" y1="21" x2="16" y2="21" stroke="#64748B" stroke-width="1.5" stroke-linecap="round"/><line x1="12" y1="17" x2="12" y2="21" stroke="#64748B" stroke-width="1.5"/></svg>';
            }}
            return '<span class="os-icon">' + svg + '</span>';
        }}

        function renderSessions() {{
            var grid = document.getElementById('sessions-grid');
            var data = sessionsData[currentTab] || [];
            if (searchQuery) {{
                var q = searchQuery.toLowerCase();
                data = data.filter(function(p) {{
                    return (p.id && p.id.toLowerCase().indexOf(q) >= 0) ||
                           (p.hostname && p.hostname.toLowerCase().indexOf(q) >= 0) ||
                           (p.alias && p.alias.toLowerCase().indexOf(q) >= 0) ||
                           (p.username && p.username.toLowerCase().indexOf(q) >= 0);
                }});
            }}
            if (!data || data.length === 0) {{
                var msg = currentTab === 'recent' ? t('Recent sessions will show here.') :
                          currentTab === 'favorites' ? t('No favorites yet.') : t('No devices discovered.');
                grid.innerHTML = '<div class="sessions-empty"><span style="color:#B0BEC5;font-size:13px;">' + msg + '</span></div>';
                return;
            }}
            var html = '';
            data.forEach(function(p) {{
                var name = p.alias || p.username || p.hostname || '';
                if (name.length > 20) name = name.substring(0, 18) + '..';
                var isFav = favSet.has(p.id);
                var eid = (p.id||'').replace(/'/g,"\\'");
                html += '<div class="session-card" ondblclick="quickConnect(\'' + eid + '\')" ' +
                    'onclick="selectPeer(\'' + eid + '\')" ' +
                    'oncontextmenu="event.preventDefault();showCtxMenu(event,\'' + eid + '\')">' +
                    getOsIcon(p.platform) +
                    '<div class="peer-name" title="' + (p.alias || p.username || p.hostname || '').replace(/"/g,'&quot;') + '">' +
                        (name || '&nbsp;') + '</div>' +
                    '<div class="peer-id">' + formatId(p.id) + '</div>' +
                    '<span class="fav-icon ' + (isFav ? 'active' : '') + '" onclick="event.stopPropagation();toggleFav(\'' + eid + '\')">&#9829;</span>' +
                    '<span class="menu-icon" onclick="event.stopPropagation();showCtxMenu(event,\'' + eid + '\')">&#8942;</span>' +
                    '</div>';
            }});
            grid.innerHTML = html;
        }}

        function switchTab(tab) {{
            currentTab = tab;
            document.querySelectorAll('.tab').forEach(function(t) {{
                t.classList.toggle('active', t.getAttribute('data-tab') === tab);
            }});
            if (tab === 'discovered') callRust('discover');
            renderSessions();
        }}

        function filterSessions(q) {{ searchQuery = q; renderSessions(); }}
        function selectPeer(id) {{ document.getElementById('remote-id').value = id; }}
        function quickConnect(id) {{ document.getElementById('remote-id').value = id; doConnect(); }}

        function doConnect() {{
            var id = document.getElementById('remote-id').value.trim();
            if (id) callRust('new_remote', [id, 'connect', false]);
        }}
        function doTransfer() {{
            var id = document.getElementById('remote-id').value.trim();
            if (id) callRust('new_remote', [id, 'file-transfer', false]);
        }}

        function copyId() {{
            var id = document.getElementById('my-id').textContent;
            if (id && id !== '...') callRust('copy_text', [id]);
        }}
        function copyPassword() {{
            var pw = document.getElementById('my-password').textContent;
            if (pw && pw !== '------') callRust('copy_text', [pw]);
        }}

        function invitePeer() {{
            document.getElementById('invite-id-input').value = '';
            document.getElementById('invite-modal').classList.add('active');
            setTimeout(function() {{ document.getElementById('invite-id-input').focus(); }}, 100);
        }}

        function sendInvite() {{
            var peerId = document.getElementById('invite-id-input').value.trim().replace(/\\s+/g, '');
            if (!peerId) return;
            var myId = document.getElementById('my-id').textContent.replace(/\\s+/g, '');
            var pw = document.getElementById('my-password').textContent;
            callRust('new_remote', [peerId, 'invite', false, myId, pw]);
            closeModal('invite-modal');
        }}

        function toggleUnattended(el) {{
            callRust('set_option', ['unattended-access', el.checked ? 'true' : '']);
            if (el.checked) {{
                callRust('permanent_password');
            }} else {{
                callRust('temporary_password');
            }}
        }}

        function toggleFav(id) {{
            if (favSet.has(id)) {{ favSet.delete(id); }} else {{ favSet.add(id); }}
            callRust('store_fav_from_json', [JSON.stringify(Array.from(favSet))]);
            sessionsData.favorites = sessionsData.recent.filter(function(p) {{ return favSet.has(p.id); }});
            renderSessions();
        }}

        function showCtxMenu(e, id) {{
            ctxPeerId = id;
            var menu = document.getElementById('ctx-menu');
            var favItem = document.getElementById('ctx-fav-item');
            favItem.textContent = favSet.has(id) ? t('Remove from Favorites') : t('Add to Favorites');
            menu.style.left = Math.min(e.clientX, window.innerWidth - 180) + 'px';
            menu.style.top = Math.min(e.clientY, window.innerHeight - 280) + 'px';
            menu.classList.add('active');
        }}
        function hideCtxMenu() {{ document.getElementById('ctx-menu').classList.remove('active'); }}
        document.addEventListener('click', hideCtxMenu);

        function ctxAction(action) {{
            hideCtxMenu();
            if (!ctxPeerId) return;
            if (action === 'connect') {{
                callRust('new_remote', [ctxPeerId, 'connect', false]);
            }} else if (action === 'transfer') {{
                callRust('new_remote', [ctxPeerId, 'file-transfer', false]);
            }} else if (action === 'tunnel') {{
                callRust('new_remote', [ctxPeerId, 'port-forward', false]);
            }} else if (action === 'rename') {{
                var newName = prompt('Enter new name for ' + ctxPeerId + ':');
                if (newName !== null) callRust('set_peer_option', [ctxPeerId, 'alias', newName]);
                setTimeout(function() {{ callRust('get_recent_sessions'); }}, 300);
            }} else if (action === 'copy-id') {{
                callRust('copy_text', [ctxPeerId]);
            }} else if (action === 'toggle-fav') {{
                toggleFav(ctxPeerId);
            }} else if (action === 'wol') {{
                callRust('wol', [ctxPeerId]);
            }} else if (action === 'forget-pw') {{
                callRust('forget_password', [ctxPeerId]);
            }} else if (action === 'remove') {{
                if (confirm('Remove ' + ctxPeerId + ' from session list?')) {{
                    callRust('remove_peer', [ctxPeerId]);
                    setTimeout(function() {{ callRust('get_recent_sessions'); }}, 300);
                }}
            }}
        }}

        function openModal(id) {{ document.getElementById(id).classList.add('active'); }}
        function closeModal(id) {{ document.getElementById(id).classList.remove('active'); }}

        function openSettings() {{
            callRust('get_options_json');
            update2FAStatus();
            openModal('settings-modal');
        }}
        function setOpt(key, checked) {{
            var enableOpts = ['enable-keyboard','enable-clipboard','enable-file-transfer',
                              'enable-tunnel','enable-remote-restart','enable-lan-discovery','enable-wol',
                              'enable-audio'];
            if (enableOpts.indexOf(key) >= 0) {{
                callRust('set_option', [key, checked ? '' : 'N']);
            }} else {{
                callRust('set_option', [key, checked ? 'Y' : '']);
            }}
            if (key === 'allow-darktheme') {{
                document.documentElement.classList.toggle('darktheme', checked);
            }}
        }}
        function setOptInverted(key, checked) {{
            if (key === 'enable-audio') {{
                callRust('set_option', ['enable-audio', checked ? 'N' : '']);
            }} else if (key === 'stop-service') {{
                callRust('set_option', ['stop-service', checked ? '' : 'Y']);
            }}
        }}
        function loadSettingsFromOptions(opts) {{
            optionsCache = opts;
            var chkEnable = function(id, enableKey) {{
                var el = document.getElementById(id);
                if (el) el.checked = opts[enableKey] !== 'N';
            }};
            chkEnable('opt-enable-keyboard', 'enable-keyboard');
            chkEnable('opt-enable-clipboard', 'enable-clipboard');
            chkEnable('opt-enable-file-transfer', 'enable-file-transfer');
            chkEnable('opt-enable-tunnel', 'enable-tunnel');
            chkEnable('opt-enable-remote-restart', 'enable-remote-restart');
            chkEnable('opt-enable-lan-discovery', 'enable-lan-discovery');
            chkEnable('opt-enable-wol', 'enable-wol');
            var audioEl = document.getElementById('opt-enable-audio');
            if (audioEl) audioEl.checked = opts['enable-audio'] === 'N';
            var stopEl = document.getElementById('opt-stop-service');
            if (stopEl) stopEl.checked = !opts['stop-service'];
            var directEl = document.getElementById('opt-direct-server');
            if (directEl) directEl.checked = !!opts['direct-server'];
            var darkEl = document.getElementById('opt-allow-darktheme');
            if (darkEl) darkEl.checked = !!opts['allow-darktheme'];
            if (opts['allow-darktheme']) {{
                document.documentElement.classList.add('darktheme');
            }} else {{
                document.documentElement.classList.remove('darktheme');
            }}
            var recordEl = document.getElementById('opt-enable-record-session');
            if (recordEl) recordEl.checked = opts['enable-record-session'] !== 'N';
            var autoRecEl = document.getElementById('opt-allow-auto-record-incoming');
            if (autoRecEl) autoRecEl.checked = opts['allow-auto-record-incoming'] === 'Y';
        }}

        function openPasswordDialog() {{
            callRust('permanent_password_for_dialog');
            var currentPw = document.getElementById('my-password').textContent;
            document.getElementById('pw-random-display').textContent = (currentPw && currentPw !== '------') ? currentPw : '';
            var pwLen = optionsCache['temporary-password-length'] || '6';
            document.querySelectorAll('.pw-length-btn').forEach(function(b) {{
                b.classList.toggle('active', b.textContent == pwLen);
            }});
            document.getElementById('perm-pw-1').value = '';
            document.getElementById('perm-pw-2').value = '';
            document.getElementById('pw-error').style.display = 'none';
            openModal('password-modal');
        }}
        function openSocksDialog() {{
            callRust('get_socks_json');
            openModal('socks-modal');
        }}
        function saveSocks() {{
            var proxy = document.getElementById('socks-proxy').value.trim();
            var username = document.getElementById('socks-username').value.trim();
            var password = document.getElementById('socks-password').value.trim();
            callRust('set_socks', [proxy, username, password]);
            document.getElementById('socks-status').textContent = proxy ? proxy : '';
            closeModal('socks-modal');
        }}
        function openNetworkDialog() {{
            callRust('get_custom_api_url');
            openModal('network-modal');
        }}
        function toggleNetworkInput() {{
            var isCustom = document.getElementById('net-custom').checked;
            document.getElementById('custom-url-section').style.display = isCustom ? 'block' : 'none';
            document.getElementById('network-error').style.display = 'none';
        }}
        function saveNetwork() {{
            var errEl = document.getElementById('network-error');
            errEl.style.display = 'none';
            if (document.getElementById('net-default').checked) {{
                callRust('set_custom_api_url', ['']);
                document.getElementById('network-status').textContent = '';
                closeModal('network-modal');
                return;
            }}
            var url = document.getElementById('custom-api-url').value.trim();
            if (!url) {{
                errEl.textContent = 'Please enter a URL';
                errEl.style.display = 'block';
                return;
            }}
            if (url.indexOf('://') < 0) url = 'http://' + url;
            url = url.replace(/\/+$/, '');
            errEl.textContent = 'Validating...';
            errEl.style.display = 'block';
            errEl.style.color = '#64748B';
            fetch(url + '/api/login', {{ method: 'GET', mode: 'cors' }})
                .then(function(r) {{ return r.text(); }})
                .then(function(body) {{
                    if (body.indexOf('rendezvous') >= 0 || body.indexOf('turnservers') >= 0 || body.length > 0) {{
                        callRust('set_custom_api_url', [url]);
                        document.getElementById('network-status').textContent = url;
                        closeModal('network-modal');
                    }} else {{
                        errEl.style.color = '#EF4444';
                        errEl.textContent = 'Invalid server: unexpected response';
                    }}
                }})
                .catch(function(e) {{
                    errEl.style.color = '#EF4444';
                    errEl.textContent = 'Could not connect: ' + e.message;
                }});
        }}
        function setOptRecording(key, checked) {{
            callRust('set_option', [key, checked ? '' : 'N']);
        }}
        function refreshTempPassword() {{
            callRust('update_temporary_password');
        }}
        function setTempPwLen(n) {{
            callRust('set_option', ['temporary-password-length', String(n)]);
            callRust('update_temporary_password');
            document.querySelectorAll('.pw-length-btn').forEach(function(b) {{
                b.classList.toggle('active', b.textContent == String(n));
            }});
        }}
        function savePermanentPassword() {{
            var pw1 = document.getElementById('perm-pw-1').value;
            var pw2 = document.getElementById('perm-pw-2').value;
            var errEl = document.getElementById('pw-error');
            if (pw1 !== pw2) {{
                errEl.textContent = 'Passwords do not match';
                errEl.style.display = 'block';
                return;
            }}
            if (pw1.length > 0 && pw1.length < 6) {{
                errEl.textContent = 'Password must be at least 6 characters';
                errEl.style.display = 'block';
                return;
            }}
            errEl.style.display = 'none';
            callRust('set_permanent_password', [pw1]);
            document.getElementById('perm-pw-1').value = '';
            document.getElementById('perm-pw-2').value = '';
            var toggle = document.getElementById('unattended-toggle');
            if (toggle && toggle.checked && pw1) {{
                document.getElementById('my-password').textContent = pw1;
            }} else if (!pw1) {{
                callRust('temporary_password');
            }}
        }}

        function openAbout() {{
            closeModal('settings-modal');
            callRust('get_version');
            callRust('get_fingerprint');
            openModal('about-modal');
        }}

        var pending2FASecret = '';
        function open2FASetup() {{
            window._tfa_action = 'toggle';
            callRust('has_valid_2fa');
        }}
        function show2FASetupModal() {{
            callRust('generate2fa');
        }}
        function verify2FACode() {{
            var code = document.getElementById('tfa-code-input').value.trim();
            if (code.length !== 6) {{
                document.getElementById('tfa-error').style.display = 'block';
                return;
            }}
            callRust('verify2fa', [code]);
        }}
        function update2FAStatus() {{
            callRust('has_valid_2fa');
        }}

        var availableLangs = [];
        var currentLangCode = '';
        function openLanguagePicker() {{
            callRust('get_langs');
            callRust('get_local_option', ['lang']);
            openModal('lang-modal');
        }}
        function renderLangList() {{
            var el = document.getElementById('lang-list');
            if (!el || !availableLangs.length) return;
            var html = '<div class="lang-item' + (!currentLangCode ? ' active' : '') + '" onclick="selectLang(\'\')">Default (System)</div>';
            for (var i = 0; i < availableLangs.length; i++) {{
                var l = availableLangs[i];
                var code = l[0], name = l[1];
                var cls = code === currentLangCode ? ' active' : '';
                html += '<div class="lang-item' + cls + '" onclick="selectLang(\'' + code + '\')">' + name + '</div>';
            }}
            el.innerHTML = html;
            var nameEl = document.getElementById('current-lang-name');
            if (nameEl) {{
                if (!currentLangCode) {{ nameEl.textContent = 'Default'; }}
                else {{
                    var found = false;
                    for (var j = 0; j < availableLangs.length; j++) {{
                        if (availableLangs[j][0] === currentLangCode) {{ nameEl.textContent = availableLangs[j][1]; found = true; break; }}
                    }}
                    if (!found) nameEl.textContent = currentLangCode;
                }}
            }}
        }}
        function selectLang(code) {{
            currentLangCode = code;
            callRust('set_local_option', ['lang', code]);
            renderLangList();
            loadTranslations();
        }}

        var translations = {{}};
        var translationKeys = [
            'This Device', 'Remote Control', 'Your ID', 'Password', 'Unattended Access',
            'Copy', 'Invite', 'Set', 'Partner ID', 'Connect', 'Transfer File',
            'Recent Sessions', 'Favorites', 'Discovered', 'Search ID',
            'Recent sessions will show here.', 'No favorites yet.', 'No devices discovered.',
            'Settings', 'Remote Access', 'Keyboard/Mouse', 'Clipboard', 'File Transfer',
            'Remote Restart', 'TCP Tunneling', 'Wake On LAN', 'Audio Input', 'Mute',
            'Network', 'Direct IP Access', 'LAN Discovery', 'Security',
            'Allow Incoming Connections', 'Appearance', 'Dark Theme', 'Language',
            'About', 'Close', 'Connected', 'Disconnect', 'Accept', 'Dismiss',
            'Rename', 'Remove', 'Add to Favorites', 'Remove from Favorites',
            'Uninstall', 'Waiting for new connection ...',
            'Enable Keyboard/Mouse', 'Enable Clipboard', 'Enable File Transfer',
            'Enable remote restart', 'Enable TCP Tunneling',
            'Invite A Device', 'Ready', 'Connecting...', 'Not Ready',
            'Enter Remote ID', 'ID / Relay Server',
            'TCP Tunneling', 'Copy ID', 'Forget Password',
            'Password Settings', 'One-time password length',
            'Generate New Temporary Password', 'Set permanent password',
            'Confirm Password', 'Cancel', 'OK', 'Save',
            'Input an ID to send a connection invitation for this device:',
            'Two-Factor Authentication', 'Two-Factor Authentication Setup',
            'enable-2fa-desc', 'enable-2fa-desc-verify', 'Verify',
            'About HopToDesk', 'Website', 'Privacy Statement',
            'Recording', 'Enable Recording Session', 'Automatically record incoming sessions',
            'SOCKS5 Proxy', 'Hostname', 'Username',
            'Choose Network', 'HopToDesk Network (Default)', 'Custom Network Settings',
            'API URL',
            'This software is licensed under', 'Source code is available', 'here'
        ];
        function loadTranslations() {{
            callRust('translate_batch', [JSON.stringify(translationKeys)]);
        }}
        function t(key) {{
            return translations[key] || key;
        }}
        function formatId(id) {{
            if (!id) return '...';
            var s = id.replace(/\\s+/g, '');
            return s.replace(/(.{{3}})(?=.)/g, '$1 ');
        }}
        function applyTranslations() {{
            document.querySelectorAll('[data-t]').forEach(function(el) {{
                var key = el.getAttribute('data-t');
                if (el.children.length === 0) {{
                    el.textContent = t(key);
                }}
            }});
            // Update About license/source lines with embedded links
            var licEl = document.getElementById('about-license');
            if (licEl) licEl.innerHTML = t('This software is licensed under') + ' <a href="#" onclick="callRust(\'open_url\',[\'https://www.gnu.org/licenses/agpl-3.0.html\']);return false" style="color:#2C8CFF">AGPL 3.0</a>';
            var srcEl = document.getElementById('about-source');
            if (srcEl) srcEl.innerHTML = t('Source code is available') + ' <a href="#" onclick="callRust(\'open_url\',[\'https://www.gitlab.com/hoptodesk/hoptodesk\']);return false" style="color:#2C8CFF">' + t('here') + '</a>';
        }}
        loadTranslations();

        window.onRustResponse = function(method, data) {{
            if (method === 'get_id') {{
                document.getElementById('my-id').textContent = formatId(data);
            }} else if (method === 'get_connect_status') {{
                var s = (typeof data === 'string') ? JSON.parse(data) : data;
                var dot = document.getElementById('status-dot');
                var txt = document.getElementById('status-text');
                if (s.status_num === 1) {{
                    dot.className = 'status-dot online';
                    txt.textContent = t('Ready');
                }} else if (s.status_num === 0) {{
                    dot.className = 'status-dot connecting';
                    txt.textContent = t('Connecting...');
                }} else {{
                    dot.className = 'status-dot offline';
                    txt.textContent = t('Not Ready');
                }}
                if (s.id) document.getElementById('my-id').textContent = formatId(s.id);
            }} else if (method === 'temporary_password') {{
                var toggle = document.getElementById('unattended-toggle');
                if (!toggle || !toggle.checked) {{
                    document.getElementById('my-password').textContent = data || '------';
                }}
                var rdEl = document.getElementById('pw-random-display');
                if (rdEl) rdEl.textContent = data || '';
            }} else if (method === 'permanent_password_for_dialog') {{
                if (data) {{
                    document.getElementById('perm-pw-1').value = data;
                    document.getElementById('perm-pw-2').value = data;
                }}
            }} else if (method === 'permanent_password') {{
                if (data) {{
                    document.getElementById('my-password').textContent = data;
                }} else {{
                    // No permanent password set — auto-generate one
                    var chars = 'abcdefghijkmnpqrstuvwxyz23456789';
                    var pw = '';
                    for (var i = 0; i < 6; i++) pw += chars.charAt(Math.floor(Math.random() * chars.length));
                    callRust('set_permanent_password', [pw]);
                    document.getElementById('my-password').textContent = pw;
                }}
            }} else if (method === 'get_recent_sessions') {{
                var sessions = (typeof data === 'string') ? JSON.parse(data) : data;
                sessionsData.recent = sessions || [];
                sessionsData.favorites = sessionsData.recent.filter(function(p) {{ return favSet.has(p.id); }});
                renderSessions();
            }} else if (method === 'get_fav_json') {{
                var favs = (typeof data === 'string') ? JSON.parse(data) : data;
                if (Array.isArray(favs)) {{
                    favSet = new Set(favs);
                    sessionsData.favorites = sessionsData.recent.filter(function(p) {{ return favSet.has(p.id); }});
                    if (currentTab === 'favorites') renderSessions();
                }}
            }} else if (method === 'get_lan_peers') {{
                var peers = (typeof data === 'string') ? JSON.parse(data) : data;
                sessionsData.discovered = peers || [];
                if (currentTab === 'discovered') renderSessions();
            }} else if (method === 'get_option') {{
                if (pendingOptionKey === 'unattended-access') {{
                    var isUnattended = (data === 'true');
                    document.getElementById('unattended-toggle').checked = isUnattended;
                    if (isUnattended) {{
                        callRust('permanent_password');
                    }}
                }}
                pendingOptionKey = '';
            }} else if (method === 'get_options_json') {{
                var opts = (typeof data === 'string') ? JSON.parse(data) : data;
                if (opts && typeof opts === 'object') loadSettingsFromOptions(opts);
            }} else if (method === 'get_version') {{
                document.getElementById('about-version').textContent = 'Version: ' + (data || '?');
            }} else if (method === 'get_fingerprint') {{
                var fpEl = document.getElementById('about-fingerprint');
                if (data) fpEl.textContent = 'Fingerprint: ' + data;
            }} else if (method === 'has_valid_2fa') {{
                var enabled = (data === 'true');
                var statusEl = document.getElementById('tfa-status');
                if (statusEl) statusEl.textContent = enabled ? 'On' : 'Off';
                if (statusEl) statusEl.style.color = enabled ? '#22C55E' : 'var(--secondary)';
                // If called from open2FASetup, handle the toggle
                if (window._tfa_action === 'toggle') {{
                    window._tfa_action = '';
                    if (enabled) {{
                        callRust('set_option', ['2fa', '']);
                        var s2 = document.getElementById('tfa-status');
                        if (s2) {{ s2.textContent = 'Off'; s2.style.color = 'var(--secondary)'; }}
                    }} else {{
                        show2FASetupModal();
                    }}
                }}
            }} else if (method === 'generate2fa') {{
                pending2FASecret = data || '';
                callRust('generate_2fa_img_src', [pending2FASecret]);
            }} else if (method === 'generate_2fa_img_src') {{
                var imgEl = document.getElementById('tfa-qr-img');
                if (imgEl) imgEl.src = data || '';
                document.getElementById('tfa-code-input').value = '';
                document.getElementById('tfa-error').style.display = 'none';
                closeModal('settings-modal');
                openModal('tfa-modal');
            }} else if (method === 'verify2fa') {{
                if (data === 'true') {{
                    closeModal('tfa-modal');
                    var s3 = document.getElementById('tfa-status');
                    if (s3) {{ s3.textContent = 'On'; s3.style.color = '#22C55E'; }}
                }} else {{
                    document.getElementById('tfa-error').style.display = 'block';
                    document.getElementById('tfa-code-input').value = '';
                    document.getElementById('tfa-code-input').focus();
                }}
            }} else if (method === 'get_langs') {{
                try {{ availableLangs = (typeof data === 'string') ? JSON.parse(data) : data; }} catch(e) {{}}
                renderLangList();
            }} else if (method === 'get_local_option') {{
                var opt = (typeof data === 'string') ? JSON.parse(data) : data;
                if (opt && opt.key === 'lang') {{
                    currentLangCode = opt.value || '';
                    renderLangList();
                }}
            }} else if (method === 'get_socks_json') {{
                try {{
                    var socks = (typeof data === 'string') ? JSON.parse(data) : data;
                    if (socks) {{
                        document.getElementById('socks-proxy').value = socks[0] || '';
                        document.getElementById('socks-username').value = socks[1] || '';
                        document.getElementById('socks-password').value = socks[2] || '';
                        document.getElementById('socks-status').textContent = socks[0] || '';
                    }}
                }} catch(e) {{}}
            }} else if (method === 'get_custom_api_url') {{
                var url = (data || '').replace(/^"|"$/g, '');
                if (url) {{
                    document.getElementById('net-custom').checked = true;
                    document.getElementById('net-default').checked = false;
                    document.getElementById('custom-url-section').style.display = 'block';
                    document.getElementById('custom-api-url').value = url;
                    document.getElementById('network-status').textContent = url;
                }} else {{
                    document.getElementById('net-default').checked = true;
                    document.getElementById('net-custom').checked = false;
                    document.getElementById('custom-url-section').style.display = 'none';
                    document.getElementById('custom-api-url').value = '';
                    document.getElementById('network-status').textContent = '';
                }}
            }} else if (method === 'translate_batch_result') {{
                try {{
                    var tr = (typeof data === 'string') ? JSON.parse(data) : data;
                    if (tr && typeof tr === 'object') {{
                        for (var k in tr) translations[k] = tr[k];
                    }}
                    applyTranslations();
                }} catch(e) {{}}
            }}
        }};

        function getOption(key) {{
            pendingOptionKey = key;
            callRust('get_option', [key]);
        }}

        callRust('get_id');
        callRust('get_connect_status');
        callRust('temporary_password');
        callRust('get_recent_sessions');
        callRust('get_fav_json');
        callRust('get_lan_peers');
        callRust('get_custom_api_url');
        getOption('unattended-access');

        setInterval(function() {{ callRust('get_connect_status'); }}, 3000);
        setInterval(function() {{ callRust('get_recent_sessions'); callRust('get_fav_json'); }}, 30000);
    </script>
</body>
</html>"##, title = title)
}

fn get_remote_page_html() -> String {
    r##"<!DOCTYPE html>
<html>
<head>
    <meta charset="UTF-8">
    <title>Remote Session</title>
    <style>
        * { margin: 0; padding: 0; box-sizing: border-box; }
        html, body { width: 100%; height: 100%; background: #000; color: #fff; font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; overflow: hidden; }
        .toolbar { background: #1a1a2e; padding: 2px 8px; height: 36px; border-bottom: 1px solid #333;
                   display: flex; align-items: center; gap: 2px; white-space: nowrap; position: relative; z-index: 50; }
        .toolbar-left { display: flex; align-items: center; gap: 4px; min-width: 0; flex-shrink: 1; }
        .toolbar-center { display: flex; align-items: center; gap: 2px; flex-shrink: 0; }
        .toolbar-right { margin-left: auto; flex-shrink: 0; }
        .toolbar-sep { width: 1px; height: 20px; background: #444; margin: 0 4px; flex-shrink: 0; }
        .tb { background: transparent; color: #aaa; border: 1px solid transparent; border-radius: 4px;
              padding: 3px 6px; font-size: 11px; cursor: pointer; display: flex; align-items: center; gap: 3px;
              white-space: nowrap; height: 28px; }
        .tb:hover { background: #2a2a4a; color: #fff; border-color: #444; }
        .tb.active { background: #2a2a4a; color: #4af; border-color: #4af; }
        .tb.danger:hover { color: #ff6b6b; }
        .tb svg { width: 16px; height: 16px; flex-shrink: 0; }
        .tb .label { display: inline; }
        .peer-info { color: #888; font-size: 12px; overflow: hidden; text-overflow: ellipsis; max-width: 200px; }
        .secure-icon { display: flex; align-items: center; }
        .secure-icon svg { width: 18px; height: 18px; }
        .canvas-wrap { position: absolute; top: 36px; left: 0; right: 0; bottom: 0; display: flex;
                       align-items: center; justify-content: center; overflow: hidden; }
        .canvas-wrap.with-chat { right: 300px; }
        canvas { max-width: 100%; max-height: 100%; display: none; }
        .canvas-wrap.view-original canvas { max-width: none; max-height: none; }
        .canvas-wrap.view-stretch canvas { width: 100%; height: 100%; max-width: none; max-height: none; }
        .status-overlay { text-align: center; }
        .status-overlay h2 { font-size: 20px; font-weight: 600; margin-bottom: 8px; }
        .status-overlay p { font-size: 14px; color: #aaa; }
        .dropdown { display: none; position: absolute; top: 34px; background: #1e1e2e; border: 1px solid #444;
                    border-radius: 8px; padding: 4px 0; min-width: 180px; z-index: 200; box-shadow: 0 4px 16px rgba(0,0,0,0.5); }
        .dropdown.open { display: block; }
        .dd-item { padding: 6px 14px; font-size: 12px; color: #ccc; cursor: pointer; display: flex; align-items: center; gap: 8px; }
        .dd-item:hover { background: #2a2a4a; color: #fff; }
        .dd-item.selected { color: #4af; }
        .dd-item.selected::before { content: '\2713'; margin-right: 2px; }
        .dd-sep { height: 1px; background: #444; margin: 4px 0; }
        .dd-header { padding: 4px 14px; font-size: 10px; color: #666; text-transform: uppercase; letter-spacing: 0.5px; }
        .chat-panel { display: none; position: absolute; top: 36px; right: 0; bottom: 0; width: 300px;
                      background: #1a1a2e; border-left: 1px solid #333; z-index: 40; flex-direction: column; }
        .chat-panel.open { display: flex; }
        .chat-header { padding: 8px 12px; border-bottom: 1px solid #333; display: flex; align-items: center; justify-content: space-between; }
        .chat-header span { font-size: 13px; font-weight: 600; }
        .chat-close { background: none; border: none; color: #888; cursor: pointer; font-size: 16px; }
        .chat-messages { flex: 1; overflow-y: auto; padding: 8px 12px; }
        .chat-msg { margin-bottom: 8px; font-size: 12px; }
        .chat-msg .name { color: #4af; font-weight: 600; }
        .chat-msg .time { color: #666; font-size: 10px; margin-left: 6px; }
        .chat-msg .text { color: #ccc; margin-top: 2px; word-break: break-word; }
        .chat-input-wrap { padding: 8px; border-top: 1px solid #333; display: flex; gap: 6px; }
        .chat-input-wrap input { flex: 1; padding: 6px 10px; background: #2a2a3a; border: 1px solid #444;
                                 border-radius: 6px; color: #eee; font-size: 12px; outline: none; }
        .chat-input-wrap input:focus { border-color: #4af; }
        .chat-input-wrap button { padding: 6px 12px; background: #2C8CFF; color: #fff; border: none;
                                  border-radius: 6px; font-size: 12px; cursor: pointer; }
        .modal-overlay { display: none; position: absolute; top: 0; left: 0; width: 100%; height: 100%;
                         background: rgba(0,0,0,0.7); z-index: 100; text-align: center; padding-top: 15%; }
        .modal-overlay.active { display: block; }
        .modal { display: inline-block; background: #1e1e2e; border-radius: 12px; padding: 24px;
                 min-width: 340px; max-width: 440px; color: #eee; border: 1px solid #444; text-align: left; }
        .modal h2 { font-size: 16px; margin-bottom: 12px; font-weight: 600; }
        .modal p { font-size: 13px; color: #aaa; margin-bottom: 12px; }
        .modal input { width: 90%; padding: 10px 12px; border: 1px solid #555; border-radius: 6px;
                       font-size: 14px; background: #2a2a3a; color: #eee; outline: none; margin-bottom: 8px; }
        .modal input:focus { border-color: #2C8CFF; }
        .modal-buttons { text-align: right; margin-top: 16px; }
        .modal-btn { padding: 8px 20px; border-radius: 6px; font-size: 13px; cursor: pointer; font-weight: 500;
                     display: inline-block; margin-left: 8px; }
        .modal-btn.primary { background: #2C8CFF; color: white; border: none; }
        .modal-btn.primary:hover { background: #1a7ae6; }
        .modal-btn.secondary { background: transparent; color: #aaa; border: 1px solid #555; }
        .quality-bar { display: none; position: absolute; bottom: 8px; right: 12px; background: rgba(0,0,0,0.7);
                       color: #aaa; font-size: 11px; padding: 4px 8px; border-radius: 4px; z-index: 5; }
        .security-code-grid { font-family: 'Courier New', monospace; font-size: 14px; line-height: 1.8;
                              letter-spacing: 2px; text-align: center; color: #ccc; }
    </style>
</head>
<body>
    <div class="toolbar" id="toolbar">
        <div class="toolbar-left">
            <button class="tb" id="fullscreen-btn" onclick="toggleFullscreen()" title="Full Screen" style="display:none">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M8 3H5a2 2 0 0 0-2 2v3m18 0V5a2 2 0 0 0-2-2h-3m0 18h3a2 2 0 0 0 2-2v-3M3 16v3a2 2 0 0 0 2 2h3"/></svg>
            </button>
            <span class="secure-icon" id="secure-icon" title=""></span>
            <span class="peer-info" id="peer-info">Connecting...</span>
            <button class="tb" id="display-selector-btn" onclick="toggleDropdown('display-switch-menu',this)" style="display:none" title="Switch Display">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="2" y="3" width="20" height="14" rx="2" ry="2"/><path d="M8 21h8m-4-4v4"/></svg>
                <span id="display-num"></span>
            </button>
        </div>
        <div class="toolbar-sep" id="sep1" style="display:none"></div>
        <div class="toolbar-center" id="toolbar-btns" style="display:none">
            <button class="tb" id="chat-btn" onclick="toggleChat()" title="Chat">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/></svg>
            </button>
            <button class="tb" id="action-btn" onclick="toggleDropdown('action-menu',this)" title="Control Actions">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M13 2L3 14h9l-1 8 10-12h-9l1-8z"/></svg>
            </button>
            <button class="tb" id="display-settings-btn" onclick="toggleDropdown('display-settings-menu',this)" title="Display Settings">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2" ry="2"/><path d="M8 21h8m-4-4v4"/></svg>
            </button>
            <button class="tb" id="keyboard-btn" onclick="toggleKeyboard()" title="Keyboard">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="2" y="4" width="20" height="16" rx="2"/><path d="M6 8h.01M10 8h.01M14 8h.01M18 8h.01M6 12h.01M10 12h.01M14 12h.01M18 12h.01M8 16h8"/></svg>
            </button>
            <div class="toolbar-sep"></div>
            <button class="tb" id="file-transfer-btn" onclick="openFileTransfer()" title="Transfer File">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><polyline points="14 2 14 8 20 8"/><path d="M12 18v-6m-3 3l3-3 3 3"/></svg>
            </button>
            <button class="tb" id="screenshot-btn" onclick="takeScreenshot()" title="Screenshot">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M23 19a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4l2-3h6l2 3h4a2 2 0 0 1 2 2z"/><circle cx="12" cy="13" r="4"/></svg>
            </button>
            <button class="tb" id="record-btn" onclick="toggleRecording()" title="Record Session">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><circle cx="12" cy="12" r="4" fill="currentColor"/></svg>
            </button>
            <button class="tb" id="switch-sides-btn" onclick="callRust('switch_sides')" title="Switch Sides" style="display:none">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M16 3l4 4-4 4"/><path d="M20 7H4"/><path d="M8 21l-4-4 4-4"/><path d="M4 17h16"/></svg>
            </button>
            <button class="tb" id="privacy-mode-btn" onclick="togglePrivacyMode()" title="Privacy Mode" style="display:none">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10 12a2 2 0 1 0 4 0a2 2 0 0 0-4 0"/><path d="M21 12c-2.4 4-5.4 6-9 6c-3.6 0-6.6-2-9-6c2.4-4 5.4-6 9-6c3.6 0 6.6 2 9 6"/></svg>
            </button>
            <button class="tb" id="block-input-btn" title="Input Blocked" style="display:none;color:#EF4444">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><line x1="4.93" y1="4.93" x2="19.07" y2="19.07"/></svg>
            </button>
        </div>
        <div class="toolbar-right">
            <button class="tb danger" onclick="disconnect()">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M18.36 5.64a9 9 0 1 1-12.73 0"/><line x1="12" y1="2" x2="12" y2="12"/></svg>
                <span class="label">Disconnect</span>
            </button>
        </div>

        <div class="dropdown" id="action-menu">
            <div class="dd-item" onclick="callRust('ctrl_alt_del');hideDropdowns()">Ctrl + Alt + Del</div>
            <div class="dd-item" onclick="callRust('lock_screen');hideDropdowns()">Lock Screen</div>
            <div class="dd-item" onclick="callRust('restart_remote_device');hideDropdowns()">Restart Remote Device</div>
            <div class="dd-sep"></div>
            <div class="dd-item" onclick="callRust('refresh');hideDropdowns()">Refresh</div>
        </div>

        <div class="dropdown" id="display-settings-menu">
            <div class="dd-header">View Style</div>
            <div class="dd-item selected" data-style="shrink" onclick="setViewStyle('shrink')">Shrink</div>
            <div class="dd-item" data-style="original" onclick="setViewStyle('original')">Original</div>
            <div class="dd-item" data-style="stretch" onclick="setViewStyle('stretch')">Stretch</div>
            <div class="dd-sep"></div>
            <div class="dd-header">Image Quality</div>
            <div class="dd-item selected" data-quality="balanced" onclick="setImageQuality('balanced')">Balanced</div>
            <div class="dd-item" data-quality="best" onclick="setImageQuality('best')">Best image quality</div>
            <div class="dd-item" data-quality="low" onclick="setImageQuality('low')">Optimize reaction time</div>
            <div class="dd-item" data-quality="custom" onclick="setImageQuality('custom')">Custom</div>
        </div>

        <div class="dropdown" id="display-switch-menu"></div>
    </div>

    <div class="canvas-wrap" id="canvas-wrap">
        <div class="status-overlay" id="status-overlay">
            <h2 id="status-title">Connecting...</h2>
            <p id="status-text">Establishing connection to remote device</p>
        </div>
        <canvas id="remote-canvas"></canvas>
        <div class="quality-bar" id="quality-bar"></div>
    </div>

    <div class="chat-panel" id="chat-panel">
        <div class="chat-header"><span>Chat</span><button class="chat-close" onclick="toggleChat()">&times;</button></div>
        <div class="chat-messages" id="chat-messages"></div>
        <div class="chat-input-wrap">
            <input id="chat-input" placeholder="Type a message..." onkeydown="if(event.key==='Enter')sendChat()">
            <button onclick="sendChat()">Send</button>
        </div>
    </div>

    <div class="modal-overlay" id="password-modal">
        <div class="modal">
            <h2 id="pw-dialog-title">Enter Password</h2>
            <p id="pw-dialog-text">Please enter the password for the remote device.</p>
            <input type="password" id="pw-input" placeholder="Password"
                   onkeydown="if(event.key==='Enter')submitPassword()">
            <div class="modal-buttons">
                <button class="modal-btn secondary" onclick="cancelPassword()">Cancel</button>
                <button class="modal-btn primary" onclick="submitPassword()">OK</button>
            </div>
        </div>
    </div>

    <div class="modal-overlay" id="msg-modal">
        <div class="modal">
            <h2 id="msg-title">Message</h2>
            <p id="msg-text"></p>
            <div class="modal-buttons" id="msg-buttons">
                <button class="modal-btn primary" onclick="closeMsgBox()">OK</button>
            </div>
        </div>
    </div>
<script>
var connected = false;
var canvas = document.getElementById('remote-canvas');
var ctx = canvas.getContext('2d');
var keyboardEnabled = true;
var currentMsgType = '';
var fetchingFrame = false;
var frameServerUrl = '';
var isFullscreen = false;
var viewStyle = 'shrink';
var chatOpen = false;
var displays = [];
var currentDisplay = 0;
var peerPlatform = '';
var chatMessages = [];

function callRust(method, args) {
    window.ipc.postMessage(JSON.stringify({ method: method, args: args || [] }));
}

function disconnect() { callRust('close'); }

function submitPassword() {
    var pw = document.getElementById('pw-input').value;
    if (pw) {
        callRust('login', [pw, '', '', false]);
        document.getElementById('password-modal').classList.remove('active');
        document.getElementById('pw-input').value = '';
        document.getElementById('status-title').textContent = 'Authenticating...';
        document.getElementById('status-text').textContent = 'Verifying password';
    }
}

function cancelPassword() {
    document.getElementById('password-modal').classList.remove('active');
    disconnect();
}

function closeMsgBox() {
    document.getElementById('msg-modal').classList.remove('active');
    currentMsgType = '';
}

function toggleKeyboard() {
    keyboardEnabled = !keyboardEnabled;
    var btn = document.getElementById('keyboard-btn');
    btn.classList.toggle('active', keyboardEnabled);
    btn.title = keyboardEnabled ? 'Keyboard (ON)' : 'Keyboard (OFF)';
}

function toggleFullscreen() {
    callRust('fullscreen');
}

function takeScreenshot() {
    callRust('screenshot');
}

var isRecording = false;
function toggleRecording() {
    isRecording = !isRecording;
    callRust('record_screen', [isRecording]);
    var btn = document.getElementById('record-btn');
    btn.classList.toggle('active', isRecording);
    btn.title = isRecording ? 'Stop Recording' : 'Record Session';
    if (isRecording) {
        btn.style.color = '#EF4444';
    } else {
        btn.style.color = '';
    }
}

function openFileTransfer() {
    callRust('open_file_transfer');
}

var privacyModeOn = false;
function togglePrivacyMode() {
    privacyModeOn = !privacyModeOn;
    callRust('toggle_privacy_mode', [privacyModeOn]);
    var btn = document.getElementById('privacy-mode-btn');
    btn.classList.toggle('active', privacyModeOn);
    if (privacyModeOn) {
        btn.innerHTML = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10.585 10.587a2 2 0 0 0 2.829 2.828"/><path d="M16.681 16.673a8.717 8.717 0 0 1-4.681 1.327c-3.6 0-6.6-2-9-6c1.272-2.12 2.712-3.678 4.32-4.674m2.86-1.146a9.055 9.055 0 0 1 1.82-.18c3.6 0 6.6 2 9 6c-.666 1.11-1.379 2.067-2.138 2.87"/><path d="M3 3l18 18"/></svg>';
    } else {
        btn.innerHTML = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10 12a2 2 0 1 0 4 0a2 2 0 0 0-4 0"/><path d="M21 12c-2.4 4-5.4 6-9 6c-3.6 0-6.6-2-9-6c2.4-4 5.4-6 9-6c3.6 0 6.6 2 9 6"/></svg>';
    }
}

function toggleChat() {
    chatOpen = !chatOpen;
    document.getElementById('chat-panel').classList.toggle('open', chatOpen);
    document.getElementById('canvas-wrap').classList.toggle('with-chat', chatOpen);
    document.getElementById('chat-btn').classList.toggle('active', chatOpen);
    if (chatOpen) document.getElementById('chat-input').focus();
}

function sendChat() {
    var input = document.getElementById('chat-input');
    var text = input.value.trim();
    if (!text) return;
    callRust('send_chat', [text]);
    addChatMsg('Me', text);
    input.value = '';
}

function addChatMsg(name, text) {
    var container = document.getElementById('chat-messages');
    var now = new Date();
    var time = now.getHours().toString().padStart(2,'0') + ':' + now.getMinutes().toString().padStart(2,'0');
    var div = document.createElement('div');
    div.className = 'chat-msg';
    div.innerHTML = '<div><span class="name">' + name + '</span><span class="time">' + time + '</span></div><div class="text">' + text.replace(/</g,'&lt;').replace(/>/g,'&gt;') + '</div>';
    container.appendChild(div);
    container.scrollTop = container.scrollHeight;
}

function toggleDropdown(id, btnEl) {
    var dd = document.getElementById(id);
    var wasOpen = dd.classList.contains('open');
    hideDropdowns();
    if (!wasOpen) {
        var rect = btnEl.getBoundingClientRect();
        dd.style.left = Math.max(0, rect.left) + 'px';
        dd.classList.add('open');
    }
}

function hideDropdowns() {
    var dds = document.querySelectorAll('.dropdown');
    for (var i = 0; i < dds.length; i++) dds[i].classList.remove('open');
}

function setViewStyle(style) {
    viewStyle = style;
    var wrap = document.getElementById('canvas-wrap');
    wrap.classList.remove('view-original', 'view-stretch');
    if (style === 'original') wrap.classList.add('view-original');
    else if (style === 'stretch') wrap.classList.add('view-stretch');
    var items = document.querySelectorAll('#display-settings-menu .dd-item[data-style]');
    for (var i = 0; i < items.length; i++) {
        items[i].classList.toggle('selected', items[i].getAttribute('data-style') === style);
    }
    callRust('set_view_style', [style]);
    hideDropdowns();
}

function setImageQuality(quality) {
    var items = document.querySelectorAll('#display-settings-menu .dd-item[data-quality]');
    for (var i = 0; i < items.length; i++) {
        items[i].classList.toggle('selected', items[i].getAttribute('data-quality') === quality);
    }
    callRust('save_image_quality', [quality]);
    hideDropdowns();
}

function switchDisplay(idx) {
    callRust('switch_display', [idx]);
    hideDropdowns();
}

function updateDisplayMenu() {
    var menu = document.getElementById('display-switch-menu');
    menu.innerHTML = '';
    for (var i = 0; i < displays.length; i++) {
        var item = document.createElement('div');
        item.className = 'dd-item' + (i === currentDisplay ? ' selected' : '');
        item.textContent = 'Display ' + (i + 1);
        item.setAttribute('data-idx', i);
        item.onclick = (function(idx) { return function() { switchDisplay(idx); }; })(i);
        menu.appendChild(item);
    }
    document.getElementById('display-selector-btn').style.display = displays.length > 1 ? 'flex' : 'none';
    document.getElementById('display-num').textContent = displays.length > 1 ? (currentDisplay + 1) : '';
}

document.addEventListener('click', function(e) {
    if (!e.target.closest('.dropdown')) {
        var btn = e.target.closest('.tb');
        if (!btn || !btn.getAttribute('onclick') || btn.getAttribute('onclick').indexOf('toggleDropdown') < 0) {
            hideDropdowns();
        }
    }
});

window.onRustResponse = function(method, data) {
    try {
        if (method === 'msgbox') {
            currentMsgType = data.type || '';
            if (data.type === 'input-password' || data.type === 'password') {
                document.getElementById('msg-modal').classList.remove('active');
                document.getElementById('pw-dialog-title').textContent = data.title || 'Enter Password';
                document.getElementById('pw-dialog-text').textContent = data.text || 'Please enter the password.';
                document.getElementById('password-modal').classList.add('active');
                setTimeout(function() { document.getElementById('pw-input').focus(); }, 100);
            } else if (data.type === 'connecting') {
                document.getElementById('status-title').textContent = data.title || 'Connecting...';
                document.getElementById('status-text').textContent = data.text || '';
            } else {
                document.getElementById('password-modal').classList.remove('active');
                document.getElementById('msg-title').textContent = data.title || 'Message';
                document.getElementById('msg-text').textContent = data.text || '';
                document.getElementById('msg-modal').classList.add('active');
            }
        } else if (method === 'set_frame_port') {
            frameServerUrl = 'http://127.0.0.1:' + data + '/frame.jpg';
        } else if (method === 'set_display') {
            canvas.width = data.w;
            canvas.height = data.h;
            canvas.style.display = 'block';
            document.getElementById('status-overlay').style.display = 'none';
        } else if (method === 'new_frame') {
            if (fetchingFrame) return;
            fetchingFrame = true;
            var img = new Image();
            img.onload = function() {
                try {
                    canvas.width = img.naturalWidth;
                    canvas.height = img.naturalHeight;
                    ctx.drawImage(img, 0, 0);
                } catch(e) {}
                fetchingFrame = false;
            };
            img.onerror = function() { fetchingFrame = false; };
            img.src = frameServerUrl + '?' + Date.now();
        } else if (method === 'on_connected') {
            connected = true;
            document.getElementById('password-modal').classList.remove('active');
            document.getElementById('msg-modal').classList.remove('active');
            document.getElementById('status-title').textContent = 'Connected';
            document.getElementById('status-text').textContent = 'Waiting for display...';
            document.getElementById('toolbar-btns').style.display = 'flex';
            document.getElementById('fullscreen-btn').style.display = 'flex';
            document.getElementById('sep1').style.display = 'block';
            document.getElementById('keyboard-btn').classList.add('active');
            callRust('get_view_style');
            callRust('get_image_quality');
        } else if (method === 'set_peer_info') {
            var info = (data.username ? data.username + '@' : '') + (data.hostname || '');
            if (data.platform) { info += ' (' + data.platform + ')'; peerPlatform = data.platform; }
            document.getElementById('peer-info').textContent = info;
            if (data.platform === 'Windows' || data.platform === 'Mac OS' || data.platform === 'Linux') {
                document.getElementById('switch-sides-btn').style.display = 'flex';
                document.getElementById('privacy-mode-btn').style.display = 'flex';
            }
        } else if (method === 'set_connection_type') {
            var icon = document.getElementById('secure-icon');
            var secured = data.secured;
            var direct = data.direct;
            var title = (direct ? 'Direct' : 'Relayed') + ' and ' + (secured ? 'encrypted' : 'unencrypted') + ' connection';
            var color = secured ? '#22C55E' : '#EF4444';
            icon.title = title;
            icon.innerHTML = '<svg viewBox="0 0 24 24" fill="none" stroke="' + color + '" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="11" width="18" height="11" rx="2" ry="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>' + (!direct ? '<circle cx="12" cy="16.5" r="1.5"/>' : '') + '</svg>';
        } else if (method === 'set_displays') {
            displays = data.displays || [];
            currentDisplay = data.current || 0;
            updateDisplayMenu();
        } else if (method === 'new_message') {
            addChatMsg(data.name || 'Remote', data.text || '');
        } else if (method === 'update_quality_status') {
            var qbar = document.getElementById('quality-bar');
            qbar.style.display = 'block';
            var parts = [];
            if (data.fps) parts.push(data.fps + ' FPS');
            if (data.delay) parts.push(data.delay + 'ms');
            if (data.speed) parts.push(data.speed);
            qbar.textContent = parts.join(' | ');
        } else if (method === 'cancel_msgbox') {
            document.getElementById('msg-modal').classList.remove('active');
            document.getElementById('password-modal').classList.remove('active');
        } else if (method === 'set_permission') {
            if (data.name === 'keyboard') {
                keyboardEnabled = data.value;
                var kb = document.getElementById('keyboard-btn');
                if (kb) { data.value ? kb.classList.add('active') : kb.classList.remove('active'); }
            } else if (data.name === 'file') {
                var ft = document.getElementById('file-transfer-btn');
                if (ft) ft.style.display = data.value ? 'flex' : 'none';
            }
        } else if (method === 'set_cursor_data') {
            var wrap = document.getElementById('canvas-wrap');
            if (wrap && data.url) {
                wrap.style.cursor = 'url(' + data.url + ') ' + (data.hotx || 0) + ' ' + (data.hoty || 0) + ', auto';
            }
        } else if (method === 'set_cursor_id') {
            var wrap = document.getElementById('canvas-wrap');
            if (wrap) wrap.style.cursor = data;
        } else if (method === 'update_block_input_state') {
            var btn = document.getElementById('block-input-btn');
            if (btn) btn.style.display = data ? 'flex' : 'none';
        } else if (method === 'screenshot_saved') {
        } else if (method === 'get_view_style') {
            if (data) {
                viewStyle = data;
                var wrap = document.getElementById('canvas-wrap');
                wrap.classList.remove('view-original', 'view-stretch');
                if (data === 'original') wrap.classList.add('view-original');
                else if (data === 'stretch') wrap.classList.add('view-stretch');
                var items = document.querySelectorAll('#display-settings-menu .dd-item[data-style]');
                for (var i = 0; i < items.length; i++) {
                    items[i].classList.toggle('selected', items[i].getAttribute('data-style') === data);
                }
            }
        } else if (method === 'get_image_quality') {
            if (data) {
                var items = document.querySelectorAll('#display-settings-menu .dd-item[data-quality]');
                for (var i = 0; i < items.length; i++) {
                    items[i].classList.toggle('selected', items[i].getAttribute('data-quality') === data);
                }
            }
        } else if (method === 'get_toggle_option') {
            // handled per-caller
        } else if (method === 'get_keyboard_mode') {
            // handled per-caller
        }
    } catch(err) {
        document.title = 'ERR: ' + err.message;
    }
};

document.addEventListener('keydown', function(e) {
    if (!connected || !keyboardEnabled) return;
    if (e.target.id === 'chat-input') return;
    e.preventDefault();
    callRust('key_down', [e.key, e.code, e.keyCode, e.ctrlKey, e.altKey, e.shiftKey, e.metaKey]);
});

document.addEventListener('keyup', function(e) {
    if (!connected || !keyboardEnabled) return;
    if (e.target.id === 'chat-input') return;
    e.preventDefault();
    callRust('key_up', [e.key, e.code, e.keyCode, e.ctrlKey, e.altKey, e.shiftKey, e.metaKey]);
});

canvas.addEventListener('mousemove', function(e) {
    if (!connected) return;
    var rect = canvas.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return;
    var scaleX = canvas.width / rect.width;
    var scaleY = canvas.height / rect.height;
    callRust('mouse_move', [Math.round((e.clientX - rect.left) * scaleX), Math.round((e.clientY - rect.top) * scaleY)]);
});

canvas.addEventListener('mousedown', function(e) {
    if (!connected) return;
    var rect = canvas.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return;
    var scaleX = canvas.width / rect.width;
    var scaleY = canvas.height / rect.height;
    callRust('mouse_down', [e.button, Math.round((e.clientX - rect.left) * scaleX), Math.round((e.clientY - rect.top) * scaleY)]);
});

canvas.addEventListener('mouseup', function(e) {
    if (!connected) return;
    var rect = canvas.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return;
    var scaleX = canvas.width / rect.width;
    var scaleY = canvas.height / rect.height;
    callRust('mouse_up', [e.button, Math.round((e.clientX - rect.left) * scaleX), Math.round((e.clientY - rect.top) * scaleY)]);
});

canvas.addEventListener('wheel', function(e) {
    if (!connected) return;
    e.preventDefault();
    var rect = canvas.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) return;
    var scaleX = canvas.width / rect.width;
    var scaleY = canvas.height / rect.height;
    callRust('mouse_wheel', [e.deltaY > 0 ? 1 : -1, Math.round((e.clientX - rect.left) * scaleX), Math.round((e.clientY - rect.top) * scaleY)]);
});

canvas.addEventListener('contextmenu', function(e) { e.preventDefault(); });
</script>
</body>
</html>"##.to_string()
}

fn get_cm_page_html() -> String {
    r##"<!DOCTYPE html>
<html>
<head>
    <meta charset="UTF-8">
    <title>Connection Manager</title>
    <style>
        :root {
            --cm-bg: #F0F2F5; --cm-card: #fff; --cm-text: #1E293B; --cm-secondary: #64748B;
            --cm-muted: #94A3B8; --cm-border: #E2E8F0; --cm-input-bg: #fff;
            --cm-chat-bg: #fff; --cm-code-bg: #fff; --cm-code-text: #475569;
            --cm-msg-text: #475569; --cm-chat-icon-bg: #E2E8F0; --cm-chat-icon-hover: #CBD5E1;
            --cm-perm-off: #CBD5E1; --cm-invite-bg: #FEF3C7; --cm-invite-text: #92400E;
        }
        html.darktheme {
            --cm-bg: #0F172A; --cm-card: #1E293B; --cm-text: #F1F5F9; --cm-secondary: #94A3B8;
            --cm-muted: #64748B; --cm-border: #334155; --cm-input-bg: #0F172A;
            --cm-chat-bg: #1E293B; --cm-code-bg: #0F172A; --cm-code-text: #94A3B8;
            --cm-msg-text: #CBD5E1; --cm-chat-icon-bg: #334155; --cm-chat-icon-hover: #475569;
            --cm-perm-off: #475569; --cm-invite-bg: #422006; --cm-invite-text: #FDE68A;
        }
        * { margin: 0; padding: 0; box-sizing: border-box; }
        html, body { width: 100%; height: 100%; font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
                     background: var(--cm-bg); color: var(--cm-text); overflow: hidden; }
        .tabs-bar { display: none; background: var(--cm-card); border-bottom: 1px solid var(--cm-border); height: 32px;
                    align-items: center; overflow: hidden; padding: 0 4px; }
        .tabs-bar.show { display: flex; }
        .tab { display: inline-block; height: 24px; line-height: 24px; padding: 0 12px; font-size: 12px;
               cursor: pointer; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; max-width: 80px;
               border-radius: 4px 4px 0 0; margin: 4px 2px 0; color: var(--cm-secondary); }
        .tab.active { background: var(--cm-bg); font-weight: 600; color: var(--cm-text); border: 1px solid var(--cm-border); border-bottom: none; }
        .tab .badge { display: inline-block; width: 15px; height: 15px; line-height: 15px; border-radius: 50%;
                      background: #EF4444; color: #fff; font-size: 10px; text-align: center; margin-left: 4px; }
        .main { display: flex; height: 100%; }
        .main.has-tabs { height: calc(100% - 32px); }
        .left-panel { flex: 1; padding: 16px; overflow-y: auto; background: var(--cm-bg); position: relative;
                     display: flex; flex-direction: column; }
        #conn-view { display: none; flex: 1; flex-direction: column; }
        #conn-view.show { display: flex; }
        .conn-content { flex: 1; }
        .right-panel { display: none; width: 50%; background: var(--cm-card); border-left: 1px solid var(--cm-border);
                       flex-direction: column; }
        .right-panel.open { display: flex; }
        .empty-state { display: flex; flex-direction: column; align-items: center; justify-content: center;
                       height: 100%; color: var(--cm-muted); }
        .empty-state p { font-size: 14px; margin-top: 8px; }
        .icon-and-id { display: flex; gap: 16px; align-items: flex-start; }
        .avatar { width: 80px; height: 80px; border-radius: 8px; display: flex; align-items: center;
                  justify-content: center; color: #fff; font-weight: 700; font-size: 36px; flex-shrink: 0;
                  background-size: cover; background-position: center; }
        .id-block { flex: 1; min-width: 0; }
        .peer-name { font-weight: 600; font-size: 15px; color: var(--cm-text); overflow: hidden; text-overflow: ellipsis; }
        .peer-id { font-size: 13px; color: #2C8CFF; margin-top: 2px; }
        .conn-time-row { margin-top: 6px; font-size: 12px; color: var(--cm-secondary); }
        .sec-toggle { display: inline-block; margin-top: 8px; font-size: 12px; color: #2C8CFF; cursor: pointer;
                      padding: 3px 10px; border: 1px solid #2C8CFF; border-radius: 4px; }
        .sec-toggle:hover, .sec-toggle.active { background: #2C8CFF; color: #fff; }
        .sec-code { display: none; margin-top: 8px; font-family: 'Courier New', monospace; font-size: 12px;
                    line-height: 1.8; color: var(--cm-code-text); background: var(--cm-code-bg); border-radius: 6px; padding: 6px 10px; }
        .sec-code.show { display: block; }
        .section-label { font-size: 12px; color: var(--cm-secondary); margin-top: 14px; margin-bottom: 6px; font-weight: 500; }
        .permissions { display: flex; flex-wrap: wrap; gap: 8px; margin-top: 4px; }
        .perm-icon { width: 44px; height: 44px; border-radius: 6px; display: flex; align-items: center;
                     justify-content: center; cursor: pointer; user-select: none; }
        .perm-icon.on { background: #2C8CFF; }
        .perm-icon.off { background: var(--cm-perm-off); }
        .perm-icon:active { opacity: 0.6; }
        .perm-icon svg { width: 22px; height: 22px; fill: #fff; }
        .buttons { text-align: center; padding: 12px 0 4px; }
        .buttons button { min-width: 80px; height: 38px; margin: 4px; border-radius: 8px; font-size: 14px;
                          font-weight: 500; cursor: pointer; border: none; color: #fff; }
        .btn-accept { background: #2C8CFF; }
        .btn-accept:hover { background: #1A7AE6; }
        .btn-dismiss { background: #2C8CFF; }
        .btn-dismiss:hover { background: #1A7AE6; }
        .btn-disconnect { background: #EF4444; width: 160px !important; font-size: 15px !important; }
        .btn-disconnect:hover { background: #DC2626; }
        .chat-icon { position: absolute; right: 10px; top: 10px; width: 32px; height: 32px;
                     display: flex; align-items: center; justify-content: center; cursor: pointer;
                     border-radius: 4px; background: var(--cm-chat-icon-bg); }
        .chat-icon:hover { background: var(--cm-chat-icon-hover); }
        .chat-icon svg { width: 20px; height: 20px; opacity: 0.6; }
        .chat-icon:hover svg { opacity: 1; }
        .chat-header { padding: 10px 12px; font-size: 13px; font-weight: 600; border-bottom: 1px solid var(--cm-border); }
        .chat-msgs { flex: 1; overflow-y: auto; padding: 8px 12px; }
        .chat-msg { margin-bottom: 8px; font-size: 12px; }
        .chat-msg .name { color: #2C8CFF; font-weight: 600; }
        .chat-msg .text { color: var(--cm-msg-text); margin-top: 2px; }
        .chat-msg .time { color: var(--cm-muted); font-size: 10px; margin-left: 6px; }
        .chat-input-row { display: flex; gap: 6px; padding: 8px 12px; border-top: 1px solid var(--cm-border); }
        .chat-input-row input { flex: 1; padding: 7px 10px; border: 1px solid var(--cm-border); border-radius: 6px;
                                font-size: 12px; outline: none; background: var(--cm-input-bg); color: var(--cm-text); }
        .chat-input-row input:focus { border-color: #2C8CFF; }
        .chat-input-row button { padding: 7px 14px; background: #2C8CFF; color: #fff; border: none;
                                 border-radius: 6px; font-size: 12px; cursor: pointer; }
        .invite-msg { padding: 10px; background: var(--cm-invite-bg); border-radius: 6px; margin-top: 12px;
                      font-size: 13px; color: var(--cm-invite-text); }
        .port-info { margin-top: 12px; font-size: 13px; color: var(--cm-secondary); }
    </style>
</head>
<body>
    <div class="tabs-bar" id="tabs-bar"></div>
    <div class="main" id="main-area">
        <div class="left-panel" id="left-panel">
            <div class="empty-state" id="empty-state">
                <p>Waiting for new connection ...</p>
            </div>
            <div id="conn-view"></div>
        </div>
        <div class="right-panel" id="right-panel">
            <div class="chat-header">Chat</div>
            <div class="chat-msgs" id="chat-msgs"></div>
            <div class="chat-input-row">
                <input id="chat-input" placeholder="Type a message..." onkeydown="if(event.key==='Enter')sendChatMsg()">
                <button onclick="sendChatMsg()">Send</button>
            </div>
        </div>
    </div>
<script>
var connections = {};
var curId = -1;
var showChat = false;
var showSecCode = false;

function callRust(method, args) {
    window.ipc.postMessage(JSON.stringify({ method: method, args: args || [] }));
}

callRust('get_option', ['allow-darktheme']);

function esc(s) { return (s || '').replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;'); }

function string2RGB(str) {
    var hash = 0;
    for (var i = 0; i < str.length; i++) hash = str.charCodeAt(i) + ((hash << 5) - hash);
    var r = (hash >> 16) & 0xFF, g = (hash >> 8) & 0xFF, b = hash & 0xFF;
    r = Math.floor(r * 0.6 + 60); g = Math.floor(g * 0.6 + 60); b = Math.floor(b * 0.6 + 60);
    return 'rgb(' + r + ',' + g + ',' + b + ')';
}

function getElapsed(startTime) {
    var s = Math.floor((Date.now() - startTime) / 1000);
    var h = Math.floor(s / 3600); var m = Math.floor((s % 3600) / 60); s = s % 60;
    var d = Math.floor(h / 24); h = h % 24;
    var out = (h<10?'0':'') + h + ':' + (m<10?'0':'') + m + ':' + (s<10?'0':'') + s;
    if (d > 0) out = d + (d > 1 ? ' days ' : ' day ') + out;
    return out;
}

function getInitial(name) { return (name || '?').charAt(0).toUpperCase(); }

var permSvg = {
    keyboard: '<svg viewBox="0 0 32 32"><rect x="3" y="8" width="26" height="16" rx="2" fill="none" stroke="#fff" stroke-width="2"/><rect x="7" y="12" width="3" height="3" rx="0.5" fill="#fff"/><rect x="12" y="12" width="3" height="3" rx="0.5" fill="#fff"/><rect x="17" y="12" width="3" height="3" rx="0.5" fill="#fff"/><rect x="22" y="12" width="3" height="3" rx="0.5" fill="#fff"/><rect x="9" y="17" width="14" height="3" rx="0.5" fill="#fff"/></svg>',
    clipboard: '<svg viewBox="0 0 32 32"><path d="M20 4h-8a2 2 0 00-2 2H8a2 2 0 00-2 2v18a2 2 0 002 2h16a2 2 0 002-2V8a2 2 0 00-2-2h-2a2 2 0 00-2-2zm-8 2h8v2h-8V6z" fill="#fff"/></svg>',
    audio: '<svg viewBox="0 0 32 32"><path d="M16 4a5 5 0 00-5 5v6a5 5 0 0010 0V9a5 5 0 00-5-5z" fill="#fff"/><path d="M8 15a8 8 0 0016 0" fill="none" stroke="#fff" stroke-width="2"/><line x1="16" y1="25" x2="16" y2="28" stroke="#fff" stroke-width="2"/><line x1="12" y1="28" x2="20" y2="28" stroke="#fff" stroke-width="2"/></svg>',
    file: '<svg viewBox="0 0 32 32"><path d="M6 4h12l8 8v16a2 2 0 01-2 2H6a2 2 0 01-2-2V6a2 2 0 012-2z" fill="#fff"/><path d="M18 4v8h8" fill="none" stroke="#9CA3AF" stroke-width="1"/></svg>',
    restart: '<svg viewBox="0 0 32 32"><path d="M16 4a12 12 0 110 24 12 12 0 010-24z" fill="none" stroke="#fff" stroke-width="2"/><path d="M16 4v8l6-4z" fill="#fff"/></svg>'
};

function renderTabs() {
    var keys = Object.keys(connections);
    var bar = document.getElementById('tabs-bar');
    if (keys.length <= 1) { bar.className = 'tabs-bar'; return; }
    bar.className = 'tabs-bar show';
    var html = '';
    for (var i = 0; i < keys.length; i++) {
        var c = connections[keys[i]];
        var cls = c.id == curId ? 'tab active' : 'tab';
        html += '<div class="' + cls + '" onclick="switchTab(' + c.id + ')">' + esc(c.name || 'NA');
        if (c.unreaded > 0) html += '<span class="badge">' + c.unreaded + '</span>';
        html += '</div>';
    }
    bar.innerHTML = html;
    document.getElementById('main-area').className = 'main has-tabs';
}

function switchTab(id) {
    curId = id;
    if (connections[id]) connections[id].unreaded = 0;
    renderCurrent();
    renderTabs();
    renderChatMessages();
}

function renderCurrent() {
    var keys = Object.keys(connections);
    var empty = document.getElementById('empty-state');
    var view = document.getElementById('conn-view');
    if (keys.length === 0) { empty.style.display = 'flex'; view.classList.remove('show'); return; }
    empty.style.display = 'none'; view.classList.add('show');
    if (curId < 0 || !connections[curId]) curId = parseInt(keys[0]);
    var c = connections[curId];
    var html = '<div class="conn-content">';
    if (!c.is_file_transfer && !c.is_port_forward) {
        html += '<div class="chat-icon" onclick="toggleChat()" title="Chat"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M21 15a2 2 0 01-2 2H7l-4 4V5a2 2 0 012-2h14a2 2 0 012 2z"/></svg></div>';
    }
    html += '<div class="icon-and-id">';
    html += '<div class="avatar" style="background:' + string2RGB(c.name || 'NA') + '">' + getInitial(c.name) + '</div>';
    html += '<div class="id-block">';
    html += '<div class="peer-name">' + esc(c.name || 'NA') + '</div>';
    html += '<div class="peer-id">(' + esc(c.peer_id) + ')</div>';
    html += '<div class="conn-time-row">' + (c.authorized ? 'Connected ' : 'Waiting... ') + '<span id="elapsed">' + (c.authorized ? getElapsed(c.startTime) : '') + '</span></div>';
    if (c.security_numbers && !c.is_file_transfer && !c.is_port_forward) {
        html += '<span class="sec-toggle' + (showSecCode ? ' active' : '') + '" onclick="toggleSecCode()">Security Code</span>';
    }
    html += '</div></div>';
    if (c.security_numbers && showSecCode) {
        html += '<div class="sec-code show">' + esc(c.security_numbers) + '</div>';
    }
    if (c.is_invite) {
        html += '<div class="invite-msg">Accept invite to connect to this computer?</div>';
    }
    if (c.is_port_forward) {
        html += '<div class="port-info">Port Forwarding: ' + esc(c.port_forward || '') + '</div>';
    }
    if (!c.is_file_transfer && !c.is_port_forward && !c.disconnected && c.authorized && !c.is_invite) {
        html += '<div class="section-label">Permissions</div>';
        html += '<div class="permissions">';
        var perms = ['keyboard','clipboard','audio','file','restart'];
        for (var p = 0; p < perms.length; p++) {
            var pn = perms[p];
            var on = c[pn];
            html += '<div class="perm-icon ' + (on ? 'on' : 'off') + '" onclick="togglePerm(' + c.id + ',\'' + pn + '\',' + !on + ')" title="' + pn.charAt(0).toUpperCase() + pn.slice(1) + '">' + (permSvg[pn] || '') + '</div>';
        }
        html += '</div>';
    }
    html += '</div>';
    html += '<div class="buttons">';
    if (c.is_invite) {
        html += '<button class="btn-accept" onclick="callRust(\'cm_accept_invite\',[' + c.id + '])">Accept</button>';
        html += '<button class="btn-dismiss" onclick="callRust(\'cm_decline_invite\',[' + c.id + '])">Dismiss</button>';
    } else if (c.disconnected) {
        html += '<button class="btn-disconnect" onclick="doRemove(' + c.id + ')">Close</button>';
    } else if (!c.authorized) {
        html += '<button class="btn-accept" onclick="doAuthorize(' + c.id + ')">Accept</button>';
        html += '<button class="btn-dismiss" onclick="doClose(' + c.id + ')">Dismiss</button>';
    } else {
        html += '<button class="btn-disconnect" onclick="doClose(' + c.id + ')">Disconnect</button>';
    }
    html += '</div>';
    view.innerHTML = html;
}

function togglePerm(id, name, enable) {
    callRust('cm_switch_permission', [id, name, enable]);
    if (connections[id]) { connections[id][name] = enable; renderCurrent(); }
}

function toggleSecCode() {
    showSecCode = !showSecCode;
    renderCurrent();
}

function toggleChat() {
    showChat = !showChat;
    document.getElementById('right-panel').className = showChat ? 'right-panel open' : 'right-panel';
    if (showChat) renderChatMessages();
}

function sendChatMsg() {
    var input = document.getElementById('chat-input');
    var text = input.value.trim();
    if (!text || curId < 0) return;
    callRust('cm_send_msg', [curId, text]);
    if (!connections[curId].msgs) connections[curId].msgs = [];
    connections[curId].msgs.push({ name: 'Me', text: text, time: getNowStr() });
    input.value = '';
    renderChatMessages();
}

function getNowStr() {
    var d = new Date();
    return (d.getHours()<10?'0':'') + d.getHours() + ':' + (d.getMinutes()<10?'0':'') + d.getMinutes();
}

function renderChatMessages() {
    var el = document.getElementById('chat-msgs');
    if (!el || curId < 0 || !connections[curId]) return;
    var msgs = connections[curId].msgs || [];
    var html = '';
    for (var i = 0; i < msgs.length; i++) {
        var m = msgs[i];
        html += '<div class="chat-msg"><span class="name">' + esc(m.name) + '</span><span class="time">' + m.time + '</span><div class="text">' + esc(m.text) + '</div></div>';
    }
    el.innerHTML = html;
    el.scrollTop = el.scrollHeight;
}

function doAuthorize(id) {
    callRust('cm_authorize', [id]);
    if (connections[id]) {
        connections[id].authorized = true;
        connections[id].startTime = Date.now();
        renderCurrent();
        renderTabs();
    }
}

function doClose(id) {
    callRust('cm_close', [id]);
    delete connections[id];
    var keys = Object.keys(connections);
    if (keys.length === 0) {
        callRust('cm_quit', []);
    } else {
        curId = parseInt(keys[0]);
        renderCurrent();
        renderTabs();
    }
}

function doRemove(id) {
    callRust('cm_remove_disconnected', [id]);
    delete connections[id];
    var keys = Object.keys(connections);
    if (keys.length === 0) {
        callRust('cm_quit', []);
    } else {
        curId = parseInt(keys[0]);
        renderCurrent();
        renderTabs();
    }
}

setInterval(function() {
    var keys = Object.keys(connections);
    for (var i = 0; i < keys.length; i++) {
        var c = connections[keys[i]];
        if (!c.authorized) c.startTime = Date.now();
    }
    if (curId >= 0 && connections[curId] && connections[curId].authorized) {
        var el = document.getElementById('elapsed');
        if (el) el.textContent = getElapsed(connections[curId].startTime);
    }
}, 1000);

window.onRustResponse = function(method, data) {
    try {
        if (method === 'add_connection') {
            if (connections[data.id]) {
                connections[data.id].authorized = data.authorized;
                if (data.authorized && !connections[data.id].startTime) connections[data.id].startTime = Date.now();
            } else {
                data.startTime = Date.now();
                data.msgs = [];
                data.unreaded = 0;
                data.disconnected = false;
                connections[data.id] = data;
                curId = data.id;
            }
            renderCurrent();
            renderTabs();
        } else if (method === 'remove_connection') {
            if (data.close) {
                delete connections[data.id];
            } else {
                if (connections[data.id]) connections[data.id].disconnected = true;
            }
            var keys = Object.keys(connections);
            if (keys.length === 0) {
                callRust('cm_quit', []);
            } else {
                if (!connections[curId]) curId = parseInt(keys[0]);
                renderCurrent();
                renderTabs();
            }
        } else if (method === 'new_message') {
            if (connections[data.id]) {
                if (!connections[data.id].msgs) connections[data.id].msgs = [];
                connections[data.id].msgs.push({ name: connections[data.id].name, text: data.text, time: getNowStr() });
                if (data.id !== curId) {
                    connections[data.id].unreaded = (connections[data.id].unreaded || 0) + 1;
                    renderTabs();
                } else {
                    showChat = true;
                    document.getElementById('right-panel').className = 'right-panel open';
                    renderChatMessages();
                }
            }
        } else if (method === 'show_elevation') {
        } else if (method === 'change_theme') {
            if (data === 'Y' || data === true || data === '"Y"') {
                document.documentElement.classList.add('darktheme');
            } else {
                document.documentElement.classList.remove('darktheme');
            }
        } else if (method === 'get_option') {
            if (data === 'Y') {
                document.documentElement.classList.add('darktheme');
            }
        }
    } catch(err) {
        console.error('CM error:', err);
    }
};
</script>
</body>
</html>"##.to_string()
}

fn get_file_transfer_page_html() -> String {
    r##"<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<title>File Transfer</title>
<style>
:root {
    --ft-bg:#f8f9fa; --ft-card:#fff; --ft-text:#333; --ft-secondary:#555; --ft-muted:#888;
    --ft-border:#dee2e6; --ft-border-light:#eee; --ft-border-row:#f0f0f0;
    --ft-hover:#f0f7ff; --ft-selected:#d4e9ff; --ft-th-bg:#f8f9fa;
    --ft-btn-hover:#f0f0f0; --ft-btn-hover2:#e9ecef; --ft-progress-bg:#e9ecef;
    --ft-modal-bg:#fff; --ft-modal-overlay:rgba(0,0,0,0.4); --ft-input-bg:#fff;
}
html.darktheme {
    --ft-bg:#0F172A; --ft-card:#1E293B; --ft-text:#F1F5F9; --ft-secondary:#94A3B8; --ft-muted:#64748B;
    --ft-border:#334155; --ft-border-light:#334155; --ft-border-row:#334155;
    --ft-hover:#334155; --ft-selected:#1E3A5F; --ft-th-bg:#1E293B;
    --ft-btn-hover:#334155; --ft-btn-hover2:#334155; --ft-progress-bg:#475569;
    --ft-modal-bg:#1E293B; --ft-modal-overlay:rgba(0,0,0,0.6); --ft-input-bg:#0F172A;
}
* { margin:0; padding:0; box-sizing:border-box; }
body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; background:var(--ft-bg); color:var(--ft-text); height:100vh; display:flex; flex-direction:column; overflow:hidden; }
.header { display:flex; align-items:center; background:var(--ft-card); height:40px; padding:0 12px; gap:8px; border-bottom:1px solid var(--ft-border); flex-shrink:0; }
.header .info { font-size:13px; color:var(--ft-secondary); flex:1; }
.header .hbtn { background:none; border:1px solid var(--ft-border); color:var(--ft-secondary); cursor:pointer; padding:4px 12px; border-radius:4px; font-size:12px; }
.header .hbtn:hover { background:var(--ft-btn-hover); color:var(--ft-text); }
.header .danger { color:#dc3545; border-color:#dc3545; }
.header .danger:hover { background:#dc3545; color:#fff; }
.panels { display:flex; flex:1; overflow:hidden; gap:6px; padding:6px; }
.panel { flex:1; display:flex; flex-direction:column; background:var(--ft-card); border-radius:8px; border:1px solid var(--ft-border); overflow:hidden; }
.panel-title { display:flex; align-items:center; padding:8px 12px; gap:8px; border-bottom:1px solid var(--ft-border-light); }
.panel-title svg { width:18px; height:18px; flex-shrink:0; }
.panel-title .label { font-size:13px; font-weight:600; color:var(--ft-text); }
.panel-title .plat { font-size:11px; color:var(--ft-muted); margin-left:2px; }
.navbar { display:flex; align-items:center; padding:4px 8px; gap:4px; border-bottom:1px solid var(--ft-border-light); }
.navbar .nbtn { background:none; border:none; cursor:pointer; padding:4px 6px; border-radius:4px; color:var(--ft-muted); display:flex; align-items:center; }
.navbar .nbtn:hover { background:var(--ft-btn-hover2); color:var(--ft-text); }
.navbar .nbtn svg { width:16px; height:16px; }
.navbar .path-input { flex:1; border:1px solid var(--ft-border); border-radius:4px; padding:3px 8px; font-size:12px; font-family:monospace; color:var(--ft-text); outline:none; background:var(--ft-input-bg); }
.navbar .path-input:focus { border-color:#2C8CFF; }
.opbar { display:flex; align-items:center; padding:4px 8px; gap:4px; border-bottom:1px solid var(--ft-border-light); }
.opbar .obtn { background:none; border:none; cursor:pointer; padding:4px 6px; border-radius:4px; color:var(--ft-muted); display:flex; align-items:center; gap:3px; font-size:11px; }
.opbar .obtn:hover { background:var(--ft-btn-hover2); color:var(--ft-text); }
.opbar .obtn svg { width:14px; height:14px; }
.opbar .spacer { flex:1; }
.opbar .send-btn { border:2px solid #2C8CFF; color:#2C8CFF; background:none; cursor:pointer; padding:3px 14px; border-radius:4px; font-size:12px; font-weight:600; display:flex; align-items:center; gap:4px; }
.opbar .send-btn:hover { background:#2C8CFF; color:#fff; }
.opbar .send-btn:disabled { opacity:0.3; cursor:default; }
.opbar .send-btn:disabled:hover { background:none; color:#2C8CFF; }
.file-list { flex:1; overflow-y:auto; font-size:12px; }
.file-list table { width:100%; border-collapse:collapse; }
.file-list th { background:var(--ft-th-bg); position:sticky; top:0; text-align:left; padding:5px 8px; font-weight:600; color:var(--ft-secondary); font-size:11px; border-bottom:1px solid var(--ft-border); z-index:1; }
.file-list td { padding:4px 8px; border-bottom:1px solid var(--ft-border-row); cursor:pointer; white-space:nowrap; overflow:hidden; text-overflow:ellipsis; max-width:220px; }
.file-list tr:hover { background:var(--ft-hover); }
.file-list tr.selected { background:var(--ft-selected); }
.file-list .icon { width:22px; text-align:center; }
.file-list .icon svg { width:16px; height:16px; vertical-align:middle; }
.file-list .size { text-align:right; color:var(--ft-muted); }
.file-list .time { color:var(--ft-muted); }
.file-list .fname { color:var(--ft-text); }
.file-list .fname.is-dir { font-weight:500; }
.jobs-panel { max-height:140px; overflow-y:auto; border-top:1px solid var(--ft-border); background:var(--ft-card); flex-shrink:0; }
.jobs-panel .job { display:flex; align-items:center; padding:5px 12px; gap:8px; font-size:12px; border-bottom:1px solid var(--ft-border-row); }
.jobs-panel .job .arrow { color:#2C8CFF; font-size:14px; }
.jobs-panel .job .name { flex:1; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; color:var(--ft-text); }
.jobs-panel .job .progress { width:140px; height:6px; background:var(--ft-progress-bg); border-radius:3px; overflow:hidden; }
.jobs-panel .job .progress-fill { height:100%; background:#2C8CFF; border-radius:3px; transition:width 0.3s; }
.jobs-panel .job .speed { color:#2C8CFF; min-width:70px; text-align:right; font-size:11px; }
.jobs-panel .job .cancel { background:none; border:none; color:#dc3545; cursor:pointer; font-size:14px; padding:2px 4px; border-radius:3px; }
.jobs-panel .job .cancel:hover { background:#fff0f0; }
.status-bar { display:flex; align-items:center; background:var(--ft-card); padding:4px 12px; font-size:11px; color:var(--ft-muted); border-top:1px solid var(--ft-border); flex-shrink:0; }
.status-bar span { margin-right:16px; }
.modal-overlay { display:none; position:fixed; top:0; left:0; right:0; bottom:0; background:var(--ft-modal-overlay); z-index:1000; align-items:center; justify-content:center; }
.modal-overlay.active { display:flex; }
.modal { background:var(--ft-modal-bg); border-radius:8px; padding:20px; min-width:320px; max-width:420px; box-shadow:0 4px 20px rgba(0,0,0,0.15); }
.modal h3 { margin-bottom:12px; font-size:15px; color:var(--ft-text); }
.modal p { margin-bottom:12px; font-size:13px; color:var(--ft-secondary); }
.modal .btns { display:flex; gap:8px; justify-content:flex-end; }
.modal .btns button { padding:6px 16px; border:none; border-radius:4px; cursor:pointer; font-size:12px; }
.modal .btn-primary { background:#2C8CFF; color:#fff; }
.modal .btn-primary:hover { background:#1a7ae6; }
.modal .btn-secondary { background:var(--ft-btn-hover2); color:var(--ft-text); }
.modal .btn-secondary:hover { background:var(--ft-border); }
</style>
</head>
<body>
<div class="header">
    <span class="info" id="peer-info">Connecting...</span>
    <span id="status-text" style="font-size:11px;color:#888"></span>
    <button class="hbtn danger" onclick="callRust('close')">Disconnect</button>
</div>
<div class="panels">
    <div class="panel" id="local-panel">
        <div class="panel-title">
            <svg viewBox="0 0 24 24" fill="none" stroke="#555" stroke-width="1.5"><rect x="2" y="3" width="20" height="14" rx="2"/><line x1="8" y1="21" x2="16" y2="21" stroke-linecap="round"/><line x1="12" y1="17" x2="12" y2="21"/></svg>
            <span class="label">Local Computer</span>
        </div>
        <div class="navbar">
            <button class="nbtn" onclick="goHome(false)" title="Home"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M3 12l9-8 9 8"/><path d="M5 10v10h5v-6h4v6h5V10"/></svg></button>
            <button class="nbtn" onclick="goUp(false)" title="Up"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 19V5m0 0l-7 7m7-7l7 7"/></svg></button>
            <input class="path-input" id="local-path" onkeydown="if(event.key==='Enter')navigateTo(this.value,false)">
            <button class="nbtn" onclick="refreshDir(false)" title="Refresh"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M1 4v6h6"/><path d="M23 20v-6h-6"/><path d="M20.49 9A9 9 0 005.64 5.64L1 10m22 4l-4.64 4.36A9 9 0 013.51 15"/></svg></button>
        </div>
        <div class="opbar">
            <button class="obtn" onclick="createDir(false)" title="New Folder"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M22 19a2 2 0 01-2 2H4a2 2 0 01-2-2V5a2 2 0 012-2h5l2 3h9a2 2 0 012 2z"/><line x1="12" y1="11" x2="12" y2="17"/><line x1="9" y1="14" x2="15" y2="14"/></svg></button>
            <button class="obtn" onclick="deleteSelected()" title="Delete"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 01-2 2H7a2 2 0 01-2-2V6m3 0V4a2 2 0 012-2h4a2 2 0 012 2v2"/></svg></button>
            <span class="spacer"></span>
            <button class="send-btn" id="send-btn" onclick="sendFiles()" disabled>Send &#x25B6;</button>
        </div>
        <div class="file-list" id="local-files">
            <table><thead><tr><th class="icon"></th><th>Name</th><th class="size">Size</th><th class="time">Modified</th></tr></thead>
            <tbody id="local-tbody"></tbody></table>
        </div>
    </div>
    <div class="panel" id="remote-panel">
        <div class="panel-title">
            <svg viewBox="0 0 24 24" fill="none" stroke="#555" stroke-width="1.5"><rect x="2" y="3" width="20" height="14" rx="2"/><line x1="8" y1="21" x2="16" y2="21" stroke-linecap="round"/><line x1="12" y1="17" x2="12" y2="21"/></svg>
            <span class="label">Remote Computer</span>
            <span class="plat" id="remote-plat"></span>
        </div>
        <div class="navbar">
            <button class="nbtn" onclick="goHome(true)" title="Home"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M3 12l9-8 9 8"/><path d="M5 10v10h5v-6h4v6h5V10"/></svg></button>
            <button class="nbtn" onclick="goUp(true)" title="Up"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M12 19V5m0 0l-7 7m7-7l7 7"/></svg></button>
            <input class="path-input" id="remote-path" onkeydown="if(event.key==='Enter')navigateTo(this.value,true)">
            <button class="nbtn" onclick="refreshDir(true)" title="Refresh"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M1 4v6h6"/><path d="M23 20v-6h-6"/><path d="M20.49 9A9 9 0 005.64 5.64L1 10m22 4l-4.64 4.36A9 9 0 013.51 15"/></svg></button>
        </div>
        <div class="opbar">
            <button class="obtn" onclick="createDir(true)" title="New Folder"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M22 19a2 2 0 01-2 2H4a2 2 0 01-2-2V5a2 2 0 012-2h5l2 3h9a2 2 0 012 2z"/><line x1="12" y1="11" x2="12" y2="17"/><line x1="9" y1="14" x2="15" y2="14"/></svg></button>
            <button class="obtn" onclick="deleteSelected()" title="Delete"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 01-2 2H7a2 2 0 01-2-2V6m3 0V4a2 2 0 012-2h4a2 2 0 012 2v2"/></svg></button>
            <span class="spacer"></span>
            <button class="send-btn" id="recv-btn" onclick="receiveFiles()" disabled>&#x25C0; Receive</button>
        </div>
        <div class="file-list" id="remote-files">
            <table><thead><tr><th class="icon"></th><th>Name</th><th class="size">Size</th><th class="time">Modified</th></tr></thead>
            <tbody id="remote-tbody"></tbody></table>
        </div>
    </div>
</div>
<div class="jobs-panel" id="jobs-panel"></div>
<div class="status-bar"><span id="total-info">Ready</span></div>

<div class="modal-overlay" id="override-modal">
    <div class="modal">
        <h3>File Already Exists</h3>
        <p id="override-msg">Overwrite?</p>
        <label style="font-size:12px;display:block;margin-bottom:12px"><input type="checkbox" id="override-remember"> Remember for remaining files</label>
        <div class="btns">
            <button class="btn-secondary" onclick="overrideConfirm(false)">Skip</button>
            <button class="btn-primary" onclick="overrideConfirm(true)">Overwrite</button>
        </div>
    </div>
</div>
<div class="modal-overlay" id="delete-modal">
    <div class="modal">
        <h3>Confirm Delete</h3>
        <p id="delete-msg"></p>
        <div class="btns">
            <button class="btn-secondary" onclick="deleteConfirm(false)">Cancel</button>
            <button class="btn-primary" onclick="deleteConfirm(true)">Delete</button>
        </div>
    </div>
</div>
<div class="modal-overlay" id="password-modal">
    <div class="modal">
        <h3 id="pw-dialog-title">Enter Password</h3>
        <p id="pw-dialog-text">Please enter the password for the remote device.</p>
        <input type="password" id="pw-input" placeholder="Password" style="width:100%;padding:8px;margin:8px 0;border:1px solid #dee2e6;border-radius:4px;font-size:13px" onkeydown="if(event.key==='Enter')submitPassword()">
        <div class="btns">
            <button class="btn-secondary" onclick="callRust('close')">Cancel</button>
            <button class="btn-primary" onclick="submitPassword()">OK</button>
        </div>
    </div>
</div>
<div class="modal-overlay" id="msg-modal">
    <div class="modal">
        <h3 id="msg-title">Message</h3>
        <p id="msg-text"></p>
        <div class="btns">
            <button class="btn-primary" onclick="document.getElementById('msg-modal').classList.remove('active')">OK</button>
        </div>
    </div>
</div>

<script>
var localPath = '';
var remotePath = '';
var localFiles = [];
var remoteFiles = [];
var localSelected = {};
var remoteSelected = {};
var jobs = {};
var nextJobId = 1;
var pendingOverride = null;
var pendingDelete = null;
var homeDir = '';
var connected = false;

function callRust(method, args) {
    window.ipc.postMessage(JSON.stringify({method: method, args: args || []}));
}

function submitPassword() {
    var pw = document.getElementById('pw-input').value;
    if (pw) {
        callRust('login', [pw, '', '', false]);
        document.getElementById('password-modal').classList.remove('active');
        document.getElementById('pw-input').value = '';
        document.getElementById('status-text').textContent = 'Authenticating...';
    }
}

function formatSize(bytes) {
    if (bytes === 0) return '';
    if (bytes < 1024) return bytes + ' B';
    if (bytes < 1048576) return (bytes/1024).toFixed(1) + ' KB';
    if (bytes < 1073741824) return (bytes/1048576).toFixed(1) + ' MB';
    return (bytes/1073741824).toFixed(2) + ' GB';
}

function formatTime(ts) {
    if (!ts) return '';
    var d = new Date(ts * 1000);
    return d.toLocaleDateString() + ' ' + d.toLocaleTimeString([], {hour:'2-digit',minute:'2-digit'});
}

var svgFolder = '<svg viewBox="0 0 24 24" fill="#F0C36D" stroke="#D4A843" stroke-width="1"><path d="M22 19a2 2 0 01-2 2H4a2 2 0 01-2-2V5a2 2 0 012-2h5l2 3h9a2 2 0 012 2z"/></svg>';
var svgFile = '<svg viewBox="0 0 24 24" fill="none" stroke="#999" stroke-width="1.5"><path d="M14 2H6a2 2 0 00-2 2v16a2 2 0 002 2h12a2 2 0 002-2V8z"/><polyline points="14 2 14 8 20 8"/></svg>';

function renderFileList(tbodyId, files, selected, isRemote) {
    var tbody = document.getElementById(tbodyId);
    var html = '';
    for (var i = 0; i < files.length; i++) {
        var f = files[i];
        var isDir = f.type === 1 || f.type === 2 || f.type === 3;
        var sel = selected[i] ? ' selected' : '';
        html += '<tr class="' + sel + '" onclick="selectFile(' + i + ',' + isRemote + ',event)" ondblclick="openEntry(' + i + ',' + isRemote + ')">';
        html += '<td class="icon">' + (isDir ? svgFolder : svgFile) + '</td>';
        html += '<td class="fname' + (isDir ? ' is-dir' : '') + '">' + escapeHtml(f.name) + '</td>';
        html += '<td class="size">' + (isDir ? '' : formatSize(f.size)) + '</td>';
        html += '<td class="time">' + formatTime(f.time) + '</td>';
        html += '</tr>';
    }
    tbody.innerHTML = html;
}

function escapeHtml(s) {
    return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
}

function selectFile(idx, isRemote, e) {
    var sel = isRemote ? remoteSelected : localSelected;
    if (e && e.ctrlKey) {
        sel[idx] = !sel[idx];
    } else if (e && e.shiftKey) {
    } else {
        if (isRemote) remoteSelected = {}; else localSelected = {};
        sel = isRemote ? remoteSelected : localSelected;
        sel[idx] = true;
    }
    var tbodyId = isRemote ? 'remote-tbody' : 'local-tbody';
    var files = isRemote ? remoteFiles : localFiles;
    renderFileList(tbodyId, files, sel, isRemote);
    updateButtons();
}

function openEntry(idx, isRemote) {
    var files = isRemote ? remoteFiles : localFiles;
    var f = files[idx];
    if (!f) return;
    var isDir = f.type === 1 || f.type === 2 || f.type === 3;
    if (isDir) {
        var curPath = isRemote ? remotePath : localPath;
        var sep = curPath.indexOf('/') >= 0 || curPath === '' ? '/' : '\\';
        var newPath = curPath;
        if (newPath && !newPath.endsWith(sep)) newPath += sep;
        newPath += f.name;
        navigateTo(newPath, isRemote);
    }
}

function navigateTo(path, isRemote) {
    if (isRemote) {
        remotePath = path;
        document.getElementById('remote-path').value = path;
        callRust('ft_read_remote_dir', [path, true]);
    } else {
        localPath = path;
        document.getElementById('local-path').value = path;
        callRust('ft_read_local_dir', [path, true]);
    }
}

function goUp(isRemote) {
    var p = isRemote ? remotePath : localPath;
    if (!p) return;
    var sep = p.indexOf('/') >= 0 ? '/' : '\\';
    var parts = p.split(sep);
    parts.pop();
    var newPath = parts.join(sep);
    if (!newPath && sep === '/') newPath = '/';
    navigateTo(newPath, isRemote);
}

function goHome(isRemote) {
    if (isRemote) {
        callRust('ft_read_remote_dir', ['', true]);
    } else {
        navigateTo(homeDir || '/', false);
    }
}

function refreshDir(isRemote) {
    navigateTo(isRemote ? remotePath : localPath, isRemote);
}

function getSelectedFiles(isRemote) {
    var sel = isRemote ? remoteSelected : localSelected;
    var files = isRemote ? remoteFiles : localFiles;
    var result = [];
    for (var k in sel) {
        if (sel[k] && files[k]) result.push(files[k]);
    }
    return result;
}

function updateButtons() {
    var localSel = getSelectedFiles(false);
    var remoteSel = getSelectedFiles(true);
    document.getElementById('send-btn').disabled = localSel.length === 0;
    document.getElementById('recv-btn').disabled = remoteSel.length === 0;
}

function sendFiles() {
    var sel = getSelectedFiles(false);
    if (!sel.length) return;
    for (var i = 0; i < sel.length; i++) {
        var f = sel[i];
        var jobId = nextJobId++;
        var localSep = localPath.indexOf('/') >= 0 ? '/' : '\\';
        var remoteSep = remotePath.indexOf('\\') >= 0 ? '\\' : '/';
        var srcPath = localPath + (localPath.endsWith(localSep) ? '' : localSep) + f.name;
        var dstPath = remotePath + (remotePath.endsWith(remoteSep) ? '' : remoteSep) + f.name;
        callRust('ft_send_files', [jobId, 0, srcPath, dstPath, 0, true, false]);
        addJobUI(jobId, f.name, 'upload');
    }
}

function receiveFiles() {
    var sel = getSelectedFiles(true);
    if (!sel.length) return;
    for (var i = 0; i < sel.length; i++) {
        var f = sel[i];
        var jobId = nextJobId++;
        var remoteSep = remotePath.indexOf('\\') >= 0 ? '\\' : '/';
        var localSep = localPath.indexOf('/') >= 0 ? '/' : '\\';
        var srcPath = remotePath + (remotePath.endsWith(remoteSep) ? '' : remoteSep) + f.name;
        var dstPath = localPath + (localPath.endsWith(localSep) ? '' : localSep) + f.name;
        callRust('ft_send_files', [jobId, 0, srcPath, dstPath, 0, true, true]);
        addJobUI(jobId, f.name, 'download');
    }
}

function createDir(isRemote) {
    var name = prompt('Folder name:');
    if (!name) return;
    var path = isRemote ? remotePath : localPath;
    var sep = path.indexOf('/') >= 0 ? '/' : '\\';
    var fullPath = path + (path.endsWith(sep) ? '' : sep) + name;
    callRust('ft_create_dir', [0, fullPath, isRemote]);
    setTimeout(function() { refreshDir(isRemote); }, 500);
}

function deleteSelected() {
    var localSel = getSelectedFiles(false);
    var remoteSel = getSelectedFiles(true);
    var files = localSel.length > 0 ? localSel : remoteSel;
    var isRemote = localSel.length === 0;
    if (!files.length) return;
    pendingDelete = { files: files, isRemote: isRemote };
    document.getElementById('delete-msg').textContent = 'Delete ' + files.length + ' item(s)? "' + files[0].name + '"' + (files.length > 1 ? '...' : '');
    document.getElementById('delete-modal').classList.add('active');
}

function deleteConfirm(yes) {
    document.getElementById('delete-modal').classList.remove('active');
    if (yes && pendingDelete) {
        var path = pendingDelete.isRemote ? remotePath : localPath;
        var sep = path.indexOf('/') >= 0 ? '/' : '\\';
        for (var i = 0; i < pendingDelete.files.length; i++) {
            var f = pendingDelete.files[i];
            var fullPath = path + (path.endsWith(sep) ? '' : sep) + f.name;
            var isDir = f.type === 1 || f.type === 2 || f.type === 3;
            if (isDir) {
                callRust('ft_remove_dir_all', [0, fullPath, pendingDelete.isRemote, true]);
            } else {
                callRust('ft_remove_file', [0, fullPath, 0, pendingDelete.isRemote]);
            }
        }
        setTimeout(function() { refreshDir(pendingDelete.isRemote); }, 500);
    }
    pendingDelete = null;
}

function overrideConfirm(yes) {
    document.getElementById('override-modal').classList.remove('active');
    if (pendingOverride) {
        var remember = document.getElementById('override-remember').checked;
        callRust('ft_set_confirm_override', [pendingOverride.id, pendingOverride.file_num, yes, remember, pendingOverride.is_upload]);
    }
    pendingOverride = null;
}

function addJobUI(id, name, direction) {
    jobs[id] = { name: name, direction: direction, progress: 0, speed: '' };
    renderJobs();
}

function renderJobs() {
    var panel = document.getElementById('jobs-panel');
    var html = '';
    for (var id in jobs) {
        var j = jobs[id];
        var arrow = j.direction === 'upload' ? '<span class="arrow">&#x25B6;</span>' : '<span class="arrow">&#x25C0;</span>';
        html += '<div class="job" id="job-' + id + '">';
        html += arrow;
        html += '<span class="name">' + escapeHtml(j.name) + '</span>';
        if (j.done) {
            html += '<div class="progress"><div class="progress-fill" style="width:100%;background:#22c55e"></div></div>';
        } else {
            html += '<div class="progress"><div class="progress-fill" style="width:' + j.progress + '%"></div></div>';
        }
        html += '<span class="speed">' + (j.speed || '') + '</span>';
        html += '<button class="cancel" onclick="dismissJob(' + id + ')" title="' + (j.done ? 'Dismiss' : 'Cancel') + '">&#x2715;</button>';
        html += '</div>';
    }
    panel.innerHTML = html;
}

function dismissJob(id) {
    if (jobs[id] && jobs[id].done) {
        delete jobs[id];
        renderJobs();
    } else {
        cancelJob(id);
    }
}

function cancelJob(id) {
    callRust('ft_cancel_job', [id]);
    delete jobs[id];
    renderJobs();
}

window.onRustResponse = function(method, data) {
    if (method === 'on_connected') {
        connected = true;
        document.getElementById('password-modal').classList.remove('active');
        document.getElementById('msg-modal').classList.remove('active');
        document.getElementById('status-text').textContent = 'Connected';
        callRust('ft_get_home_dir');
    } else if (method === 'set_home_dir') {
        homeDir = data.path || '';
        localPath = homeDir;
        document.getElementById('local-path').value = localPath;
        callRust('ft_read_local_dir', [localPath, true]);
        callRust('ft_read_remote_dir', ['', true]);
    } else if (method === 'set_peer_info') {
        var info = (data.username ? data.username + '@' : '') + (data.hostname || '');
        if (data.platform) info += ' (' + data.platform + ')';
        document.getElementById('peer-info').textContent = 'File Transfer - ' + info;
        if (data.platform) document.getElementById('remote-plat').textContent = '(' + data.platform + ')';
    } else if (method === 'update_folder_files') {
        if (data.id && data.id > 0) return;
        var entries = data.entries || [];
        var path = data.path || '';
        if (data.is_local) {
            localFiles = entries;
            localPath = path;
            localSelected = {};
            document.getElementById('local-path').value = path;
            renderFileList('local-tbody', localFiles, localSelected, false);
        } else {
            remoteFiles = entries;
            remotePath = path;
            remoteSelected = {};
            document.getElementById('remote-path').value = path;
            renderFileList('remote-tbody', remoteFiles, remoteSelected, true);
        }
        updateButtons();
        document.getElementById('total-info').textContent = entries.length + ' items';
    } else if (method === 'job_progress') {
        if (jobs[data.id]) {
            jobs[data.id].speed = data.speed || '';
            jobs[data.id].finished_size = data.finished_size || 0;
            if (data.progress) {
                jobs[data.id].progress = Math.min(100, data.progress);
            } else if (data.finished_size > 0) {
                jobs[data.id].progress = Math.min(90, (jobs[data.id].progress || 0) + 5);
            }
            renderJobs();
        }
    } else if (method === 'job_done') {
        if (jobs[data.id]) {
            jobs[data.id].progress = 100;
            jobs[data.id].speed = 'Done \u2713';
            jobs[data.id].done = true;
            renderJobs();
            document.getElementById('total-info').textContent = jobs[data.id].name + ' - Transfer complete';
            setTimeout(function() { refreshDir(false); refreshDir(true); }, 1000);
        }
    } else if (method === 'job_error') {
        if (jobs[data.id]) {
            jobs[data.id].speed = 'Error';
            renderJobs();
        }
        document.getElementById('status-text').textContent = 'Error: ' + (data.err || 'unknown');
    } else if (method === 'override_file_confirm') {
        pendingOverride = data;
        document.getElementById('override-msg').textContent = 'File "' + (data.to || '') + '" already exists. Overwrite?';
        document.getElementById('override-modal').classList.add('active');
    } else if (method === 'confirm_delete_files') {
        pendingDelete = { id: data.id, file_num: data.file_num };
        document.getElementById('delete-msg').textContent = 'Delete ' + data.cnt + ' item(s)? "' + (data.name || '') + '"';
        document.getElementById('delete-modal').classList.add('active');
    } else if (method === 'msgbox') {
        if (data.type === 'input-password' || data.type === 'password') {
            document.getElementById('msg-modal').classList.remove('active');
            document.getElementById('pw-dialog-title').textContent = data.title || 'Enter Password';
            document.getElementById('pw-dialog-text').textContent = data.text || 'Please enter the password.';
            document.getElementById('password-modal').classList.add('active');
            setTimeout(function() { document.getElementById('pw-input').focus(); }, 100);
        } else if (data.type === 'connecting') {
            document.getElementById('status-text').textContent = data.text || 'Connecting...';
        } else {
            document.getElementById('password-modal').classList.remove('active');
            document.getElementById('msg-title').textContent = data.title || 'Message';
            document.getElementById('msg-text').textContent = data.text || '';
            document.getElementById('msg-modal').classList.add('active');
        }
    } else if (method === 'cancel_msgbox') {
        document.getElementById('password-modal').classList.remove('active');
        document.getElementById('msg-modal').classList.remove('active');
    } else if (method === 'get_option') {
        if (data === 'Y') document.documentElement.classList.add('darktheme');
    }
};
callRust('get_option', ['allow-darktheme']);
</script>
</body>
</html>"##.to_string()
}

fn get_port_forward_page_html() -> String {
    r##"<!DOCTYPE html>
<html>
<head>
<meta charset="UTF-8">
<title>Port Forward</title>
<style>
:root {
    --pf-bg:#f0f4f8; --pf-header:#fff; --pf-header-border:#E2E8F0; --pf-text:#1E293B;
    --pf-title:#2C8CFF; --pf-info:#64748B; --pf-sub:#64748B; --pf-duration:#94A3B8;
    --pf-tunnel-bg:#F1F5F9; --pf-tunnel-border:#E2E8F0; --pf-port:#2C8CFF; --pf-arrow:#94A3B8; --pf-remote:#64748B;
    --pf-tip:#94A3B8; --pf-tip-border:#E2E8F0;
    --pf-modal-bg:#fff; --pf-modal-text:#333; --pf-modal-sub:#666; --pf-modal-border:#E2E8F0;
    --pf-input-bg:#fff; --pf-input-border:#E2E8F0; --pf-input-text:#333;
    --pf-overlay:rgba(0,0,0,0.4); --pf-btn-sec-text:#64748B; --pf-btn-sec-border:#E2E8F0;
}
html.darktheme {
    --pf-bg:#1a1a2e; --pf-header:#16213e; --pf-header-border:#0f3460; --pf-text:#e0e0e0;
    --pf-title:#4da8da; --pf-info:#a0b4d0; --pf-sub:#a0b4d0; --pf-duration:#64748B;
    --pf-tunnel-bg:#16213e; --pf-tunnel-border:#0f3460; --pf-port:#4da8da; --pf-arrow:#64748B; --pf-remote:#a0b4d0;
    --pf-tip:#64748B; --pf-tip-border:#0f3460;
    --pf-modal-bg:#1e1e2e; --pf-modal-text:#eee; --pf-modal-sub:#aaa; --pf-modal-border:#444;
    --pf-input-bg:#2a2a3a; --pf-input-border:#555; --pf-input-text:#eee;
    --pf-overlay:rgba(0,0,0,0.7); --pf-btn-sec-text:#aaa; --pf-btn-sec-border:#555;
}
* { margin:0; padding:0; box-sizing:border-box; }
body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; background:var(--pf-bg); color:var(--pf-text); height:100vh; display:flex; flex-direction:column; }
.header { background:var(--pf-header); padding:12px 20px; border-bottom:1px solid var(--pf-header-border); display:flex; align-items:center; justify-content:space-between; }
.header-title { font-size:15px; font-weight:600; color:var(--pf-title); }
.header-info { font-size:12px; color:var(--pf-info); }
.content { flex:1; display:flex; flex-direction:column; align-items:center; justify-content:center; padding:20px; }
.status-icon { width:64px; height:64px; margin-bottom:16px; }
.status-icon.initializing { color:#f0ad4e; }
.status-icon.waiting { color:#4da8da; }
.status-icon.connected { color:#22c55e; }
.status-icon.error { color:#ef4444; }
.status-text { font-size:18px; font-weight:600; margin-bottom:8px; text-align:center; }
.status-sub { font-size:13px; color:var(--pf-sub); text-align:center; max-width:360px; }
.duration { font-size:12px; color:var(--pf-duration); margin-top:8px; font-family:'Courier New',monospace; }
.tunnel-info { margin-top:20px; background:var(--pf-tunnel-bg); border:1px solid var(--pf-tunnel-border); border-radius:8px; padding:16px; width:100%; max-width:420px; }
.tunnel-row { display:flex; align-items:center; justify-content:center; gap:12px; padding:6px 0; font-size:13px; }
.tunnel-port { font-family:'Courier New',monospace; font-weight:600; color:var(--pf-port); font-size:14px; }
.tunnel-arrow { color:var(--pf-arrow); }
.tunnel-remote { font-family:'Courier New',monospace; color:var(--pf-remote); }
.tip { margin-top:12px; font-size:11px; color:var(--pf-tip); text-align:center; padding:8px; border-top:1px solid var(--pf-tip-border); }
.modal-overlay { display:none; position:fixed; top:0; left:0; width:100%; height:100%; background:var(--pf-overlay); z-index:100; align-items:center; justify-content:center; }
.modal-overlay.active { display:flex; }
.modal { background:var(--pf-modal-bg); border-radius:12px; padding:24px; min-width:340px; color:var(--pf-modal-text); border:1px solid var(--pf-modal-border); }
.modal h2 { font-size:16px; margin-bottom:12px; }
.modal p { font-size:13px; color:var(--pf-modal-sub); margin-bottom:12px; }
.modal input { width:100%; padding:8px 12px; border:1px solid var(--pf-input-border); border-radius:6px; font-size:13px; background:var(--pf-input-bg); color:var(--pf-input-text); outline:none; margin-bottom:8px; }
.modal-buttons { text-align:right; margin-top:12px; }
.modal-btn { padding:8px 20px; border-radius:6px; font-size:13px; cursor:pointer; border:none; margin-left:8px; }
.modal-btn.primary { background:#2C8CFF; color:white; }
.modal-btn.secondary { background:transparent; color:var(--pf-btn-sec-text); border:1px solid var(--pf-btn-sec-border); }
</style>
</head>
<body>
<div class="header">
    <span class="header-title" id="header-title">Port Forward</span>
    <span class="header-info" id="peer-info">Connecting...</span>
</div>
<div class="content">
    <svg class="status-icon initializing" id="status-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5">
        <circle cx="12" cy="12" r="10"/><path d="M12 6v6l4 2"/>
    </svg>
    <div class="status-text" id="status-text">Initializing...</div>
    <div class="status-sub" id="status-sub">Setting up port forwarding tunnel</div>
    <div class="duration" id="duration" style="display:none">00:00</div>
    <div class="tunnel-info" id="tunnel-info" style="display:none"></div>
    <div class="tip" id="tip" style="display:none">Do not close this window while the tunnel is active</div>
</div>
<div class="modal-overlay" id="pw-modal">
    <div class="modal">
        <h2 id="pw-title">Enter Password</h2>
        <p id="pw-text">Please enter the password.</p>
        <input type="password" id="pw-input" placeholder="Password" onkeydown="if(event.key==='Enter')submitPassword()">
        <div class="modal-buttons">
            <button class="modal-btn secondary" onclick="callRust('close_by_id','')">Cancel</button>
            <button class="modal-btn primary" onclick="submitPassword()">OK</button>
        </div>
    </div>
</div>
<div class="modal-overlay" id="msg-modal">
    <div class="modal">
        <h2 id="msg-title">Message</h2>
        <p id="msg-text"></p>
        <div class="modal-buttons">
            <button class="modal-btn primary" onclick="document.getElementById('msg-modal').classList.remove('active')">OK</button>
        </div>
    </div>
</div>
<script>
var connected = false;
var startTime = null;
var durationTimer = null;

function callRust(method, args) {
    window.ipc.postMessage(JSON.stringify({ method: method, args: Array.isArray(args) ? args : [args] }));
}

function submitPassword() {
    var pw = document.getElementById('pw-input').value;
    callRust('input_password', [pw]);
    document.getElementById('pw-modal').classList.remove('active');
}

function setState(state, msg) {
    var icon = document.getElementById('status-icon');
    var text = document.getElementById('status-text');
    var sub = document.getElementById('status-sub');
    icon.className = 'status-icon ' + state;
    if (state === 'initializing') {
        icon.innerHTML = '<circle cx="12" cy="12" r="10"/><path d="M12 6v6l4 2"/>';
        text.textContent = 'Initializing...';
        sub.textContent = msg || 'Setting up port forwarding tunnel';
    } else if (state === 'waiting') {
        icon.innerHTML = '<path d="M22 12h-4l-3 9L9 3l-3 9H2"/>';
        text.textContent = 'Listening...';
        sub.textContent = msg || 'Tunnel is active, waiting for connections';
        document.getElementById('tip').style.display = 'block';
    } else if (state === 'connected') {
        icon.innerHTML = '<path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/><polyline points="22 4 12 14.01 9 11.01"/>';
        text.textContent = 'Connected';
        sub.textContent = msg || 'Port forwarding tunnel is active';
        document.getElementById('tip').style.display = 'block';
        if (!startTime) {
            startTime = Date.now();
            document.getElementById('duration').style.display = 'block';
            durationTimer = setInterval(updateDuration, 1000);
        }
    } else if (state === 'error') {
        icon.innerHTML = '<circle cx="12" cy="12" r="10"/><line x1="15" y1="9" x2="9" y2="15"/><line x1="9" y1="9" x2="15" y2="15"/>';
        text.textContent = 'Error';
        sub.textContent = msg || 'Connection failed';
    }
}

function updateDuration() {
    if (!startTime) return;
    var secs = Math.floor((Date.now() - startTime) / 1000);
    var h = Math.floor(secs / 3600);
    var m = Math.floor((secs % 3600) / 60);
    var s = secs % 60;
    var str = h > 0 ? h + ':' + pad(m) + ':' + pad(s) : pad(m) + ':' + pad(s);
    document.getElementById('duration').textContent = str;
}

function pad(n) { return n < 10 ? '0' + n : '' + n; }

function showTunnel(localPort, remoteHost, remotePort) {
    var info = document.getElementById('tunnel-info');
    info.style.display = 'block';
    info.innerHTML = '<div class="tunnel-row">' +
        '<span class="tunnel-port">localhost:' + localPort + '</span>' +
        '<span class="tunnel-arrow">&rarr;</span>' +
        '<span class="tunnel-remote">' + (remoteHost || 'localhost') + ':' + remotePort + '</span>' +
        '</div>';
}

window.onRustResponse = function(method, data) {
    try {
        if (method === 'msgbox') {
            if (data.type === 'input-password' || data.type === 'password') {
                document.getElementById('pw-title').textContent = data.title || 'Enter Password';
                document.getElementById('pw-text').textContent = data.text || '';
                document.getElementById('pw-modal').classList.add('active');
                setTimeout(function() { document.getElementById('pw-input').focus(); }, 100);
            } else if (data.type === 'connecting') {
                setState('initializing', data.text || 'Connecting...');
            } else {
                document.getElementById('msg-title').textContent = data.title || 'Message';
                document.getElementById('msg-text').textContent = data.text || '';
                document.getElementById('msg-modal').classList.add('active');
            }
        } else if (method === 'on_connected') {
            connected = true;
            setState('waiting', 'Tunnel established, waiting for connections');
        } else if (method === 'set_peer_info') {
            var info = (data.username ? data.username + '@' : '') + (data.hostname || '');
            if (data.platform) info += ' (' + data.platform + ')';
            document.getElementById('peer-info').textContent = info;
        } else if (method === 'cancel_msgbox') {
            document.getElementById('pw-modal').classList.remove('active');
            document.getElementById('msg-modal').classList.remove('active');
        } else if (method === 'set_port_forward') {
            showTunnel(data.local_port, data.remote_host, data.remote_port);
            setState('connected', 'Port forwarding active');
        } else if (method === 'get_option') {
            if (data === 'Y') document.documentElement.classList.add('darktheme');
        }
    } catch(err) {
        document.title = 'ERR: ' + err.message;
    }
};
callRust('get_option', ['allow-darktheme']);
</script>
</body>
</html>"##.to_string()
}

fn handle_ipc_message(message: &str) {
    let mut ui = UI {};
    match serde_json::from_str::<serde_json::Value>(message) {
        Ok(msg) => {
            let method = msg["method"].as_str().unwrap_or("");
            let args = msg["args"].as_array();
            log::info!("[IPC] method={}, args={:?}", method, args);

            fn arg_s(args: Option<&Vec<serde_json::Value>>, i: usize) -> String {
                args.and_then(|a| a.get(i)).and_then(|v| v.as_str()).unwrap_or("").to_string()
            }
            fn arg_b(args: Option<&Vec<serde_json::Value>>, i: usize) -> bool {
                args.and_then(|a| a.get(i)).and_then(|v| v.as_bool()).unwrap_or(false)
            }
            fn arg_i(args: Option<&Vec<serde_json::Value>>, i: usize) -> i32 {
                args.and_then(|a| a.get(i)).and_then(|v| v.as_i64()).unwrap_or(0) as i32
            }

            match method {
                "get_id" => {
                    let id = ui.get_id();
                    send_to_webview("get_id", &format!("\"{}\"", id));
                }
                "get_connect_status" => {
                    let json = ui.get_connect_status_json();
                    send_to_webview("get_connect_status", &format!("'{}'", json));
                }
                "temporary_password" => {
                    let pw = ui.temporary_password();
                    send_to_webview("temporary_password", &format!("\"{}\"", pw));
                }
                "permanent_password" => {
                    let pw = ui.permanent_password();
                    send_to_webview("permanent_password", &format!("\"{}\"", pw));
                }
                "get_recent_sessions" => {
                    let json = ui.get_recent_sessions_json();
                    send_to_webview("get_recent_sessions", &format!("'{}'", json));
                }
                "get_option" => {
                    let val = ui.get_option(arg_s(args, 0));
                    send_to_webview("get_option", &format!("\"{}\"", val));
                }
                "get_local_option" => {
                    let key = arg_s(args, 0);
                    let val = ui.get_local_option(key.clone());
                    let escaped_key = key.replace('\\', "\\\\").replace('"', "\\\"");
                    let escaped_val = val.replace('\\', "\\\\").replace('"', "\\\"");
                    send_to_webview("get_local_option", &format!("{{\"key\":\"{}\",\"value\":\"{}\"}}", escaped_key, escaped_val));
                }
                "get_peer_option" => {
                    let val = ui.get_peer_option(arg_s(args, 0), arg_s(args, 1));
                    send_to_webview("get_peer_option", &format!("\"{}\"", val));
                }
                "get_options_json" => {
                    let json = ui.get_options_json();
                    send_to_webview("get_options_json", &format!("'{}'", json));
                }
                "get_peer_json" => {
                    let json = ui.get_peer_json(arg_s(args, 0));
                    send_to_webview("get_peer_json", &format!("'{}'", json));
                }
                "get_fav_json" => {
                    let json = ui.get_fav_json();
                    send_to_webview("get_fav_json", &format!("'{}'", json));
                }
                "get_lan_peers" => {
                    let json = ui.get_lan_peers();
                    send_to_webview("get_lan_peers", &format!("'{}'", json));
                }
                "get_icon" => {
                    let icon = ui.get_icon();
                    send_to_webview("get_icon", &format!("\"{}\"", icon));
                }
                "get_version" => {
                    let v = ui.get_version();
                    send_to_webview("get_version", &format!("\"{}\"", v));
                }
                "get_fingerprint" => {
                    let fp = ui.get_fingerprint();
                    send_to_webview("get_fingerprint", &format!("\"{}\"", fp));
                }
                "get_app_name" => {
                    let n = ui.get_app_name();
                    send_to_webview("get_app_name", &format!("\"{}\"", n));
                }
                "is_installed" => {
                    let v = ui.is_installed();
                    send_to_webview("is_installed", &format!("{}", v));
                }
                "get_sound_inputs" => {
                    let json = ui.get_sound_inputs_json();
                    send_to_webview("get_sound_inputs", &format!("'{}'", json));
                }
                "get_uuid" => {
                    let u = ui.get_uuid();
                    send_to_webview("get_uuid", &format!("\"{}\"", u));
                }
                "get_langs" => {
                    let l = ui.get_langs();
                    send_to_webview("get_langs", &format!("'{}'", l));
                }
                "translate" => {
                    let key = arg_s(args, 0);
                    let result = crate::lang::translate(key);
                    let escaped = result.replace('\\', "\\\\").replace('"', "\\\"");
                    send_to_webview("translate_result", &format!("\"{}\"", escaped));
                }
                "translate_batch" => {
                    let keys_str = arg_s(args, 0);
                    if let Ok(keys) = serde_json::from_str::<Vec<String>>(&keys_str) {
                        let mut map = serde_json::Map::new();
                        for key in keys {
                            let tr = crate::lang::translate(key.clone());
                            map.insert(key, serde_json::Value::String(tr));
                        }
                        let json = serde_json::Value::Object(map).to_string();
                        send_to_webview("translate_batch_result", &format!("'{}'", json.replace('\'', "\\'")));
                    }
                }
                "get_remote_id" => {
                    let id = ui.get_remote_id();
                    send_to_webview("get_remote_id", &format!("\"{}\"", id));
                }
                "get_size_json" => {
                    let s = ui.get_size_json();
                    send_to_webview("get_size_json", &format!("'{}'", s));
                }
                "get_async_job_status" => {
                    let s = ui.get_async_job_status();
                    send_to_webview("get_async_job_status", &format!("\"{}\"", s));
                }
                "get_login_device_info" => {
                    let s = ui.get_login_device_info();
                    send_to_webview("get_login_device_info", &format!("'{}'", s));
                }
                "get_teamid" => {
                    let t = ui.get_teamid();
                    send_to_webview("get_teamid", &format!("\"{}\"", t));
                }

                "new_remote" => {
                    ui.new_remote(arg_s(args, 0), arg_s(args, 1), arg_b(args, 2));
                }
                "set_option" => {
                    ui.set_option(arg_s(args, 0), arg_s(args, 1));
                }
                "set_local_option" => {
                    ui.set_local_option(arg_s(args, 0), arg_s(args, 1));
                }
                "set_peer_option" => {
                    ui.set_peer_option(arg_s(args, 0), arg_s(args, 1), arg_s(args, 2));
                }
                "set_options_from_json" => {
                    ui.set_options_from_json(arg_s(args, 0));
                }
                "set_remote_id" => {
                    ui.set_remote_id(arg_s(args, 0));
                }
                "set_permanent_password" => {
                    ui.set_permanent_password(arg_s(args, 0));
                }
                "permanent_password_for_dialog" => {
                    let pw = ui.permanent_password();
                    send_to_webview("permanent_password_for_dialog", &format!("\"{}\"", pw));
                }
                "update_temporary_password" => {
                    ui.update_temporary_password();
                    let pw = ui.temporary_password();
                    send_to_webview("temporary_password", &format!("\"{}\"", pw));
                }
                "goto_install" => {
                    ui.goto_install();
                }
                "copy_text" => {
                    ui.copy_text(arg_s(args, 0));
                }
                "open_url" => {
                    ui.open_url(arg_s(args, 0));
                }
                "has_valid_2fa" => {
                    let v = ui.has_valid_2fa();
                    send_to_webview("has_valid_2fa", if v { "\"true\"" } else { "\"false\"" });
                }
                "generate2fa" => {
                    let secret = ui.generate2fa();
                    send_to_webview("generate2fa", &format!("\"{}\"", secret));
                }
                "generate_2fa_img_src" => {
                    let src = ui.generate_2fa_img_src(arg_s(args, 0));
                    send_to_webview("generate_2fa_img_src", &format!("\"{}\"", src));
                }
                "verify2fa" => {
                    let ok = ui.verify2fa(arg_s(args, 0));
                    send_to_webview("verify2fa", if ok { "\"true\"" } else { "\"false\"" });
                }
                "discover" => {
                    ui.discover();
                }
                "remove_peer" => {
                    ui.remove_peer(arg_s(args, 0));
                }
                "forget_password" => {
                    ui.forget_password(arg_s(args, 0));
                }
                "get_socks_json" => {
                    let json = ui.get_socks_json();
                    send_to_webview("get_socks_json", &format!("'{}'", json));
                }
                "set_socks" => {
                    ui.set_socks(arg_s(args, 0), arg_s(args, 1), arg_s(args, 2));
                }
                "get_custom_api_url" => {
                    let url = ui.get_custom_api_url();
                    send_to_webview("get_custom_api_url", &format!("\"{}\"", url));
                }
                "set_custom_api_url" => {
                    ui.set_custom_api_url(arg_s(args, 0));
                }
                "store_fav_from_json" => {
                    ui.store_fav_from_json(arg_s(args, 0));
                }
                "set_version_sync" => {
                    ui.set_version_sync();
                }
                "check_hwcodec" => {
                    ui.check_hwcodec();
                }
                "post_request" => {
                    ui.post_request(arg_s(args, 0), arg_s(args, 1), arg_s(args, 2));
                }
                "get_request" => {
                    ui.get_request(arg_s(args, 0), arg_s(args, 1));
                }
                "wol" => {
                    ui.send_wol(arg_s(args, 0));
                }
                "login" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let password = arg_s(args, 0);
                        let os_username = arg_s(args, 1);
                        let os_password = arg_s(args, 2);
                        let remember = arg_b(args, 3);
                        session.login(os_username, os_password, password, remember);
                    }
                }
                "close" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.close();
                    }
                    if let Some(ref proxy) = *EVENT_LOOP_PROXY.lock().unwrap() {
                        proxy.send_event("cmd:close".to_string()).ok();
                    }
                }
                "ctrl_alt_del" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.ctrl_alt_del();
                    }
                }
                "key_down" | "key_up" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let key_char = arg_s(args, 0);
                        let code = arg_s(args, 1);
                        let key_code = arg_i(args, 2);
                        let ctrl = arg_b(args, 3);
                        let alt = arg_b(args, 4);
                        let shift = arg_b(args, 5);
                        let meta = arg_b(args, 6);
                        let down = method == "key_down";

                        use hbb_common::message_proto::{KeyEvent, ControlKey};

                        let mut key_event = KeyEvent::new();

                        let mapped = match code.as_str() {
                            "KeyA" => { key_event.set_chr('a' as _); true },
                            "KeyB" => { key_event.set_chr('b' as _); true },
                            "KeyC" => { key_event.set_chr('c' as _); true },
                            "KeyD" => { key_event.set_chr('d' as _); true },
                            "KeyE" => { key_event.set_chr('e' as _); true },
                            "KeyF" => { key_event.set_chr('f' as _); true },
                            "KeyG" => { key_event.set_chr('g' as _); true },
                            "KeyH" => { key_event.set_chr('h' as _); true },
                            "KeyI" => { key_event.set_chr('i' as _); true },
                            "KeyJ" => { key_event.set_chr('j' as _); true },
                            "KeyK" => { key_event.set_chr('k' as _); true },
                            "KeyL" => { key_event.set_chr('l' as _); true },
                            "KeyM" => { key_event.set_chr('m' as _); true },
                            "KeyN" => { key_event.set_chr('n' as _); true },
                            "KeyO" => { key_event.set_chr('o' as _); true },
                            "KeyP" => { key_event.set_chr('p' as _); true },
                            "KeyQ" => { key_event.set_chr('q' as _); true },
                            "KeyR" => { key_event.set_chr('r' as _); true },
                            "KeyS" => { key_event.set_chr('s' as _); true },
                            "KeyT" => { key_event.set_chr('t' as _); true },
                            "KeyU" => { key_event.set_chr('u' as _); true },
                            "KeyV" => { key_event.set_chr('v' as _); true },
                            "KeyW" => { key_event.set_chr('w' as _); true },
                            "KeyX" => { key_event.set_chr('x' as _); true },
                            "KeyY" => { key_event.set_chr('y' as _); true },
                            "KeyZ" => { key_event.set_chr('z' as _); true },
                            "Digit0" => { key_event.set_chr('0' as _); true },
                            "Digit1" => { key_event.set_chr('1' as _); true },
                            "Digit2" => { key_event.set_chr('2' as _); true },
                            "Digit3" => { key_event.set_chr('3' as _); true },
                            "Digit4" => { key_event.set_chr('4' as _); true },
                            "Digit5" => { key_event.set_chr('5' as _); true },
                            "Digit6" => { key_event.set_chr('6' as _); true },
                            "Digit7" => { key_event.set_chr('7' as _); true },
                            "Digit8" => { key_event.set_chr('8' as _); true },
                            "Digit9" => { key_event.set_chr('9' as _); true },
                            "Comma" => { key_event.set_chr(',' as _); true },
                            "Period" => { key_event.set_chr('.' as _); true },
                            "Slash" => { key_event.set_chr('/' as _); true },
                            "Semicolon" => { key_event.set_chr(';' as _); true },
                            "Quote" => { key_event.set_chr('\'' as _); true },
                            "BracketLeft" => { key_event.set_chr('[' as _); true },
                            "BracketRight" => { key_event.set_chr(']' as _); true },
                            "Backslash" => { key_event.set_chr('\\' as _); true },
                            "Minus" => { key_event.set_chr('-' as _); true },
                            "Equal" => { key_event.set_chr('=' as _); true },
                            "Backquote" => { key_event.set_chr('`' as _); true },
                            "F1" => { key_event.set_control_key(ControlKey::F1); true },
                            "F2" => { key_event.set_control_key(ControlKey::F2); true },
                            "F3" => { key_event.set_control_key(ControlKey::F3); true },
                            "F4" => { key_event.set_control_key(ControlKey::F4); true },
                            "F5" => { key_event.set_control_key(ControlKey::F5); true },
                            "F6" => { key_event.set_control_key(ControlKey::F6); true },
                            "F7" => { key_event.set_control_key(ControlKey::F7); true },
                            "F8" => { key_event.set_control_key(ControlKey::F8); true },
                            "F9" => { key_event.set_control_key(ControlKey::F9); true },
                            "F10" => { key_event.set_control_key(ControlKey::F10); true },
                            "F11" => { key_event.set_control_key(ControlKey::F11); true },
                            "F12" => { key_event.set_control_key(ControlKey::F12); true },
                            "Enter" => { key_event.set_control_key(ControlKey::Return); true },
                            "Backspace" => { key_event.set_control_key(ControlKey::Backspace); true },
                            "Tab" => { key_event.set_control_key(ControlKey::Tab); true },
                            "Space" => { key_event.set_control_key(ControlKey::Space); true },
                            "Escape" => { key_event.set_control_key(ControlKey::Escape); true },
                            "Delete" => { key_event.set_control_key(ControlKey::Delete); true },
                            "Insert" => { key_event.set_control_key(ControlKey::Insert); true },
                            "Home" => { key_event.set_control_key(ControlKey::Home); true },
                            "End" => { key_event.set_control_key(ControlKey::End); true },
                            "PageUp" => { key_event.set_control_key(ControlKey::PageUp); true },
                            "PageDown" => { key_event.set_control_key(ControlKey::PageDown); true },
                            "ArrowUp" => { key_event.set_control_key(ControlKey::UpArrow); true },
                            "ArrowDown" => { key_event.set_control_key(ControlKey::DownArrow); true },
                            "ArrowLeft" => { key_event.set_control_key(ControlKey::LeftArrow); true },
                            "ArrowRight" => { key_event.set_control_key(ControlKey::RightArrow); true },
                            "CapsLock" => { key_event.set_control_key(ControlKey::CapsLock); true },
                            "NumLock" => { key_event.set_control_key(ControlKey::NumLock); true },
                            "ScrollLock" => { key_event.set_control_key(ControlKey::Scroll); true },
                            "PrintScreen" => { key_event.set_control_key(ControlKey::Snapshot); true },
                            "Pause" => { key_event.set_control_key(ControlKey::Pause); true },
                            "ContextMenu" => { key_event.set_control_key(ControlKey::Apps); true },
                            "ShiftLeft" => { key_event.set_control_key(ControlKey::Shift); true },
                            "ShiftRight" => { key_event.set_control_key(ControlKey::RShift); true },
                            "ControlLeft" => { key_event.set_control_key(ControlKey::Control); true },
                            "ControlRight" => { key_event.set_control_key(ControlKey::RControl); true },
                            "AltLeft" => { key_event.set_control_key(ControlKey::Alt); true },
                            "AltRight" => { key_event.set_control_key(ControlKey::RAlt); true },
                            "MetaLeft" => { key_event.set_control_key(ControlKey::Meta); true },
                            "MetaRight" => { key_event.set_control_key(ControlKey::RWin); true },
                            "Numpad0" => { key_event.set_control_key(ControlKey::Numpad0); true },
                            "Numpad1" => { key_event.set_control_key(ControlKey::Numpad1); true },
                            "Numpad2" => { key_event.set_control_key(ControlKey::Numpad2); true },
                            "Numpad3" => { key_event.set_control_key(ControlKey::Numpad3); true },
                            "Numpad4" => { key_event.set_control_key(ControlKey::Numpad4); true },
                            "Numpad5" => { key_event.set_control_key(ControlKey::Numpad5); true },
                            "Numpad6" => { key_event.set_control_key(ControlKey::Numpad6); true },
                            "Numpad7" => { key_event.set_control_key(ControlKey::Numpad7); true },
                            "Numpad8" => { key_event.set_control_key(ControlKey::Numpad8); true },
                            "Numpad9" => { key_event.set_control_key(ControlKey::Numpad9); true },
                            "NumpadEnter" => { key_event.set_control_key(ControlKey::NumpadEnter); true },
                            "NumpadAdd" => { key_event.set_control_key(ControlKey::Add); true },
                            "NumpadSubtract" => { key_event.set_control_key(ControlKey::Subtract); true },
                            "NumpadMultiply" => { key_event.set_control_key(ControlKey::Multiply); true },
                            "NumpadDivide" => { key_event.set_control_key(ControlKey::Divide); true },
                            "NumpadDecimal" => { key_event.set_control_key(ControlKey::Decimal); true },
                            _ => {
                                log::debug!("Unmapped key code: {} (key: {})", code, key_char);
                                false
                            }
                        };

                        if mapped {
                            if ctrl { key_event.modifiers.push(ControlKey::Control.into()); }
                            if alt { key_event.modifiers.push(ControlKey::Alt.into()); }
                            if shift { key_event.modifiers.push(ControlKey::Shift.into()); }
                            if meta { key_event.modifiers.push(ControlKey::Meta.into()); }

                            if down {
                                key_event.down = true;
                            }
                            session.send_key_event(&key_event);
                        }
                    }
                }
                "mouse_move" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let x = arg_i(args, 0);
                        let y = arg_i(args, 1);
                        session.send_mouse(3, x, y, false, false, false, false);
                    }
                }
                "mouse_down" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let button = arg_i(args, 0);
                        let x = arg_i(args, 1);
                        let y = arg_i(args, 2);
                        let mask = match button {
                            0 => (1 << 3) | 1,
                            1 => (4 << 3) | 1,
                            2 => (2 << 3) | 1,
                            _ => return,
                        };
                        session.send_mouse(mask, x, y, false, false, false, false);
                    }
                }
                "mouse_up" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let button = arg_i(args, 0);
                        let x = arg_i(args, 1);
                        let y = arg_i(args, 2);
                        let mask = match button {
                            0 => (1 << 3) | 2,
                            1 => (4 << 3) | 2,
                            2 => (2 << 3) | 2,
                            _ => return,
                        };
                        session.send_mouse(mask, x, y, false, false, false, false);
                    }
                }
                "mouse_wheel" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let delta = arg_i(args, 0);
                        let x = arg_i(args, 1);
                        let y = arg_i(args, 2);
                        let mask = if delta > 0 {
                            (120 << 3) | 3
                        } else {
                            ((-120i32 as u32 as i32) << 3) | 3
                        };
                        session.send_mouse(mask, x, y, false, false, false, false);
                    }
                }
                "fullscreen" => {
                    if let Some(ref proxy) = *EVENT_LOOP_PROXY.lock().unwrap() {
                        proxy.send_event("cmd:fullscreen".to_string()).ok();
                    }
                }
                "switch_display" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.switch_display(arg_i(args, 0));
                    }
                }
                "send_chat" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.send_chat(arg_s(args, 0));
                    }
                }
                "restart_remote_device" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.restart_remote_device();
                    }
                }
                "lock_screen" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.lock_screen();
                    }
                }
                "set_view_style" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.save_view_style(arg_s(args, 0));
                    }
                }
                "set_image_quality" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.save_image_quality(arg_s(args, 0));
                    }
                }
                "screenshot" => {
                    use crate::ui::remote::CURRENT_FRAME;
                    if let Some(ref frame) = *CURRENT_FRAME.lock().unwrap() {
                        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                        let dir = format!("{}/Pictures", home);
                        let _ = std::fs::create_dir_all(&dir);
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let path = format!("{}/HopToDesk_Screenshot_{}.jpg", dir, ts);
                        if std::fs::write(&path, frame).is_ok() {
                            log::info!("[remote-wry] Screenshot saved to {}", path);
                            send_to_webview("screenshot_saved", &format!("\"{}\"", path));
                        }
                    }
                }
                "record_screen" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.record_screen(arg_b(args, 0));
                    }
                }
                "toggle_option" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.toggle_option(arg_s(args, 0));
                    }
                }
                "get_toggle_option" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let name = arg_s(args, 0);
                        let val = session.get_toggle_option(name.clone());
                        send_to_webview("get_toggle_option", &format!("{{\"name\":\"{}\",\"value\":{}}}", name, val));
                    }
                }
                "get_keyboard_mode" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let mode = session.get_keyboard_mode();
                        send_to_webview("get_keyboard_mode", &format!("\"{}\"", mode));
                    }
                }
                "save_keyboard_mode" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.save_keyboard_mode(arg_s(args, 0));
                    }
                }
                "get_view_style" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let style = session.get_view_style();
                        send_to_webview("get_view_style", &format!("\"{}\"", style));
                    }
                }
                "get_image_quality" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let q = session.get_image_quality();
                        send_to_webview("get_image_quality", &format!("\"{}\"", q));
                    }
                }
                "save_image_quality" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.save_image_quality(arg_s(args, 0));
                    }
                }
                "get_custom_image_quality" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let q = session.get_custom_image_quality();
                        let json = serde_json::to_string(&q).unwrap_or_else(|_| "[]".to_string());
                        send_to_webview("get_custom_image_quality", &json);
                    }
                }
                "save_custom_image_quality" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.save_custom_image_quality(arg_i(args, 0));
                    }
                }
                "input_os_password" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.input_os_password(arg_s(args, 0), arg_b(args, 1));
                    }
                }
                "elevate_direct" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.elevate_direct();
                    }
                }
                "can_elevate" => {
                    let val = crate::ui_cm_interface::can_elevate();
                    send_to_webview("can_elevate", &format!("{}", val));
                }
                "elevate_portable" => {
                    crate::ui_cm_interface::elevate_portable(arg_i(args, 0));
                }
                "switch_sides" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.switch_sides();
                    }
                }
                "refresh" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.refresh_video(0);
                    }
                }
                "toggle_privacy_mode" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let on = arg_b(args, 0);
                        session.toggle_privacy_mode("".to_string(), on);
                    }
                }
                "open_file_transfer" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let id = session.id.clone();
                        let password = session.password.clone();
                        std::thread::spawn(move || {
                            if let Ok(exe) = std::env::current_exe() {
                                let mut cmd = std::process::Command::new(exe);
                                cmd.arg("--file-transfer").arg(&id);
                                if !password.is_empty() {
                                    cmd.arg("--password").arg(&password);
                                }
                                if let Err(e) = cmd.spawn() {
                                    log::error!("Failed to spawn file transfer: {}", e);
                                }
                            }
                        });
                    }
                }
                "ft_get_home_dir" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let home = session.get_home_dir();
                        send_to_webview("set_home_dir", &format!("{{\"path\":\"{}\"}}", home.replace('\\', "\\\\").replace('"', "\\\"")));
                    }
                }
                "ft_read_local_dir" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let path = arg_s(args, 0);
                        let include_hidden = arg_b(args, 1);
                        let result_json = session.read_dir(path.clone(), include_hidden);
                        if result_json != "null" {
                            if let Ok(fd) = serde_json::from_str::<serde_json::Value>(&result_json) {
                                let mut data = fd.clone();
                                data["is_local"] = serde_json::json!(true);
                                data["path"] = serde_json::json!(path);
                                send_to_webview("update_folder_files", &data.to_string());
                            }
                        }
                    }
                }
                "ft_read_remote_dir" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let path = arg_s(args, 0);
                        let include_hidden = arg_b(args, 1);
                        session.read_remote_dir(path, include_hidden);
                    }
                }
                "ft_send_files" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let id = arg_i(args, 0);
                        let file_type = arg_i(args, 1);
                        let path = arg_s(args, 2);
                        let to = arg_s(args, 3);
                        let file_num = arg_i(args, 4);
                        let include_hidden = arg_b(args, 5);
                        let is_remote = arg_b(args, 6);
                        session.send_files(id, file_type, path, to, file_num, include_hidden, is_remote);
                    }
                }
                "ft_cancel_job" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        session.cancel_job(arg_i(args, 0));
                    }
                }
                "ft_create_dir" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let id = arg_i(args, 0);
                        let path = arg_s(args, 1);
                        let is_remote = arg_b(args, 2);
                        session.create_dir(id, path, is_remote);
                    }
                }
                "ft_remove_file" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let id = arg_i(args, 0);
                        let path = arg_s(args, 1);
                        let file_num = arg_i(args, 2);
                        let is_remote = arg_b(args, 3);
                        session.remove_file(id, path, file_num, is_remote);
                    }
                }
                "ft_remove_dir_all" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let id = arg_i(args, 0);
                        let path = arg_s(args, 1);
                        let is_remote = arg_b(args, 2);
                        let include_hidden = arg_b(args, 3);
                        session.remove_dir_all(id, path, is_remote, include_hidden);
                    }
                }
                "ft_set_confirm_override" => {
                    if let Some(ref session) = *CUR_SESSION.lock().unwrap() {
                        let id = arg_i(args, 0);
                        let file_num = arg_i(args, 1);
                        let need_override = arg_b(args, 2);
                        let remember = arg_b(args, 3);
                        let is_upload = arg_b(args, 4);
                        session.set_confirm_override_file(id, file_num, need_override, remember, is_upload);
                    }
                }
                "cm_authorize" => {
                    if let Some(ref cm) = *CM_INSTANCE.lock().unwrap() {
                        cm.authorize(arg_i(args, 0));
                    }
                }
                "cm_close" => {
                    if let Some(ref cm) = *CM_INSTANCE.lock().unwrap() {
                        cm.close(arg_i(args, 0));
                    }
                }
                "cm_switch_permission" => {
                    if let Some(ref cm) = *CM_INSTANCE.lock().unwrap() {
                        cm.switch_permission(arg_i(args, 0), arg_s(args, 1), arg_b(args, 2));
                    }
                }
                "cm_send_msg" => {
                    if let Some(ref cm) = *CM_INSTANCE.lock().unwrap() {
                        cm.send_msg(arg_i(args, 0), arg_s(args, 1));
                    }
                }
                "cm_accept_invite" => {
                    if let Some(ref mut cm) = *CM_INSTANCE.lock().unwrap() {
                        cm.accept_invite(arg_i(args, 0));
                    }
                }
                "cm_decline_invite" => {
                    if let Some(ref mut cm) = *CM_INSTANCE.lock().unwrap() {
                        cm.decline_invite(arg_i(args, 0));
                    }
                }
                "cm_remove_disconnected" => {
                    if let Some(ref cm) = *CM_INSTANCE.lock().unwrap() {
                        cm.remove_disconnected_connection(arg_i(args, 0));
                    }
                }
                "cm_quit" => {
                    if let Some(ref cm) = *CM_INSTANCE.lock().unwrap() {
                        cm.quit();
                    }
                }
                _ => {
                    log::warn!("[IPC] Unknown method: {}", method);
                }
            }
        }
        Err(e) => {
            log::error!("[IPC] Failed to parse message: {} - {}", message, e);
        }
    }
}

fn resolve_hostname(hostname: &str) -> Option<String> {
    let mut addrs = (hostname, 0).to_socket_addrs().ok()?;
    let ip = addrs.next()?.ip().to_string();
    Some(ip)
}

fn is_numeric_id(peer_id: &str) -> bool {
    let len = peer_id.len();
    len >= 10 && len <= 12 && peer_id.chars().all(|c| c.is_ascii_digit())
}

impl UI {
    fn recent_sessions_updated(&self) -> bool {
        recent_sessions_updated()
    }

    fn get_id(&self) -> String {
        ipc::get_id()
    }

    fn temporary_password(&mut self) -> String {
        temporary_password()
    }

    fn update_temporary_password(&self) {
        update_temporary_password()
    }

    fn permanent_password(&self) -> String {
        permanent_password()
    }

    fn set_permanent_password(&self, password: String) {
        set_permanent_password(password);
    }

    fn get_remote_id(&mut self) -> String {
        get_remote_id()
    }

    fn set_remote_id(&mut self, id: String) {
        set_remote_id(id);
    }

    fn goto_install(&mut self) {
        goto_install();
    }

    fn install_me(&mut self, _options: String, _path: String) {
        install_me(_options, _path, false, false, false);
    }

    fn run_without_install(&self) {
        run_without_install();
    }

    fn show_run_without_install(&self) -> bool {
        show_run_without_install()
    }

    fn get_option(&self, key: String) -> String {
        get_option(key)
    }

    fn get_local_option(&self, key: String) -> String {
        get_local_option(key)
    }

    fn set_local_option(&self, key: String, value: String) {
        set_local_option(key, value);
    }

    fn peer_has_password(&self, id: String) -> bool {
        peer_has_password(id)
    }

    fn forget_password(&self, id: String) {
        forget_password(id)
    }

    fn get_peer_option(&self, id: String, name: String) -> String {
        get_peer_option(id, name)
    }

    fn set_peer_option(&self, id: String, name: String, value: String) {
        set_peer_option(id, name, value)
    }

    fn get_options_json(&self) -> String {
        get_options()
    }

    fn test_if_valid_server(&self, host: String) -> String {
        test_if_valid_server(host)
    }

    fn get_sound_inputs_json(&self) -> String {
        serde_json::to_string(&get_sound_inputs()).unwrap_or_default()
    }

    fn set_options_from_json(&self, json: String) {
        if let Ok(m) = serde_json::from_str::<HashMap<String, String>>(&json) {
            set_options(m);
        }
    }

    fn set_option(&self, key: String, value: String) {
        set_option(key, value);
    }

    fn get_config_option(&self, key: String) -> String {
        Config::get_option(&key)
    }

    fn set_config_option(&self, key: String, value: String) {
        Config::set_option(key, value);
    }

    fn requires_update(&self) -> bool {
        if env!("CARGO_PKG_NAME") != ["hop", "todesk"].concat() {
            return false;
        }
        get_version_number(crate::VERSION) < get_version_number(&Config::get_option("api_version"))
    }

    fn running_qs(&self) -> bool {
        env::args().any(|arg| arg == "--qs") ||
        env::current_exe().ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().contains("-qs")))
            .unwrap_or(false)
    }

    fn copy_text(&self, text: String) {
        copy_text_impl(&text)
    }

    fn set_version_sync(&self) {
        set_version_sync()
    }

    fn install_path(&mut self) -> String {
        install_path()
    }

    fn install_options(&self) -> String {
        install_options()
    }

    fn get_socks_json(&self) -> String {
        serde_json::to_string(&get_socks()).unwrap_or_default()
    }

    fn set_socks(&self, proxy: String, username: String, password: String) {
        set_socks(proxy, username, password)
    }

    fn is_installed(&self) -> bool {
        is_installed()
    }

    fn is_root(&self) -> bool {
        is_root()
    }

    fn is_release(&self) -> bool {
        #[cfg(not(debug_assertions))]
        return true;
        #[cfg(debug_assertions)]
        return false;
    }

    fn is_share_rdp(&self) -> bool {
        is_share_rdp()
    }

    fn set_share_rdp(&self, _enable: bool) {
        set_share_rdp(_enable);
    }

    fn is_installed_lower_version(&self) -> bool {
        is_installed_lower_version()
    }

    fn closing(&mut self, x: i32, y: i32, w: i32, h: i32) {
        crate::server::input_service::fix_key_down_timeout_at_exit();
        closing(x, y, w, h);
    }

    fn get_size_json(&self) -> String {
        let s = LocalConfig::get_size();
        serde_json::json!([s.0, s.1, s.2, s.3]).to_string()
    }

    fn get_mouse_time(&self) -> f64 {
        get_mouse_time()
    }

    fn check_mouse_time(&self) {
        check_mouse_time()
    }

    fn get_connect_status_json(&self) -> String {
        let x = get_connect_status();
        serde_json::json!({
            "status_num": x.status_num,
            "key_confirmed": x.key_confirmed,
            "id": x.id
        }).to_string()
    }

    fn get_peer_json(&self, id: String) -> String {
        let c = get_peer(id.clone());
        serde_json::json!({
            "id": id,
            "username": c.info.username,
            "hostname": c.info.hostname,
            "platform": c.info.platform,
            "alias": c.options.get("alias").unwrap_or(&"".to_owned()).clone(),
        }).to_string()
    }

    fn get_fav_json(&self) -> String {
        serde_json::to_string(&get_fav()).unwrap_or_default()
    }

    fn store_fav_from_json(&self, json: String) {
        if let Ok(fav) = serde_json::from_str::<Vec<String>>(&json) {
            store_fav(fav);
        }
    }

    fn get_recent_sessions_json(&self) -> String {
        let peers: Vec<serde_json::Value> = PeerConfig::peers(None)
            .drain(..)
            .map(|p| {
                serde_json::json!({
                    "id": p.0,
                    "username": p.2.info.username,
                    "hostname": p.2.info.hostname,
                    "platform": p.2.info.platform,
                    "alias": p.2.options.get("alias").unwrap_or(&"".to_owned()).clone(),
                })
            })
            .collect();
        serde_json::to_string(&peers).unwrap_or_default()
    }

    fn get_icon(&mut self) -> String {
        get_icon()
    }

    fn remove_peer(&mut self, id: String) {
        PeerConfig::remove(&id);
    }

    fn remove_discovered(&mut self, id: String) {
        remove_discovered(id);
    }

    fn send_wol(&mut self, id: String) {
        crate::lan::send_wol(id)
    }

    fn new_remote(&self, id: String, remote_type: String, force_relay: bool) {
        crate::ui::new_remote(id, remote_type, force_relay, None, None);
    }

    fn is_process_trusted(&mut self, _prompt: bool) -> bool {
        is_process_trusted(_prompt)
    }

    fn is_can_screen_recording(&mut self, _prompt: bool) -> bool {
        is_can_screen_recording(_prompt)
    }

    fn is_installed_daemon(&mut self, _prompt: bool) -> bool {
        is_installed_daemon(_prompt)
    }

    fn get_error(&mut self) -> String {
        get_error()
    }

    fn is_login_wayland(&mut self) -> bool {
        is_login_wayland()
    }

    fn current_is_wayland(&mut self) -> bool {
        current_is_wayland()
    }

    fn get_new_version(&self) -> String {
        get_new_version()
    }

    fn get_version(&self) -> String {
        get_version()
    }

    fn get_fingerprint(&self) -> String {
        get_fingerprint()
    }

    fn get_app_name(&self) -> String {
        get_app_name()
    }

    fn get_software_ext(&self) -> String {
        #[cfg(windows)]
        let p = "exe";
        #[cfg(target_os = "macos")]
        let p = "dmg";
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        let p = "pkg";
        p.to_owned()
    }

    fn get_software_store_path(&self) -> String {
        let mut p = std::env::temp_dir();
        let name = crate::SOFTWARE_UPDATE_URL
            .lock()
            .unwrap()
            .split("/")
            .last()
            .map(|x| x.to_owned())
            .unwrap_or(crate::get_app_name());
        p.push(name);
        format!("{}", p.to_string_lossy())
    }

    fn create_shortcut(&self, _id: String) {
    }

    fn discover(&self) {
        std::thread::spawn(move || {
            allow_err!(crate::lan::discover());
        });
    }

    fn get_lan_peers(&self) -> String {
        serde_json::to_string(&get_lan_peers()).unwrap_or_default()
    }

    fn get_uuid(&self) -> String {
        get_uuid()
    }

    fn open_url(&self, url: String) {
        let p = if std::path::Path::new("/usr/local/bin/firefox").exists() {
            "/usr/local/bin/firefox"
        } else if std::path::Path::new("/usr/local/bin/epiphany").exists() {
            "/usr/local/bin/epiphany"
        } else if std::path::Path::new("/usr/local/bin/xdg-open").exists() {
            "/usr/local/bin/xdg-open"
        } else {
            "xdg-open"
        };
        allow_err!(std::process::Command::new(p).arg(&url).spawn());
    }

    fn get_teamid(&self) -> String {
        use std::path::Path;
        if Path::new(&Config::path("TeamID.toml")).exists() {
            if let Ok(body) = std::fs::read_to_string(Config::path("TeamID.toml")) {
                return body;
            }
        }
        String::from("(none)")
    }

    fn post_request(&self, url: String, body: String, header: String) {
        post_request(url, body, header)
    }

    fn get_request(&self, url: String, header: String) {
        get_request(url, header)
    }

    fn get_async_job_status(&self) -> String {
        get_async_job_status()
    }

    fn t(&self, name: String) -> String {
        crate::client::translate(name)
    }

    fn is_xfce(&self) -> bool {
        crate::platform::is_xfce()
    }

    fn has_hwcodec(&self) -> bool {
        has_hwcodec()
    }

    fn has_vram(&self) -> bool {
        has_vram()
    }

    fn get_langs(&self) -> String {
        get_langs()
    }

    fn video_save_directory(&self, root: bool) -> String {
        video_save_directory(root)
    }

    fn handle_relay_id(&self, id: String) -> String {
        handle_relay_id(&id).to_owned()
    }

    fn get_login_device_info(&self) -> String {
        get_login_device_info_json()
    }

    fn support_remove_wallpaper(&self) -> bool {
        support_remove_wallpaper()
    }

    fn has_valid_2fa(&self) -> bool {
        has_valid_2fa()
    }

    fn generate2fa(&self) -> String {
        generate2fa()
    }

    fn verify2fa(&self, code: String) -> bool {
        verify2fa(code)
    }

    fn generate_2fa_img_src(&self, data: String) -> String {
        let v = qrcode_generator::to_png_to_vec(data, qrcode_generator::QrCodeEcc::Low, 200)
            .unwrap_or_default();
        let s = hbb_common::sodiumoxide::base64::encode(
            v,
            hbb_common::sodiumoxide::base64::Variant::Original,
        );
        format!("data:image/png;base64,{s}")
    }

    fn check_hwcodec(&self) {
        check_hwcodec()
    }

    fn get_custom_api_url(&self) -> String {
        if let Ok(Some(v)) = ipc::get_config("custom-api-url") {
            v
        } else {
            "".to_owned()
        }
    }

    fn set_custom_api_url(&self, url: String) {
        match ipc::set_config("custom-api-url", url) {
            Ok(()) => {},
            Err(e) => log::info!("Could not set custom API URL {e}"),
        }
    }

    fn send_peer_invite(&mut self, remote_id: String, self_id: String, password: String) {
        log::info!("[UI] send_peer_invite called for remote_id: {}", remote_id);
        if let Ok(s) = crate::ui_interface::SENDER.lock() {
            hbb_common::allow_err!(s.send(crate::ipc::Data::Invite(remote_id, self_id, password)));
        }
    }

    fn submit_ticket(&self, email: String, subject: String, description: String, priority: String) -> String {
        match crate::dashboard::submit_ticket(&email, &subject, &description, &priority) {
            Ok(ticket_id) => {
                serde_json::json!({"success": true, "ticket_id": ticket_id}).to_string()
            }
            Err(e) => {
                serde_json::json!({"success": false, "error": e.to_string()}).to_string()
            }
        }
    }

    fn get_my_tickets(&self) -> String {
        match crate::dashboard::get_my_tickets() {
            Ok(tickets) => tickets.to_string(),
            Err(e) => {
                log::error!("get_my_tickets failed: {}", e);
                "[]".to_string()
            }
        }
    }

    fn get_conversation(&self, ticket_id: String) -> String {
        let tid: i64 = ticket_id.parse().unwrap_or(0);
        match crate::dashboard::get_conversation(tid) {
            Ok(messages) => messages.to_string(),
            Err(e) => {
                log::error!("get_conversation failed: {}", e);
                "[]".to_string()
            }
        }
    }

    fn get_attachments(&self, ticket_id: String) -> String {
        let tid: i64 = ticket_id.parse().unwrap_or(0);
        match crate::dashboard::get_attachments(tid) {
            Ok(attachments) => attachments.to_string(),
            Err(e) => {
                log::error!("get_attachments failed: {}", e);
                "[]".to_string()
            }
        }
    }

    fn add_reply(&self, ticket_id: String, message: String) -> String {
        let tid: i64 = ticket_id.parse().unwrap_or(0);
        match crate::dashboard::add_reply(tid, &message) {
            Ok(()) => serde_json::json!({"success": true}).to_string(),
            Err(e) => serde_json::json!({"success": false, "error": e.to_string()}).to_string(),
        }
    }

    fn upload_attachment(&self, ticket_id: String, file_path: String) -> String {
        let tid: i64 = ticket_id.parse().unwrap_or(0);
        match crate::dashboard::upload_attachment(tid, &file_path) {
            Ok(()) => serde_json::json!({"success": true}).to_string(),
            Err(e) => serde_json::json!({"success": false, "error": e.to_string()}).to_string(),
        }
    }

    fn pick_file(&self) -> String {
        "".to_string()
    }

    fn get_ticket_reply_counter(&self) -> String {
        crate::dashboard::get_ticket_reply_counter().to_string()
    }

    fn open_ticket_portal(&self) -> String {
        match crate::run_me(vec!["--ticket"]) {
            Ok(_) => "ok".to_string(),
            Err(e) => e.to_string(),
        }
    }

    fn get_file_size(&self, path: String) -> String {
        let path = crate::dashboard::percent_decode_path(&path);
        match std::fs::metadata(&path) {
            Ok(m) => m.len().to_string(),
            Err(_) => "0".to_string(),
        }
    }
}

use serde::Deserialize;
#[derive(Deserialize)]
struct Version {
    winversion: String,
    linuxversion: String,
    macversion: String,
    none: String,
}

async fn get_version_(refresh_api: bool) -> String {
    if refresh_api {
        hbb_common::api::erase_api().await;
    }
    match hbb_common::api::call_api().await {
        Ok(v) => {
            match serde_json::from_value::<Version>(v.clone()) {
                Ok(body) => {
                    return body.linuxversion;
                }
                Err(_e) => {
                    let json_str = serde_json::to_string(&v).unwrap_or_default();
                    let b64 = base64::encode(json_str, base64::Variant::Original);
                    log::error!("Invalid API response: {}", b64);
                    return "".to_owned();
                }
            }
        }
        Err(e) => {
            log::error!("get_version error {:?}, refresh_api: {:?}", e, refresh_api);
            return "".to_owned();
        }
    }
}

fn copy_text_impl(text: &str) {
    let text_clip = Clipboard {
        compress: false,
        content: text.to_owned().into_bytes().into(),
        format: ClipboardFormat::Text.into(),
        ..Default::default()
    };
    update_clipboard(vec![text_clip], ClipboardSide::Client);
}

pub fn set_version_sync() {
    set_version_sync_impl()
}

fn set_version_sync_impl() {
    let rt = Runtime::new().unwrap();
    rt.block_on(async {
        let v = get_version_(true).await;
        if !v.is_empty() {
            Config::set_option("api_version".to_string(), v);
        }
    });
}

fn set_version() {
    std::thread::spawn(|| {
        if let Ok(rt) = tokio::runtime::Runtime::new() {
            rt.block_on(async {
                let v = get_version_(false).await;
                if !v.is_empty() {
                    Config::set_option("api_version".to_string(), v);
                }
            });
        }
    });
}

pub fn get_icon() -> String {
    let icon_data = include_bytes!("../res/icon.png");
    let base64_str = base64::encode(icon_data, base64::Variant::Original);
    format!("data:image/png;base64,{}", base64_str)
}

pub fn new_remote(id: String, remote_type: String, force_relay: bool, self_id_opt: Option<String>, invite_password_opt: Option<String>) {
    let mut lock = CHILDREN.lock().unwrap();
    let mut args = vec![];
    if remote_type == "invite" {
        args.push("--invite".to_string());
        args.push(id.clone());
        if let Some(sid) = self_id_opt {
            args.push(sid);
        }
        if let Some(pwd) = invite_password_opt {
            args.push(pwd);
        }
    } else {
        args.push(format!("--{}", remote_type));
        args.push(id.clone());
        if let Some(pwd) = invite_password_opt {
            args.push("--password".to_string());
            args.push(pwd);
        }
    }

    if force_relay {
        if remote_type != "invite" {
            args.push("".to_string());
        }
        args.push("--relay".to_string());
    }
    let key = (id.clone(), remote_type.clone());
    if let Some(c) = lock.1.get_mut(&key) {
        if let Ok(Some(_)) = c.try_wait() {
            lock.1.remove(&key);
        } else {
            if remote_type == "rdp" {
                allow_err!(c.kill());
                std::thread::sleep(std::time::Duration::from_millis(30));
                c.try_wait().ok();
                lock.1.remove(&key);
            } else {
                return;
            }
        }
    }

    let cmd = match std::env::current_exe() {
        Ok(c) => c,
        Err(e) => { log::error!("Failed to get exe: {}", e); return; }
    };
    match std::process::Command::new(cmd).args(&args).stdin(Stdio::piped()).spawn() {
        Ok(mut child) => {
            let stdin = child.stdin.take();
            if let Some(s) = stdin {
                CHILD_STDINS.lock().unwrap().insert(id.clone(), s);
            }
            lock.1.insert(key, child);
        }
        Err(err) => {
            log::error!("Failed to spawn remote: {}", err);
        }
    }
}

#[inline]
pub fn recent_sessions_updated() -> bool {
    let mut children = CHILDREN.lock().unwrap();
    if children.0 {
        children.0 = false;
        true
    } else {
        false
    }
}


#[cfg(not(any(feature = "flutter", feature = "cli")))]
pub fn list_active_connections() -> Vec<(String, String, bool)> {
    let lock = CHILDREN.lock().unwrap();
    lock.1.iter().map(|((id, conn_type), _child)| {
        (id.clone(), conn_type.clone(), true)
    }).collect()
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
pub fn send_to_child(peer_id: &str, cmd: &str) -> bool {
    let mut stdins = CHILD_STDINS.lock().unwrap();
    if let Some(stdin) = stdins.get_mut(peer_id) {
        use std::io::Write;
        let msg = format!("{}\n", cmd);
        stdin.write_all(msg.as_bytes()).is_ok()
    } else {
        false
    }
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
pub fn send_dismiss_to_all_children() -> usize {
    let mut stdins = CHILD_STDINS.lock().unwrap();
    let mut count = 0;
    for (_, stdin) in stdins.iter_mut() {
        use std::io::Write;
        if stdin.write_all(b"dismiss\n").is_ok() {
            count += 1;
        }
    }
    count
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
pub fn close_remote_connection(peer_id: &str) {
    let mut lock = CHILDREN.lock().unwrap();
    let key_to_remove: Option<(String, String)> = lock.1.keys()
        .find(|(id, _)| id == peer_id)
        .cloned();
    if let Some(key) = key_to_remove {
        if let Some(mut child) = lock.1.remove(&key) {
            child.kill().ok();
        }
        CHILD_STDINS.lock().unwrap().remove(peer_id);
    }
}
