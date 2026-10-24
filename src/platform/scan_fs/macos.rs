use std::{
    ffi::{OsStr, OsString},
    io,
    os::{
        fd::{AsFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path, PathBuf},
};

use rustix::fs::{self, AtFlags, Dir, FileType, Mode, OFlags, Stat};

use super::{DirectoryReader, EntryMetadata, FileIdentity, FileSystem};
use crate::models::EntryKind;

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

pub struct RootFileSystem {
    root: PathBuf,
    fd: OwnedFd,
    metadata: EntryMetadata,
}

impl RootFileSystem {
    pub fn open(root: &Path) -> io::Result<Self> {
        // Strip trailing separators and '.' so NOFOLLOW still applies to the
        // final selected component. Initial parent aliases (e.g. /tmp) are OK.
        let normalized: PathBuf = root.components().collect();
        let observed =
            metadata_from_stat(fs::statat(fs::CWD, &normalized, AtFlags::SYMLINK_NOFOLLOW)?)?;
        if observed.kind != EntryKind::Directory {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "请选择真实目录；不跟随符号链接。",
            ));
        }
        let fd = fs::open(&normalized, DIRECTORY_FLAGS, Mode::empty())?;
        let metadata = metadata_from_stat(fs::fstat(&fd)?)?;
        if metadata.identity != observed.identity {
            return Err(io::Error::other("扫描根目录在打开前已被替换，请重新选择。"));
        }
        Ok(Self {
            root: root.into(),
            fd,
            metadata,
        })
    }

    fn open_relative(&self, path: &Path) -> io::Result<OwnedFd> {
        let relative = path.strip_prefix(&self.root).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "目录不在所选扫描根目录内。")
        })?;
        let mut current: Option<OwnedFd> = None;
        // Walk one component at a time: NOFOLLOW on just the last component
        // would still follow an ancestor replaced by a symlink. At most the
        // root and two temporary descriptors are open, independent of depth.
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "无效的扫描相对路径。",
                ));
            };
            let parent = current.as_ref().map_or(self.fd.as_fd(), |fd| fd.as_fd());
            current = Some(fs::openat(parent, name, DIRECTORY_FLAGS, Mode::empty())?);
        }
        match current {
            Some(fd) => Ok(fd),
            None => Ok(fs::openat(&self.fd, ".", DIRECTORY_FLAGS, Mode::empty())?),
        }
    }
}

impl FileSystem for RootFileSystem {
    fn root_metadata(&self) -> EntryMetadata {
        self.metadata
    }

    fn read_dir(
        &self,
        path: &Path,
        expected: FileIdentity,
    ) -> io::Result<Box<dyn DirectoryReader>> {
        let fd = self.open_relative(path)?;
        if metadata_from_stat(fs::fstat(&fd)?)?.identity != expected {
            return Err(io::Error::other(
                "目录在发现后已被替换，未读取替换后的内容。",
            ));
        }
        Ok(Box::new(Entries { dir: Dir::new(fd)? }))
    }
}

struct Entries {
    dir: Dir,
}

impl Iterator for Entries {
    type Item = io::Result<OsString>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.dir.next()? {
                Ok(entry) => {
                    let name = entry.file_name().to_bytes();
                    if name != b"." && name != b".." {
                        return Some(Ok(OsStr::from_bytes(name).to_os_string()));
                    }
                }
                Err(error) => return Some(Err(error.into())),
            }
        }
    }
}

impl DirectoryReader for Entries {
    fn metadata(&self, name: &OsStr) -> io::Result<EntryMetadata> {
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "无效的目录条目名称。",
            ));
        }
        metadata_from_stat(fs::statat(self.dir.fd()?, name, AtFlags::SYMLINK_NOFOLLOW)?)
    }
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
    })
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
