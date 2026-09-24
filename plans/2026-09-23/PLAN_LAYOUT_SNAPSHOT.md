# Layout Snapshot Export Plan

Date: 2026-09-23
Status: Implemented; PR open
Branch: `feature/layout-snapshot`
Scope: export-focused first slice (save/load restore orchestration deferred)

## Progress

- [x] Step 1 - Branch + plan + read existing query/DTO/IPC patterns
- [x] Step 2 - Expose `processPath` on native windows and `WindowDto`
- [x] Step 3 - Add LayoutSnapshot DTOs + conversion from monitor tree
- [x] Step 4 - Add `query layout` and `query ignored` IPC/CLI wiring
- [x] Step 5 - Optional CLI `--output` to write durable snapshot JSON
- [x] Step 6 - Unit tests for snapshot conversion
- [x] Step 7 - Build verify (`cargo build --release` / wm-cli)
- [x] Step 8 - Commit, push, open PR against `glazewm-spotcobuild`
- [x] Step 9 - Mark plan Implemented / PR open (restore remains follow-up)

## Objective

Ship a unified JSON export of the current GlazeWM layout: monitors, workspaces,
tiling tree, window identity/state, ignored windows, and useful WM status
(paused, binding modes, version). This is the foundation for later save /
load / restore. This PR is **export-only**; restore orchestration is documented
as follow-up and must not block the PR.

Tiling vs floating is the "dock" signal: user config Super+T is
`toggle-floating`, so snapshot `state` of `tiling` vs `floating` records whether
a window was docked into the tiling tree.

## Constraints and invariants

- Prefer extending the existing `query` / IPC `ClientResponseData` path; do not
  invent a second protocol.
- Work only on `feature/layout-snapshot`. Do not commit to `glazewm-spotcobuild`
  or `master`.
- Minimal invasive changes; match existing serde `camelCase` / clap style.
- Live `query layout` may include ephemeral handles/Uuids for debugging;
  durable persisted form strips or documents ephemeral fields and relies on
  `processPath` / `processName` / `className` / `titleHint` identity.
- If GlazeWM is not running, compile-test is enough; note live query untested.
- No force-push; no `--no-verify` unless hooks block for a reportable reason.
- Restore is Step N follow-up (stub OK); do not block PR on full restore.

## Design notes

### Reuse `query monitors` tree

`query monitors` already returns the full nested
`Monitor -> Workspace -> Split|Window` tree via existing DTOs. Layout snapshot
conversion walks that tree (and WM status fields) into a persistence-oriented
schema with **local ids** for focus order, so restore does not depend on live
Uuids.

### Gaps this PR closes

1. **`processPath`** — Windows `process_name()` already calls
   `QueryFullProcessImageNameW` then discards the directory. Keep the full
   image path on `NativeWindowProperties` / `WindowDto` as `processPath`.
2. **Ignored windows** — `WmState.ignored_windows: Vec<NativeWindow>` is not
   exposed. Add `query ignored` returning identity-bearing DTOs; also embed
   `ignoredWindows` inside `query layout`.
3. **`query layout`** — returns `LayoutSnapshot`-shaped JSON suitable for
   persistence (plus optional ephemeral debug fields when live).
4. **CLI** — `glazewm-cli query layout` / `query ignored` work like other
   queries (IPC parse of `AppCommand::Query`).
5. **Optional file write** — `glazewm-cli query layout -o path.json` writes the
   durable (ephemeral-stripped) snapshot JSON to disk; IPC still returns the
   live payload on stdout.
6. **Restore** — follow-up Step N only.

### Schema (Rust / serde camelCase)

```
LayoutSnapshot {
  version: u32,              // schema version, start at 1
  capturedAt: String,        // RFC3339 UTC
  glazewmVersion: Option<String>,
  paused: bool,
  bindingModes: Vec<String>, // binding mode names
  monitors: Vec<SnapshotMonitor>,
  ignoredWindows: Vec<SnapshotWindow>,
}

SnapshotMonitor {
  hardwareId / devicePath / deviceName / bounds (x,y,width,height),
  focusedWorkspaceName: Option<String>,
  workspaces: Vec<SnapshotWorkspace>,
}

SnapshotWorkspace {
  name,
  tilingDirection,
  childFocusOrder: Vec<String>, // localIds
  root: SnapshotNode,
}

SnapshotNode {
  localId: String,
  kind: "split" | "window",
  tilingSize: Option<f32>,
  tilingDirection: Option<TilingDirection>,
  children: Option<Vec<SnapshotNode>>,
  window: Option<SnapshotWindow>,
}

SnapshotWindow {
  identity: { processPath?, processName, className?, titleHint? },
  state: tiling | floating | fullscreen | minimized (+ floating/fullscreen flags),
  floatingPlacement?: Rect,
  // ephemeral live: id?, handle?
}
```

### Durable vs live

- `LayoutSnapshot::strip_ephemeral()` clears Uuid/handle debug fields.
- CLI `--output` writes stripped JSON; raw IPC response keeps live fields.

## Step 1 - Branch + plan + read patterns

- [x] Create `feature/layout-snapshot` from current HEAD.
- [x] Write this plan file.
- [x] Confirm patterns in:
  - `packages/wm-common/src/dtos/window_dto.rs`
  - `packages/wm-platform/.../windows/native_window.rs` (`process_name`)
  - `packages/wm-common/src/app_command.rs` (`QueryCommand`)
  - `packages/wm-common/src/ipc.rs` (`ClientResponseData`)
  - `packages/wm/src/ipc_server.rs`
  - `packages/wm-cli/src/lib.rs`

## Step 2 - Expose `processPath`

- [x] Windows `NativeWindow::process_path()` returns full image path; refactor
      `process_name()` to derive the basename from it.
- [x] Public `wm_platform::NativeWindow::process_path()`.
- [x] macOS: return `Err` or empty Optional — `WindowDto.process_path` is
      `Option<String>` (`None` when unavailable).
- [x] Cache on `NativeWindowProperties`; populate `WindowDto.process_path` in
      tiling / non-tiling `to_dto()`.

## Step 3 - LayoutSnapshot DTOs + conversion

- [x] New `packages/wm-common/src/layout_snapshot.rs` (exported from `lib.rs`).
- [x] `LayoutSnapshot::from_monitor_dtos(...)` walking `ContainerDto` monitors
      tree; assign `localId`s; map `childFocusOrder` Uuids -> localIds.
- [x] `strip_ephemeral()` for durable persistence.
- [x] Document Super+T / tiling-vs-floating dock signal in this plan (done).

## Step 4 - Query wiring

- [x] `QueryCommand::Layout` and `QueryCommand::Ignored`.
- [x] `ClientResponseData::Layout(LayoutSnapshot)` and
      `ClientResponseData::Ignored(IgnoredWindowsData)`.
- [x] `ipc_server` handlers: build snapshot from `wm.state.monitors()` DTOs +
      paused + binding mode names + version; ignored from
      `wm.state.ignored_windows` (best-effort native queries).

## Step 5 - CLI `--output`

- [x] `QueryCommand::Layout { output: Option<PathBuf> }`.
- [x] `wm-cli`: if `output` set, write
      `serde_json::to_vec_pretty(snapshot.strip_ephemeral())` to path; still
      print full client response to stdout. Strip `--output`/`-o` from the IPC
      message so the server only sees `query layout`.

## Step 6 - Unit tests

- [x] `layout_snapshot` module tests: synthetic `ContainerDto` monitor tree
      converts to expected localIds / focus order / window identity; strip
      clears ephemeral fields.

## Step 7 - Build verify

```powershell
# Prefer existing build.bat pattern (loads VsDevCmd)
$env:BUILD_BAT_NOPAUSE=1
.\build.bat
# Or targeted:
cargo build -p wm-common --release
cargo build -p wm-cli --release
cargo test -p wm-common --lib
```

## Step 8 - Commit / push / PR

- [x] Commit with author `spotco` / `mootothemax@gmail.com`.
- [x] `git push -u origin feature/layout-snapshot`.
- [x] `gh pr create` base `glazewm-spotcobuild`.

## Step 9 - Follow-up: restore orchestration (NOT blocking)

- [ ] Match monitors by hardwareId/devicePath/deviceName/bounds.
- [ ] Ensure target apps running (launch via processPath when missing).
- [ ] Rebuild split tree / move windows / set tiling|floating|fullscreen|
      minimized to match snapshot (dock = tiling).
- [ ] Re-apply ignored set.
- [ ] Minimal prototype/stub may land later; full restore is a separate PR.

## Verification commands

```powershell
cargo test -p wm-common --lib
cargo build --release -p wm-cli -p wm
# Live (requires running GlazeWM built from this branch):
glazewm-cli query layout
glazewm-cli query ignored
glazewm-cli query layout -o $env:TEMP\glazewm-layout.json
```
