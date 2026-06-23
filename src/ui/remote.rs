use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex, RwLock},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use hbb_common::{
    allow_err, fs::TransferJobMeta, log, message_proto::*, rendezvous_proto::ConnType,
};

use crate::{
    client::*,
    ui::terminal_emulator::{build_update_payload, TerminalEmulator},
    ui_session_interface::{InvokeUiSession, Session},
};

static MCP_DISMISS_FLAG: AtomicBool = AtomicBool::new(false);
static MCP_QUERY_STATE_FLAG: AtomicBool = AtomicBool::new(false);

lazy_static::lazy_static! {
    static ref MCP_CLICK_BUTTON: Mutex<String> = Mutex::new(String::new());
    static ref MCP_CHAT_MSG: Mutex<String> = Mutex::new(String::new());
    static ref MCP_PASSWORD: Mutex<String> = Mutex::new(String::new());
    static ref MCP_DEBUG: Mutex<String> = Mutex::new(String::new());
    static ref MCP_STATE_RESULT: Mutex<String> = Mutex::new(String::new());
}

static LAST_FRAME_TIME: AtomicU64 = AtomicU64::new(0);
static FRAME_COUNT: AtomicU64 = AtomicU64::new(0);
const MIN_FRAME_INTERVAL_MS: u64 = 150;
const MAX_FRAME_DIMENSION: usize = 960;

lazy_static::lazy_static! {
    pub static ref CURRENT_FRAME: Mutex<Option<Vec<u8>>> = Mutex::new(None);
    pub static ref FRAME_SERVER_PORT: Mutex<u16> = Mutex::new(0);
}

fn send_to_remote_webview(method: &str, data: &str) {
    let script = format!("window.onRustResponse && window.onRustResponse('{}', {})", method, data);
    if let Some(ref sender) = *super::WEBVIEW_SENDER.lock().unwrap() {
        sender.send(script).ok();
    }
    if let Some(ref proxy) = *super::EVENT_LOOP_PROXY.lock().unwrap() {
        proxy.send_event("frame".to_string()).ok();
    }
}

pub fn start_frame_server() {
    use std::net::TcpListener;
    use std::io::{Read as IoRead, Write as IoWrite};

    let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind frame server");
    let port = listener.local_addr().unwrap().port();
    *FRAME_SERVER_PORT.lock().unwrap() = port;
    log::info!("[remote-wry] Frame server started on port {}", port);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if let Ok(mut stream) = stream {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);

                let frame_data = CURRENT_FRAME.lock().unwrap().clone();
                if let Some(jpeg) = frame_data {
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                        jpeg.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(&jpeg);
                } else {
                    let _ = stream.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
                }
            }
        }
    });
}

pub fn start_stdin_command_reader() {
    std::thread::spawn(|| {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        let reader = stdin.lock();
        for line in reader.lines() {
            match line {
                Ok(cmd) => {
                    let cmd = cmd.trim().to_string();
                    if cmd == "dismiss" {
                        MCP_DISMISS_FLAG.store(true, Ordering::SeqCst);
                    } else if cmd == "query_state" {
                        MCP_STATE_RESULT.lock().unwrap().clear();
                        MCP_QUERY_STATE_FLAG.store(true, Ordering::SeqCst);
                    } else if cmd.starts_with("click_button:") {
                        let button_id = cmd["click_button:".len()..].to_string();
                        *MCP_CLICK_BUTTON.lock().unwrap() = button_id;
                    } else if cmd.starts_with("chat:") {
                        let msg = cmd["chat:".len()..].to_string();
                        *MCP_CHAT_MSG.lock().unwrap() = msg;
                    } else if cmd.starts_with("password:") {
                        let pw = cmd["password:".len()..].to_string();
                        *MCP_PASSWORD.lock().unwrap() = pw;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

pub fn start_file_command_watcher() {
    std::thread::spawn(|| {
        let cmd_file = std::env::temp_dir().join("htd-mcp-cmd.txt");
        loop {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if cmd_file.exists() {
                match std::fs::read_to_string(&cmd_file) {
                    Ok(contents) => {
                        let _ = std::fs::remove_file(&cmd_file);
                        for line in contents.lines() {
                            let cmd = line.trim().to_string();
                            if cmd.is_empty() { continue; }
                            if cmd == "dismiss" {
                                MCP_DISMISS_FLAG.store(true, Ordering::SeqCst);
                            } else if cmd == "query_state" {
                                MCP_STATE_RESULT.lock().unwrap().clear();
                                MCP_QUERY_STATE_FLAG.store(true, Ordering::SeqCst);
                            } else if cmd.starts_with("click_button:") {
                                *MCP_CLICK_BUTTON.lock().unwrap() = cmd["click_button:".len()..].to_string();
                            } else if cmd.starts_with("chat:") {
                                *MCP_CHAT_MSG.lock().unwrap() = cmd["chat:".len()..].to_string();
                            } else if cmd.starts_with("password:") {
                                *MCP_PASSWORD.lock().unwrap() = cmd["password:".len()..].to_string();
                            }
                        }
                    }
                    Err(_) => {
                        let _ = std::fs::remove_file(&cmd_file);
                    }
                }
            }
        }
    });
}

#[derive(Clone, Default)]
pub struct SciterHandler {
    close_state: HashMap<String, String>,
    terminals: Arc<Mutex<HashMap<i32, TerminalEmulator>>>,
}

impl SciterHandler {
    fn feed_terminal(&self, id: i32, bytes: &[u8]) {
        let mut map = self.terminals.lock().unwrap();
        let emu = map.entry(id).or_insert_with(|| TerminalEmulator::new(24, 80));
        let update = emu.feed(bytes);
        let payload = build_update_payload(&update);
        drop(map);
        self.dispatch_terminal_update(id, &update, payload);
    }

    fn drop_terminal(&self, id: i32) {
        self.terminals.lock().unwrap().remove(&id);
    }

    pub fn resize_terminal(&self, id: i32, rows: u16, cols: u16) {
        let mut map = self.terminals.lock().unwrap();
        let emu = map.entry(id).or_insert_with(|| TerminalEmulator::new(rows, cols));
        emu.resize(rows, cols);
        let update = emu.set_view(0);
        let payload = build_update_payload(&update);
        drop(map);
        self.dispatch_terminal_update(id, &update, payload);
    }

    fn dispatch_terminal_update(
        &self,
        id: i32,
        update: &crate::ui::terminal_emulator::FrameUpdate,
        payload: String,
    ) {
        send_to_remote_webview("terminalUpdate", &format!(
            "{{\"id\":{},\"rows\":{},\"cur_r\":{},\"cur_c\":{},\"cur_vis\":{},\"app_cursor\":{},\"bracketed_paste\":{},\"full\":{}}}",
            id, payload, update.cursor_row, update.cursor_col, update.cursor_visible,
            update.app_cursor_keys, update.bracketed_paste, update.full_repaint
        ));
    }
}

impl InvokeUiSession for SciterHandler {
    fn set_cursor_data(&self, cd: CursorData) {
        log::debug!("[remote-wry] set_cursor_data: id={}, {}x{}", cd.id, cd.width, cd.height);
        if cd.width <= 0 || cd.height <= 0 || cd.colors.is_empty() {
            return;
        }
        let w = cd.width as usize;
        let h = cd.height as usize;
        let expected = w * h * 4;
        if cd.colors.len() < expected {
            return;
        }
        let mut png_buf = Vec::new();
        if repng::encode(&mut png_buf, w as u32, h as u32, &cd.colors[..expected]).is_ok() {
            use hbb_common::base64::{engine::general_purpose::STANDARD, Engine};
            let b64 = STANDARD.encode(&png_buf);
            let data_url = format!("data:image/png;base64,{}", b64);
            send_to_remote_webview("set_cursor_data",
                &format!("{{\"url\":\"{}\",\"hotx\":{},\"hoty\":{}}}", data_url, cd.hotx, cd.hoty));
        }
    }

    fn set_display(&self, _x: i32, _y: i32, w: i32, h: i32, _cursor_embedded: bool) {
        log::info!("[remote-wry] set_display: {}x{}", w, h);
        send_to_remote_webview("set_display", &format!("{{\"w\":{},\"h\":{}}}", w, h));
    }

    fn update_privacy_mode(&self) {
        log::debug!("[remote-wry] update_privacy_mode");
    }

    fn set_permission(&self, name: &str, value: bool) {
        log::debug!("[remote-wry] set_permission: {}={}", name, value);
        let escaped_name = name.replace('"', "\\\"");
        send_to_remote_webview("set_permission",
            &format!("{{\"name\":\"{}\",\"value\":{}}}", escaped_name, value));
    }

    fn close_success(&self) {
        log::info!("[remote-wry] close_success");
        send_to_remote_webview("cancel_msgbox", "null");
    }

    fn update_quality_status(&self, status: QualityStatus) {
        let speed = status.speed.as_deref().unwrap_or("");
        let fps = status.fps.values().next().copied().unwrap_or(0);
        let delay = status.delay.unwrap_or(0);
        send_to_remote_webview("update_quality_status",
            &format!("{{\"fps\":{},\"delay\":{},\"speed\":\"{}\"}}", fps, delay, speed));
    }

    fn set_cursor_id(&self, id: String) {
        log::debug!("[remote-wry] set_cursor_id: {}", id);
        let css_cursor = match id.as_str() {
            "default" => "default",
            "hand" | "pointer" => "pointer",
            "text" | "ibeam" => "text",
            "cross" | "crosshair" => "crosshair",
            "move" => "move",
            "not-allowed" | "no" => "not-allowed",
            "wait" | "busy" => "wait",
            "progress" | "working" => "progress",
            "help" => "help",
            "n-resize" | "ns-resize" => "ns-resize",
            "e-resize" | "ew-resize" => "ew-resize",
            "ne-resize" | "nesw-resize" => "nesw-resize",
            "nw-resize" | "nwse-resize" => "nwse-resize",
            _ => "default",
        };
        send_to_remote_webview("set_cursor_id", &format!("\"{}\"", css_cursor));
    }

    fn set_cursor_position(&self, cp: CursorPosition) {
        log::debug!("[remote-wry] set_cursor_position: ({}, {})", cp.x, cp.y);
    }

    fn set_connection_type(&self, is_secured: bool, direct: bool, security_numbers: String, _avatar_image: String) {
        log::info!("[remote-wry] set_connection_type: secured={}, direct={}, security={}", is_secured, direct, security_numbers);
        send_to_remote_webview("set_connection_type",
            &format!("{{\"secured\":{},\"direct\":{},\"security_numbers\":\"{}\"}}",
                is_secured, direct, security_numbers));
    }

    fn set_fingerprint(&self, _fingerprint: String) {}

    fn job_error(&self, id: i32, err: String, file_num: i32) {
        log::error!("[remote-wry] job_error: id={}, err={}, file_num={}", id, err, file_num);
        let escaped = err.replace('\\', "\\\\").replace('"', "\\\"");
        send_to_remote_webview("job_error", &format!("{{\"id\":{},\"err\":\"{}\",\"file_num\":{}}}", id, escaped, file_num));
    }

    fn job_done(&self, id: i32, file_num: i32) {
        log::info!("[remote-wry] job_done: id={}, file_num={}", id, file_num);
        send_to_remote_webview("job_done", &format!("{{\"id\":{},\"file_num\":{}}}", id, file_num));
    }

    fn clear_all_jobs(&self) {
        log::info!("[remote-wry] clear_all_jobs");
        send_to_remote_webview("clear_all_jobs", "null");
    }

    fn load_last_job(&self, cnt: i32, job_json: &str, auto_start: bool) {
        log::info!("[remote-wry] load_last_job: cnt={}, auto_start={}", cnt, auto_start);
        let escaped = job_json.replace('\\', "\\\\").replace('"', "\\\"");
        send_to_remote_webview("load_last_job", &format!(
            "{{\"cnt\":{},\"job\":\"{}\",\"auto_start\":{}}}", cnt, escaped, auto_start));
    }

    fn new_message(&self, msg: String) {
        log::info!("[remote-wry] new_message: {}", msg);
        let escaped = msg.replace('\\', "\\\\").replace('"', "\\\"");
        send_to_remote_webview("new_message",
            &format!("{{\"name\":\"Remote\",\"text\":\"{}\"}}", escaped));
    }

    fn update_transfer_list(&self) {
        log::debug!("[remote-wry] update_transfer_list");
        send_to_remote_webview("update_transfer_list", "null");
    }

    fn confirm_delete_files(&self, id: i32, cnt: i32, name: String) {
        log::info!("[remote-wry] confirm_delete_files: id={}, cnt={}, name={}", id, cnt, name);
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        send_to_remote_webview("confirm_delete_files", &format!("{{\"id\":{},\"cnt\":{},\"name\":\"{}\"}}", id, cnt, escaped));
    }

    fn override_file_confirm(
        &self,
        id: i32,
        file_num: i32,
        to: String,
        is_upload: bool,
        is_identical: bool,
    ) {
        log::info!("[remote-wry] override_file_confirm: id={}, file_num={}, to={}, identical={}", id, file_num, to, is_identical);
        let escaped = to.replace('\\', "\\\\").replace('"', "\\\"");
        send_to_remote_webview("override_file_confirm", &format!(
            "{{\"id\":{},\"file_num\":{},\"to\":\"{}\",\"is_upload\":{},\"is_identical\":{}}}",
            id, file_num, escaped, is_upload, is_identical));
    }

    fn update_block_input_state(&self, on: bool) {
        send_to_remote_webview("update_block_input_state", &format!("{}", on));
    }

    fn job_progress(&self, id: i32, file_num: i32, speed: f64, finished_size: f64) {
        let speed_str = if speed > 1048576.0 {
            format!("{:.1} MB/s", speed / 1048576.0)
        } else if speed > 1024.0 {
            format!("{:.0} KB/s", speed / 1024.0)
        } else {
            format!("{:.0} B/s", speed)
        };
        send_to_remote_webview("job_progress", &format!(
            "{{\"id\":{},\"file_num\":{},\"speed\":\"{}\",\"finished_size\":{}}}",
            id, file_num, speed_str, finished_size));
    }

    fn adapt_size(&self) {
        log::debug!("[remote-wry] adapt_size");
    }

    fn on_rgba(&self, _display: usize, rgba: &mut scrap::ImageRgb) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let last = LAST_FRAME_TIME.load(Ordering::Relaxed);
        let count = FRAME_COUNT.load(Ordering::Relaxed);

        let interval = if count < 5 { 500 } else { MIN_FRAME_INTERVAL_MS };
        if now - last < interval {
            return;
        }
        LAST_FRAME_TIME.store(now, Ordering::Relaxed);
        FRAME_COUNT.fetch_add(1, Ordering::Relaxed);

        let w = rgba.w;
        let h = rgba.h;
        if w == 0 || h == 0 { return; }

        let rgba_data = &rgba.raw;
        let expected_len = w * h * 4;
        if rgba_data.len() < expected_len { return; }

        let max_dim = MAX_FRAME_DIMENSION;
        let (out_w, out_h, rgb_data) = if w > max_dim || h > max_dim {
            let scale = max_dim as f64 / w.max(h) as f64;
            let nw = (w as f64 * scale) as usize;
            let nh = (h as f64 * scale) as usize;
            let mut out = Vec::with_capacity(nw * nh * 3);
            for dy in 0..nh {
                let sy = dy * h / nh;
                for dx in 0..nw {
                    let sx = dx * w / nw;
                    let i = (sy * w + sx) * 4;
                    out.push(rgba_data[i]);
                    out.push(rgba_data[i + 1]);
                    out.push(rgba_data[i + 2]);
                }
            }
            (nw, nh, out)
        } else {
            let mut rgb = Vec::with_capacity(w * h * 3);
            for pixel in rgba_data.chunks_exact(4) {
                rgb.push(pixel[0]);
                rgb.push(pixel[1]);
                rgb.push(pixel[2]);
            }
            (w, h, rgb)
        };

        use image::ImageEncoder;
        let mut jpeg_buf = Vec::new();
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_buf, 40);
        if encoder.write_image(&rgb_data, out_w as u32, out_h as u32, image::ColorType::Rgb8).is_ok() {
            log::debug!("[remote-wry] Frame {}: {}x{} → {}x{}, JPEG {} bytes",
                count, w, h, out_w, out_h, jpeg_buf.len());
            *CURRENT_FRAME.lock().unwrap() = Some(jpeg_buf);
            send_to_remote_webview("new_frame", "true");
        }
    }

    fn msgbox(&self, msgtype: &str, title: &str, text: &str, link: &str, retry: bool) {
        log::info!("[remote-wry] msgbox: type={}, title={}, text={}, link={}, retry={}", msgtype, title, text, link, retry);
        let escaped_title = title.replace('\\', "\\\\").replace('"', "\\\"");
        let escaped_text = text.replace('\\', "\\\\").replace('"', "\\\"");
        let escaped_type = msgtype.replace('\\', "\\\\").replace('"', "\\\"");
        send_to_remote_webview("msgbox",
            &format!("{{\"type\":\"{}\",\"title\":\"{}\",\"text\":\"{}\",\"link\":\"{}\",\"retry\":{}}}",
                escaped_type, escaped_title, escaped_text, link, retry));
    }

    fn cancel_msgbox(&self, _tag: &str) {
        log::debug!("[remote-wry] cancel_msgbox");
        send_to_remote_webview("cancel_msgbox", "null");
    }

    fn switch_back(&self, _peer_id: &str) {
        log::info!("[remote-wry] switch_back");
    }

    fn portable_service_running(&self, _running: bool) {}

    fn on_voice_call_started(&self) {
        log::info!("[remote-wry] on_voice_call_started");
    }

    fn on_voice_call_closed(&self, _reason: &str) {
        log::info!("[remote-wry] on_voice_call_closed");
    }

    fn on_voice_call_waiting(&self) {
        log::info!("[remote-wry] on_voice_call_waiting");
    }

    fn on_voice_call_incoming(&self) {
        log::info!("[remote-wry] on_voice_call_incoming");
    }

    fn get_rgba(&self, _display: usize) -> *const u8 {
        std::ptr::null()
    }

    fn next_rgba(&self, _display: usize) {}

    fn switch_display(&self, display: &hbb_common::message_proto::SwitchDisplay) {
        log::info!("[remote-wry] switch_display: idx={}, x={}, y={}, w={}, h={}",
            display.display, display.x, display.y, display.width, display.height);
        send_to_remote_webview("switch_display", &format!(
            "{{\"display\":{},\"x\":{},\"y\":{},\"width\":{},\"height\":{}}}",
            display.display, display.x, display.y, display.width, display.height));
    }

    fn set_peer_info(&self, peer_info: &hbb_common::message_proto::PeerInfo) {
        log::info!("[remote-wry] set_peer_info: username={}, hostname={}, platform={}, version={}",
            peer_info.username, peer_info.hostname, peer_info.platform, peer_info.version);
        let escaped_user = peer_info.username.replace('\\', "\\\\").replace('"', "\\\"");
        let escaped_host = peer_info.hostname.replace('\\', "\\\\").replace('"', "\\\"");
        let escaped_platform = peer_info.platform.replace('\\', "\\\\").replace('"', "\\\"");
        let escaped_version = peer_info.version.replace('\\', "\\\\").replace('"', "\\\"");
        let sas_enabled = peer_info.sas_enabled;
        let num_displays = peer_info.displays.len();
        send_to_remote_webview("set_peer_info",
            &format!("{{\"username\":\"{}\",\"hostname\":\"{}\",\"platform\":\"{}\",\"version\":\"{}\",\"sas_enabled\":{},\"num_displays\":{},\"dashboard_linked\":{}}}",
                escaped_user, escaped_host, escaped_platform, escaped_version, sas_enabled, num_displays, peer_info.dashboard_linked));
    }

    fn set_displays(&self, displays: &Vec<hbb_common::message_proto::DisplayInfo>) {
        log::info!("[remote-wry] set_displays: {} displays", displays.len());
        let display_json: Vec<String> = displays.iter().map(|d| {
            format!("{{\"x\":{},\"y\":{},\"width\":{},\"height\":{}}}", d.x, d.y, d.width, d.height)
        }).collect();
        send_to_remote_webview("set_displays",
            &format!("{{\"displays\":[{}],\"current\":0}}", display_json.join(",")));
    }

    fn set_platform_additions(&self, _data: &str) {
        log::debug!("[remote-wry] set_platform_additions");
    }

    fn on_connected(&self, conn_type: hbb_common::rendezvous_proto::ConnType) {
        log::info!("[remote-wry] on_connected: {:?}", conn_type);
        if conn_type == ConnType::DEFAULT_CONN {
            crate::keyboard::client::start_grab_loop();
        }
        send_to_remote_webview("on_connected", "null");
    }

    fn update_folder_files(
        &self,
        id: i32,
        entries: &Vec<hbb_common::message_proto::FileEntry>,
        path: String,
        is_local: bool,
        only_count: bool,
    ) {
        log::info!("[remote-wry] update_folder_files: id={}, path={}, is_local={}, entries={}", id, path, is_local, entries.len());
        let mut fd = make_fd_json(id, entries, only_count);
        fd["is_local"] = serde_json::json!(is_local);
        fd["path"] = serde_json::json!(path);
        send_to_remote_webview("update_folder_files", &fd.to_string());
    }

    fn set_multiple_windows_session(&self, _sessions: Vec<hbb_common::message_proto::WindowsSession>) {
        log::debug!("[remote-wry] set_multiple_windows_session");
    }

    fn set_current_display(&self, disp_idx: i32) {
        log::info!("[remote-wry] set_current_display: {}", disp_idx);
        send_to_remote_webview("set_current_display", &format!("{}", disp_idx));
    }

    fn update_record_status(&self, start: bool) {
        log::debug!("[remote-wry] update_record_status: {}", start);
        send_to_remote_webview("update_record_status", &format!("{}", start));
    }

    fn handle_screenshot_resp(&self, _sid: String, msg: String) {
        log::info!("[remote-wry] handle_screenshot_resp: {}", msg);
        let escaped = msg.replace('\\', "\\\\").replace('"', "\\\"");
        send_to_remote_webview("handle_screenshot_resp", &format!("\"{}\"", escaped));
    }

    fn handle_terminal_response(&self, resp: TerminalResponse) {
        use hbb_common::message_proto::terminal_response::Union;
        match resp.union {
            Some(Union::Opened(opened)) => {
                send_to_remote_webview("terminalOpened",
                    &format!("{{\"id\":{},\"success\":{},\"message\":\"{}\"}}",
                        opened.terminal_id, opened.success,
                        opened.message.replace('\\', "\\\\").replace('"', "\\\"")));
            }
            Some(Union::Data(data)) => {
                let output = if data.compressed {
                    hbb_common::compress::decompress(&data.data)
                } else {
                    data.data.to_vec()
                };
                self.feed_terminal(data.terminal_id, &output);
            }
            Some(Union::Closed(closed)) => {
                self.drop_terminal(closed.terminal_id);
                send_to_remote_webview("terminalClosed",
                    &format!("{{\"id\":{},\"exit_code\":{}}}", closed.terminal_id, closed.exit_code));
            }
            Some(Union::Error(error)) => {
                send_to_remote_webview("terminalError",
                    &format!("{{\"id\":{},\"message\":\"{}\"}}", error.terminal_id,
                        error.message.replace('\\', "\\\\").replace('"', "\\\"")));
            }
            _ => {}
        }
    }

}

pub struct SciterSession {
    pub session: Session<SciterHandler>,
}

impl Deref for SciterSession {
    type Target = Session<SciterHandler>;

    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

impl DerefMut for SciterSession {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.session
    }
}

impl SciterSession {
    pub fn new(cmd: String, id: String, password: String, tokenexp: String, args: Vec<String>) -> Self {
        let force_relay = args.contains(&"--relay".to_string());
        let handler = SciterHandler {
            ..Default::default()
        };

        let session: Session<SciterHandler> = Session {
            id: id.clone(),
            password: password.clone(),
            tokenexp: tokenexp.clone(),
            args,
            ui_handler: handler,
            server_keyboard_enabled: Arc::new(RwLock::new(true)),
            server_file_transfer_enabled: Arc::new(RwLock::new(true)),
            server_clipboard_enabled: Arc::new(RwLock::new(true)),
            ..Default::default()
        };
        let conn_type = if cmd.eq("--file-transfer") {
            ConnType::FILE_TRANSFER
        } else if cmd.eq("--view-camera") {
            ConnType::VIEW_CAMERA
        } else if cmd.eq("--port-forward") {
            ConnType::PORT_FORWARD
        } else if cmd.eq("--rdp") {
            ConnType::RDP
        } else {
            ConnType::DEFAULT_CONN
        };

        let invoked_switch_uuid = crate::core_main::SWITCH_SIDES_INVOKED_UUID.lock().unwrap().take();

        session
            .lc
            .write()
            .unwrap()
            .initialize(
                id,
                conn_type,
                invoked_switch_uuid,
                force_relay,
                None,
                None,
                None,
                tokenexp,
            );

        start_stdin_command_reader();
        start_file_command_watcher();

        SciterSession { session }
    }

    pub fn inner(&self) -> Session<SciterHandler> {
        self.session.clone()
    }

    pub fn t(&self, name: String) -> String {
        crate::client::translate(name)
    }

    pub fn get_icon(&self) -> String {
        super::get_icon()
    }
}

pub fn make_fd_json(id: i32, entries: &Vec<FileEntry>, only_count: bool) -> serde_json::Value {
    let mut n: u64 = 0;
    let mut file_entries = Vec::new();
    for entry in entries {
        n += entry.size;
        if !only_count {
            file_entries.push(serde_json::json!({
                "name": entry.name,
                "type": if entry.entry_type.value() == 0 { 1 } else { entry.entry_type.value() },
                "time": entry.modified_time as f64,
                "size": entry.size as f64,
            }));
        }
    }
    let mut result = serde_json::json!({
        "id": id,
        "num_entries": entries.len() as i32,
        "total_size": n as f64,
    });
    if !only_count {
        result["entries"] = serde_json::json!(file_entries);
    }
    result
}
