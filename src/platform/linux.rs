use super::{gtk_sudo, CursorData, ResultType};
use desktop::Desktop;
pub use hbb_common::platform::linux::*;
use hbb_common::{
    allow_err,
    anyhow::anyhow,
    bail,
    config::{keys::OPTION_ALLOW_LINUX_HEADLESS, Config},
    libc::{c_char, c_int, c_long, c_uint, c_void},
    log,
    message_proto::{DisplayInfo, Resolution},
    regex::{Captures, Regex},
    users::{get_user_by_name, os::unix::UserExt},
};
use libxdo_sys::{self, xdo_t, Window};
use std::{
    cell::RefCell,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::{Child, Command},
    string::String,
    sync::atomic::{AtomicBool, Ordering},
    sync::Arc,
    time::{Duration, Instant},
};
use terminfo::{capability as cap, Database};
use wallpaper;

pub const PA_SAMPLE_RATE: u32 = 48000;
static mut UNMODIFIED: bool = true;

const INVALID_TERM_VALUES: [&str; 3] = ["", "unknown", "dumb"];
const SHELL_PROCESSES: [&str; 4] = ["bash", "zsh", "fish", "sh"];

const TERM_XTERM_256COLOR: &str = "xterm-256color";
const TERM_SCREEN_256COLOR: &str = "screen-256color";
const TERM_XTERM: &str = "xterm";

lazy_static::lazy_static! {
    pub static ref IS_X11: bool = hbb_common::platform::linux::is_x11_or_headless();
    static ref CACHED_TERM: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    static ref DATABASE_XTERM_256COLOR: Option<Database> = {
        match Database::from_name(TERM_XTERM_256COLOR) {
            Ok(database) => Some(database),
            Err(err) => {
                log::error!("Failed to initialize {} database: {}", TERM_XTERM_256COLOR, err);
                None
            }
        }
    };
    static ref SUDO_E_PRESERVES_ENV: bool = {
        if !is_root() {
            log::warn!("Not running as root, SUDO_E_PRESERVES_ENV check skipped");
            false
        } else {
            let key = format!("__RUSTDESK_SUDO_E_TEST_{}", std::process::id());
            let val = "1";
            let expected = format!("{key}={val}");
            Command::new("sudo")
                .env(&key, val)
                .args(["-n", "-E", "env"])
                .output()
                .map(|o| {
                    o.status.success()
                        && String::from_utf8_lossy(&o.stdout).contains(expected.as_str())
                })
                .unwrap_or(false)
        }
    };
}

thread_local! {
    static XDO: RefCell<*mut xdo_t> = RefCell::new({
        let xdo = unsafe { libxdo_sys::xdo_new(std::ptr::null()) };
        if xdo.is_null() {
            log::warn!("Failed to create xdo context, xdo functions will be disabled");
        } else {
            log::info!("xdo context created successfully");
        }
        xdo
    });
    static DISPLAY: RefCell<*mut c_void> = RefCell::new(unsafe { XOpenDisplay(std::ptr::null())});
}

#[link(name = "X11")]
extern "C" {
    fn XOpenDisplay(display_name: *const c_char) -> *mut c_void;

}

#[link(name = "Xfixes")]
extern "C" {
    fn XFixesGetCursorImage(dpy: *mut c_void) -> *const xcb_xfixes_get_cursor_image;
    fn XFree(data: *mut c_void);
}

#[repr(C)]
pub struct xcb_xfixes_get_cursor_image {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub xhot: u16,
    pub yhot: u16,
    pub cursor_serial: c_long,
    pub pixels: *const c_long,
}

#[inline]
pub fn is_headless_allowed() -> bool {
    Config::get_option(OPTION_ALLOW_LINUX_HEADLESS) == "Y"
}

#[inline]
pub fn is_login_screen_wayland() -> bool {
    let values = get_values_of_seat0_with_gdm_wayland(&[0, 2]);
    is_gdm_user(&values[1]) && get_display_server_of_session(&values[0]) == DISPLAY_SERVER_WAYLAND
}

#[inline]
fn sleep_millis(millis: u64) {
    std::thread::sleep(Duration::from_millis(millis));
}

pub fn get_cursor_pos() -> Option<(i32, i32)> {
    let mut res = None;
    XDO.with(|xdo| {
        if let Ok(xdo) = xdo.try_borrow() {
            if xdo.is_null() {
                return;
            }
            let mut x: c_int = 0;
            let mut y: c_int = 0;
            unsafe {
                libxdo_sys::xdo_get_mouse_location(
                    *xdo as *const _,
                    &mut x as _,
                    &mut y as _,
                    std::ptr::null_mut(),
                );
            }
            res = Some((x, y));
        }
    });
    res
}

pub fn set_cursor_pos(x: i32, y: i32) -> bool {
    let mut res = false;
    XDO.with(|xdo| {
        match xdo.try_borrow() {
            Ok(xdo) => {
                if xdo.is_null() {
                    log::debug!("set_cursor_pos: xdo is null");
                    return;
                }
                unsafe {
                    let ret = libxdo_sys::xdo_move_mouse(*xdo as *const _, x, y, 0);
                    if ret != 0 {
                        log::debug!(
                            "set_cursor_pos: xdo_move_mouse failed with code {} for coordinates ({}, {})",
                            ret, x, y
                        );
                    }
                    res = ret == 0;
                }
            }
            Err(_) => {
                log::debug!("set_cursor_pos: failed to borrow xdo");
            }
        }
    });
    res
}

pub fn clip_cursor(_rect: Option<(i32, i32, i32, i32)>) -> bool {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        log::debug!("clip_cursor called (no-op on Linux, this message is logged only once)");
    }
    true
}

pub fn reset_input_cache() {}

pub fn get_focused_display(displays: Vec<DisplayInfo>) -> Option<usize> {
    let mut res = None;
    XDO.with(|xdo| {
        if let Ok(xdo) = xdo.try_borrow() {
            if xdo.is_null() {
                return;
            }
            let mut x: c_int = 0;
            let mut y: c_int = 0;
            let mut width: c_uint = 0;
            let mut height: c_uint = 0;
            let mut window: Window = 0;

            unsafe {
                if libxdo_sys::xdo_get_active_window(*xdo as *const _, &mut window) != 0 {
                    return;
                }
                if libxdo_sys::xdo_get_window_location(
                    *xdo as *const _,
                    window,
                    &mut x as _,
                    &mut y as _,
                    std::ptr::null_mut(),
                ) != 0
                {
                    return;
                }
                if libxdo_sys::xdo_get_window_size(
                    *xdo as *const _,
                    window,
                    &mut width,
                    &mut height,
                ) != 0
                {
                    return;
                }
                let center_x = x + (width / 2) as c_int;
                let center_y = y + (height / 2) as c_int;
                res = displays.iter().position(|d| {
                    center_x >= d.x
                        && center_x < d.x + d.width
                        && center_y >= d.y
                        && center_y < d.y + d.height
                });
            }
        }
    });
    res
}

pub fn get_cursor() -> ResultType<Option<u64>> {
    let mut res = None;
    DISPLAY.with(|conn| {
        if let Ok(d) = conn.try_borrow_mut() {
            if !d.is_null() {
                unsafe {
                    let img = XFixesGetCursorImage(*d);
                    if !img.is_null() {
                        res = Some((*img).cursor_serial as u64);
                        XFree(img as _);
                    }
                }
            }
        }
    });
    Ok(res)
}

pub fn get_cursor_data(hcursor: u64) -> ResultType<CursorData> {
    let mut res = None;
    DISPLAY.with(|conn| {
        if let Ok(ref mut d) = conn.try_borrow_mut() {
            if !d.is_null() {
                unsafe {
                    let img = XFixesGetCursorImage(**d);
                    if !img.is_null() && hcursor == (*img).cursor_serial as u64 {
                        let mut cd: CursorData = Default::default();
                        cd.hotx = (*img).xhot as _;
                        cd.hoty = (*img).yhot as _;
                        cd.width = (*img).width as _;
                        cd.height = (*img).height as _;
                        cd.id = (*img).cursor_serial as _;
                        let pixels =
                            std::slice::from_raw_parts((*img).pixels, (cd.width * cd.height) as _);
                        let mut cd_colors = vec![0_u8; pixels.len() * 4];
                        for y in 0..cd.height {
                            for x in 0..cd.width {
                                let pos = (y * cd.width + x) as usize;
                                let p = pixels[pos];
                                let a = (p >> 24) & 0xff;
                                let r = (p >> 16) & 0xff;
                                let g = (p >> 8) & 0xff;
                                let b = (p >> 0) & 0xff;
                                if a == 0 {
                                    continue;
                                }
                                let pos = pos * 4;
                                cd_colors[pos] = r as _;
                                cd_colors[pos + 1] = g as _;
                                cd_colors[pos + 2] = b as _;
                                cd_colors[pos + 3] = a as _;
                            }
                        }
                        cd.colors = cd_colors.into();
                        res = Some(cd);
                    }
                    if !img.is_null() {
                        XFree(img as _);
                    }
                }
            }
        }
    });
    match res {
        Some(x) => Ok(x),
        _ => bail!("Failed to get cursor image of {}", hcursor),
    }
}

#[cfg(target_os = "linux")]
fn start_uinput_service() {
    use crate::server::uinput::service;
    std::thread::spawn(|| {
        service::start_service_control();
    });
    std::thread::spawn(|| {
        service::start_service_keyboard();
    });
    std::thread::spawn(|| {
        service::start_service_mouse();
    });
}
#[cfg(target_os = "freebsd")]
fn start_uinput_service() {
}

fn suggest_best_term() -> String {
    if is_running_in_tmux() || is_running_in_screen() {
        return TERM_SCREEN_256COLOR.to_string();
    }
    if term_supports_256_colors(TERM_XTERM_256COLOR) {
        return TERM_XTERM_256COLOR.to_string();
    }
    TERM_XTERM.to_string()
}

fn is_running_in_tmux() -> bool {
    std::env::var("TMUX").is_ok()
}

fn is_running_in_screen() -> bool {
    std::env::var("STY").is_ok()
}

fn supports_256_colors(db: &Database) -> bool {
    db.get::<cap::MaxColors>().map_or(false, |n| n.0 >= 256)
}

fn term_supports_256_colors(term: &str) -> bool {
    match term {
        TERM_XTERM_256COLOR => DATABASE_XTERM_256COLOR
            .as_ref()
            .map_or(false, |db| supports_256_colors(db)),
        _ => Database::from_name(term).map_or(false, |db| supports_256_colors(&db)),
    }
}

fn get_cur_term(uid: &str) -> Option<String> {
    if let Ok(cache) = CACHED_TERM.lock() {
        if let Some(ref cached) = *cache {
            if cached == TERM_XTERM_256COLOR {
                return Some(cached.clone());
            }
        }
    }

    if uid.is_empty() {
        return None;
    }

    if let Ok(term) = std::env::var("TERM") {
        if term == TERM_XTERM_256COLOR {
            if let Ok(mut cache) = CACHED_TERM.lock() {
                *cache = Some(term.clone());
            }
            return Some(term);
        }
    }

    let terms = get_all_term_values(uid);

    if terms.iter().any(|t| t == TERM_XTERM_256COLOR) {
        if let Ok(mut cache) = CACHED_TERM.lock() {
            *cache = Some(TERM_XTERM_256COLOR.to_string());
        }
        return Some(TERM_XTERM_256COLOR.to_string());
    }

    let fallback = terms.into_iter().next();
    if let Some(ref term) = fallback {
        log::debug!(
            "TERM_XTERM_256COLOR not found, using fallback TERM: {}",
            term
        );
    }
    fallback
}

fn get_all_term_values(uid: &str) -> Vec<String> {
    let Ok(uid_num) = uid.parse::<u32>() else {
        return Vec::new();
    };

    let shell_pattern = SHELL_PROCESSES
        .iter()
        .map(|p| format!(r"(^|/){p}(\s|$)"))
        .collect::<Vec<_>>()
        .join("|");
    let Ok(re) = Regex::new(&shell_pattern) else {
        return Vec::new();
    };

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };

    let mut terms = Vec::new();

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(pid_str) = file_name.to_str() else {
            continue;
        };
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let proc_path = entry.path();

        if let Ok(meta) = std::fs::metadata(&proc_path) {
            use std::os::unix::fs::MetadataExt;
            if meta.uid() != uid_num {
                continue;
            }
        } else {
            continue;
        }

        let cmdline_path = proc_path.join("cmdline");
        let Ok(cmdline) = std::fs::read(&cmdline_path) else {
            continue;
        };
        let exe_end = cmdline
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(cmdline.len());
        let exe_str = String::from_utf8_lossy(&cmdline[..exe_end]);
        if !re.is_match(&exe_str) {
            continue;
        }

        let environ_path = proc_path.join("environ");
        let Ok(environ) = std::fs::read(&environ_path) else {
            continue;
        };

        for part in environ.split(|&b| b == 0) {
            if part.is_empty() {
                continue;
            }
            if let Some(eq) = part.iter().position(|&b| b == b'=') {
                let key_bytes = &part[..eq];
                if key_bytes == b"TERM" {
                    let val_bytes = &part[eq + 1..];
                    let term = String::from_utf8_lossy(val_bytes).into_owned();
                    if !INVALID_TERM_VALUES.contains(&term.as_str()) && !terms.contains(&term) {
                        if term == TERM_XTERM_256COLOR {
                            return vec![term];
                        }
                        terms.push(term);
                    }
                    break;
                }
            }
        }
    }

    terms
}

#[inline]
fn try_start_server_(desktop: Option<&Desktop>) -> ResultType<Option<Child>> {
    match desktop {
        Some(desktop) => {
            let mut envs = vec![];
            if !desktop.display.is_empty() {
                envs.push(("DISPLAY", desktop.display.clone()));
            }
            if !desktop.xauth.is_empty() {
                envs.push(("XAUTHORITY", desktop.xauth.clone()));
            }
            if !desktop.wl_display.is_empty() {
                envs.push(("WAYLAND_DISPLAY", desktop.wl_display.clone()));
            }
            if !desktop.home.is_empty() {
                envs.push(("HOME", desktop.home.clone()));
            }
            if !desktop.dbus.is_empty() {
                envs.push(("DBUS_SESSION_BUS_ADDRESS", desktop.dbus.clone()));
            }
            envs.push((
                "TERM",
                get_cur_term(&desktop.uid).unwrap_or_else(|| suggest_best_term()),
            ));
            run_as_user(
                vec!["--server"],
                Some((desktop.uid.clone(), desktop.username.clone())),
                envs,
            )
        }
        None => Ok(Some(crate::run_me(vec!["--server"])?)),
    }
}

#[inline]
fn start_server(desktop: Option<&Desktop>, server: &mut Option<Child>) {
    match try_start_server_(desktop) {
        Ok(ps) => *server = ps,
        Err(err) => {
            log::error!("Failed to start server: {}", err);
        }
    }
}

fn stop_server(server: &mut Option<Child>) {
    if let Some(mut ps) = server.take() {
        allow_err!(ps.kill());
        sleep_millis(30);
        match ps.try_wait() {
            Ok(Some(_status)) => {}
            Ok(None) => {
                let _res = ps.wait();
            }
            Err(e) => log::error!("error attempting to wait: {e}"),
        }
    }
}

fn set_x11_env(desktop: &Desktop) {
    log::info!("DISPLAY: {}", desktop.display);
    log::info!("XAUTHORITY: {}", desktop.xauth);
    if !desktop.display.is_empty() {
        std::env::set_var("DISPLAY", &desktop.display);
    }
    if !desktop.xauth.is_empty() {
        std::env::set_var("XAUTHORITY", &desktop.xauth);
    }
}

#[inline]
fn stop_rustdesk_servers() {
    let _ = run_cmds(&format!(
        r##"ps -ef | grep -E '{} +--server' | awk '{{print $2}}' | xargs -r kill -9"##,
        crate::get_app_name().to_lowercase(),
    ));
}

#[inline]
fn stop_subprocess() {
    let _ = run_cmds(&format!(
        r##"ps -ef | grep '/etc/{}/xorg.conf' | grep -v grep | awk '{{print $2}}' | xargs -r kill -9"##,
        crate::get_app_name().to_lowercase(),
    ));
    let _ = run_cmds(&format!(
        r##"ps -ef | grep -E '{} +--cm-no-ui' | grep -v grep | awk '{{print $2}}' | xargs -r kill -9"##,
        crate::get_app_name().to_lowercase(),
    ));
}

fn should_start_server(
    try_x11: bool,
    is_display_changed: bool,
    uid: &mut String,
    desktop: &Desktop,
    cm0: &mut bool,
    last_restart: &mut Instant,
    server: &mut Option<Child>,
) -> bool {
    let cm = get_cm();
    let mut start_new = false;
    let mut should_kill = false;

    if desktop.is_headless() {
        if !uid.is_empty() {
            *uid = "".to_owned();
            should_kill = true;
        }
    } else if is_display_changed || desktop.uid != *uid && !desktop.uid.is_empty() {
        *uid = desktop.uid.clone();
        if try_x11 {
            set_x11_env(&desktop);
        }
        should_kill = true;
    }

    if !should_kill
        && !cm
        && ((*cm0 && last_restart.elapsed().as_secs() > 60)
            || last_restart.elapsed().as_secs() > 3600)
    {
        should_kill = true;
        log::info!("restart server");
    }

    if should_kill {
        if let Some(ps) = server.as_mut() {
            allow_err!(ps.kill());
            sleep_millis(30);
            *last_restart = Instant::now();
        }
    }

    if let Some(ps) = server.as_mut() {
        match ps.try_wait() {
            Ok(Some(_)) => {
                *server = None;
                start_new = true;
            }
            _ => {}
        }
    } else {
        start_new = true;
    }
    *cm0 = cm;
    start_new
}

fn force_stop_server() {
    stop_rustdesk_servers();
    sleep_millis(super::SERVICE_INTERVAL);
}

pub fn start_os_service() {
    check_if_stop_service();
    stop_rustdesk_servers();
    stop_subprocess();
    start_uinput_service();

    std::thread::spawn(|| {
        allow_err!(crate::ipc::start(crate::POSTFIX_SERVICE));
    });

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    let (mut display, mut xauth): (String, String) = ("".to_owned(), "".to_owned());
    let mut desktop = Desktop::default();
    let mut sid = "".to_owned();
    let mut uid = "".to_owned();
    let mut server: Option<Child> = None;
    let mut user_server: Option<Child> = None;
    if let Err(err) = ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    }) {
        println!("Failed to set Ctrl-C handler: {}", err);
    }

    let mut cm0 = false;
    let mut last_restart = Instant::now();
    while running.load(Ordering::SeqCst) {
        desktop.refresh();

        if desktop.username == "root" || desktop.is_login_wayland() {
            stop_server(&mut user_server);
            if should_start_server(
                true,
                false,
                &mut uid,
                &desktop,
                &mut cm0,
                &mut last_restart,
                &mut server,
            ) {
                stop_subprocess();
                force_stop_server();
                start_server(None, &mut server);
            }
        } else if desktop.username != "" {
            stop_server(&mut server);

            let is_display_changed = desktop.display != display || desktop.xauth != xauth;
            display = desktop.display.clone();
            xauth = desktop.xauth.clone();

            if should_start_server(
                !desktop.is_wayland(),
                is_display_changed,
                &mut uid,
                &desktop,
                &mut cm0,
                &mut last_restart,
                &mut user_server,
            ) {
                stop_subprocess();
                force_stop_server();
                start_server(Some(&desktop), &mut user_server);
            }
        } else {
            force_stop_server();
            stop_server(&mut user_server);
            stop_server(&mut server);
        }

        let keeps_headless = sid.is_empty() && desktop.is_headless();
        let keeps_session = sid == desktop.sid;
        if keeps_headless || keeps_session {
            sleep_millis(500);
        } else {
            sleep_millis(super::SERVICE_INTERVAL);
        }
        if !desktop.is_headless() {
            sid = desktop.sid.clone();
        }
    }

    if let Some(ps) = user_server.take().as_mut() {
        allow_err!(ps.kill());
    }
    if let Some(ps) = server.take().as_mut() {
        allow_err!(ps.kill());
    }
    log::info!("Exit");
}

#[inline]
pub fn get_active_user_id_name() -> (String, String) {
    #[cfg(target_os = "freebsd")]
    {
        if let Ok(output) = Command::new("w").arg("-h").output() {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 && (parts[1].starts_with("tty") || parts[1].starts_with(":")) {
                    let username = parts[0].to_string();
                    if let Ok(id_out) = Command::new("id").arg("-u").arg(&username).output() {
                        let uid = String::from_utf8_lossy(&id_out.stdout).trim().to_string();
                        if !uid.is_empty() {
                            return (uid, username);
                        }
                    }
                }
            }
        }
        let uid = unsafe { hbb_common::libc::getuid() }.to_string();
        let username = crate::username();
        (uid, username)
    }
    #[cfg(not(target_os = "freebsd"))]
    {
        let vec_id_name = get_values_of_seat0(&[1, 2]);
        (vec_id_name[0].clone(), vec_id_name[1].clone())
    }
}

#[inline]
pub fn get_active_userid() -> String {
    #[cfg(target_os = "freebsd")]
    {
        get_active_user_id_name().0
    }
    #[cfg(not(target_os = "freebsd"))]
    {
        get_values_of_seat0(&[1])[0].clone()
    }
}

fn get_cm() -> bool {
    if let Ok(output) = Command::new(CMD_PS.as_str()).args(vec!["aux"]).output() {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if line.contains(&format!(
                "{} --cm",
                std::env::current_exe()
                    .unwrap_or("".into())
                    .to_string_lossy()
            )) {
                return true;
            }
        }
    }
    false
}

pub fn is_login_wayland() -> bool {
    let files = ["/etc/gdm3/custom.conf", "/etc/gdm/custom.conf"];
    match (
        Regex::new(r"# *WaylandEnable *= *false"),
        Regex::new(r"WaylandEnable *= *true"),
    ) {
        (Ok(pat1), Ok(pat2)) => {
            for file in files {
                if let Ok(contents) = std::fs::read_to_string(file) {
                    return pat1.is_match(&contents) || pat2.is_match(&contents);
                }
            }
        }
        _ => {}
    }
    false
}

#[inline]
pub fn current_is_wayland() -> bool {
    return is_desktop_wayland() && unsafe { UNMODIFIED };
}

fn _get_display_manager() -> String {
    if let Ok(x) = std::fs::read_to_string("/etc/X11/default-display-manager") {
        if let Some(x) = x.split("/").last() {
            return x.to_owned();
        }
    }
    "gdm3".to_owned()
}

#[inline]
pub fn get_active_username() -> String {
    #[cfg(target_os = "freebsd")]
    {
        get_active_user_id_name().1
    }
    #[cfg(not(target_os = "freebsd"))]
    {
        get_values_of_seat0(&[2])[0].clone()
    }
}

pub fn get_user_home_by_name(username: &str) -> Option<PathBuf> {
    return match get_user_by_name(username) {
        None => None,
        Some(user) => {
            let home = user.home_dir();
            if Path::is_dir(home) {
                Some(PathBuf::from(home))
            } else {
                None
            }
        }
    };
}

pub fn get_active_user_home() -> Option<PathBuf> {
    let username = get_active_username();
    if !username.is_empty() {
        match get_user_home_by_name(&username) {
            None => {
                let home = PathBuf::from(format!("/home/{}", username));
                if home.exists() {
                    return Some(home);
                }
            }
            Some(home) => {
                return Some(home);
            }
        }
    }
    None
}

pub fn get_env_var(k: &str) -> String {
    match std::env::var(k) {
        Ok(v) => v,
        Err(_e) => "".to_owned(),
    }
}

fn is_flatpak() -> bool {
    std::path::PathBuf::from("/.flatpak-info").exists()
}

pub fn is_prelogin() -> bool {
    if is_flatpak() {
        return false;
    }
    let name = get_active_username();
    if let Ok(res) = run_cmds(&format!("getent passwd {}", name)) {
        return res.contains("/bin/false") || res.contains("/usr/sbin/nologin");
    }
    false
}

pub fn is_locked() -> bool {
    if is_prelogin() {
        return false;
    }

    let values = get_values_of_seat0(&[0]);
    if values.is_empty() {
        log::debug!("Failed to check is locked, values vector is empty.");
        return false;
    }
    let session = &values[0];
    if session.is_empty() {
        log::debug!("Failed to check is locked, session is empty.");
        return false;
    }
    is_session_locked(session)
}

pub fn is_root() -> bool {
    crate::username() == "root"
}

fn is_opensuse() -> bool {
    if let Ok(res) = run_cmds("cat /etc/os-release | grep opensuse") {
        if !res.is_empty() {
            return true;
        }
    }
    false
}

pub fn run_as_user<I, K, V>(
    arg: Vec<&str>,
    user: Option<(String, String)>,
    envs: I,
) -> ResultType<Option<Child>>
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let (uid, username) = match user {
        Some(id_name) => id_name,
        None => get_active_user_id_name(),
    };
    let cmd = std::env::current_exe()?;
    if uid.is_empty() {
        bail!("No valid uid");
    }

    #[cfg(target_os = "freebsd")]
    if uid == "0" || username == "root" {
        let mut child_cmd = Command::new(&cmd);
        child_cmd.args(&arg);
        if let Ok(display) = std::env::var("DISPLAY") {
            child_cmd.env("DISPLAY", display);
        }
        if let Ok(xauth) = std::env::var("XAUTHORITY") {
            child_cmd.env("XAUTHORITY", xauth);
        }
        child_cmd.env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));
        for (k, v) in envs {
            child_cmd.env(k, v);
        }
        let task = child_cmd.spawn()?;
        return Ok(Some(task));
    }

    let xdg = &format!("XDG_RUNTIME_DIR=/run/user/{uid}");
    if *SUDO_E_PRESERVES_ENV {
        let mut args = vec![xdg, "-u", &username, cmd.to_str().unwrap_or("")];
        args.append(&mut arg.clone());
        args.insert(0, "-E");
        let task = Command::new("sudo").envs(envs).args(args).spawn()?;
        Ok(Some(task))
    } else {
        fn is_valid_env_key(key: &str) -> bool {
            let mut it = key.chars();
            match it.next() {
                Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
                _ => return false,
            }
            it.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }

        let mut sudo = Command::new("sudo");
        sudo.arg("-u").arg(&username).arg("--").arg("env").arg(xdg);

        for (k, v) in envs {
            let key = k.as_ref().to_string_lossy();
            if !is_valid_env_key(&key) {
                log::warn!("Skipping environment variable with invalid key: '{}'. Only [A-Za-z_][A-Za-z0-9_]* are allowed in sudo context.", key);
                continue;
            }
            let mut arg = OsString::from(&*key);
            arg.push("=");
            arg.push(v.as_ref());
            sudo.arg(arg);
        }

        sudo.arg(cmd).args(arg);
        let task = sudo.spawn()?;
        Ok(Some(task))
    }
}

pub fn get_pa_monitor() -> String {
    get_pa_sources()
        .drain(..)
        .map(|x| x.0)
        .filter(|x| x.contains("monitor"))
        .next()
        .unwrap_or("".to_owned())
}

pub fn get_pa_source_name(desc: &str) -> String {
    get_pa_sources()
        .drain(..)
        .filter(|x| x.1 == desc)
        .map(|x| x.0)
        .next()
        .unwrap_or("".to_owned())
}

#[cfg(target_os = "linux")]
pub fn get_pa_sources() -> Vec<(String, String)> {
    use pulsectl::controllers::*;
    let mut out = Vec::new();
    match SourceController::create() {
        Ok(mut handler) => {
            if let Ok(devices) = handler.list_devices() {
                for dev in devices.clone() {
                    out.push((
                        dev.name.unwrap_or("".to_owned()),
                        dev.description.unwrap_or("".to_owned()),
                    ));
                }
            }
        }
        Err(err) => {
            log::error!("Failed to get_pa_sources: {:?}", err);
        }
    }
    out
}
#[cfg(target_os = "freebsd")]
pub fn get_pa_sources() -> Vec<(String, String)> {
    Vec::new()
}

#[cfg(target_os = "linux")]
pub fn get_default_pa_source() -> Option<(String, String)> {
    use pulsectl::controllers::*;
    match SourceController::create() {
        Ok(mut handler) => {
            if let Ok(dev) = handler.get_default_device() {
                return Some((
                    dev.name.unwrap_or("".to_owned()),
                    dev.description.unwrap_or("".to_owned()),
                ));
            }
        }
        Err(err) => {
            log::error!("Failed to get_pa_source: {:?}", err);
        }
    }
    None
}
#[cfg(target_os = "freebsd")]
pub fn get_default_pa_source() -> Option<(String, String)> {
    None
}

pub fn lock_screen() {
    Command::new("xdg-screensaver").arg("lock").spawn().ok();
}

pub fn toggle_blank_screen(_v: bool) {
}

pub fn block_input(_v: bool) -> (bool, String) {
    (true, "".to_owned())
}

pub fn is_installed() -> bool {
    if let Ok(p) = std::env::current_exe() {
        p.to_str().unwrap_or_default().starts_with("/usr")
            || p.to_str().unwrap_or_default().starts_with("/nix/store")
    } else {
        false
    }
}

fn get_envs<'a>(
    uid: &str,
    process_pat: &str,
    names: &[&'a str],
) -> std::collections::HashMap<&'a str, String> {
    debug_assert!(
        names.len() <= 64,
        "get_envs: names.len() must be <= 64, got {}",
        names.len()
    );

    let empty: std::collections::HashMap<&'a str, String> =
        names.iter().map(|&n| (n, String::new())).collect();

    let Ok(uid_num) = uid.parse::<u32>() else {
        return empty;
    };
    let Ok(re) = Regex::new(process_pat) else {
        return empty;
    };

    let name_indices: std::collections::HashMap<&'a str, usize> =
        names.iter().enumerate().map(|(i, &n)| (n, i)).collect();

    let mut best = empty.clone();
    let mut best_count = 0usize;
    let mut best_mask: u64 = 0;

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return best;
    };

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(pid_str) = file_name.to_str() else {
            continue;
        };
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let proc_path = entry.path();

        if let Ok(meta) = std::fs::metadata(&proc_path) {
            use std::os::unix::fs::MetadataExt;
            if meta.uid() != uid_num {
                continue;
            }
        } else {
            continue;
        }

        let cmdline_path = proc_path.join("cmdline");
        let Ok(cmdline) = std::fs::read(&cmdline_path) else {
            continue;
        };
        let cmdline_str = String::from_utf8_lossy(&cmdline).replace('\0', " ");
        if !re.is_match(&cmdline_str) {
            continue;
        }

        let environ_path = proc_path.join("environ");
        let Ok(environ) = std::fs::read(&environ_path) else {
            continue;
        };

        let mut found = empty.clone();
        let mut found_count = 0usize;
        let mut found_mask: u64 = 0;

        for part in environ.split(|&b| b == 0) {
            if part.is_empty() {
                continue;
            }
            let Some(eq) = part.iter().position(|&b| b == b'=') else {
                continue;
            };
            let key_bytes = &part[..eq];
            let val_bytes = &part[eq + 1..];

            let Ok(key) = std::str::from_utf8(key_bytes) else {
                continue;
            };
            if let Some(slot) = found.get_mut(key) {
                if slot.is_empty() {
                    *slot = String::from_utf8_lossy(val_bytes).into_owned();
                    found_count += 1;

                    if let Some(&idx) = name_indices.get(key) {
                        let total = names.len();
                        if total <= 64 {
                            let bit = 1u64 << (total - 1 - idx);
                            found_mask |= bit;
                        }
                    }

                    if found_count == names.len() {
                        return found;
                    }
                }
            }
        }

        if found_count > best_count || (found_count == best_count && found_mask > best_mask) {
            best = found;
            best_count = found_count;
            best_mask = found_mask;
        }
    }

    best
}

#[inline]
fn get_env(name: &str, uid: &str, process: &str) -> String {
    let cmd = format!("ps -u {} -f | grep -E '{}' | grep -v 'grep' | tail -1 | awk '{{print $2}}' | xargs -I__ cat /proc/__/environ 2>/dev/null | tr '\\0' '\\n' | grep '^{}=' | tail -1 | sed 's/{}=//g'", uid, process, name, name);
    if let Ok(x) = run_cmds(&cmd) {
        x.trim_end().to_string()
    } else {
        "".to_owned()
    }
}

#[inline]
fn get_env_from_pid(name: &str, pid: &str) -> String {
    let cmd = format!("cat /proc/{}/environ 2>/dev/null | tr '\\0' '\\n' | grep '^{}=' | tail -1 | sed 's/{}=//g'", pid, name, name);
    if let Ok(x) = run_cmds(&cmd) {
        x.trim_end().to_string()
    } else {
        "".to_owned()
    }
}

#[link(name = "gtk-3")]
extern "C" {
    fn gtk_main_quit();
}

pub fn quit_gui() {
    std::process::exit(0);
}

pub fn check_super_user_permission() -> ResultType<bool> {
    gtk_sudo::run(vec!["echo"])?;
    Ok(true)
}

type GtkSettingsPtr = *mut c_void;
type GObjectPtr = *mut c_void;
#[link(name = "gtk-3")]
extern "C" {
    fn gtk_settings_get_default() -> GtkSettingsPtr;
}

#[link(name = "gobject-2.0")]
extern "C" {
    fn g_object_get(object: GObjectPtr, first_property_name: *const c_char, ...);
}

pub fn get_double_click_time() -> u32 {
    unsafe {
        let mut double_click_time = 0u32;
        let Ok(property) = std::ffi::CString::new("gtk-double-click-time") else {
            return 0;
        };
        let settings = gtk_settings_get_default();
        g_object_get(
            settings,
            property.as_ptr(),
            &mut double_click_time as *mut u32,
            0 as *const c_void,
        );
        double_click_time
    }
}

#[inline]
fn get_width_height_from_captures<'t>(caps: &Captures<'t>) -> Option<(i32, i32)> {
    match (caps.name("width"), caps.name("height")) {
        (Some(width), Some(height)) => {
            match (
                width.as_str().parse::<i32>(),
                height.as_str().parse::<i32>(),
            ) {
                (Ok(width), Ok(height)) => {
                    return Some((width, height));
                }
                _ => {}
            }
        }
        _ => {}
    }
    None
}

#[inline]
fn get_xrandr_conn_pat(name: &str) -> String {
    format!(
        r"{}\s+connected.+?(?P<width>\d+)x(?P<height>\d+)\+(?P<x>\d+)\+(?P<y>\d+).*?\n",
        name
    )
}

pub fn resolutions(name: &str) -> Vec<Resolution> {
    let resolutions_pat = r"(?P<resolutions>(\s*\d+x\d+\s+\d+.*\n)+)";
    let connected_pat = get_xrandr_conn_pat(name);
    let mut v = vec![];
    if let Ok(re) = Regex::new(&format!("{}{}", connected_pat, resolutions_pat)) {
        match run_cmds("xrandr --query | tr -s ' '") {
            Ok(xrandr_output) => {
                if let Some(caps) = re.captures(&xrandr_output) {
                    if let Some(resolutions) = caps.name("resolutions") {
                        let resolution_pat =
                            r"\s*(?P<width>\d+)x(?P<height>\d+)\s+(?P<rates>(\d+\.\d+\D*)+)\s*\n";
                        let Ok(resolution_re) = Regex::new(&format!(r"{}", resolution_pat)) else {
                            log::error!("Regex new failed");
                            return vec![];
                        };
                        for resolution_caps in resolution_re.captures_iter(resolutions.as_str()) {
                            if let Some((width, height)) =
                                get_width_height_from_captures(&resolution_caps)
                            {
                                let resolution = Resolution {
                                    width,
                                    height,
                                    ..Default::default()
                                };
                                if !v.contains(&resolution) {
                                    v.push(resolution);
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => log::error!("Failed to run xrandr query, {}", e),
        }
    }

    v
}

pub fn current_resolution(name: &str) -> ResultType<Resolution> {
    let xrandr_output = run_cmds("xrandr --query | tr -s ' '")?;
    let re = Regex::new(&get_xrandr_conn_pat(name))?;
    if let Some(caps) = re.captures(&xrandr_output) {
        if let Some((width, height)) = get_width_height_from_captures(&caps) {
            return Ok(Resolution {
                width,
                height,
                ..Default::default()
            });
        }
    }
    bail!("Failed to find current resolution for {}", name);
}

pub fn change_resolution_directly(name: &str, width: usize, height: usize) -> ResultType<()> {
    Command::new("xrandr")
        .args(vec![
            "--output",
            name,
            "--mode",
            &format!("{}x{}", width, height),
        ])
        .spawn()?;
    Ok(())
}

#[inline]
pub fn is_xwayland_running() -> bool {
    if let Ok(output) = run_cmds("pgrep -a Xwayland") {
        return output.contains("Xwayland");
    }
    false
}

mod desktop {
    use super::*;

    pub const XFCE4_PANEL: &str = "xfce4-panel";
    pub const SDDM_GREETER: &str = "sddm-greeter";

    const XDG_DESKTOP_PORTAL: &str = "xdg-desktop-portal";
    const XWAYLAND: &str = "Xwayland";
    const IBUS_DAEMON: &str = "ibus-daemon";
    const PLASMA_KDED: &str = "kded[0-9]+";
    const GNOME_GOA_DAEMON: &str = "goa-daemon";

    const ENV_KEY_DISPLAY: &str = "DISPLAY";
    const ENV_KEY_XAUTHORITY: &str = "XAUTHORITY";
    const ENV_KEY_WAYLAND_DISPLAY: &str = "WAYLAND_DISPLAY";
    const ENV_KEY_DBUS_SESSION_BUS_ADDRESS: &str = "DBUS_SESSION_BUS_ADDRESS";

    #[derive(Debug, Clone, Default)]
    pub struct Desktop {
        pub sid: String,
        pub username: String,
        pub uid: String,
        pub protocol: String,
        pub display: String,
        pub xauth: String,
        pub home: String,
        pub dbus: String,
        pub is_rustdesk_subprocess: bool,
        pub wl_display: String,
    }

    impl Desktop {
        #[inline]
        pub fn is_wayland(&self) -> bool {
            self.protocol == DISPLAY_SERVER_WAYLAND
        }

        #[inline]
        pub fn is_login_wayland(&self) -> bool {
            super::is_gdm_user(&self.username) && self.protocol == DISPLAY_SERVER_WAYLAND
        }

        #[inline]
        pub fn is_headless(&self) -> bool {
            self.sid.is_empty() || self.is_rustdesk_subprocess
        }

        fn get_display_xauth_wayland(&mut self) {
            for _ in 1..=10 {
                let mut envs = get_envs(
                    &self.uid,
                    XDG_DESKTOP_PORTAL,
                    &[
                        ENV_KEY_WAYLAND_DISPLAY,
                        ENV_KEY_DBUS_SESSION_BUS_ADDRESS,
                        ENV_KEY_DISPLAY,
                        ENV_KEY_XAUTHORITY,
                    ],
                );
                self.display = envs.remove(ENV_KEY_DISPLAY).unwrap_or_default();
                self.xauth = envs.remove(ENV_KEY_XAUTHORITY).unwrap_or_default();
                self.wl_display = envs.remove(ENV_KEY_WAYLAND_DISPLAY).unwrap_or_default();
                self.dbus = envs
                    .remove(ENV_KEY_DBUS_SESSION_BUS_ADDRESS)
                    .unwrap_or_default();
                let has_wayland = !self.wl_display.is_empty();
                let has_dbus = !self.dbus.is_empty();
                if has_wayland && has_dbus {
                    return;
                }
                sleep_millis(300);
            }
        }

        fn get_display_xauth_xwayland(&mut self) {
            let tray = format!("{} +--tray", crate::get_app_name().to_lowercase());
            for _ in 1..=10 {
                let display_proc = vec![
                    XDG_DESKTOP_PORTAL,
                    XWAYLAND,
                    IBUS_DAEMON,
                    GNOME_GOA_DAEMON,
                    PLASMA_KDED,
                    tray.as_str(),
                ];
                for proc in display_proc {
                    self.display = get_env(ENV_KEY_DISPLAY, &self.uid, proc);
                    self.xauth = get_env(ENV_KEY_XAUTHORITY, &self.uid, proc);
                    self.wl_display = get_env(ENV_KEY_WAYLAND_DISPLAY, &self.uid, proc);
                    self.dbus = get_env(ENV_KEY_DBUS_SESSION_BUS_ADDRESS, &self.uid, proc);
                    if !self.display.is_empty() && !self.xauth.is_empty() {
                        return;
                    }
                }
                sleep_millis(300);
            }
        }

        fn get_display_x11(&mut self) {
            for _ in 1..=10 {
                let display_proc = vec![
                    XWAYLAND,
                    IBUS_DAEMON,
                    GNOME_GOA_DAEMON,
                    PLASMA_KDED,
                    XFCE4_PANEL,
                    SDDM_GREETER,
                ];
                for proc in display_proc {
                    self.display = get_env(ENV_KEY_DISPLAY, &self.uid, proc);
                    if !self.display.is_empty() {
                        break;
                    }
                }
                if !self.display.is_empty() {
                    break;
                }
                sleep_millis(300);
            }

            if self.display.is_empty() {
                self.display = Self::get_display_by_user(&self.username);
            }
            if self.display.is_empty() {
                self.display = ":0".to_owned();
            }
            self.display = self
                .display
                .replace(&hbb_common::whoami::hostname(), "")
                .replace("localhost", "");
        }

        fn get_home(&mut self) {
            self.home = "".to_string();

            let cmd = format!(
                "getent passwd '{}' | awk -F':' '{{print $6}}'",
                &self.username
            );
            self.home = run_cmds_trim_newline(&cmd).unwrap_or(format!("/home/{}", &self.username));
        }

        fn get_xauth_from_xorg(&mut self) {
            if let Ok(output) = run_cmds(&format!(
                "ps -u {} -f | grep 'Xorg' | grep -v 'grep'",
                &self.uid
            )) {
                for line in output.lines() {
                    let mut auth_found = false;

                    for v in line.split_whitespace() {
                        if v == "-auth" {
                            auth_found = true;
                        } else if auth_found {
                            if std::path::Path::new(v).is_absolute()
                                && std::path::Path::new(v).exists()
                            {
                                self.xauth = v.to_string();
                            } else {
                                if let Some(pid) = line.split_whitespace().nth(1) {
                                    let mut base_dir: String = String::from("/home");
                                    let home_dir = get_env_from_pid("HOME", pid);
                                    if home_dir.is_empty() {
                                        if let Some(home) = get_user_home_by_name(&self.username) {
                                            base_dir = home.as_path().to_string_lossy().to_string();
                                        };
                                    } else {
                                        base_dir = home_dir;
                                    }
                                    if Path::new(&base_dir).exists() {
                                        self.xauth = format!("{}/{}", base_dir, v);
                                    };
                                } else {
                                }
                            }
                            return;
                        }
                    }
                }
            }
        }

        fn get_xauth_x11(&mut self) {
            let tray = format!("{} +--tray", crate::get_app_name().to_lowercase());
            for _ in 1..=10 {
                let display_proc = vec![
                    XWAYLAND,
                    IBUS_DAEMON,
                    GNOME_GOA_DAEMON,
                    PLASMA_KDED,
                    XFCE4_PANEL,
                    SDDM_GREETER,
                    tray.as_str(),
                ];
                for proc in display_proc {
                    self.xauth = get_env("XAUTHORITY", &self.uid, proc);
                    if !self.xauth.is_empty() {
                        break;
                    }
                }
                if !self.xauth.is_empty() {
                    break;
                }
                sleep_millis(300);
            }

            if self.xauth.is_empty() {
                self.get_xauth_from_xorg();
            }

            if self.xauth.is_empty() {
                let gdm = format!("/run/user/{}/gdm/Xauthority", self.uid);
                self.xauth = if std::path::Path::new(&gdm).exists() {
                    gdm
                } else {
                    let username = &self.username;
                    match get_user_home_by_name(username) {
                        None => {
                            if username == "root" {
                                format!("/{}/.Xauthority", username)
                            } else {
                                let tmp = format!("/home/{}/.Xauthority", username);
                                if std::path::Path::new(&tmp).exists() {
                                    tmp
                                } else {
                                    format!("/var/lib/{}/.Xauthority", username)
                                }
                            }
                        }
                        Some(home) => {
                            format!(
                                "{}/.Xauthority",
                                home.as_path().to_string_lossy().to_string()
                            )
                        }
                    }
                };
            }
        }

        fn get_display_by_user(user: &str) -> String {
            if let Ok(output) = std::process::Command::new("w").arg(&user).output() {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    let mut iter = line.split_whitespace();
                    let b = iter.nth(2);
                    if let Some(b) = b {
                        if b.starts_with(":") {
                            return b.to_owned();
                        }
                    }
                }
            }
            let mut last = "".to_owned();
            if let Ok(output) = std::process::Command::new("ls")
                .args(vec!["-l", "/tmp/.X11-unix/"])
                .output()
            {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    let mut iter = line.split_whitespace();
                    let user_field = iter.nth(2);
                    if let Some(x) = iter.last() {
                        if x.starts_with("X") {
                            last = x.replace("X", ":").to_owned();
                            if user_field == Some(&user) {
                                return last;
                            }
                        }
                    }
                }
            }
            last
        }

        fn set_is_subprocess(&mut self) {
            self.is_rustdesk_subprocess = false;
            let cmd = format!(
                "ps -ef | grep '{}/xorg.conf' | grep -v grep | wc -l",
                crate::get_app_name().to_lowercase()
            );
            if let Ok(res) = run_cmds(&cmd) {
                if res.trim() != "0" {
                    self.is_rustdesk_subprocess = true;
                }
            }
        }

        pub fn refresh(&mut self) {
            #[cfg(target_os = "freebsd")]
            {
                if self.username.is_empty() {
                    let (uid, username) = get_active_user_id_name();
                    if username.is_empty() {
                        *self = Self::default();
                        self.is_rustdesk_subprocess = false;
                        return;
                    }
                    self.uid = uid;
                    self.username = username;
                    self.sid = "freebsd".to_owned();
                    self.protocol = "x11".to_owned();
                    self.get_home();
                }
                if self.display.is_empty() {
                    if let Ok(d) = std::env::var("DISPLAY") {
                        self.display = d;
                    } else {
                        if let Ok(output) = Command::new("ls").arg("/tmp/.X11-unix/").output() {
                            for entry in String::from_utf8_lossy(&output.stdout).lines() {
                                if entry.starts_with("X") {
                                    self.display = entry.replace("X", ":").to_owned();
                                    break;
                                }
                            }
                        }
                    }
                }
                if self.xauth.is_empty() {
                    if let Ok(xa) = std::env::var("XAUTHORITY") {
                        self.xauth = xa;
                    }
                }
                return;
            }
            #[cfg(not(target_os = "freebsd"))]
            {
                if !self.sid.is_empty() && is_active_and_seat0(&self.sid) {
                    if is_xwayland_running() && !self.is_login_wayland() {
                        self.get_display_xauth_xwayland();
                        self.is_rustdesk_subprocess = false;
                    } else if self.is_wayland() {
                        self.get_display_xauth_wayland();
                    }
                    return;
                }

                let seat0_values = get_values_of_seat0_with_gdm_wayland(&[0, 1, 2]);
                if seat0_values[0].is_empty() {
                    *self = Self::default();
                    self.is_rustdesk_subprocess = false;
                    return;
                }

                self.sid = seat0_values[0].clone();
                self.uid = seat0_values[1].clone();
                self.username = seat0_values[2].clone();
                self.protocol = get_display_server_of_session(&self.sid).into();
                if self.is_login_wayland() {
                    self.display = "".to_owned();
                    self.xauth = "".to_owned();
                    self.is_rustdesk_subprocess = false;
                    return;
                }

                self.get_home();
                if self.is_wayland() {
                    if is_xwayland_running() {
                        self.get_display_xauth_xwayland();
                    } else {
                        self.get_display_xauth_wayland();
                    }
                    self.is_rustdesk_subprocess = false;
                } else {
                    self.get_display_x11();
                    self.get_xauth_x11();
                    self.set_is_subprocess();
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_desktop_env() {
            let mut d = Desktop::default();
            d.refresh();
            if d.username == "root" {
                assert_eq!(d.home, "/root");
            } else {
                if !d.username.is_empty() {
                    let home = super::super::get_env_var("HOME");
                    if !home.is_empty() {
                        assert_eq!(d.home, home);
                    } else {
                    }
                }
            }
        }
    }
}

pub struct WakeLock(Option<keepawake::AwakeHandle>);

impl WakeLock {
    pub fn new(display: bool, idle: bool, sleep: bool) -> Self {
        WakeLock(
            keepawake::Builder::new()
                .display(display)
                .idle(idle)
                .sleep(sleep)
                .create()
                .ok(),
        )
    }
}

fn has_cmd(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .status()
        .map(|x| x.success())
        .unwrap_or_default()
}

pub fn run_cmds_privileged(cmds: &str) -> bool {
    crate::platform::gtk_sudo::run(vec![cmds]).is_ok()
}

pub fn run_me_with(secs: u32) {
    let exe = std::env::current_exe()
        .unwrap_or("".into())
        .to_string_lossy()
        .to_string();
    std::process::Command::new(CMD_SH.as_str())
        .arg("-c")
        .arg(&format!("sleep {secs}; {exe}"))
        .spawn()
        .ok();
}

fn switch_service(stop: bool) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    Config::set_option("stop-service".into(), if stop { "Y" } else { "" }.into());
    if home != "/root" && !Config::get().is_empty() {
        let p = format!(".config/{}", crate::get_app_name().to_lowercase());
        let app_name0 = crate::get_app_name();
        format!("cp -f {home}/{p}/{app_name0}.toml /root/{p}/; cp -f {home}/{p}/{app_name0}2.toml /root/{p}/;")
    } else {
        "".to_owned()
    }
}

pub fn uninstall_service(show_new_window: bool, _: bool) -> bool {
    #[cfg(target_os = "freebsd")]
    {
        log::info!("Uninstalling service (FreeBSD rc.d)...");
        let cp = switch_service(true);
        let app_name = crate::get_app_name().to_lowercase();
        if !run_cmds_privileged(&format!(
            "{cp} service {app_name} stop; sysrc -x {app_name}_enable 2>/dev/null; rm -f /usr/local/etc/rc.d/{app_name};"
        )) {
            Config::set_option("stop-service".into(), "".into());
            return true;
        }
        if show_new_window {
            run_me_with(2);
        }
        std::process::exit(0);
    }
    #[cfg(target_os = "linux")]
    {
        if !has_cmd("systemctl") {
            return false;
        }
        log::info!("Uninstalling service...");
        let cp = switch_service(true);
        let app_name = crate::get_app_name().to_lowercase();
        if !run_cmds_privileged(&format!(
            "{cp} systemctl disable {app_name}; systemctl stop {app_name};"
        )) {
            Config::set_option("stop-service".into(), "".into());
            return true;
        }
        if show_new_window {
            run_me_with(2);
        }
        std::process::exit(0);
    }
}

pub fn install_service() -> bool {
    let _installing = crate::platform::InstallingService::new();
    #[cfg(target_os = "freebsd")]
    {
        log::info!("Installing service (FreeBSD rc.d)...");
        let cp = switch_service(false);
        let app_name = crate::get_app_name().to_lowercase();
        let exe = std::env::current_exe().unwrap_or_default();
        let exe_str = exe.to_string_lossy();
        let rc_script = format!("\
#!/bin/sh\n\
# PROVIDE: {name}\n\
# REQUIRE: DAEMON NETWORKING\n\
# KEYWORD: shutdown\n\
. /etc/rc.subr\n\
name=\"{name}\"\n\
rcvar=\"{name}_enable\"\n\
command=\"{cmd}\"\n\
command_args=\"--service\"\n\
pidfile=\"/var/run/{name}.pid\"\n\
start_cmd=\"{name}_start\"\n\
{name}_start() {{ /usr/sbin/daemon -p /var/run/{name}.pid {cmd} --service; }}\n\
load_rc_config $name\n\
run_rc_command \"$1\"\n",
            name = app_name, cmd = exe_str
        );
        let rc_path = format!("/usr/local/etc/rc.d/{}", app_name);
        let tmp_rc = format!("/tmp/{}.rc", app_name);
        if std::fs::write(&tmp_rc, &rc_script).is_err() {
            log::error!("Failed to write temp rc.d script");
            Config::set_option("stop-service".into(), "Y".into());
            return true;
        }
        if !run_cmds_privileged(&format!(
            "{cp} cp {tmp_rc} {rc_path}; chmod +x {rc_path}; \
             sysrc {app_name}_enable=YES; \
             service {app_name} start;"
        )) {
            Config::set_option("stop-service".into(), "Y".into());
        }
        std::fs::remove_file(&tmp_rc).ok();
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        if !has_cmd("systemctl") {
            return false;
        }
        log::info!("Installing service...");
        let cp = switch_service(false);
        let app_name = crate::get_app_name().to_lowercase();
        if !run_cmds_privileged(&format!(
            "{cp} systemctl enable {app_name}; systemctl start {app_name};"
        )) {
            Config::set_option("stop-service".into(), "Y".into());
        }
        return true;
    }
}

fn check_if_stop_service() {
    if Config::get_option("stop-service".into()) == "Y" {
        let app_name = crate::get_app_name().to_lowercase();
        #[cfg(target_os = "freebsd")]
        allow_err!(run_cmds(&format!(
            "service {app_name} stop; sysrc -x {app_name}_enable 2>/dev/null"
        )));
        #[cfg(target_os = "linux")]
        allow_err!(run_cmds(&format!(
            "systemctl disable {app_name}; systemctl stop {app_name}"
        )));
    }
}

pub fn check_autostart_config() -> ResultType<()> {
    let home = std::env::var("HOME").unwrap_or_default();
    let app_name = crate::get_app_name().to_lowercase();
    let path = format!("{home}/.config/autostart");
    let file = format!("{path}/{app_name}.desktop");
    std::fs::remove_file(&file).ok();
    Ok(())
}

pub struct WallPaperRemover {
    old_path: String,
    old_path_dark: Option<String>,
}

impl WallPaperRemover {
    pub fn new() -> ResultType<Self> {
        let start = std::time::Instant::now();
        let old_path = wallpaper::get().map_err(|e| anyhow!(e.to_string()))?;
        let old_path_dark = wallpaper::get_dark().ok();
        if old_path.is_empty() && old_path_dark.clone().unwrap_or_default().is_empty() {
            bail!("already solid color");
        }
        wallpaper::set_from_path("").map_err(|e| anyhow!(e.to_string()))?;
        wallpaper::set_dark_from_path("").ok();
        log::info!(
            "created wallpaper remover,  old_path: {:?}, old_path_dark: {:?}, elapsed: {:?}",
            old_path,
            old_path_dark,
            start.elapsed(),
        );
        Ok(Self {
            old_path,
            old_path_dark,
        })
    }

    pub fn support() -> bool {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        if wallpaper::gnome::is_compliant(&desktop) || desktop.as_str() == "XFCE" {
            return wallpaper::get().is_ok();
        }
        false
    }
}

impl Drop for WallPaperRemover {
    fn drop(&mut self) {
        allow_err!(wallpaper::set_from_path(&self.old_path).map_err(|e| anyhow!(e.to_string())));
        if let Some(old_path_dark) = &self.old_path_dark {
            allow_err!(wallpaper::set_dark_from_path(old_path_dark.as_str())
                .map_err(|e| anyhow!(e.to_string())));
        }
    }
}

#[inline]
pub fn is_x11() -> bool {
    *IS_X11
}

#[inline]
pub fn is_selinux_enforcing() -> bool {
    match run_cmds("getenforce") {
        Ok(output) => output.trim() == "Enforcing",
        Err(_) => match run_cmds("sestatus") {
            Ok(output) => {
                for line in output.lines() {
                    if line.contains("Current mode:") {
                        return line.contains("enforcing");
                    }
                }
                false
            }
            Err(_) => false,
        },
    }
}
