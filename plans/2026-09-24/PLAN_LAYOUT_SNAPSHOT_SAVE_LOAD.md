# Layout Snapshot Save/Load Plan

Date: 2026-09-24
Status: Implemented (tray + CLI); PR open
Branch: `feature/layout-snapshot-save-load`
Scope: tray context-menu save/load + CLI save/load/inspect + best-effort restore (no app launch)

## Progress

- [x] Step 1 - Branch + plan + read tray/IPC/command patterns
- [x] Step 2 - Pure matcher + scoring in `wm-common` with unit tests
- [x] Step 3 - Tray menu items + request channels (copy / save / load)
- [x] Step 4 - Save path: build durable snapshot, clipboard + file dialog
- [x] Step 5 - Load path: file dialog + best-effort restore orchestration
- [x] Step 6 - Wire main loop handlers; deps (`arboard`, `rfd`)
- [x] Step 7 - `cargo test -p wm-common --lib` + compile `wm`
- [x] Step 8 - Commit, push, open PR against `glazewm-spotcobuild`
- [x] Step 9 - Address PR review: macOS class_name, monitor match, Minimized prev_state, UTF-8 fuzzy (b17c3bd9)
- [x] Step 10 - CLI save-layout / load-layout / query layout-match (+ inspect-layout alias)
- [x] Step 11 - PR review round 2: full WindowState equality + prev_state, floating W+H,
      empty workspace→monitor, IPC path-with-spaces quoting

## Objective

Add GlazeWM system-tray actions to **copy**, **save**, and **load** a
`LayoutSnapshot` JSON, reusing the export schema from PR #1. Load is
**best-effort restore** against currently running managed windows — missing
apps are **not** launched in v1 (documented follow-up).

Tray menu (preferred wording):

1. **Copy layout snapshot** — clipboard only (pretty durable JSON).
2. **Save layout snapshot…** — native save dialog + also copy to clipboard.
3. **Load layout snapshot…** — native open dialog, then restore.

## Constraints and invariants

- Work only on `feature/layout-snapshot-save-load` from latest
  `origin/glazewm-spotcobuild`. Do not commit to `glazewm-spotcobuild` / `main`.
- Reuse `LayoutSnapshot::from_monitor_dtos` / `strip_ephemeral` /
  `into_durable` (same path as `QueryCommand::Layout`).
- Pure matching/scoring lives in `wm-common` (no HWND); unit-tested.
- Restore orchestration in `packages/wm/src/commands/general/`; call existing
  commands (`move_window_to_workspace`, `update_window_state`,
  `set_window_position`, `set_focused_descendant`, `focus_workspace`,
  `resize_tiling_container`, `move_container_within_tree`).
- Best-effort priorities: (1) workspace assignment (2) tiling vs
  floating/minimized/fullscreen (+ `prev_state` awareness) (3) rough sibling
  order / tiling sizes (4) nested-split rebuild only if low risk.
- Do **not** launch missing processes. Do **not** require perfect nested-split
  isomorphism.
- Accept `version == 1`; reject other/missing versions carefully.
- Keep macOS compiling; prefer cross-platform crates (`arboard`, `rfd`).
- File dialogs / clipboard: prefer `dispatcher.dispatch_sync` when UI-thread
  care is needed (matches existing tray patterns).
- Git author: `git -c user.name=spotco -c user.email=mootothemax@gmail.com`.
  Do not set global git config. No force-push; no `--no-verify`.

## Design notes

### Tray → main loop

Today the tray only signals `config_reload_rx` / `exit_rx`. Extend with:

- `copy_layout_snapshot_rx`
- `save_layout_snapshot_rx`
- `load_layout_snapshot_rx`

Tray menu handlers send on those channels (no WM state on the tray thread).
Main loop builds/loads snapshots with access to `wm.state`, then runs dialogs
via `dispatcher.dispatch_sync` when needed.

### Matcher (`wm-common`)

```
score(candidate, snapshot_window) -> u32
- process_path exact (case-insensitive): high
- process_name exact (case-insensitive): high
- class_name exact: medium
- title_hint vs title: exact > contains > fuzzy (normalized equality / substring)
match_windows(snapshot_windows, live_windows) -> Vec<(snapshot_key, live_id)>
  greedy by score; threshold skips weak matches; no double-assign
```

### Restore algorithm (best-effort)

1. Deserialize `LayoutSnapshot`; require `version == LAYOUT_SNAPSHOT_VERSION` (1).
2. Match monitors (hardwareId → devicePath → deviceName → bounds).
3. Collect all `SnapshotWindow` leaves (ignored: skip / leave alone).
4. Enumerate live managed windows from `wm.state`; run matcher.
5. For each matched pair: move to target workspace (by snapshot workspace
   name on matched monitor); apply full `WindowState` equality (Floating/
   Fullscreen config fields) and `prev_state` for minimized **and**
   non-minimized; restore floating Rect X/Y/W/H; place empty workspaces
   from snapshot monitor structure.
6. Optionally reorder tiling toward snapshot sibling order; set
   `tiling_size` ratios when possible.
7. Activate focused workspace from snapshot if present.
8. Log summary via `tracing::info` (matched / unmatched counts).

### Save path

1. Build snapshot same as `QueryCommand::Layout`.
2. `into_durable()` before write/clipboard.
3. Pretty-print `serde_json`.
4. Clipboard set as UTF-8 text (`arboard`).
5. Save dialog default name: `glazewm-layout-YYYYMMDD-HHMMSS.json`.

## Steps

### Step 1 - Branch + plan

- [x] Create branch from `origin/glazewm-spotcobuild`.
- [x] Write this plan.

### Step 2 - Matcher module + tests

- [ ] `packages/wm-common/src/layout_snapshot_matcher.rs`
- [ ] Export from `lib.rs`
- [ ] Unit tests: perfect path, name+title, ambiguous titles, no double
      assign, weak match skipped

### Step 3 - Tray UI + channels

- [ ] Menu items + `TrayMenuId` variants
- [ ] Unbounded channels on `SystemTray`

### Step 4 - Save orchestration

- [ ] Shared `build_layout_snapshot(&WmState) -> LayoutSnapshot`
- [ ] Clipboard copy helper; save dialog + write file

### Step 5 - Load orchestration

- [ ] `load_layout_snapshot.rs` in `commands/general/`
- [ ] Open dialog; deserialize; match; restore; summary log

### Step 6 - Main loop + deps

- [ ] Handle new tray receivers in `main.rs`
- [ ] Add `arboard`, `rfd` to `packages/wm/Cargo.toml`

### Step 7 - Verify

```
cargo test -p wm-common --lib
cargo check -p wm
# On Asus (optional): build.bat / VsDevCmd cargo build -p wm
```

### Step 8 - Commit / push / PR

- [ ] Commit(s), push, `gh pr create` base `glazewm-spotcobuild`

## CLI surface (save / load / inspect)

Added for reproducible scripting (no tray dialogs):

| Command | Effect |
|---------|--------|
| `glazewm-cli save-layout <path.json>` | Durable snapshot write (same as `query layout -o`) |
| `glazewm-cli query layout -o <path.json>` | Unchanged |
| `glazewm-cli load-layout <path.json>` | IPC → running WM best-effort restore (`read_layout_snapshot_file` + `load_layout_snapshot`) |
| `glazewm-cli command load-layout <path.json>` | Same restore via `InvokeCommand::LoadLayout` |
| `glazewm-cli query layout-match <path.json>` | Dry-run match report JSON (no mutation) |
| `glazewm-cli inspect-layout <path.json>` | Alias of layout-match |

Match report fields: `matched[]` (snapshot/live identity, `score`, optional `plannedWorkspaceMove`), `unmatchedSnapshot[]`, `unmatchedLive[]`. Pure report builder + scoring live in `wm-common` (`build_layout_match_report`, `match_windows_with_scores`).

### Step 10 - CLI save / load / inspect

- [x] `save-layout` / `load-layout` / `inspect-layout` + `query layout-match`
- [x] `InvokeCommand::LoadLayout` + IPC `LoadLayout` / `LayoutMatch` responses
- [x] Unit tests for report builder in `layout_snapshot_matcher`
- [x] Smoke on Asus (see verification)

## Verification commands (CLI)

```
cargo test -p wm-common --lib
# build (Asus): build.bat   OR   cargo build -p wm -p wm-cli --release
# Live (WM running from this build):
glazewm-cli save-layout %TEMP%\glazewm-smoke.json
glazewm-cli query layout-match %TEMP%\glazewm-smoke.json
# optional (mutates desktop):
glazewm-cli load-layout %TEMP%\glazewm-smoke.json
glazewm-cli query layout -o %TEMP%\glazewm-smoke-after.json
```

Not CLI-testable: tray file dialogs / clipboard copy (manual).

## Follow-up (explicitly out of scope)

- Launch missing apps via `processPath`
- ~~Perfect nested-split tree isomorphism / full tree rebuild~~ (done: best-effort prune + rebuild)
- Re-apply ignored-window set from snapshot
- Toast UI for success/failure (tracing is enough for v1)

## Verification commands

```
cargo test -p wm-common --lib
cargo check -p wm
# Live (requires running GlazeWM from this branch):
# Tray → Copy / Save… / Load…
```

