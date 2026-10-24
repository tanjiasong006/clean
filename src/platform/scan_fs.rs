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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct EntryMetadata {
    pub kind: EntryKind,
    pub allocated_bytes: u64,
    pub identity: FileIdentity,
    /// Hard-link count from `st_nlink`. A file with a single link can never be
    /// an alias, so the scanner keeps it out of the deduplication table.
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

#[cfg(not(target_os = "macos"))]
pub struct RootFileSystem;

#[cfg(not(target_os = "macos"))]
impl RootFileSystem {
    pub fn open(_: &std::path::Path) -> io::Result<Self> {
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

    fn open_root(&self) -> io::Result<Box<dyn Directory>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "当前平台尚不支持目录扫描。",
        ))
    }
}
