# Windows 单文件记录决策与迁移计划

- 日期：2026-10-25
- 状态：提议（尚未实施）
- 范围：扫描结果中的单个普通文件——身份、硬链接、名称、分配量、重解析点。
- 不包含：目录聚合、界面、大文件索引策略、Windows 打包。

## 背景与现状

生产代码已能交叉编译到 Windows：

```sh
cargo check --locked --target x86_64-pc-windows-msvc --bin clean
```

该命令通过，仅有一条 dead_code 警告。`--all-targets` 有 16 个错误，全部位于测试代码，来自 `std::os::unix`、`MetadataExt::{blocks,nlink}` 和 `OsStringExt::from_vec`。

当前缺口集中在平台层：`src/platform/mod.rs:12-34` 和 `src/platform/scan_fs.rs:65-90` 在非 macOS 平台返回 `Unsupported`。文件记录相关的现有约束：

- `EntryMetadata { kind, allocated_bytes, identity, links }` 由平台层给出。
- `scanner/mod.rs:521` 仅对 `links > 1` 的普通文件保留身份，用于硬链接去重。
- `models` 不保留身份和链接数，只保留 8 字节的（别名 ID、首个 ID）对。
- 扫描结果不落盘，偏好文件才持久化。

## 决策

### D1. 文件身份键按平台定义

```rust
#[cfg(unix)]
pub struct FileIdentity { pub device: u64, pub inode: u64 }

#[cfg(windows)]
pub struct FileIdentity { pub volume: u64, pub file_id: u128 }
```

两种平台各提供 `FileIdentity::new(part1, part2)` 构造函数，测试字面量改用构造函数。

理由：扫描器只要求 `Eq + Hash`，不读取字段。macOS 的哈希键大小和行为不变；Windows 对应 `FILE_ID_INFO { VolumeSerialNumber, FileId: FILE_ID_128 }`。不为了 Windows 把 macOS 的键从 16 字节扩到 32 字节。

### D2. 硬链接判定：默认每文件探测

`FILE_ID_EXTD_DIR_INFORMATION` 提供 128 位 FileId、AllocationSize、属性和重解析标签，但没有 NumberOfLinks。现有 `links > 1` 门槛无法从批量枚举直接获得。

| 方案 | 做法 | 代价 |
| --- | --- | --- |
| A（默认） | 每个普通文件用父目录句柄相对 `NtCreateFile`，只请求 `FILE_READ_ATTRIBUTES`，共享读写删除，随后 `GetFileInformationByHandle` 取 NumberOfLinks | 每文件一次额外 open；内存模型与 macOS 一致 |
| B | 所有普通文件都视为潜在别名，`links = 2`，去重表覆盖所有文件 | 无额外系统调用；3M 文件约多 70-140 MiB 瞬时内存 |
| C | Windows 不做硬链接去重 | 违反 AGENTS.md，不采用 |

先实现 A，并设可测量门槛：若 100 万文件基准中探测使扫描墙钟时间增加超过约 25%-30%，切换到 B。把策略收敛为平台模块中的一个函数，切换时不改 `scanner`。

`EntryMetadata.links` 字段名保持不变，语义改为“别名候选”：

- macOS：真实 `st_nlink`。
- Windows 方案 A：探测得到的链接数。
- Windows 方案 B：身份有效时为 2。
- 身份不可用时：1，表示不进入去重表。

探测失败（文件被独占锁定、权限拒绝）按 `links = 1` 处理，不把扫描标记为不完整；文档注明被锁定文件可能不被识别为别名。

### D3. 身份不可用的卷禁用去重

以下情况不得把不同文件合并：

- FileId 全零。
- 卷序列号为 0。
- FAT/exFAT 等不支持稳定文件 ID 的文件系统。
- 部分网络共享。

处理：该条目 `links = 1`，不进入去重表；不报错、不标记不完整，在 README 中注明。不要用路径哈希伪造身份。

### D4. 重解析点与 EntryKind

现有 kind 只有 2 bit，四个值已用满，无法新增“重解析点”类别，按属性分类：

- 目录 + `FILE_ATTRIBUTE_REPARSE_POINT`：一律不遍历。`IO_REPARSE_TAG_SYMLINK`、`IO_REPARSE_TAG_MOUNT_POINT` 记为 `Symlink`，其他标签记为 `Other`。
- 文件 + `IO_REPARSE_TAG_SYMLINK`：记为 `Symlink`，不计字节。
- 文件 + 其他重解析标签（OneDrive 等云占位符）：按普通 `File` 处理并计入 AllocationSize，避免漏计云文件；不读取内容，不触发下载。
- 其他属性组合：`Other`。

平台差异：README 当前说明扫描允许跨挂载点；Windows 上挂载点本身是重解析点，跳过意味着不跨卷。v1 明确跳过并记录差异；若要跨卷，需要额外的卷循环检测（卷序列号 + 根 FileId），不纳入首版。

### D5. 目录自身分配量

NTFS 对目录报告的 AllocationSize 通常为 0。接受文件系统返回值，不合成、不按子项反推。需要写入平台差异：Windows 上目录自身分配量通常不计入，与 macOS 口径不同。

### D6. 名称编码

当前非 unix 回退在 `models/mod.rs:359-378` 使用 `to_string_lossy` 往返。合法但罕见的非法 UTF-16 代理项会被替换字符破坏，之后复制路径或在资源管理器中定位会得到错误路径。

建议在 `models` 内增加 Windows 编解码：

- 编码：`OsStr::encode_wide()` 转 WTF-8，保留孤立代理项；ASCII 仍占 1 字节，不破坏紧凑字典。
- 解码：WTF-8 转 UTF-16，再 `OsString::from_wide`。
- 展示仍使用 `to_string_lossy`，只影响显示，不影响路径还原。
- 排序为 WTF-8 字节序，与资源管理器本地化排序不同，但跨平台一致。

若首版压缩范围，可以暂用 lossy，但必须在 README 明确限制，不能声称已支持任意 Windows 文件名。

### D7. 扫描期内存

方案 A 不改变现有内存模型。方案 B 需要为 Windows 单独记录峰值，并把 Windows 数据与 macOS 的 3M 文件基准分开记录，不复用同一组数字。

## 迁移计划

扫描结果不落盘，没有用户数据迁移。迁移对象是内部类型和行为，前提是 macOS 零回归。

### M0. 身份类型拆分（约 0.5 天）

- `platform/scan_fs.rs`：按 `cfg(unix)` / `cfg(windows)` 定义 `FileIdentity`，提供构造函数。
- `platform/scan_fs/macos.rs`：改用构造函数。
- `scanner/tests.rs` 的假文件系统字面量改用构造函数。
- 验收：macOS 现有测试全部通过；哈希键布局和行为不变。

### M1. 非扫描平台能力（约 1 天）

- 新增 `platform/windows.rs`：`GetDiskFreeSpaceExW`、`explorer /select`、`%APPDATA%` 偏好路径、`USERPROFILE` 主目录、`FOLDERID_Downloads` 下载目录。
- `platform/mod.rs` 分发；界面文案和快捷键提示平台化。
- 验收：Windows 可启动、可选择目录、可显示容量；扫描仍返回“不支持”，其余功能不报错。
- 先增加只构建 `--bin clean` 的 Windows CI 检查，避免被测试代码阻塞。

### M2. 枚举与句柄锚定（约 1.5-2 天）

- `NtQueryDirectoryFile` + `FILE_ID_EXTD_DIR_INFORMATION` 批量枚举、缓冲区边界解析、NTSTATUS 转 `io::Error`。
- 根目录用 `FILE_ID_INFO` 取卷序列号；拒绝重解析根目录。
- `NtCreateFile` 相对父句柄打开子目录，`OBJ_DONT_REPARSE`，打开后校验 FileId。
- 单元测试：合成缓冲区解析、截断、零 FileId、重解析标签分类、目录被替换。
- 验收：真实临时目录扫描通过；符号链接和 junction 不越界；读取失败保留其他结果。

### M3. 硬链接策略与基准（约 0.5-1 天）

- 方案 A 探测；使用同一父句柄相对打开，避免长路径。
- 用 `CreateHardLinkW` 构造夹具，验证只计一次并生成别名标记。
- 跑 100 万文件基准，记录墙钟和内存；超过门槛则切到方案 B，只改平台模块和文档。
- 验收：硬链接夹具正确；性能结论记录在案。

### M4. 名称编解码（约 1 天，可与 M2 并行）

- `models` 内 WTF-8 编解码和往返测试，覆盖孤立代理项和 255 单元长名。
- 验收：Windows 上 `to_path_buf` 精确还原原始名称；macOS 名称行为不变。

### M5. 测试与 CI 迁移（约 1 天）

当前 Windows `--all-targets` 的 16 个错误来自：

- `scanner/tests.rs:4,100,703,713`：`MetadataExt::{blocks,nlink}`。
- `scanner/tests.rs:393,434,472,500,530`：`std::os::unix::fs::symlink`。
- `models/tests.rs:451,489,613`：`OsStringExt::from_vec`。

做法：

- 测试辅助函数 `allocated_bytes(path)`、`symlink_file/dir()` 按平台实现；Windows 分配量用句柄查询，符号链接创建失败时跳过相关用例。
- 非 UTF-8 名称用例加 `#[cfg(unix)]`；Windows 增加宽字符和代理项用例。
- CI 增加 Windows job：fmt、clippy、test、build、core_perf。
- 验收：macOS 测试数量不减少；Windows 测试全绿。

### M6. 文档与实机验收（约 0.5-1 天）

更新 README/AGENTS 的平台差异：硬链接探测、目录分配量为 0、挂载点不跨卷、云占位符、名称编码；补 Windows 性能记录。

## 回滚与兼容

- Windows 代码全部放在 `cfg(target_os = "windows")` 模块；macOS 只改 `FileIdentity` 的构造方式，行为不变。回滚等于删除模块和分发分支。
- 扫描结果不持久化，没有数据回滚问题。
- 偏好文件在 Windows 使用新路径，不存在从 macOS 导入的问题。
- 从方案 A 切到 B 时只改 Windows 平台模块的 `links` 输出，`scanner` 的门槛逻辑不变。
- 名称编解码集中在 `models` 的一个模块，切换 lossy/WTF-8 只改一处。

## 默认值与待确认项

- 硬链接先采用方案 A，以 100 万文件基准作为切换依据。
- Windows 首版不跨挂载点。
- 名称编码建议首版实现 WTF-8；若压缩范围，必须记录 lossy 限制。
