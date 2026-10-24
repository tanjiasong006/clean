# Clean

使用 Rust、GPUI 和 GPUI Kit 构建的磁盘空间分析工具。先支持 macOS，功能稳定后再适配 Windows。

当前提供最小应用窗口和关于弹窗，尚未实现目录扫描或清理。架构和首版范围见 [AGENTS.md](AGENTS.md)。

## 开发环境

- macOS 15 或更高版本。
- Rust stable，包含 Cargo、rustfmt 和 Clippy；工具链要求见 `rust-toolchain.toml`。
- Xcode 或 Xcode Command Line Tools；使用 `xcode-select -p` 检查开发工具路径。

GPUI Kit 使用统一依赖入口，包含匹配的 GPUI、组件和默认图标资源。参考[官方安装说明](https://gpui-kit.com/docs/installation/)及[入门文档](https://gpui-kit.com/docs/getting-started/)。

## 运行与检查

仓库已包含 `Cargo.lock`，使用锁定的依赖运行和检查：

```sh
cargo run --locked
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
```

首次构建会下载并编译 GPUI 依赖，耗时较长。当前没有业务测试；加入扫描逻辑时补充临时目录集成测试。

## 项目结构

```text
src/main.rs       应用入口和窗口初始化
src/ui/           GPUI Kit 界面
```

后续按需增加 `scanner`、`analysis`、`platform` 和 `models` 模块，职责见 `AGENTS.md`。

## 版本管理

主分支使用 `main`，提交 `Cargo.lock` 以固定应用依赖。构建产物、日志及本地环境配置由 `.gitignore` 排除；`.gitattributes` 统一文本换行。

macOS CI 对主分支推送和拉取请求执行格式、Clippy、测试及构建检查。
