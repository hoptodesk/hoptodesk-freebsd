use crate::ios::ffi::*;
use crate::{Frame, Pixfmt};
use lazy_static::lazy_static;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{io, time::Duration};

static CAPTURER_FRAME_LOG_COUNTER: AtomicU64 = AtomicU64::new(0);

lazy_static! {
    pub(crate) static ref SCREEN_SIZE: Mutex<(u16, u16, u16)> = Mutex::new((0, 0, 0));
}

pub struct Capturer {
    display: Display,
    rgba: Vec<u8>,
    saved_raw_data: Vec<u8>,
}

impl Capturer {
    pub fn new(display: Display) -> io::Result<Capturer> {
        Ok(Capturer {
            display,
            rgba: Vec::new(),
            saved_raw_data: Vec::new(),
        })
    }

    pub fn width(&self) -> usize {
        self.display.width() as usize
    }

    pub fn height(&self) -> usize {
        self.display.height() as usize
    }
}

impl crate::TraitCapturer for Capturer {
    fn frame<'a>(&'a mut self, _timeout: Duration) -> io::Result<Frame<'a>> {
        // Update display size from the shared FFI state
        let size = crate::ios::ffi::get_screen_size();
        if size.0 != 0 && size.1 != 0 {
            self.display.rect.w = size.0;
            self.display.rect.h = size.1;
        }

        if self.width() == 0 || self.height() == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        if get_video_raw(&mut self.rgba, &mut self.saved_raw_data).is_some() {
            let count = CAPTURER_FRAME_LOG_COUNTER.fetch_add(1, Ordering::Relaxed);
            if count < 5 || count % 300 == 0 {
                nslog(&format!("[ios_capturer] frame captured: {}x{}, rgba_len={}, count={}",
                    self.width(), self.height(), self.rgba.len(), count));
            }
            Ok(Frame::PixelBuffer(PixelBuffer::new(
                &self.rgba,
                self.width(),
                self.height(),
            )))
        } else {
            Err(io::ErrorKind::WouldBlock.into())
        }
    }
}

pub struct PixelBuffer<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
    stride: Vec<usize>,
}

impl<'a> PixelBuffer<'a> {
    pub fn new(data: &'a [u8], width: usize, height: usize) -> Self {
        let stride0 = if height > 0 { data.len() / height } else { 0 };
        let mut stride = Vec::new();
        stride.push(stride0);
        PixelBuffer {
            data,
            width,
            height,
            stride,
        }
    }
}

impl<'a> crate::TraitPixelBuffer for PixelBuffer<'a> {
    fn data(&self) -> &[u8] {
        self.data
    }

    fn width(&self) -> usize {
        self.width
    }

    fn height(&self) -> usize {
        self.height
    }

    fn stride(&self) -> Vec<usize> {
        self.stride.clone()
    }

    fn pixfmt(&self) -> Pixfmt {
        Pixfmt::BGRA
    }
}

pub struct Display {
    default: bool,
    rect: Rect,
}

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
struct Rect {
    pub x: i16,
    pub y: i16,
    pub w: u16,
    pub h: u16,
}

impl Display {
    pub fn primary() -> io::Result<Display> {
        let size = crate::ios::ffi::get_screen_size();
        Ok(Display {
            default: true,
            rect: Rect {
                x: 0,
                y: 0,
                w: size.0,
                h: size.1,
            },
        })
    }

    pub fn all() -> io::Result<Vec<Display>> {
        Ok(vec![Display::primary()?])
    }

    pub fn width(&self) -> usize {
        self.rect.w as usize
    }

    pub fn height(&self) -> usize {
        self.rect.h as usize
    }

    pub fn origin(&self) -> (i32, i32) {
        let r = self.rect;
        (r.x as _, r.y as _)
    }

    pub fn is_online(&self) -> bool {
        true
    }

    pub fn is_primary(&self) -> bool {
        self.default
    }

    pub fn name(&self) -> String {
        "iOS".into()
    }

    pub fn refresh_size() {
        // Size is updated dynamically via ios_set_screen_size FFI
    }

    pub fn fix_quality() -> u16 {
        let scale = crate::ios::ffi::get_screen_size().2;
        if scale <= 0 {
            1
        } else {
            scale * scale
        }
    }
}

pub fn screen_size() -> (u16, u16, u16) {
    crate::ios::ffi::get_screen_size()
}

pub fn is_start() -> Option<bool> {
    // On iOS, the broadcast extension controls start/stop.
    // If we have a valid screen size, consider it started.
    let size = crate::ios::ffi::get_screen_size();
    Some(size.0 > 0 && size.1 > 0)
}
