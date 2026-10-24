use std::{
    ffi::CString, io, mem::MaybeUninit, os::unix::ffi::OsStrExt, path::Path, process::Command,
};

use crate::models::DiskSpace;

pub fn disk_space(path: &Path) -> io::Result<DiskSpace> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "路径包含 NUL 字节。"))?;
    let mut stats = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: path is NUL-terminated and stats points to writable storage of the
    // correct size. The structure is read only after statfs reports success.
    let result = unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful statfs call initialized stats.
    let stats = unsafe { stats.assume_init() };
    space_from_blocks(stats.f_blocks, stats.f_bavail, u64::from(stats.f_bsize))
}

fn space_from_blocks(blocks: u64, available: u64, block_size: u64) -> io::Result<DiskSpace> {
    let bytes = |blocks: u64| {
        blocks
            .checked_mul(block_size)
            .ok_or_else(|| io::Error::other("卷容量超出支持的范围。"))
    };
    Ok(DiskSpace {
        capacity: bytes(blocks)?,
        available: bytes(available)?,
    })
}

fn reveal_command(path: &Path) -> io::Result<Command> {
    // An absolute argument cannot be interpreted as an open option. Do not
    // canonicalize: revealing a symlink must select the link, not its target.
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Finder 定位需要绝对路径。",
        ));
    }
    let mut command = Command::new("/usr/bin/open");
    command.arg("-R").arg(path);
    Ok(command)
}

pub fn reveal_in_finder(path: &Path) -> io::Result<()> {
    let mut command = reveal_command(path)?;
    std::fs::symlink_metadata(path)?;
    let output = command.output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "Finder 定位失败（{}）：{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

pub fn appearance_preference_path() -> std::io::Result<std::path::PathBuf> {
    super::home_directory()
        .map(|home| home.join("Library/Application Support/Clean/appearance"))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "无法获取主目录。"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_and_available_use_filesystem_block_size() {
        let space = space_from_blocks(100, 30, 4096).unwrap();
        assert_eq!(space.capacity, 409600);
        assert_eq!(space.available, 122880);
        let large = space_from_blocks(1 << 33, 1 << 32, 4096).unwrap();
        assert_eq!(large.capacity, 1 << 45);
        assert_eq!(large.available, 1 << 44);
        assert!(space_from_blocks(u64::MAX, 1, 4096).is_err());
        assert!(space_from_blocks(1, u64::MAX, 4096).is_err());
    }

    #[test]
    fn queries_selected_volume_and_reports_invalid_paths() {
        let space = disk_space(&std::env::temp_dir()).unwrap();
        assert!(space.capacity > 0);
        assert!(space.available <= space.capacity);
        assert!(disk_space(Path::new("/\0invalid")).is_err());
        assert!(disk_space(Path::new("/dev/null/not-a-directory")).is_err());
    }

    #[test]
    fn finder_uses_one_literal_argument_without_shell_interpolation() {
        let path = Path::new("/tmp/-file with 'quotes';$(echo test)");
        let command = reveal_command(path).unwrap();
        assert_eq!(command.get_program(), "/usr/bin/open");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, vec![std::ffi::OsStr::new("-R"), path.as_os_str()]);
        assert!(reveal_command(Path::new("-option")).is_err());
    }

    #[test]
    fn missing_finder_target_returns_error_before_launch() {
        assert!(reveal_in_finder(Path::new("/dev/null/not-a-file")).is_err());
    }
}
