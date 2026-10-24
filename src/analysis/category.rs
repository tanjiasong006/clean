use crate::models::EntryKind;

/// A coarse, presentation-level grouping used for data visualization. This is a
/// name-based heuristic: it never reads file content and never affects size
/// accounting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Category {
    Applications,
    Documents,
    Images,
    Video,
    Audio,
    Developer,
    Archives,
    Cache,
    System,
    #[default]
    Other,
}

impl Category {
    pub const ALL: [Self; 10] = [
        Self::Applications,
        Self::Documents,
        Self::Images,
        Self::Video,
        Self::Audio,
        Self::Developer,
        Self::Archives,
        Self::Cache,
        Self::System,
        Self::Other,
    ];

    pub fn index(self) -> usize {
        match self {
            Self::Applications => 0,
            Self::Documents => 1,
            Self::Images => 2,
            Self::Video => 3,
            Self::Audio => 4,
            Self::Developer => 5,
            Self::Archives => 6,
            Self::Cache => 7,
            Self::System => 8,
            Self::Other => 9,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Applications => "应用",
            Self::Documents => "文档",
            Self::Images => "图片",
            Self::Video => "视频",
            Self::Audio => "音频",
            Self::Developer => "开发",
            Self::Archives => "归档",
            Self::Cache => "缓存",
            Self::System => "系统",
            Self::Other => "其他",
        }
    }

    pub fn classify(name: &str, kind: EntryKind) -> Self {
        if matches!(kind, EntryKind::Symlink | EntryKind::Other) {
            return Self::Other;
        }
        let name = name.to_lowercase();
        if kind == EntryKind::Directory {
            return Self::classify_directory(&name);
        }
        name.rsplit_once('.')
            .map(|(_, extension)| Self::classify_extension(extension))
            .unwrap_or_default()
    }

    fn classify_directory(name: &str) -> Self {
        match name {
            "applications" | "application" => return Self::Applications,
            "documents" | "document" | "desktop" => return Self::Documents,
            "pictures" | "photos" | "images" | "image" => return Self::Images,
            "movies" | "videos" | "video" => return Self::Video,
            "music" | "audio" | "sounds" => return Self::Audio,
            "caches" | "cache" | ".cache" | "deriveddata" | "temp" | "tmp" => {
                return Self::Cache;
            }
            "library" | "system" | "usr" | "bin" | "sbin" | "etc" | "var" | "private" | "opt"
            | "cores" => return Self::System,
            "node_modules" | "target" | "build" | "dist" | "vendor" | ".git" | ".cargo"
            | ".rustup" | ".npm" | ".pnpm-store" | ".gradle" | ".m2" | ".venv" | "venv"
            | "__pycache__" | ".next" | ".nuxt" | ".tox" | "pods" | ".build" | "project"
            | "projects" | "workspace" | "workspaces" | "repos" | "repo" | "code" | "src"
            | "dev" | ".vscode" | ".codex" | ".cursor" | ".idea" | ".zed" => {
                return Self::Developer;
            }
            _ => {}
        }
        if name.ends_with(".app") {
            Self::Applications
        } else if name.ends_with(".framework") || name.ends_with(".bundle") {
            Self::System
        } else if name.ends_with(".photoslibrary") || name.ends_with(".photolibrary") {
            Self::Images
        } else if name.ends_with(".musiclibrary") {
            Self::Audio
        } else if name.ends_with(".imovielibrary") || name.ends_with(".tvlibrary") {
            Self::Video
        } else {
            Self::Other
        }
    }

    fn classify_extension(extension: &str) -> Self {
        match extension {
            "png" | "jpg" | "jpeg" | "gif" | "heic" | "heif" | "webp" | "tif" | "tiff" | "bmp"
            | "svg" | "ico" | "raw" | "dng" | "cr2" | "nef" | "arw" | "psd" | "ai" | "sketch"
            | "fig" => Self::Images,
            "mp4" | "mov" | "m4v" | "avi" | "mkv" | "webm" | "wmv" | "flv" | "mpg" | "mpeg"
            | "m2ts" | "mts" => Self::Video,
            "mp3" | "m4a" | "aac" | "wav" | "flac" | "ogg" | "opus" | "aiff" | "aif" | "alac"
            | "mid" | "midi" => Self::Audio,
            "zip" | "tar" | "gz" | "tgz" | "bz2" | "tbz2" | "xz" | "txz" | "7z" | "rar" | "zst"
            | "lz4" | "dmg" | "iso" | "pkg" | "mpkg" | "deb" | "rpm" | "cab" => Self::Archives,
            "rs" | "go" | "py" | "pyc" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" | "c"
            | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "java" | "kt" | "kts" | "swift" | "m"
            | "mm" | "rb" | "php" | "sh" | "zsh" | "bash" | "fish" | "lua" | "sql" | "html"
            | "css" | "scss" | "sass" | "less" | "vue" | "svelte" | "toml" | "yaml" | "yml"
            | "json" | "jsonc" | "lock" | "wasm" | "o" | "a" | "so" | "dylib" | "class" | "jar"
            | "war" | "ipynb" | "gradle" => Self::Developer,
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "key" | "pages"
            | "numbers" | "txt" | "md" | "markdown" | "rtf" | "csv" | "tsv" | "epub" | "mobi" => {
                Self::Documents
            }
            "plist" | "log" | "kext" | "ips" | "spin" | "crash" | "panic" => Self::System,
            "cache" | "tmp" | "temp" => Self::Cache,
            _ => Self::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_directories_and_bundles_map_to_their_category() {
        for (name, category) in [
            ("Applications", Category::Applications),
            ("Documents", Category::Documents),
            ("Desktop", Category::Documents),
            ("Pictures", Category::Images),
            ("Movies", Category::Video),
            ("Music", Category::Audio),
            ("Library", Category::System),
            ("Caches", Category::Cache),
            ("node_modules", Category::Developer),
            ("target", Category::Developer),
            ("project", Category::Developer),
            (".vscode", Category::Developer),
            ("Xcode.app", Category::Applications),
            ("Photos Library.photoslibrary", Category::Images),
            ("something", Category::Other),
        ] {
            assert_eq!(
                Category::classify(name, EntryKind::Directory),
                category,
                "{name}"
            );
        }
    }

    #[test]
    fn file_extensions_map_to_their_category_regardless_of_case() {
        for (name, category) in [
            ("photo.PNG", Category::Images),
            ("clip.mp4", Category::Video),
            ("track.flac", Category::Audio),
            ("bundle.zip", Category::Archives),
            ("main.rs", Category::Developer),
            ("notes.pdf", Category::Documents),
            ("com.apple.plist", Category::System),
            ("README", Category::Other),
        ] {
            assert_eq!(
                Category::classify(name, EntryKind::File),
                category,
                "{name}"
            );
        }
    }

    #[test]
    fn skipped_nodes_and_indices_stay_stable() {
        assert_eq!(
            Category::classify("Photos", EntryKind::Symlink),
            Category::Other
        );
        assert_eq!(
            Category::classify("Photos", EntryKind::Other),
            Category::Other
        );
        for (index, category) in Category::ALL.into_iter().enumerate() {
            assert_eq!(category.index(), index);
        }
    }
}
