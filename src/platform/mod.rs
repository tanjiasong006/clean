use std::path::PathBuf;

#[cfg(target_os = "macos")]
mod alloc;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use alloc::LargeAllocator;
#[cfg(target_os = "macos")]
pub use macos::{appearance_preference_path, disk_space, reveal_in_finder};

#[cfg(not(target_os = "macos"))]
pub fn disk_space(_: &std::path::Path) -> std::io::Result<crate::models::DiskSpace> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "当前平台尚不支持卷容量查询。",
    ))
}

#[cfg(not(target_os = "macos"))]
pub fn reveal_in_finder(_: &std::path::Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "当前平台尚不支持 Finder 定位。",
    ))
}

#[cfg(not(target_os = "macos"))]
pub fn appearance_preference_path() -> std::io::Result<PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "当前平台尚不支持偏好存储。",
    ))
}

pub fn home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

pub mod scan_fs;
