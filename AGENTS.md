# Repository Guidelines

## Product & Platform Decisions

Build a desktop disk-space analyzer for macOS first. Add Windows support after the macOS functionality is stable. Begin with space analysis, then introduce manual cleanup and application-specific cleanup rules in later phases.

Use Rust with GPUI and Longbridge GPUI Kit.

## Architecture & Module Organization

Start with one Rust crate using the following module boundaries:

- `src/ui/`: GPUI Kit views, components, and presentation state.
- `src/scanner/`: traversal, directory aggregation, progress, and cancellation.
- `src/analysis/`: large-file filtering, sorting, and space distribution.
- `src/platform/`: macOS integration; isolate OS-specific behavior here for future Windows support.
- `src/models/`: shared scan results and task-state types.

Keep scanning and analysis independent of GPUI. Run scanning in the background with bounded concurrency and batched updates; use virtualized lists for large results. Persist preferences, not full scan results, initially.

## Initial Scope & Interaction

Provide a space overview, directory analysis, and large-file view. Start with a navigation sidebar, breadcrumb navigation, size-sorted directory/file lists, occupancy bars, and an optional details panel. Restore selection and scroll position when navigating back. Support revealing items in Finder and copying paths.

Defer deletion, automated cleanup, duplicate detection, application uninstall, scheduled tasks, and treemaps. Implement in order: scan/navigation loop; analysis and Finder integration; performance, permissions, and macOS packaging.

## Scanning & Size Accounting

- Scan an explicitly selected location; offer home, Downloads, and custom-directory entry points.
- Show incremental results and discovered counts/bytes, without inventing percentage progress.
- Preserve partial results after cancellation and label incomplete scans clearly.
- Report inaccessible or failed directories separately; never represent them as empty.
- Do not follow symbolic links by default.
- Display only filesystem-reported allocated space (on macOS, `st_blocks × 512`), including directory allocation. Deduplicate regular files by device and inode within each scan: attribute bytes to the first observed path, retain other paths with an explicit already-counted marker, and exclude those aliases from the large-file index. Do not claim APFS shared blocks are exclusive or reclaimable space.
- Show disk capacity/free space separately; do not equate scan totals with reclaimable space. Shared-data ownership and reclaimable-space estimates require separate design.

Validate scanning with temporary-directory tests covering symlinks, hard links, cancellation, read failures, and files changing during scans. Represent idle, scanning, partial, complete, and failed states explicitly in the UI.
