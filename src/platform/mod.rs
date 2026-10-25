use std::path::PathBuf;

#[cfg(target_os = "macos")]
mod alloc;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use alloc::LargeAllocator;
#[cfg(target_os = "macos")]
pub use macos::{appearance_preference_path, disk_space, reveal_item};

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub(crate) use windows::volume_root;
#[cfg(target_os = "windows")]
pub use windows::{appearance_preference_path, disk_space, downloads_directory, reveal_item};

#[cfg(target_os = "macos")]
pub fn downloads_directory() -> Option<PathBuf> {
    home_directory().map(|home| home.join("Downloads"))
}

#[cfg(target_os = "windows")]
pub const FILE_MANAGER: &str = "资源管理器";
#[cfg(target_os = "macos")]
pub const FILE_MANAGER: &str = "Finder";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const FILE_MANAGER: &str = "文件管理器";

#[cfg(target_os = "windows")]
pub const COPY_SHORTCUT: &str = "Ctrl+C";
#[cfg(target_os = "windows")]
pub const INSPECTOR_SHORTCUT: &str = "Ctrl+I";
#[cfg(target_os = "windows")]
pub const BACK_SHORTCUT: &str = "Backspace";
#[cfg(target_os = "macos")]
pub const COPY_SHORTCUT: &str = "⌘C";
#[cfg(target_os = "macos")]
pub const INSPECTOR_SHORTCUT: &str = "⌘I";
#[cfg(target_os = "macos")]
pub const BACK_SHORTCUT: &str = "⌫";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const COPY_SHORTCUT: &str = "Ctrl+C";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const INSPECTOR_SHORTCUT: &str = "Ctrl+I";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const BACK_SHORTCUT: &str = "Backspace";

pub fn reveal_label() -> String {
    format!("在 {FILE_MANAGER} 中显示")
}

pub fn reveal_error(error: impl std::fmt::Display) -> String {
    format!("无法在 {FILE_MANAGER} 中定位：{error}")
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn disk_space(_: &std::path::Path) -> std::io::Result<crate::models::DiskSpace> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "当前平台尚不支持卷容量查询。",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn reveal_item(_: &std::path::Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "当前平台尚不支持文件管理器定位。",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn appearance_preference_path() -> std::io::Result<PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "当前平台尚不支持偏好存储。",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn downloads_directory() -> Option<PathBuf> {
    None
}

#[cfg(target_os = "windows")]
pub fn home_directory() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .filter(|home| !home.is_empty())
        .or_else(|| std::env::var_os("HOME").filter(|home| !home.is_empty()))
        .map(PathBuf::from)
}

#[cfg(not(target_os = "windows"))]
pub fn home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

pub mod scan_fs;
