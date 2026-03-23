use serde_derive::{Deserialize, Serialize};

mod client;
mod server;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "freebsd"))]
mod win_linux;

#[cfg(target_os = "windows")]
use windows::create_event_loop;
#[cfg(target_os = "macos")]
use macos::create_event_loop;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub use linux::is_supported;

pub use client::*;
pub use server::*;

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t", content = "c")]
pub enum CustomEvent {
    Cursor(Cursor),
    Clear,
    Exit,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "t")]
pub struct Cursor {
    pub x: f32,
    pub y: f32,
    pub argb: u32,
    pub btns: i32,
    pub text: String,
}
