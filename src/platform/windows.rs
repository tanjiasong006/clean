use std::{
    ffi::{OsStr, OsString},
    io,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        process::CommandExt,
    },
    path::{Component, Path, PathBuf},
    process::Command,
    ptr, slice,
};

use windows_sys::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::GetDiskFreeSpaceExW,
    System::Com::CoTaskMemFree,
    UI::Shell::{FOLDERID_Downloads, KF_FLAG_DEFAULT, SHGetKnownFolderPath},
};

use crate::models::DiskSpace;

pub fn disk_space(path: &Path) -> io::Result<DiskSpace> {
    let path = std::path::absolute(path)?;
    let root = volume_root(&path);
    let wide = wide_null(root.as_os_str())?;
    let mut available = 0_u64;
    let mut capacity = 0_u64;
    let mut free = 0_u64;
    // SAFETY: the path is NUL-terminated and all out-pointers refer to live
    // u64 locals for the duration of the call.
    let ok =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut capacity, &mut free) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(DiskSpace {
        capacity,
        available,
    })
}

pub fn reveal_item(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "资源管理器定位需要绝对路径。",
        ));
    }
    std::fs::symlink_metadata(path)?;
    let mut command = Command::new("explorer.exe");
    match path.file_name() {
        // `/select` needs a parent item; a drive root is opened directly.
        Some(_) => {
            let mut argument = OsString::from("/select,\"");
            argument.push(path.as_os_str());
            argument.push("\"");
            command.raw_arg(argument);
        }
        None => {
            command.arg(path);
        }
    }
    // Explorer reports an exit code even on success and may hand off to an
    // existing process, so only failure to launch is an error.
    command.spawn()?;
    Ok(())
}

pub fn appearance_preference_path() -> io::Result<PathBuf> {
    let appdata = std::env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| super::home_directory().map(|home| home.join("AppData/Roaming")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "无法获取应用数据目录。"))?;
    Ok(appdata.join("Clean").join("appearance"))
}

pub fn downloads_directory() -> Option<PathBuf> {
    known_folder(&FOLDERID_Downloads)
        .or_else(|| super::home_directory().map(|home| home.join("Downloads")))
}

fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    let mut raw: *mut u16 = ptr::null_mut();
    // SAFETY: the GUID and out-pointer are valid; the returned string is owned
    // by the shell allocator and copied out before being freed.
    let result = unsafe {
        SHGetKnownFolderPath(
            id,
            KF_FLAG_DEFAULT as u32,
            ptr::null_mut::<std::ffi::c_void>() as HANDLE,
            &mut raw,
        )
    };
    if result < 0 || raw.is_null() {
        return None;
    }
    let mut length = 0;
    // SAFETY: a successful call returns a NUL-terminated UTF-16 string.
    while unsafe { *raw.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: `length` counts the units before the terminator.
    let units = unsafe { slice::from_raw_parts(raw, length) };
    let path = PathBuf::from(OsString::from_wide(units));
    // SAFETY: `raw` was allocated by the shell for this call and is no longer
    // used after the copy above.
    unsafe { CoTaskMemFree(raw.cast()) };
    (!path.as_os_str().is_empty()).then_some(path)
}

pub(crate) fn volume_root(path: &Path) -> PathBuf {
    let mut components = path.components();
    if let Some(Component::Prefix(prefix)) = components.next() {
        match prefix.kind() {
            std::path::Prefix::Disk(letter) | std::path::Prefix::VerbatimDisk(letter) => {
                return PathBuf::from(format!("{}:\\", letter as char));
            }
            std::path::Prefix::UNC(server, share)
            | std::path::Prefix::VerbatimUNC(server, share) => {
                return PathBuf::from(format!(
                    "\\\\{}\\{}\\",
                    server.to_string_lossy(),
                    share.to_string_lossy()
                ));
            }
            _ => {}
        }
    }
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

fn wide_null(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = value.encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "路径包含 NUL 字符。",
        ));
    }
    wide.push(0);
    Ok(wide)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_root_keeps_the_drive_or_share_prefix() {
        assert_eq!(
            volume_root(Path::new(r"C:\Users\name\file")),
            PathBuf::from(r"C:\")
        );
        assert_eq!(
            volume_root(Path::new(r"\\server\share\folder")),
            PathBuf::from(r"\\server\share\")
        );
    }

    #[test]
    fn paths_reject_embedded_nul_and_have_one_terminator() {
        assert!(wide_null(OsStr::new("a\0b")).is_err());
        let wide = wide_null(OsStr::new("C:\\")).unwrap();
        assert_eq!(wide.last(), Some(&0));
        assert_eq!(wide.iter().filter(|&&unit| unit == 0).count(), 1);
    }
}
