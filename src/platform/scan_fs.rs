//! The filesystem boundary used by the scanner. Directory entries carry their
//! metadata from one platform read, so traversal needs no per-entry lookup.
use std::{
    ffi::{OsStr, OsString},
    io,
};

use crate::models::EntryKind;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::RootFileSystem;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(any(target_os = "windows", test))]
mod windows_records;
#[cfg(target_os = "windows")]
pub use windows::RootFileSystem;
#[cfg(all(target_os = "windows", test))]
pub use windows::allocated_bytes as windows_allocated_bytes;
#[cfg(all(target_os = "windows", test))]
pub use windows::extended_directory_ids_supported as windows_extended_directory_ids;
#[cfg(all(target_os = "windows", test))]
pub use windows::link_count as windows_link_count;
#[cfg(all(target_os = "windows", test))]
pub use windows::mark_sparse as windows_mark_sparse;

/// A file's stable identity within one scan. The scanner only requires that
/// equal identities mean the same file; it never reads the fields.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub volume: u64,
    pub file_id: u128,
}

impl FileIdentity {
    #[cfg(unix)]
    pub fn new(device: u64, inode: u64) -> Self {
        Self { device, inode }
    }

    #[cfg(windows)]
    pub fn new(volume: u64, file_id: u128) -> Self {
        Self { volume, file_id }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EntryMetadata {
    pub kind: EntryKind,
    pub allocated_bytes: u64,
    pub identity: FileIdentity,
    /// Whether the file may be a hard-link alias, expressed as a link count.
    /// A value of one keeps the file out of the deduplication table.
    pub links: u64,
}

/// One directory entry. The name is the final path component.
#[derive(Debug)]
pub struct DirEntry {
    pub name: OsString,
    pub metadata: EntryMetadata,
}

/// A failed entry. The name is present when the failure happened after the
/// entry's name was known, so the scanner can report the exact path.
#[derive(Debug)]
pub struct EntryError {
    pub name: Option<OsString>,
    pub error: io::Error,
}

#[derive(Debug)]
pub enum EntryItem {
    Entry(DirEntry),
    Error(EntryError),
}

/// An open directory. Entries borrow the handle for the duration of a read.
pub trait Directory: Send + Sync {
    fn entries(&self) -> io::Result<Box<dyn Iterator<Item = EntryItem> + '_>>;
    /// Opens a child by its single final component, relative to this handle,
    /// and verifies that it still has the identity observed after discovery.
    fn open_child(&self, name: &OsStr, expected: FileIdentity) -> io::Result<Box<dyn Directory>>;
}

pub trait FileSystem: Send + Sync {
    fn root_metadata(&self) -> EntryMetadata;
    fn open_root(&self) -> io::Result<Box<dyn Directory>>;
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub struct RootFileSystem;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl RootFileSystem {
    pub fn open(_: &std::path::Path) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "当前平台尚不支持目录扫描。",
        ))
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl FileSystem for RootFileSystem {
    fn root_metadata(&self) -> EntryMetadata {
        unreachable!("unsupported platforms cannot open a scan root")
    }

    fn open_root(&self) -> io::Result<Box<dyn Directory>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "当前平台尚不支持目录扫描。",
        ))
    }
}
