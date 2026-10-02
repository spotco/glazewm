# Plan: Owned popup Alt-Tab leaves detached parent buried

Date: 2026-10-02 (ET)
Status: product fix wired + unit tests PASS (17 platform_sync); Asus smoke PASS (modal + non-modal); commit/PR pending
Branch: `glazewm-spotcobuild` (local uncommitted helper + `platform_sync` tests)
Related: PR #13 (Super+arrow z-order / hung HWND), PR #14 (detached cycle); invariant "focusing a detached window promotes only that window"

## Progress

- [x] Extend `tools/glazewm-debug-helper` with **non-modal** and **modal** owned children
- [x] Repro on Asus under current Program Files GlazeWM (both cases)
- [x] Unit tests: characterize current bug + desired owner-group helper (modal + non-modal)
- [x] Root-cause analysis + fix plan (this note)
- [x] Product fix: wire `floating_focus_owner_group` into `normal_z_order_chain` / reorder path
- [x] Windows smoke: helper modal + non-modal Alt-Tab after fix (Asus PASS 2026-10-02)
- [x] Commit / PR (this change)

## User report

Detached (floating) window opens more windows from the **same app** — **modal and/or non-modal**. Alt-Tabbing to the app focuses only the popup; user expects the **entire detached group** (parent + popup) to come forward.

## Helper (repro)

Path: `tools/glazewm-debug-helper/` (`GlazeWmDebugHelper.sln`, Debug x64).

| Control | What it creates |
|---------|-----------------|
| **Open non-modal owned (APPWINDOW + caption)** | `GW_OWNER` = main, `WS_EX_APPWINDOW`, caption → **managed** Floating |
| **Open non-modal owned (caption, no APPWINDOW)** | Same owner/caption, no APPWINDOW → still managed |
| **Open modal owned dialog** | Same + `WS_EX_DLGMODALFRAME` + `EnableWindow(parent, FALSE)` |
| CLI | `--open-non-modal`, `--open-modal`, `--open-non-modal-no-app` |

Repro steps (both cases):

1. Run helper; `set-floating` the main "GlazeWM debug helper" window.
2. Open **non-modal** or **modal** owned child.
3. Focus a tiled window (e.g. VS Code) so the floater pair sits below the tile group.
4. Focus / Alt-Tab to the owned popup only.
5. **Expected:** popup + parent both above tiles (popup above parent).
6. **Observed:** only popup above tiles; parent stays below tiles.

## Asus evidence (2026-10-02 ~17:20 ET)

### Non-modal

After focus popup `4720436` (owner `330456`):

- z-order top: popup → tiles (Code, Grok, …) → **parent** below tiles.

### Modal

After focus modal `724112` (owner `330456`):

- Same split: modal on top, tiles, parent below.

### layout.log (modal `724112` / owner `330456`)

Smoking gun — OS briefly kept owner with popup; GlazeWM **intended** chain then buried the owner:

```
pre-reorder:  match_intended=false native=[724112, 330456, tiles...]
normal z reconcile focused=724112 Floating chain=[724112, tiles..., 330456, ...]
post-reorder: match_intended=true  native=[724112, tiles..., 330456, ...]
```

So the bug is **correct application of a wrong intent**, for both modal and non-modal.

## Root cause (shared by modal and non-modal)

1. Owned caption windows are **managed** as separate Floating containers (`manage_window`: owner + no caption → ignore; owner + caption → manage). Modal vs non-modal only differs by `EnableWindow` / `WS_EX_DLGMODALFRAME`; both have `GW_OWNER` → parent.

2. Detached-focus invariant in `normal_z_order_chain` (`Floating` / `Ignored` arms): **promote only the selected hwnd**. Peers (including the managed owner) keep prior slots — often under the tiled block after a tile focus.

3. All `SetWindowPos` z-order paths use **`SWP_NOOWNERZORDER`**, so Windows will not drag the owner along when the owned hwnd is raised. Combined with (2), GlazeWM actively undoes the OS's temporary owner+owned raise from `SetForegroundWindow`.

4. Modal does **not** need a separate z-order code path from non-modal. Same owner link, same Floating promote-only-selected bug. Cover both in tests/smoke because both appear in real apps (VS modal `#32770`, non-modal owned tool windows).

## What to fix (do not implement yet unless trivial)

### A. Chain construction (primary)

When building the normal-layer chain for a focused Floating/Ignored window:

- Resolve managed `GW_OWNER` (and owner walk) from native `owner_handle` / `GetWindow(GW_OWNER)`.
- Promote an **owner group**: `[focused, …other owned siblings in prior order…, …owners…]` then the rest of the previous order.
- Keep tiled windows as one contiguous block (existing `normalize_tiled_window_block`).

Pure helper already added (not wired):

`floating_focus_owner_group(previous_order, focused, owner_of)` in `platform_sync.rs`.

Wire points:

- `normal_z_order_chain` Floating/Ignored arms, **or**
- `reorder_focused_workspace_layers_in_workspace` before calling the chain, by expanding "focused" into the owner group.

Need `owner_of` from live workspace windows' `native().debug_info().owner_handle` (only when owner is also in the chain).

### B. `SWP_NOOWNERZORDER` (secondary / belt)

Even with a correct chain, keep `SWP_NOOWNERZORDER` for independent inserts (chain already lists owner). Do **not** rely on clearing the flag alone — without (A), peer floaters and tile grouping regress. Optional follow-up: document why the flag stays.

### C. Invariant update

Replace "focusing a detached window promotes only that window" with:

- Focusing a detached window promotes that window **and its managed Win32 owner group** (owned above owner); unrelated detached peers stay put relative to the tiled block.

### D. Out of scope for this bug

- Unmanaged owned menus (no caption) — already ignored; focus-return-to-owner on close already handled (`managed_owner_focus_target`).
- Focusing the **owner** while owned children exist — Windows keeps owned above owner if we don't break it; verify after (A) that promoting the owner still leaves owned children above it (may need to include owned children in the group when focus is the owner).

## Automated tests (landed, product fix unwired)

In `packages/wm/src/commands/general/platform_sync.rs` `tests`:

| Test | Case |
|------|------|
| `current_non_modal_owned_focus_leaves_managed_owner_below_tiles` | Bug characterization (non-modal hwnds) |
| `current_modal_owned_focus_leaves_managed_owner_below_tiles` | Bug characterization (modal hwnds) |
| `non_modal_owner_group_raises_popup_and_owner_above_tiles` | Desired helper behavior |
| `modal_owner_group_raises_dialog_and_owner_above_tiles` | Desired helper behavior |
| `floating_focus_owner_group_preserves_owned_sibling_order` | Multi-owned sibling order |
| `floating_focus_owner_group_noop_without_owner` | Unowned floater unchanged |

After wiring (A): flipped `current_*` into `non_modal_owned_focus_raises_managed_owner_above_tiles` / `modal_owned_focus_raises_managed_owner_above_tiles` (assert fixed chain via `normal_z_order_chain` + owner_of). Kept helper-level `*_owner_group_*` tests.

## Smoke after product fix

1. Helper **non-modal** owned + float parent + tile focus + Alt-Tab to popup → parent+popup above tiles.
2. Helper **modal** owned + same → parent+dialog above tiles; parent still disabled until dialog closes.
3. Unrelated floater peer below tiles; tile group contiguous.
4. Optional: VS modal dialog over floating VS main — same expectation.

## Constraints

- Do not merge PRs; soft `wm-exit` before hard kill; quote Program Files paths.
- Do not invent facts — Asus + layout.log verified both modal and non-modal.

