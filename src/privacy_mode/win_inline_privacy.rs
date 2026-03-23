use super::{PrivacyMode, PrivacyModeState, INVALID_PRIVACY_MODE_CONN_ID};
use hbb_common::{allow_err, bail, log, ResultType};
use std::ffi::CString;
use std::sync::mpsc::channel;
use winapi::{
    shared::{
        minwindef::{HINSTANCE, LPARAM, LRESULT, UINT, WPARAM},
        ntdef::NULL,
        windef::HWND,
    },
    um::{
        libloaderapi::{GetModuleHandleA, GetProcAddress},
        wingdi::*,
        winuser::*,
    },
};

pub(super) const PRIVACY_MODE_IMPL: &str = super::PRIVACY_MODE_IMPL_WIN_INLINE;

const PRIVACY_WINDOW_CLASS: &str = "HopToDeskInlinePrivacyClass\0";
const PRIVACY_WINDOW_TITLE: &str = "HopToDeskInlinePrivacy\0";
const WDA_EXCLUDEFROMCAPTURE: u32 = 0x00000011;
const WM_USER_DESTROY_PRIVACY: u32 = WM_USER + 100;

static PRIVACY_PNG: &[u8] = include_bytes!("../../res/PrivacyMode.png");

struct BitmapData {
    pixels: Vec<u8>, // BGRA pixels, top-down
    width: u32,
    height: u32,
}

/// Decode the embedded PNG into BGRA pixel data for GDI rendering.
fn decode_privacy_image() -> Option<BitmapData> {
    use image::GenericImageView;
    let img = image::load_from_memory(PRIVACY_PNG).ok()?;
    let (w, h) = img.dimensions();
    let rgba = img.to_rgba8();
    // Convert RGBA to BGRA (GDI expects BGR ordering)
    let mut bgra = rgba.into_raw();
    for chunk in bgra.chunks_exact_mut(4) {
        chunk.swap(0, 2); // R <-> B
    }
    Some(BitmapData {
        pixels: bgra,
        width: w,
        height: h,
    })
}

type SetWindowDisplayAffinityFn = unsafe extern "system" fn(HWND, u32) -> i32;

unsafe fn set_display_affinity(hwnd: HWND, affinity: u32) -> ResultType<()> {
    let lib_name = CString::new("user32.dll")?;
    let lib = GetModuleHandleA(lib_name.as_ptr());
    if lib.is_null() {
        bail!("Failed to get handle for user32.dll");
    }
    let func_name = CString::new("SetWindowDisplayAffinity")?;
    let func = GetProcAddress(lib as _, func_name.as_ptr());
    if func.is_null() {
        bail!("SetWindowDisplayAffinity not available");
    }
    let set_affinity: SetWindowDisplayAffinityFn = std::mem::transmute(func);
    if set_affinity(hwnd, affinity) == 0 {
        bail!(
            "SetWindowDisplayAffinity failed: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: UINT,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);

            // Fill background dark first
            let brush = CreateSolidBrush(RGB(24, 24, 24));
            FillRect(hdc, &ps.rcPaint, brush);
            DeleteObject(brush as _);

            // Draw the privacy mode image if available
            #[cfg(target_pointer_width = "32")]
            let ptr = GetWindowLongW(hwnd, GWL_USERDATA) as *const BitmapData;
            #[cfg(target_pointer_width = "64")]
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const BitmapData;

            if !ptr.is_null() {
                let bmp = &*ptr;
                let mut rect = std::mem::zeroed();
                GetClientRect(hwnd, &mut rect);
                let win_w = rect.right - rect.left;
                let win_h = rect.bottom - rect.top;

                let mut bmi: BITMAPINFO = std::mem::zeroed();
                bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                bmi.bmiHeader.biWidth = bmp.width as i32;
                bmi.bmiHeader.biHeight = -(bmp.height as i32); // negative = top-down
                bmi.bmiHeader.biPlanes = 1;
                bmi.bmiHeader.biBitCount = 32;
                bmi.bmiHeader.biCompression = BI_RGB;

                // Center the image maintaining aspect ratio
                let scale_x = win_w as f64 / bmp.width as f64;
                let scale_y = win_h as f64 / bmp.height as f64;
                let scale = if scale_x < scale_y { scale_x } else { scale_y };
                let dst_w = (bmp.width as f64 * scale) as i32;
                let dst_h = (bmp.height as f64 * scale) as i32;
                let dst_x = (win_w - dst_w) / 2;
                let dst_y = (win_h - dst_h) / 2;

                SetStretchBltMode(hdc, HALFTONE as i32);
                StretchDIBits(
                    hdc,
                    dst_x,
                    dst_y,
                    dst_w,
                    dst_h,
                    0,
                    0,
                    bmp.width as i32,
                    bmp.height as i32,
                    bmp.pixels.as_ptr() as _,
                    &bmi,
                    DIB_RGB_COLORS,
                    SRCCOPY,
                );
            }

            EndPaint(hwnd, &ps);
            0
        }
        _ => DefWindowProcA(hwnd, msg, w_param, l_param),
    }
}

unsafe fn create_privacy_window(bmp_data: *const BitmapData) -> ResultType<HWND> {
    let hinstance = GetModuleHandleA(NULL as _) as HINSTANCE;

    let wc = WNDCLASSEXA {
        cbSize: std::mem::size_of::<WNDCLASSEXA>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: NULL as _,
        hCursor: NULL as _,
        hbrBackground: NULL as _,
        lpszMenuName: NULL as _,
        lpszClassName: PRIVACY_WINDOW_CLASS.as_ptr() as _,
        hIconSm: NULL as _,
    };

    let atom = RegisterClassExA(&wc);
    if atom == 0 {
        bail!(
            "Failed to register privacy window class: {}",
            std::io::Error::last_os_error()
        );
    }

    // Virtual screen bounds spanning all monitors
    let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
    let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let cx = GetSystemMetrics(SM_CXVIRTUALSCREEN);
    let cy = GetSystemMetrics(SM_CYVIRTUALSCREEN);

    let ex_style = WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT;
    let style = WS_POPUP | WS_VISIBLE;

    let hwnd = CreateWindowExA(
        ex_style,
        PRIVACY_WINDOW_CLASS.as_ptr() as _,
        PRIVACY_WINDOW_TITLE.as_ptr() as _,
        style,
        x,
        y,
        cx,
        cy,
        NULL as _,
        NULL as _,
        hinstance,
        NULL,
    );

    if hwnd.is_null() {
        UnregisterClassA(PRIVACY_WINDOW_CLASS.as_ptr() as _, hinstance);
        bail!(
            "Failed to create privacy window: {}",
            std::io::Error::last_os_error()
        );
    }

    // Store bitmap data pointer in window user data for WM_PAINT
    #[cfg(target_pointer_width = "32")]
    SetWindowLongW(hwnd, GWL_USERDATA, bmp_data as i32);
    #[cfg(target_pointer_width = "64")]
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, bmp_data as isize);

    // Fully opaque, click-through
    SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA);

    // Exclude from screen capture so remote user sees the real desktop
    if let Err(e) = set_display_affinity(hwnd, WDA_EXCLUDEFROMCAPTURE) {
        log::error!("set_display_affinity failed: {}", e);
        DestroyWindow(hwnd);
        UnregisterClassA(PRIVACY_WINDOW_CLASS.as_ptr() as _, hinstance);
        bail!("SetWindowDisplayAffinity failed: {}", e);
    }

    SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
    );
    ShowWindow(hwnd, SW_SHOW);

    Ok(hwnd)
}

pub struct PrivacyModeImpl {
    impl_key: String,
    conn_id: i32,
    hwnd: u64,
    thread_id: u32,
}

impl PrivacyModeImpl {
    pub fn new(impl_key: &str) -> Self {
        Self {
            impl_key: impl_key.to_owned(),
            conn_id: INVALID_PRIVACY_MODE_CONN_ID,
            hwnd: 0,
            thread_id: 0,
        }
    }
}

impl PrivacyMode for PrivacyModeImpl {
    fn is_async_privacy_mode(&self) -> bool {
        false
    }

    fn init(&self) -> ResultType<()> {
        Ok(())
    }

    fn clear(&mut self) {
        allow_err!(self.turn_off_privacy(self.conn_id, None));
    }

    fn turn_on_privacy(&mut self, conn_id: i32) -> ResultType<bool> {
        if self.check_on_conn_id(conn_id)? {
            log::debug!("Inline privacy mode of conn {} is already on", conn_id);
            return Ok(true);
        }

        // Decode the privacy image before spawning the window thread
        let bmp_data = decode_privacy_image();
        if bmp_data.is_none() {
            log::warn!("Failed to decode PrivacyMode.png, privacy window will show dark background only");
        }

        let (tx, rx) = channel::<Result<(u64, u32), String>>();

        std::thread::spawn(move || unsafe {
            let tid = winapi::um::processthreadsapi::GetCurrentThreadId();
            // Box the bitmap data so it lives for the window's lifetime
            let bmp_box = bmp_data.map(Box::new);
            let bmp_ptr = bmp_box
                .as_ref()
                .map(|b| &**b as *const BitmapData)
                .unwrap_or(std::ptr::null());

            match create_privacy_window(bmp_ptr) {
                Ok(hwnd) => {
                    let _ = tx.send(Ok((hwnd as u64, tid)));
                    // Message loop
                    let mut msg: MSG = std::mem::zeroed();
                    while GetMessageA(&mut msg, NULL as _, 0, 0) != 0 {
                        if msg.message == WM_USER_DESTROY_PRIVACY {
                            break;
                        }
                        TranslateMessage(&msg);
                        DispatchMessageA(&msg);
                    }
                    // Cleanup
                    if IsWindow(hwnd) != 0 {
                        DestroyWindow(hwnd);
                    }
                    let hinstance = GetModuleHandleA(NULL as _);
                    UnregisterClassA(PRIVACY_WINDOW_CLASS.as_ptr() as _, hinstance as _);
                    // bmp_box is dropped here, after the window is destroyed
                    drop(bmp_box);
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                }
            }
        });

        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok((hwnd_val, tid))) => {
                self.hwnd = hwnd_val;
                self.thread_id = tid;
                super::win_input::hook()?;
                self.conn_id = conn_id;
                log::info!("Inline privacy mode enabled for conn {}", conn_id);
                Ok(true)
            }
            Ok(Err(e)) => {
                bail!("Failed to create inline privacy window: {}", e);
            }
            Err(_) => {
                bail!("Timeout waiting for privacy window creation");
            }
        }
    }

    fn turn_off_privacy(
        &mut self,
        conn_id: i32,
        state: Option<PrivacyModeState>,
    ) -> ResultType<()> {
        self.check_off_conn_id(conn_id)?;
        super::win_input::unhook()?;

        if self.thread_id != 0 {
            unsafe {
                PostThreadMessageA(self.thread_id, WM_USER_DESTROY_PRIVACY, 0, 0);
            }
            self.hwnd = 0;
            self.thread_id = 0;
        }

        if self.conn_id != INVALID_PRIVACY_MODE_CONN_ID {
            if let Some(state) = state {
                allow_err!(super::set_privacy_mode_state(
                    conn_id,
                    state,
                    PRIVACY_MODE_IMPL.to_string(),
                    1_000
                ));
            }
            self.conn_id = INVALID_PRIVACY_MODE_CONN_ID;
        }

        Ok(())
    }

    #[inline]
    fn pre_conn_id(&self) -> i32 {
        self.conn_id
    }

    #[inline]
    fn get_impl_key(&self) -> &str {
        &self.impl_key
    }
}

impl Drop for PrivacyModeImpl {
    fn drop(&mut self) {
        if self.conn_id != INVALID_PRIVACY_MODE_CONN_ID {
            allow_err!(self.turn_off_privacy(self.conn_id, None));
        }
    }
}
