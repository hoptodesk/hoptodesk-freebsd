// FreeBSD version: reuse Linux platform code since FreeBSD is very similar
// (X11, GTK, similar Unix APIs)
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub use linux::*;
#[cfg(target_os = "macos")]
pub use macos::*;
#[cfg(windows)]
pub use windows::*;

#[cfg(windows)]
pub mod windows;

#[cfg(windows)]
pub mod win_device;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "macos")]
pub mod delegate;

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub mod linux;

#[cfg(target_os = "linux")]
pub mod linux_desktop_manager;

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub mod gtk_sudo;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use hbb_common::{message_proto::CursorData, ResultType};
use std::sync::{Arc, Mutex};
#[cfg(not(any(target_os = "macos", target_os = "android", target_os = "ios")))]
pub const SERVICE_INTERVAL: u64 = 300;

lazy_static::lazy_static! {
    static ref INSTALLING_SERVICE: Arc<Mutex<bool>>= Default::default();
}

pub fn installing_service() -> bool {
    INSTALLING_SERVICE.lock().unwrap().clone()
}

pub fn is_xfce() -> bool {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        return std::env::var_os("XDG_CURRENT_DESKTOP") == Some(std::ffi::OsString::from("XFCE"));
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        return false;
    }
}

#[cfg(not(debug_assertions))]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn breakdown_callback() {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    crate::input_service::clear_remapped_keycode();
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    crate::input_service::release_device_modifiers();
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn change_resolution(name: &str, width: usize, height: usize) -> ResultType<()> {
    let cur_resolution = current_resolution(name)?;
    hbb_common::log::info!(
        "change_resolution requested: display='{}', current={}x{}, requested={}x{}",
        name, cur_resolution.width, cur_resolution.height, width, height
    );
    if cur_resolution.width as usize == width && cur_resolution.height as usize == height {
        hbb_common::log::info!("change_resolution skipped: already at requested resolution");
        return Ok(());
    }
    hbb_common::log::warn!("Change resolution of '{}' to ({},{})", name, width, height);
    change_resolution_directly(name, width, height)
}

// Android
#[cfg(target_os = "android")]
pub fn get_active_username() -> String {
    "android".into()
}

// iOS
#[cfg(target_os = "ios")]
pub fn get_active_username() -> String {
    "ios".into()
}

#[cfg(target_os = "ios")]
pub fn resolutions(_name: &str) -> Vec<hbb_common::message_proto::Resolution> {
    vec![]
}

#[cfg(target_os = "android")]
pub const PA_SAMPLE_RATE: u32 = 48000;

#[cfg(target_os = "android")]
#[derive(Default)]
pub struct WakeLock(Option<android_wakelock::WakeLock>);

#[cfg(target_os = "android")]
impl WakeLock {
    pub fn new(tag: &str) -> Self {
        let tag = format!("{}:{tag}", crate::get_app_name());
        match android_wakelock::partial(tag) {
            Ok(lock) => Self(Some(lock)),
            Err(e) => {
                hbb_common::log::error!("Failed to get wakelock: {e:?}");
                Self::default()
            }
        }
    }
}

#[cfg(not(target_os = "ios"))]
pub fn get_wakelock(_display: bool) -> WakeLock {
    hbb_common::log::info!("new wakelock, require display on: {_display}");
    #[cfg(target_os = "android")]
    return crate::platform::WakeLock::new("server");
    #[cfg(not(target_os = "android"))]
    return crate::platform::WakeLock::new(_display, true, false);
}

pub(crate) struct InstallingService;

impl InstallingService {
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "freebsd"))]
    pub fn new() -> Self {
        *INSTALLING_SERVICE.lock().unwrap() = true;
        Self
    }
}

impl Drop for InstallingService {
    fn drop(&mut self) {
        *INSTALLING_SERVICE.lock().unwrap() = false;
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
#[inline]
pub fn is_prelogin() -> bool {
    false
}


#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_cursor_data() {
        for _ in 0..30 {
            if let Some(hc) = get_cursor().unwrap() {
                let cd = get_cursor_data(hc).unwrap();
                repng::encode(
                    std::fs::File::create("cursor.png").unwrap(),
                    cd.width as _,
                    cd.height as _,
                    &cd.colors[..],
                )
                .unwrap();
            }
            #[cfg(target_os = "macos")]
            macos::is_process_trusted(false);
        }
    }
    #[test]
    fn test_get_cursor_pos() {
        for _ in 0..30 {
            assert!(!get_cursor_pos().is_none());
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn test_resolution() {
        let name = r"\\.\DISPLAY1";
        println!("current:{:?}", current_resolution(name));
        println!("change:{:?}", change_resolution(name, 2880, 1800));
        println!("resolutions:{:?}", resolutions(name));
    }
}
