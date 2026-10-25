//! Windows scan backend. Directory handles anchor every traversal: children
//! are opened by one name relative to their parent's handle, never by a full
//! path, so a renamed or replaced ancestor cannot redirect the scan.
use std::{
    ffi::{OsStr, OsString},
    io,
    mem::{MaybeUninit, size_of, size_of_val},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
    },
    path::{Component, Path, PathBuf},
    ptr,
};

use windows_sys::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_FOR_BACKUP_INTENT,
            FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
        },
    },
    Win32::{
        Foundation::{
            ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE, NTSTATUS, STATUS_ACCESS_DENIED,
            STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND,
            STATUS_SHARING_VIOLATION, UNICODE_STRING,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileAttributeTagInfo,
            FileIdExtdDirectoryInfo, FileIdExtdDirectoryRestartInfo, FileIdInfo, FileStandardInfo,
            GetFileInformationByHandleEx, GetVolumeInformationW, OPEN_EXISTING, ReOpenFile,
            SYNCHRONIZE,
        },
        System::IO::IO_STATUS_BLOCK,
    },
};

#[cfg(test)]
use super::windows_records::RECORD_HEADER_SIZE;
use super::windows_records::parse;
use super::{DirEntry, Directory, EntryError, EntryItem, EntryMetadata, FileIdentity, FileSystem};
use crate::models::EntryKind;

const DIRECTORY_ACCESS: u32 = FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const FILE_ACCESS: u32 = FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const OPEN_FLAGS: u32 = FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT;

/// Bulk reads amortize one syscall over many entries. A single entry is at most
/// the 88-byte header plus a 255-unit name, so 256 KiB never splits one.
const BUFFER_SIZE: usize = 256 * 1024;

pub struct RootFileSystem {
    handle: OwnedHandle,
    path: PathBuf,
    metadata: EntryMetadata,
    volume: u64,
}

impl RootFileSystem {
    pub fn open(root: &Path) -> io::Result<Self> {
        // `absolute` normalizes `.` and `..` without resolving the final
        // component, so the reparse-point check below still sees a link root.
        let path = std::path::absolute(root)?;
        let file = open_path(&path, DIRECTORY_ACCESS)?;
        let attributes: FILE_ATTRIBUTE_TAG_INFO =
            get_info(raw_handle(&file), FileAttributeTagInfo)?;
        if attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "请选择真实目录；不跟随符号链接或目录联接。",
            ));
        }
        if attributes.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "请选择目录。"));
        }
        let standard: FILE_STANDARD_INFO = get_info(raw_handle(&file), FileStandardInfo)?;
        let allocated_bytes = allocation(standard.AllocationSize)?;

        // Identity is only meaningful on filesystems that expose stable file
        // IDs. FAT/exFAT and some network shares report values that must not be
        // used to merge different files.
        let filesystem = volume_filesystem(&path).unwrap_or_default();
        let identity = if supports_file_ids(&filesystem) {
            get_info::<FILE_ID_INFO>(raw_handle(&file), FileIdInfo)
                .map(|info| identity_from(&info))
                .unwrap_or(INVALID_IDENTITY)
        } else {
            INVALID_IDENTITY
        };
        let volume = identity.volume;
        Ok(Self {
            handle: file,
            path,
            metadata: EntryMetadata {
                kind: EntryKind::Directory,
                allocated_bytes,
                identity,
                links: 1,
            },
            volume,
        })
    }
}

impl FileSystem for RootFileSystem {
    fn root_metadata(&self) -> EntryMetadata {
        self.metadata
    }

    fn open_root(&self) -> io::Result<Box<dyn Directory>> {
        // ReOpenFile creates a second handle to the same file object, so a
        // renamed root keeps being read while enumeration state stays fresh.
        let reopened = unsafe {
            ReOpenFile(
                raw_handle(&self.handle),
                DIRECTORY_ACCESS,
                SHARE_ALL,
                OPEN_FLAGS,
            )
        };
        if reopened != INVALID_HANDLE_VALUE {
            return Ok(Box::new(WinDirectory {
                // SAFETY: ReOpenFile returned a new owned handle.
                handle: unsafe { OwnedHandle::from_raw_handle(reopened as RawHandle) },
                volume: self.volume,
            }));
        }
        let reopen_error = io::Error::last_os_error();
        // Some file systems reject ReOpenFile; fall back to the path while
        // still verifying that it names the directory originally selected.
        let file = match open_path(&self.path, DIRECTORY_ACCESS) {
            Ok(file) => file,
            Err(_) => return Err(reopen_error),
        };
        let attributes: FILE_ATTRIBUTE_TAG_INFO =
            get_info(raw_handle(&file), FileAttributeTagInfo)?;
        if attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::other("扫描根目录在打开前已被替换，请重新选择。"));
        }
        if identity_valid(self.metadata.identity) {
            let identity = get_info::<FILE_ID_INFO>(raw_handle(&file), FileIdInfo)
                .map(|info| identity_from(&info))?;
            if identity != self.metadata.identity {
                return Err(io::Error::other("扫描根目录在打开前已被替换，请重新选择。"));
            }
        }
        Ok(Box::new(WinDirectory {
            handle: file,
            volume: self.volume,
        }))
    }
}

struct WinDirectory {
    handle: OwnedHandle,
    volume: u64,
}

impl Directory for WinDirectory {
    fn entries(&self) -> io::Result<Box<dyn Iterator<Item = EntryItem> + '_>> {
        Ok(Box::new(Entries {
            directory: self,
            buffer: vec![0; BUFFER_SIZE / size_of::<u64>()],
            cursor: 0,
            has_entry: false,
            first_fill: true,
            done: false,
        }))
    }

    fn open_child(&self, name: &OsStr, expected: FileIdentity) -> io::Result<Box<dyn Directory>> {
        validate_name(name)?;
        let child = nt_open(
            raw_handle(&self.handle),
            name,
            DIRECTORY_ACCESS,
            FILE_DIRECTORY_FILE
                | FILE_OPEN_REPARSE_POINT
                | FILE_OPEN_FOR_BACKUP_INTENT
                | FILE_SYNCHRONOUS_IO_NONALERT,
        )?;
        let attributes: FILE_ATTRIBUTE_TAG_INFO =
            get_info(raw_handle(&child), FileAttributeTagInfo)?;
        if attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::Error::other(
                "目录在发现后已被替换为符号链接或目录联接，未读取替换后的内容。",
            ));
        }
        if identity_valid(expected) {
            let identity = get_info::<FILE_ID_INFO>(raw_handle(&child), FileIdInfo)
                .map(|info| identity_from(&info))?;
            if identity != expected {
                return Err(io::Error::other(
                    "目录在发现后已被替换，未读取替换后的内容。",
                ));
            }
        }
        Ok(Box::new(WinDirectory {
            handle: child,
            volume: self.volume,
        }))
    }
}

struct Entries<'a> {
    directory: &'a WinDirectory,
    buffer: Vec<u64>,
    cursor: usize,
    has_entry: bool,
    first_fill: bool,
    done: bool,
}

impl Iterator for Entries<'_> {
    type Item = EntryItem;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.done {
                return None;
            }
            if !self.has_entry {
                match self.fill() {
                    Ok(true) => {}
                    Ok(false) => {
                        self.done = true;
                        return None;
                    }
                    Err(error) => {
                        self.done = true;
                        return Some(EntryItem::Error(EntryError { name: None, error }));
                    }
                }
            }
            let start = self.cursor;
            let result = parse_entry(
                buffer_bytes(&self.buffer),
                start,
                raw_handle(&self.directory.handle),
                self.directory.volume,
            );
            match result {
                Ok((entry, next)) => {
                    if next == 0 {
                        self.has_entry = false;
                    } else {
                        self.cursor = start + next;
                    }
                    if let Some(entry) = entry {
                        return Some(EntryItem::Entry(entry));
                    }
                }
                Err(error) => {
                    self.done = true;
                    return Some(EntryItem::Error(EntryError { name: None, error }));
                }
            }
        }
    }
}

impl Entries<'_> {
    fn fill(&mut self) -> io::Result<bool> {
        let class = if self.first_fill {
            self.first_fill = false;
            FileIdExtdDirectoryRestartInfo
        } else {
            FileIdExtdDirectoryInfo
        };
        let result = unsafe {
            GetFileInformationByHandleEx(
                raw_handle(&self.directory.handle),
                class,
                self.buffer.as_mut_ptr().cast(),
                size_of_val(&self.buffer) as u32,
            )
        };
        if result != 0 {
            self.cursor = 0;
            self.has_entry = true;
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
            Ok(false)
        } else {
            Err(error)
        }
    }
}

fn parse_entry(
    buffer: &[u8],
    start: usize,
    parent: HANDLE,
    volume: u64,
) -> io::Result<(Option<DirEntry>, usize)> {
    let record = parse(buffer, start)?;
    if record.is_dot {
        return Ok((None, record.next_offset));
    }
    let name = OsString::from_wide(&record.name_units);
    let kind = record.kind;
    let allocated_bytes = match kind {
        EntryKind::Directory | EntryKind::File => allocation(record.allocation)?,
        _ => 0,
    };
    let identity = if volume != 0 && record.file_id != 0 {
        FileIdentity::new(volume, record.file_id)
    } else {
        INVALID_IDENTITY
    };
    // FILE_ID_EXTD_DIR_INFO has no link count, so files with a usable identity
    // are probed once by name relative to the parent handle.
    let links = if kind == EntryKind::File && identity_valid(identity) {
        probe_link_count(parent, &name).unwrap_or(1)
    } else {
        1
    };
    Ok((
        Some(DirEntry {
            name,
            metadata: EntryMetadata {
                kind,
                allocated_bytes,
                identity,
                links,
            },
        }),
        record.next_offset,
    ))
}

fn open_path(path: &Path, access: u32) -> io::Result<OwnedHandle> {
    let wide = verbatim_path(path)?;
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            SHARE_ALL,
            ptr::null(),
            OPEN_EXISTING,
            OPEN_FLAGS,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned a new owned handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) })
}

fn nt_open(parent: HANDLE, name: &OsStr, access: u32, options: u32) -> io::Result<OwnedHandle> {
    let units: Vec<u16> = name.encode_wide().collect();
    let byte_length = units
        .len()
        .checked_mul(2)
        .filter(|&length| length > 0 && length <= u16::MAX as usize && !units.contains(&0))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "无效的目录条目名称。"))?;
    let mut object_name = UNICODE_STRING {
        Length: byte_length as u16,
        MaximumLength: byte_length as u16,
        Buffer: units.as_ptr() as *mut u16,
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent,
        ObjectName: &mut object_name,
        Attributes: 0,
        SecurityDescriptor: ptr::null(),
        SecurityQualityOfService: ptr::null(),
    };
    let mut status_block = IO_STATUS_BLOCK::default();
    let mut raw: HANDLE = ptr::null_mut();
    // SAFETY: every pointer refers to a live local for the duration of the
    // call; `units` outlives the call and is not NUL-terminated by design.
    let status: NTSTATUS = unsafe {
        NtCreateFile(
            &mut raw,
            access,
            &attributes,
            &mut status_block,
            ptr::null(),
            0,
            SHARE_ALL,
            FILE_OPEN,
            options,
            ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(nt_error(status));
    }
    // SAFETY: NtCreateFile returned a new owned handle.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) })
}

fn probe_link_count(parent: HANDLE, name: &OsStr) -> io::Result<u64> {
    let child = nt_open(
        parent,
        name,
        FILE_ACCESS,
        FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
    )?;
    let standard: FILE_STANDARD_INFO = get_info(raw_handle(&child), FileStandardInfo)?;
    Ok(u64::from(standard.NumberOfLinks.max(1)))
}

/// Allocation reported by the filesystem for one path. Used by tests to
/// compare scan totals against the platform's own accounting.
#[cfg(test)]
pub fn allocated_bytes(path: &Path) -> io::Result<u64> {
    let path = std::path::absolute(path)?;
    let handle = open_path(&path, FILE_ACCESS)?;
    let standard: FILE_STANDARD_INFO = get_info(raw_handle(&handle), FileStandardInfo)?;
    allocation(standard.AllocationSize)
}

/// Link count reported by the filesystem for one path.
#[cfg(test)]
pub fn link_count(path: &Path) -> io::Result<u64> {
    let path = std::path::absolute(path)?;
    let handle = open_path(&path, FILE_ACCESS)?;
    let standard: FILE_STANDARD_INFO = get_info(raw_handle(&handle), FileStandardInfo)?;
    Ok(u64::from(standard.NumberOfLinks.max(1)))
}

fn validate_name(name: &OsStr) -> io::Result<()> {
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "无效的目录条目名称。",
        )),
    }
}

fn identity_from(info: &FILE_ID_INFO) -> FileIdentity {
    FileIdentity::new(
        info.VolumeSerialNumber,
        u128::from_ne_bytes(info.FileId.Identifier),
    )
}

const INVALID_IDENTITY: FileIdentity = FileIdentity {
    volume: 0,
    file_id: 0,
};

fn identity_valid(identity: FileIdentity) -> bool {
    identity.volume != 0 && identity.file_id != 0
}

fn allocation(bytes: i64) -> io::Result<u64> {
    u64::try_from(bytes).map_err(|_| io::Error::other("已分配空间为负值。"))
}

fn supports_file_ids(filesystem: &str) -> bool {
    filesystem.eq_ignore_ascii_case("NTFS") || filesystem.eq_ignore_ascii_case("REFS")
}

fn volume_filesystem(path: &Path) -> Option<String> {
    let root = volume_root(path);
    let wide: Vec<u16> = root.as_os_str().encode_wide().chain([0]).collect();
    let mut name = [0u16; 32];
    let ok = unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    let length = name
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(name.len());
    Some(String::from_utf16_lossy(&name[..length]))
}

fn volume_root(path: &Path) -> PathBuf {
    let mut root = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) => root.push(component.as_os_str()),
            Component::RootDir => {
                root.push(component.as_os_str());
                break;
            }
            _ => break,
        }
    }
    root
}

fn verbatim_path(path: &Path) -> io::Result<Vec<u16>> {
    let units: Vec<u16> = path.as_os_str().encode_wide().collect();
    if units.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "路径包含 NUL 字符。",
        ));
    }
    let mut wide = Vec::with_capacity(units.len() + 8);
    let verbatim = units.starts_with(&['\\' as u16, '\\' as u16, '?' as u16, '\\' as u16])
        || units.starts_with(&['\\' as u16, '\\' as u16, '.' as u16, '\\' as u16]);
    if verbatim {
        wide.extend_from_slice(&units);
    } else if units.starts_with(&['\\' as u16, '\\' as u16]) {
        wide.extend("\\\\?\\UNC\\".encode_utf16());
        wide.extend_from_slice(&units[2..]);
    } else {
        wide.extend("\\\\?\\".encode_utf16());
        wide.extend_from_slice(&units);
    }
    wide.push(0);
    Ok(wide)
}

fn raw_handle(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle() as HANDLE
}

fn get_info<T>(file: HANDLE, class: i32) -> io::Result<T> {
    let mut value = MaybeUninit::<T>::zeroed();
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file,
            class,
            value.as_mut_ptr().cast(),
            size_of::<T>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful call initialized the whole structure.
    Ok(unsafe { value.assume_init() })
}

fn nt_error(status: NTSTATUS) -> io::Error {
    let kind = match status {
        STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND => io::ErrorKind::NotFound,
        STATUS_ACCESS_DENIED | STATUS_SHARING_VIOLATION => io::ErrorKind::PermissionDenied,
        STATUS_NOT_A_DIRECTORY => io::ErrorKind::NotADirectory,
        _ => {
            return io::Error::other(format!("Windows NT 状态 0x{:08X}", status as u32));
        }
    };
    io::Error::new(kind, format!("Windows NT 状态 0x{:08X}", status as u32))
}

fn buffer_bytes(buffer: &[u64]) -> &[u8] {
    // SAFETY: the slice is an initialized u64 array; reading it as bytes is
    // always valid, and the length matches the allocation.
    unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast(), size_of_val(buffer)) }
}

#[cfg(test)]
mod tests {
    use std::mem::offset_of;

    use super::*;

    #[test]
    fn record_layout_matches_the_windows_structure() {
        assert_eq!(
            RECORD_HEADER_SIZE,
            offset_of!(
                windows_sys::Win32::Storage::FileSystem::FILE_ID_EXTD_DIR_INFO,
                FileName
            )
        );
    }

    #[test]
    fn identity_is_rejected_for_zero_parts() {
        assert!(!identity_valid(INVALID_IDENTITY));
        assert!(!identity_valid(FileIdentity::new(7, 0)));
        assert!(!identity_valid(FileIdentity::new(0, 9)));
        assert!(identity_valid(FileIdentity::new(7, 9)));
        assert!(supports_file_ids("NTFS"));
        assert!(supports_file_ids("ReFS"));
        assert!(!supports_file_ids("FAT32"));
    }
}
