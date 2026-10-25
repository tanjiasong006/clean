# Repository Guidelines

## Product & Platform Decisions

Build a desktop disk-space analyzer for macOS and Windows. macOS remains the primary development machine; keep OS-specific behavior inside `src/platform/` so the Windows backend can evolve independently. Begin with space analysis, then introduce manual cleanup and application-specific cleanup rules in later phases.

Use Rust with GPUI and Longbridge GPUI Kit.

## Architecture & Module Organization

Start with one Rust crate using the following module boundaries:

- `src/ui/`: GPUI Kit views, components, and presentation state.
- `src/scanner/`: traversal, directory aggregation, progress, and cancellation.
- `src/analysis/`: large-file filtering, sorting, and space distribution.
- `src/platform/`: platform integration for macOS and Windows; isolate all OS-specific behavior here.
- `src/models/`: shared scan results and task-state types.

Keep scanning and analysis independent of GPUI. Run scanning in the background with bounded concurrency and batched updates; use virtualized lists for large results. Persist preferences, not full scan results, initially.

### Platform differences

- Windows file identity is the volume serial number plus the 128-bit file ID. Trust it only on NTFS and ReFS; disable deduplication on filesystems that do not expose stable IDs instead of inventing one.
- Windows directory enumeration does not expose a link count. Probe each regular file with a usable identity by name relative to its parent handle. A failed probe means "one link" and must not make the scan incomplete.
- On Windows, never traverse a directory reparse point. Count file reparse points such as cloud placeholders as regular files using their reported allocation, without opening their data or hydrating them. The first Windows version does not cross volumes.
- NTFS usually reports zero allocation for directories; accept the filesystem value instead of synthesizing one.
- Store Windows names as WTF-8 so lone UTF-16 surrogates survive a path round trip. Display may remain lossy; sorting follows WTF-8 byte order.


## Initial Scope & Interaction

Provide a space overview, directory analysis, and large-file view. Start with a navigation sidebar, breadcrumb navigation, size-sorted directory/file lists, occupancy bars, and an optional details panel. Restore selection and scroll position when navigating back. Support revealing items in Finder and copying paths.

Defer deletion, automated cleanup, duplicate detection, application uninstall, scheduled tasks, and treemaps. Implement in order: scan/navigation loop; analysis and Finder integration; performance, permissions, and macOS packaging.

## Scanning & Size Accounting

- Scan an explicitly selected location; offer home, Downloads, and custom-directory entry points.
- Show incremental results and discovered counts/bytes, without inventing percentage progress.
- Preserve partial results after cancellation and label incomplete scans clearly.
- Report inaccessible or failed directories separately; never represent them as empty.
- Do not follow symbolic links by default.
- Display only filesystem-reported allocated space (macOS: `st_blocks × 512`; Windows: the allocation size reported by the filesystem), including directory allocation. Deduplicate regular files by platform file identity within each scan: attribute bytes to the first observed path, retain other paths with an explicit already-counted marker, and exclude those aliases from the large-file index. Do not claim APFS shared blocks are exclusive or reclaimable space.
- Show disk capacity/free space separately; do not equate scan totals with reclaimable space. Shared-data ownership and reclaimable-space estimates require separate design.

Validate scanning with temporary-directory tests covering symlinks, hard links, cancellation, read failures, and files changing during scans. Represent idle, scanning, partial, complete, and failed states explicitly in the UI.
