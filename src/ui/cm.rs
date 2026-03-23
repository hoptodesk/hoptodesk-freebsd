use crate::ui_cm_interface::{start_ipc, ConnectionManager, InvokeUiCM};
use hbb_common::allow_err;
use std::sync::{Arc, Mutex};
use hbb_common::log;
#[cfg(target_os = "linux")]
use crate::ipc::start_pa;

#[derive(Clone, Default)]
pub struct SciterHandler {
}

fn send_to_cm_webview(method: &str, data: &str) {
    let script = format!("window.onRustResponse && window.onRustResponse('{}', {})", method, data);
    if let Some(ref sender) = *super::WEBVIEW_SENDER.lock().unwrap() {
        sender.send(script).ok();
    }
    if let Some(ref proxy) = *super::EVENT_LOOP_PROXY.lock().unwrap() {
        proxy.send_event("cm_update".to_string()).ok();
    }
}

impl InvokeUiCM for SciterHandler {
    fn add_connection(&self, client: &crate::ui_cm_interface::Client, security_numbers: String, _avatar_image: String) {
        log::info!("[CM-wry] add_connection: peer_id={}, name={}, authorized={}", client.peer_id, client.name, client.authorized);
        let name = client.name.replace('\\', "\\\\").replace('"', "\\\"");
        let peer_id = client.peer_id.replace('"', "\\\"");
        let sec = security_numbers.replace('"', "\\\"");
        send_to_cm_webview("add_connection", &format!(
            "{{\"id\":{},\"peer_id\":\"{}\",\"name\":\"{}\",\"authorized\":{},\"keyboard\":{},\"clipboard\":{},\"audio\":{},\"file\":{},\"restart\":{},\"recording\":{},\"is_file_transfer\":{},\"is_port_forward\":{},\"is_invite\":{},\"security_numbers\":\"{}\"}}",
            client.id, peer_id, name, client.authorized, client.keyboard, client.clipboard,
            client.audio, client.file, client.restart, client.recording,
            client.is_file_transfer, !client.port_forward.is_empty(), client.is_invite, sec
        ));
    }

    fn remove_connection(&self, id: i32, close: bool) {
        log::info!("[CM-wry] remove_connection: id={}, close={}", id, close);
        send_to_cm_webview("remove_connection", &format!("{{\"id\":{},\"close\":{}}}", id, close));
        if crate::ui_cm_interface::get_clients_length().eq(&0) {
            crate::platform::quit_gui();
        }
    }

    fn new_message(&self, id: i32, text: String) {
        log::info!("[CM-wry] new_message: id={}, text={}", id, text);
        let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
        send_to_cm_webview("new_message", &format!("{{\"id\":{},\"text\":\"{}\"}}", id, escaped));
    }

    fn change_theme(&self, dark: String) {
        send_to_cm_webview("change_theme", &format!("\"{}\"", dark));
    }
    fn change_language(&self) {}

    fn show_elevation(&self, show: bool) {
        log::info!("[CM-wry] show_elevation: {}", show);
        send_to_cm_webview("show_elevation", &format!("{}", show));
    }

    fn update_voice_call_state(&self, client: &crate::ui_cm_interface::Client) {
        log::info!("[CM-wry] update_voice_call_state: id={}", client.id);
    }

    fn file_transfer_log(&self, action: &str, log_msg: &str) {
        log::info!("[CM-wry] file_transfer_log: action={}, log={}", action, log_msg);
        let a = action.replace('"', "\\\"");
        let l = log_msg.replace('\\', "\\\\").replace('"', "\\\"");
        send_to_cm_webview("file_transfer_log", &format!("{{\"action\":\"{}\",\"log\":\"{}\"}}", a, l));
    }

    fn accept_invite(&self, id: i32) {
        log::info!("[CM-wry] accept_invite: id={}", id);
        send_to_cm_webview("accept_invite", &format!("{{\"id\":{}}}", id));
    }

    fn decline_invite(&self, id: i32) {
        log::info!("[CM-wry] decline_invite: id={}", id);
        send_to_cm_webview("decline_invite", &format!("{{\"id\":{}}}", id));
    }
}

pub struct SciterConnectionManager(ConnectionManager<SciterHandler>);

impl std::ops::Deref for SciterConnectionManager {
    type Target = ConnectionManager<SciterHandler>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl SciterConnectionManager {
    pub fn new() -> Self {
        #[cfg(target_os = "linux")]
        std::thread::spawn(start_pa);
        let cm = ConnectionManager {
            ui_handler: SciterHandler::default(),
        };
        let cloned = cm.clone();
        std::thread::spawn(move || start_ipc(cloned));
        SciterConnectionManager(cm)
    }

    fn get_icon(&mut self) -> String {
        super::get_icon()
    }

    fn check_click_time(&mut self, id: i32) {
        crate::ui_cm_interface::check_click_time(id);
    }

    fn get_click_time(&self) -> f64 {
        crate::ui_cm_interface::get_click_time() as _
    }

    pub fn switch_permission(&self, id: i32, name: String, enabled: bool) {
        crate::ui_cm_interface::switch_permission(id, name, enabled);
    }

    pub fn close(&self, id: i32) {
        crate::ui_cm_interface::close(id);
    }

    pub fn remove_disconnected_connection(&self, id: i32) {
        crate::ui_cm_interface::remove(id);
    }

    pub fn quit(&self) {
        log::info!("[CM quit] Closing all client connections before quit");
        crate::ui_cm_interface::close_all_clients();
        std::thread::sleep(std::time::Duration::from_millis(150));
        crate::platform::quit_gui();
    }

    pub fn authorize(&self, id: i32) {
        crate::ui_cm_interface::authorize(id);
    }

    pub fn send_msg(&self, id: i32, text: String) {
        crate::ui_cm_interface::send_chat(id, text);
    }

    fn t(&self, name: String) -> String {
        crate::client::translate(name)
    }

    fn can_elevate(&self) -> bool {
        crate::ui_cm_interface::can_elevate()
    }

    fn elevate_portable(&self, id: i32) {
        crate::ui_cm_interface::elevate_portable(id);
    }

    fn get_option(&self, key: String) -> String {
        crate::ui_interface::get_option(key)
    }

    pub fn accept_invite(&mut self, id: i32) {
        log::info!("Invite accepted for connection id {}", id);
        if let Some(client) = crate::ui_cm_interface::get_clients_lock().ok().and_then(|clients| clients.get(&id).cloned()) {
            if let Err(e) = client.get_tx().send(crate::ipc::Data::InviteResponse { id, accepted: true }) {
                log::error!("Failed to send accept response for invite: {}", e);
            }
            log::info!("Spawning new process to connect to inviter {}", client.peer_id);
            if let Ok(exe) = std::env::current_exe() {
                let mut cmd = std::process::Command::new(exe);
                cmd.arg("--connect");
                cmd.arg(client.peer_id);
                cmd.arg("--password");
                cmd.arg(client.password_to_connect_to_inviter);
                if let Err(e) = cmd.spawn() {
                    log::error!("Failed to spawn new process for invited connection: {}", e);
                }
            }
        }
        crate::ui_cm_interface::close(id);
    }

    pub fn decline_invite(&mut self, id: i32) {
        log::info!("Invite declined for connection id {}", id);
        if let Some(client) = crate::ui_cm_interface::get_clients_lock().ok().and_then(|clients| clients.get(&id).cloned()) {
            if let Err(e) = client.get_tx().send(crate::ipc::Data::InviteResponse { id, accepted: false }) {
                log::error!("Failed to send decline response for invite: {}", e);
            }
        }
        crate::ui_cm_interface::close(id);
    }
}
