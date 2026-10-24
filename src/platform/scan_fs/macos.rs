use std::{
    ffi::{OsStr, c_void},
    io,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path, PathBuf},
};

use rustix::{
    fs::{self, AtFlags, Dir, FileType, Mode, OFlags, SeekFrom, Stat},
    io::{Errno, dup},
};

use super::{DirEntry, Directory, EntryError, EntryItem, EntryMetadata, FileIdentity, FileSystem};
use crate::models::EntryKind;

// The selected root is resolved through the caller's path, so only its final
// component may not be a symlink. Descendants are opened one component at a
// time from an already open parent handle.
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Bulk attribute reads pay off with a buffer large enough to amortize the
/// syscall over many entries; each group is roughly 70 bytes plus the name.
const BULK_BUFFER_SIZE: usize = 256 * 1024;

// `fsobj_type_t` values from `sys/vnode.h`.
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;

pub struct RootFileSystem {
    fd: OwnedFd,
    metadata: EntryMetadata,
}

impl RootFileSystem {
    pub fn open(root: &Path) -> io::Result<Self> {
        // Strip trailing separators and '.' so NOFOLLOW still applies to the
        // final selected component. Initial parent aliases (e.g. /tmp) are OK.
        let normalized: PathBuf = root.components().collect();
        let observed = metadata_from_stat(retry(|| {
            fs::statat(fs::CWD, &normalized, AtFlags::SYMLINK_NOFOLLOW)
        })?)?;
        if observed.kind != EntryKind::Directory {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "请选择真实目录；不跟随符号链接。",
            ));
        }
        let fd = retry(|| fs::open(&normalized, DIRECTORY_FLAGS, Mode::empty()))?;
        let metadata = metadata_from_stat(retry(|| fs::fstat(&fd))?)?;
        if metadata.identity != observed.identity {
            return Err(io::Error::other("扫描根目录在打开前已被替换，请重新选择。"));
        }
        Ok(Self { fd, metadata })
    }
}

impl FileSystem for RootFileSystem {
    fn root_metadata(&self) -> EntryMetadata {
        self.metadata
    }

    fn open_root(&self) -> io::Result<Box<dyn Directory>> {
        Ok(Box::new(BulkDirectory { fd: dup(&self.fd)? }))
    }
}

struct BulkDirectory {
    fd: OwnedFd,
}

impl Directory for BulkDirectory {
    fn entries(&self) -> io::Result<Box<dyn Iterator<Item = EntryItem> + '_>> {
        Ok(Box::new(Entries {
            fd: self.fd.as_fd(),
            buffer: vec![0; BULK_BUFFER_SIZE],
            cursor: 0,
            remaining: 0,
            done: false,
            fallback: None,
        }))
    }

    fn open_child(&self, name: &OsStr, expected: FileIdentity) -> io::Result<Box<dyn Directory>> {
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "无效的目录条目名称。",
            ));
        }
        let fd = retry(|| fs::openat(&self.fd, name, DIRECTORY_FLAGS, Mode::empty()))?;
        if metadata_from_stat(retry(|| fs::fstat(&fd))?)?.identity != expected {
            return Err(io::Error::other(
                "目录在发现后已被替换，未读取替换后的内容。",
            ));
        }
        Ok(Box::new(BulkDirectory { fd }))
    }
}

struct Entries<'a> {
    fd: BorrowedFd<'a>,
    buffer: Vec<u8>,
    cursor: usize,
    remaining: usize,
    done: bool,
    /// Used when the volume does not implement `getattrlistbulk`.
    fallback: Option<Dir>,
}

impl Iterator for Entries<'_> {
    type Item = EntryItem;

    fn next(&mut self) -> Option<Self::Item> {
        if self.fallback.is_some() {
            return self.next_fallback();
        }
        {
            if self.remaining == 0 {
                if self.done {
                    return None;
                }
                match self.fill() {
                    Ok(true) => {}
                    Ok(false) => {
                        self.done = true;
                        return None;
                    }
                    Err(error) => {
                        if self.begin_fallback(&error) {
                            return self.next_fallback();
                        }
                        self.done = true;
                        return Some(EntryItem::Error(EntryError { name: None, error }));
                    }
                }
            }
            let start = self.cursor;
            let buffer = &self.buffer;
            let length = match u32_at(buffer, start) {
                Ok(length) => length as usize,
                Err(error) => {
                    self.done = true;
                    return Some(EntryItem::Error(EntryError { name: None, error }));
                }
            };
            let end = start.checked_add(length)?;
            if length < 24 || end > buffer.len() {
                self.done = true;
                return Some(EntryItem::Error(EntryError {
                    name: None,
                    error: malformed("目录属性缓冲区无效。"),
                }));
            }
            self.cursor = end;
            self.remaining -= 1;
            Some(match parse_entry(self.fd, &buffer[start..end]) {
                Ok(entry) => EntryItem::Entry(entry),
                Err(error) => EntryItem::Error(EntryError { name: None, error }),
            })
        }
    }
}

impl Entries<'_> {
    fn fill(&mut self) -> io::Result<bool> {
        let mut attributes = bulk_attributes();
        let count = loop {
            let count = unsafe {
                libc::getattrlistbulk(
                    self.fd.as_raw_fd(),
                    &mut attributes as *mut _ as *mut c_void,
                    self.buffer.as_mut_ptr() as *mut c_void,
                    self.buffer.len(),
                    0,
                )
            };
            if count >= 0 {
                break count;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        };
        if count == 0 {
            return Ok(false);
        }
        self.cursor = 0;
        self.remaining = count as usize;
        Ok(true)
    }

    fn begin_fallback(&mut self, error: &io::Error) -> bool {
        if !matches!(
            error.raw_os_error(),
            Some(libc::ENOTSUP) | Some(libc::EOPNOTSUPP) | Some(libc::ENOSYS) | Some(libc::EINVAL)
        ) {
            return false;
        }
        let Ok(dir) = fs::seek(self.fd, SeekFrom::Start(0))
            .map_err(|_| ())
            .and_then(|_| dup(self.fd).map_err(|_| ()))
            .and_then(|fd| Dir::new(fd).map_err(|_| ()))
        else {
            return false;
        };
        self.fallback = Some(dir);
        true
    }

    fn next_fallback(&mut self) -> Option<EntryItem> {
        let dir = self.fallback.as_mut()?;
        loop {
            let entry = match dir.next()? {
                Ok(entry) => entry,
                Err(error) => {
                    return Some(EntryItem::Error(EntryError {
                        name: None,
                        error: error.into(),
                    }));
                }
            };
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let name = OsStr::from_bytes(name);
            let metadata = match dir.fd() {
                Ok(fd) => fs::statat(fd, name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(io::Error::from)
                    .and_then(metadata_from_stat),
                Err(error) => Err(error.into()),
            };
            return Some(match metadata {
                Ok(metadata) => EntryItem::Entry(DirEntry {
                    name: name.to_os_string(),
                    metadata,
                }),
                Err(error) => EntryItem::Error(EntryError {
                    name: Some(name.to_os_string()),
                    error,
                }),
            });
        }
    }
}

fn bulk_attributes() -> libc::attrlist {
    libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_FILEID,
        volattr: 0,
        dirattr: libc::ATTR_DIR_ALLOCSIZE,
        fileattr: libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE,
        forkattr: 0,
    }
}

fn parse_entry(fd: BorrowedFd<'_>, group: &[u8]) -> io::Result<DirEntry> {
    let mut cursor = 4;
    let returned = libc::attribute_set_t {
        commonattr: u32_at(group, cursor)?,
        volattr: u32_at(group, cursor + 4)?,
        dirattr: u32_at(group, cursor + 8)?,
        fileattr: u32_at(group, cursor + 12)?,
        forkattr: u32_at(group, cursor + 16)?,
    };
    cursor += 20;

    if returned.commonattr & libc::ATTR_CMN_NAME == 0 {
        return Err(malformed("目录条目缺少名称。"));
    }
    let name_field = cursor;
    let name_offset = i32_at(group, cursor)? as i64;
    let name_length = u32_at(group, cursor + 4)? as usize;
    cursor += 8;
    let name_start = (name_field as i64 + name_offset) as usize;
    let name_end = name_start
        .checked_add(name_length)
        .filter(|&end| end <= group.len())
        .ok_or_else(|| malformed("目录条目名称超出缓冲区。"))?;
    let mut name_bytes = &group[name_start..name_end];
    if name_bytes.last() == Some(&0) {
        name_bytes = &name_bytes[..name_bytes.len() - 1];
    }
    let name = OsStr::from_bytes(name_bytes).to_os_string();

    let device = if returned.commonattr & libc::ATTR_CMN_DEVID != 0 {
        let value = u32_at(group, cursor)? as u64;
        cursor += 4;
        Some(value)
    } else {
        None
    };
    let object_type = if returned.commonattr & libc::ATTR_CMN_OBJTYPE != 0 {
        let value = u32_at(group, cursor)?;
        cursor += 4;
        Some(value)
    } else {
        None
    };
    let inode = if returned.commonattr & libc::ATTR_CMN_FILEID != 0 {
        let value = u64_at(group, cursor)?;
        cursor += 8;
        Some(value)
    } else {
        None
    };

    let directory_bytes = if returned.dirattr & libc::ATTR_DIR_ALLOCSIZE != 0 {
        let value = i64_at(group, cursor)?;
        cursor += 8;
        Some(value)
    } else {
        None
    };
    let links = if returned.fileattr & libc::ATTR_FILE_LINKCOUNT != 0 {
        let value = u32_at(group, cursor)? as u64;
        cursor += 4;
        Some(value)
    } else {
        None
    };
    let file_bytes = if returned.fileattr & libc::ATTR_FILE_ALLOCSIZE != 0 {
        let value = i64_at(group, cursor)?;
        Some(value)
    } else {
        None
    };

    let kind = object_type.map_or(EntryKind::Other, |object_type| match object_type {
        VREG => EntryKind::File,
        VDIR => EntryKind::Directory,
        VLNK => EntryKind::Symlink,
        _ => EntryKind::Other,
    });

    let needs_stat = match kind {
        EntryKind::Directory => directory_bytes.is_none() || device.is_none() || inode.is_none(),
        EntryKind::File => {
            file_bytes.is_none() || links.is_none() || device.is_none() || inode.is_none()
        }
        _ => false,
    };
    if needs_stat {
        return stat_entry(fd, &name);
    }

    let allocated_bytes = match kind {
        EntryKind::Directory => byte_count(directory_bytes.unwrap_or(0))?,
        EntryKind::File => byte_count(file_bytes.unwrap_or(0))?,
        _ => 0,
    };
    Ok(DirEntry {
        name,
        metadata: EntryMetadata {
            kind,
            allocated_bytes,
            identity: FileIdentity {
                device: device.unwrap_or(0),
                inode: inode.unwrap_or(0),
            },
            links: links.unwrap_or(1),
        },
    })
}

fn stat_entry(fd: BorrowedFd<'_>, name: &OsStr) -> io::Result<DirEntry> {
    Ok(DirEntry {
        name: name.to_os_string(),
        metadata: metadata_from_stat(fs::statat(fd, name, AtFlags::SYMLINK_NOFOLLOW)?)?,
    })
}

fn retry<T>(mut operation: impl FnMut() -> Result<T, Errno>) -> io::Result<T> {
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn malformed(message: &'static str) -> io::Error {
    io::Error::other(message)
}

fn u32_at(buffer: &[u8], at: usize) -> io::Result<u32> {
    let bytes = buffer
        .get(at..at + 4)
        .ok_or_else(|| malformed("目录属性缓冲区被截断。"))?;
    Ok(u32::from_ne_bytes(bytes.try_into().unwrap()))
}

fn i32_at(buffer: &[u8], at: usize) -> io::Result<i32> {
    let bytes = buffer
        .get(at..at + 4)
        .ok_or_else(|| malformed("目录属性缓冲区被截断。"))?;
    Ok(i32::from_ne_bytes(bytes.try_into().unwrap()))
}

fn u64_at(buffer: &[u8], at: usize) -> io::Result<u64> {
    let bytes = buffer
        .get(at..at + 8)
        .ok_or_else(|| malformed("目录属性缓冲区被截断。"))?;
    Ok(u64::from_ne_bytes(bytes.try_into().unwrap()))
}

fn i64_at(buffer: &[u8], at: usize) -> io::Result<i64> {
    let bytes = buffer
        .get(at..at + 8)
        .ok_or_else(|| malformed("目录属性缓冲区被截断。"))?;
    Ok(i64::from_ne_bytes(bytes.try_into().unwrap()))
}

fn metadata_from_stat(stat: Stat) -> io::Result<EntryMetadata> {
    let kind = match FileType::from_raw_mode(stat.st_mode) {
        FileType::Directory => EntryKind::Directory,
        FileType::RegularFile => EntryKind::File,
        FileType::Symlink => EntryKind::Symlink,
        _ => EntryKind::Other,
    };
    let allocated_bytes = if matches!(kind, EntryKind::File | EntryKind::Directory) {
        allocated_bytes(stat.st_blocks)?
    } else {
        0
    };
    Ok(EntryMetadata {
        kind,
        allocated_bytes,
        identity: FileIdentity {
            device: stat.st_dev as u64,
            inode: stat.st_ino,
        },
        links: stat.st_nlink as u64,
    })
}

/// `ATTR_*_ALLOCSIZE` values are already byte counts, unlike `st_blocks`.
fn byte_count(bytes: i64) -> io::Result<u64> {
    u64::try_from(bytes).map_err(|_| io::Error::other("已分配空间为负值。"))
}

fn allocated_bytes(blocks: i64) -> io::Result<u64> {
    u64::try_from(blocks)
        .map_err(|_| io::Error::other("已分配块数为负值。"))?
        .checked_mul(512)
        .ok_or_else(|| io::Error::other("已分配空间超出 u64 统计范围。"))
}

#[cfg(test)]
mod tests {
    use super::allocated_bytes;

    #[test]
    fn allocation_uses_512_byte_units_and_64_bit_counts() {
        assert_eq!(allocated_bytes(0).unwrap(), 0);
        assert_eq!(allocated_bytes(1).unwrap(), 512);
        assert_eq!(allocated_bytes(1_i64 << 33).unwrap(), 1_u64 << 42);
    }

    #[test]
    fn invalid_or_overflowing_allocation_is_an_error() {
        assert!(allocated_bytes(-1).is_err());
        assert!(allocated_bytes(i64::MAX).is_err());
        let largest = (u64::MAX / 512) as i64;
        assert_eq!(allocated_bytes(largest).unwrap(), largest as u64 * 512);
        assert!(allocated_bytes(largest + 1).is_err());
    }
}
