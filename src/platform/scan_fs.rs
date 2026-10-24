//! The filesystem boundary used by the scanner. Directory metadata is read
//! relative to an open directory, never by resolving a queued absolute path.
use std::{ffi::OsStr, ffi::OsString, io, path::Path};

use crate::models::EntryKind;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::RootFileSystem;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct EntryMetadata {
    pub kind: EntryKind,
    pub bytes: u64,
    pub identity: FileIdentity,
}

pub trait DirectoryReader: Iterator<Item = io::Result<OsString>> {
    fn metadata(&self, name: &OsStr) -> io::Result<EntryMetadata>;
}

pub trait FileSystem {
    fn root_metadata(&self) -> EntryMetadata;
    fn read_dir(&self, path: &Path, expected: FileIdentity)
    -> io::Result<Box<dyn DirectoryReader>>;
}

#[cfg(not(target_os = "macos"))]
pub struct RootFileSystem;

#[cfg(not(target_os = "macos"))]
impl RootFileSystem {
    pub fn open(_: &Path) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "当前平台尚不支持目录扫描。",
        ))
    }
}

#[cfg(not(target_os = "macos"))]
impl FileSystem for RootFileSystem {
    fn root_metadata(&self) -> EntryMetadata {
        unreachable!("unsupported platforms cannot open a scan root")
    }

    fn read_dir(&self, _: &Path, _: FileIdentity) -> io::Result<Box<dyn DirectoryReader>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "当前平台尚不支持目录扫描。",
        ))
    }
}
