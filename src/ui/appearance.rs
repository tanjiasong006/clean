use std::{fs, io, path::Path};

use gpui_kit::component::ThemeMode;

use crate::platform;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::System => "跟随系统",
            Self::Light => "浅色",
            Self::Dark => "深色",
        }
    }

    pub(super) fn resolve(self, system: ThemeMode) -> ThemeMode {
        match self {
            Self::System => system,
            Self::Light => ThemeMode::Light,
            Self::Dark => ThemeMode::Dark,
        }
    }

    pub(super) fn load() -> io::Result<Self> {
        Self::load_from(&platform::appearance_preference_path()?)
    }

    fn load_from(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(value) => Ok(match value.trim() {
                "light" => Self::Light,
                "dark" => Self::Dark,
                _ => Self::System,
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    pub(super) fn save(self) -> io::Result<()> {
        self.save_to(&platform::appearance_preference_path()?)
    }

    fn save_to(self, path: &Path) -> io::Result<()> {
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDirectory(std::path::PathBuf);

    impl TempDirectory {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "clean-appearance-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn system_follows_appearance_and_manual_modes_override_it() {
        assert_eq!(Appearance::default(), Appearance::System);
        for system in [ThemeMode::Light, ThemeMode::Dark] {
            assert_eq!(Appearance::System.resolve(system), system);
            assert_eq!(Appearance::Light.resolve(system), ThemeMode::Light);
            assert_eq!(Appearance::Dark.resolve(system), ThemeMode::Dark);
        }
    }

    #[test]
    fn missing_or_unknown_preference_defaults_to_system() {
        let temp = TempDirectory::new();
        let path = temp.0.join("appearance");
        assert_eq!(Appearance::load_from(&path).unwrap(), Appearance::System);
        for value in ["", "unknown", "system\n"] {
            fs::write(&path, value).unwrap();
            assert_eq!(Appearance::load_from(&path).unwrap(), Appearance::System);
        }
    }

    #[test]
    fn all_modes_persist_across_loads_and_create_the_directory() {
        let temp = TempDirectory::new();
        let path = temp.0.join("preferences/appearance");
        for appearance in [Appearance::Dark, Appearance::Light, Appearance::System] {
            appearance.save_to(&path).unwrap();
            assert_eq!(Appearance::load_from(&path).unwrap(), appearance);
        }
    }

    #[test]
    fn storage_failures_are_reported() {
        let temp = TempDirectory::new();
        assert!(Appearance::load_from(&temp.0).is_err());
        assert!(Appearance::Dark.save_to(&temp.0).is_err());
    }
}
