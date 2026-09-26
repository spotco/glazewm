# Global tiling direction + consistent Super+Shift(+Ctrl) move Plan

Date: 2026-09-26
Status: implementation, CLI smoke, and automated/runtime verification complete; visual tray hover remains desktop-only
Branch: `glazewm-spotcobuild` (GlazeWM) + `zebar-spotcobuild` (Zebar pack / provider as needed)
Scope: multi-day feature work (WM move semantics + global direction state + Zebar indicator + default config)

## Progress

- [x] Step 0 - Resolve open questions (blocking) — locked 2026-09-26
- [x] Step 1 - Document current contracts (commands, events, Zebar)
- [x] Step 2 - Add WM-global tiling direction state + commands/events/query
- [x] Step 3 - Retarget `toggle-tiling-direction` / Super+J to the global flag
- [x] Step 4 - Change `move --direction` to insert using global stack direction
- [x] Step 5 - Add opposite-direction move command + Super+Shift+Ctrl bindings
- [x] Step 6 - Unit tests for move matrices (H/V global × arrow × layouts)
- [x] Step 7 - Zebar: show + toggle global tiling direction
- [x] Step 8 - Make live `config.yaml` the spotcobuild sample default
- [x] Step 9 - CLI smoke, automated/runtime verification, and build/deploy readiness
- [ ] Desktop-only visual tray-hover observation (CLI equivalent verified)

## Objective

Make tiling **insertion / stack direction** a single **global** toggle (Super+J),
and make directional window moves always respect that global direction so
layouts stay predictable.

Today on this machine:

| Chord | Config binding | Internal command |
|-------|----------------|------------------|
| Super+Shift+Arrow (and H/K/L) | `lwin+shift+…` | `move --direction <left\|right\|up\|down>` |
| Super+J | `lwin+j` | `toggle-tiling-direction` |

Upstream GlazeWM does **not** store a per-window direction. It stores
`tiling_direction` on each **direction container** (workspace root + each
`SplitContainer`). `toggle-tiling-direction` flips the focused window’s
direction container (often by wrapping the window in an inverse split).
`move --direction` decides swap vs wrap vs `invert_workspace_tiling_direction`
by comparing the move arrow to the **parent** container’s direction.

Desired behavior (from user examples):

Starting from a layout like:

```
┌─┬─┐
│1│3│
├─┤ │
│2│ │
└─┴─┘
```

(ASCII: `13` / `23`, focus on **3**)

- Super+Shift+Left with **global = horizontal** → `31` / `32`
  (3 becomes left sibling of the 1/2 group; root stays horizontal)
- Super+Shift+Left with **global = vertical** → `1` / `2` / `3`
  (tree restructured so the stack axis is vertical)

Plus:

1. Global direction toggled by Super+J (not “whatever the focused split is”).
2. Zebar shows that **global** direction (tokyo-silence chip today shows
   `glazewm.tilingDirection` from focused-container query).
3. New Super+Shift+Ctrl+Arrow = same move algorithm but with the **opposite**
   of the current global direction.
4. Spotcobuild default sample config = current live
   `C:\Users\mooto\.glzr\glazewm\config.yaml` (including Omarchy-style
   bindings and `shutdown_commands` killing Zebar).

## Constraints and invariants

- Work on personal branches `glazewm-spotcobuild` / `zebar-spotcobuild` only;
  do not target upstream `glzr-io` defaults unless explicitly asked later.
- Keep layout **geometry** fully expressible as nested H/V splits (snapshot
  save/load continues to persist per-split `tilingDirection` for tree shape).
  Global direction is an **insertion policy**, not a second layout tree.
- Floating / fullscreen / workspace / monitor moves: preserve existing
  behavior unless a test case proves the global policy must apply there too
  (default assumption: global policy applies to **tiling** `move --direction`
  restructuring only; floating pixel nudge and cross-monitor workspace moves
  stay as today).
- Do not break IPC clients that already subscribe to
  `TilingDirectionChanged` / `query tiling-direction` — extend or add a
  parallel **global** field rather than silently changing meaning without a
  Zebar update in the same change set.
- Keep the legacy `TilingDirectionChanged` compatibility emission during the
  migration window. New clients must consume `GlobalTilingDirectionChanged`
  / `globalTilingDirection`; the two events are intentionally not equivalent.
- Prefer config bindings for Super+Shift+Ctrl chords; only add a new invoke
  command if “move with explicit stack direction” cannot be expressed as
  `set-tiling-direction` + `move` without races.
- No build/deploy until the implementation pass (this plan is design-only).

## Design notes

### Current move algorithm (summary)

`move_window_in_direction` (`packages/wm/src/commands/window/move_window_in_direction.rs`):

1. Flatten 1-child parent splits.
2. If parent direction **matches** the arrow → swap/move into sibling.
3. Else walk ancestors for a matching direction container and insert there.
4. Else `invert_workspace_tiling_direction` (wrap siblings, flip workspace
   direction, place window at edge).

That is why moves feel inconsistent: the stack axis is whatever local parent
history Super+J / previous moves left behind.

### Proposed model

Introduce `WmState.global_tiling_direction: TilingDirection` (default
Horizontal, or from config — see open questions).

**Super+J / `toggle-tiling-direction` (spotcobuild semantics) — LOCKED 2026-09-26:**

- Toggle WM-wide `global_tiling_direction` only (no wrap/flatten of focused window).
- Emit a new or extended event so Zebar updates immediately.
- Stop wrapping the focused window in an inverse split as a side effect of J
  (that side effect is the main source of “every focus has its own direction”).

**`move --direction D`:**

- Treat **global** direction as the stack axis for restructuring, not the
  focused parent’s direction.
- Conceptual rule (to implement + test):
  - Moving along an axis **parallel** to global → reorder as siblings on that
    axis (swap / insert before-after), creating/flattening splits as needed so
    the resulting sibling group’s direction == global.
  - Moving on the **orthogonal** axis → nest or reparent so the window joins
    (or creates) a split whose direction == global, matching the user’s
    horizontal vs vertical examples.
- Exact tree rewrite helpers should be extracted for unit tests (pure
  plan/apply if possible), rather than only mutating live `WmState`.

**Opposite move (Super+Shift+Ctrl+Arrow):**

- Preferred: new command
  `move --direction <dir> --tiling-direction <horizontal|vertical>`
  or `move --direction <dir> --opposite-tiling-direction`, which runs the same
  algorithm with `global.inverse()` for that one invocation without flipping
  the stored global flag.
- Config (spotcobuild default):

```yaml
  - commands: ['move --direction left --opposite-tiling-direction']
    bindings: ['lwin+shift+control+left', 'lwin+shift+control+h']
  # … down/up/right …
```

**Query / event surface:**

- Add `query global-tiling-direction` (and/or extend existing tiling-direction
  response with `globalTilingDirection`).
- Zebar provider exposes `globalTilingDirection` while keeping legacy
  `tilingDirection` as the focused-container direction. Older WM responses
  without the global field fall back to the legacy value; newer responses and
  events keep the two fields independent.
- tokyo-silence pack chip: bind icon + click to global toggle
  (`toggle-tiling-direction` after retarget, or explicit
  `toggle-global-tiling-direction`).

**Default config:**

- Replace `resources/assets/sample-config.yaml` (and any first-run copy source
  used by the installer/WM) with a sanitized copy of the live Asus config:
  Omarchy Super bindings, `shutdown_commands` → `taskkill /IM zebar.exe /F`,
  new Super+Shift+Ctrl move bindings, comments updated for global direction.
- Existing user `config.yaml` is **not** overwritten (LOCKED sample-only policy). Live Asus config already matches the intended spotcobuild defaults for this machine.

## Step 0 - Resolve open questions (blocking)

- [x] Answer questions in the chat widget / thread (see “Open questions”);
      there are no remaining open questions.
- [x] Fold the locked decisions below into the implementation before coding
      Steps 2–5.

## Step 1 - Document current contracts

- [x] Freeze a short “before” matrix of `move` outcomes on fixtures
      (2–3 window trees) under `plans/2026-09-26/fixtures/` or as unit-test
      snapshots, so we can assert “after” diffs.
- [x] Note CLI/IPC: `toggle-tiling-direction`, `set-tiling-direction`,
      `query tiling-direction`, event `tiling-direction-change`.
- [x] Note Zebar: `create-glazewm-provider.ts` + tokyo-silence
      `WorkspacesSection` chip (`glazewm.tilingDirection`).

## Step 2 - Add WM-global tiling direction state + commands/events/query

- [x] Add `global_tiling_direction` to `WmState` (and optional config default).
- [x] Persist `global_tiling_direction` in `layout.json` (LOCKED 2026-09-26) alongside the tree snapshot.
- [x] IPC query + event for global direction changes.
- [x] Unit tests: default, toggle, set, event payload.

## Step 3 - Retarget Super+J

- [x] Change `toggle_tiling_direction` / `set_tiling_direction` spotcobuild
      behavior to flip/set the **global** flag (per Step 0).
- [x] Ensure focused-container query either follows global or Zebar switches
      to the new field in the same PR.
- [x] Update sample/default keybinding comments (`Mod+J - toggle global
      tiling direction`).

## Step 4 - Global-aware `move --direction`

- [x] Refactor tiling branch of `move_window_in_direction` to take an
      explicit `stack_direction: TilingDirection` argument (from global).
- [x] Implement consistent insert/reorder rules covering the user’s
      `13/23` → horizontal vs vertical examples.
- [x] Keep floating / fullscreen / cross-monitor paths behind clear
      early-returns.
- [x] Add debug lines to `layout.log` for move decisions (direction, global,
      before/after brief tree) — helpful for Asus smoke.

## Step 5 - Opposite stack-direction move + bindings

- [x] Add CLI/IPC flag or command for opposite (or explicit) stack direction.
- [x] Wire Super+Shift+Ctrl+Arrow(+HJKL) in spotcobuild sample + live config.
- [x] Confirm no chord collisions with existing
      `lwin+ctrl+shift+f4` (wm-exit) etc.

## Step 6 - Automated tests

- [x] Table-driven tests for:
      - global H/V × move L/R/U/D on the `13/23` fixture
      - inner-window focus across all four arrows on the deeper
        `H[V[1 2] V[3 4]]` fixture
      - deeper nested splits (3+ levels)
      - only-child / single-window workspace
      - opposite-flag moves do not mutate stored global
- [x] Regression: layout snapshot save/load still round-trips nested
      directions (geometry), independent of global flag.

## Step 7 - Zebar updates (vendor tokyo-silence as spotcobuild default)

LOCKED 2026-09-26: copy live marketplace pack into the Zebar fork, set it as
the first-run / spotcobuild default, and make the global-direction chip edits
there (long-term home for future bar tweaks too).

Source today: `%AppData%\zebar\downloads\y4m3.tokyo-silence@1.0.1`
(`settings.json` startupConfigs → pack `y4m3.tokyo-silence`, widget `bar`).
Upstream is MIT (Copyright y4m3); keep LICENSE + NOTICE and credit in README.
Built-in pack pattern already exists: `resources/starter` +
`STARTER_PACK_ID = "glzr-io.starter"` in `marketplace_installer.rs`.

- [x] Copy pack into `resources/tokyo-silence` (or `resources/spotcobuild-bar`)
      under `F:\dev\zebar`.
- [x] Pack id **`spotco.tokyo-silence`** (LOCKED) so a
      marketplace update of `y4m3.tokyo-silence` cannot overwrite your edits.
- [x] Wire embed/install like starter: embed resource, install on first run,
      change default `startupConfigs` / `STARTER_PACK_ID` (or parallel
      spotcobuild constant) to the new pack + `bar` / `default`.
- [x] Provider: surface `globalTilingDirection` (and events).
- [x] In the **vendored** pack: chip shows global H/V; click runs global toggle.
- [x] Migrate this machine: point `~\.glzr\zebar\settings.json` at the
      vendored pack id (leave marketplace download alone or remove later).
- [x] Note in pack README: fork of y4m3/tokyo-silence for spotcobuild.


## Step 8 - Spotcobuild default config

- [x] Copy live Asus `config.yaml` → `resources/assets/sample-config.yaml`
      (review for machine-specific paths; keep generic where possible).
- [x] Include new opposite-move bindings + comment block for global
      direction semantics.
- [x] Commit on `glazewm-spotcobuild`.

## Step 9 - Verification

```bat
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test -p wm-common --lib
# `wm` is a bin-only package; run its unit tests through the binary target.
cargo test -p wm --bin glazewm
build.bat
```

Manual Asus smoke:

1. Hover tray: `spotcobuild-…` still present.
2. Super+J flips Zebar chip only (and global query); tree does not wrap
   unexpectedly (per Step 0).
3. Reproduce `13/23` focus-3 Super+Shift+Left under H vs V global.
4. Super+Shift+Ctrl+Left uses opposite stack axis without flipping J state.
5. Soft `wm-exit` still kills Zebar via `shutdown_commands`.

CLI smoke harness:

`scripts\smoke_global_tiling_direction.cmd` runs the equivalent runtime checks
without physical key injection. Pass `-TestSoftExit` to include the shutdown
and restart check. The harness records every CLI call/result, structural layout
hash, and relevant `layout.log` tail in
`plans/2026-09-26/GLOBAL_TILING_DIRECTION_SMOKE.log`. It snapshots and restores
the live layout so move checks are non-destructive.

Verification run 2026-09-26:

- [x] `cargo fmt --all -- --check`
- [x] `cargo clippy --all-targets --all-features -- -D warnings`
- [x] `cargo test -p wm-common --lib` (72 passed)
- [x] `cargo test -p wm --bin glazewm` (41 passed)
- [x] `build.bat` (release WM, CLI, and watcher artifacts)
- [x] Zebar provider tests (17 passed), full Zebar `build.bat`, and release
      deployment completed.
- [x] Added explicit provider-state coverage proving legacy
      `tilingDirection` remains local while `globalTilingDirection` remains
      WM-wide, with fallback behavior for older WM responses.
- [x] Deployed with the existing backup/deploy scripts; current WM starts and
      `query global-tiling-direction` returns successfully.
- [x] Opposite-move config command parses and the live config starts without
      the previous `keybindings[8].commands` fatal error (the screenshot error
      was caused by the direction selector being mutually exclusive with the
      opposite flag).
- [x] CLI toggle changed global horizontal → vertical while the structural
      layout tree stayed unchanged, then restored horizontal.
- [x] Vendored `spotco.tokyo-silence` bar launched from the deployed Zebar
      build; its cached bundle matches the fork and WM reports the ignored
      `Zebar - spotco.tokyo-silence / bar` window.
- [x] CLI-equivalent Super+J / Super+Shift+Ctrl+Arrow checks passed, including
      horizontal and vertical normal moves, opposite-axis moves, structural
      H→V→H preservation, spotco bar detection, and soft-exit Zebar shutdown
      and restart. See `GLOBAL_TILING_DIRECTION_SMOKE.log`.
- [ ] Physical key injection and visual tray-hover appearance remain desktop-only
      observations; the CLI harness verifies the tray bar process/window identity.

## Decisions (Step 0)

1. **Super+J side effects:** LOCKED — flip global flag only; no wrap/flatten.
2. **Scope of "global":** LOCKED — one flag for the whole WM (not per-workspace).
3. **Persistence:** LOCKED — survive restart via `layout.json` (with the layout snapshot).
4. **Size on restructure:** LOCKED — equalize tiling shares (same spirit as today's invert 0.5 path).
5. **Zebar pack:** LOCKED — vendor tokyo-silence into the Zebar fork as the spotcobuild default; pack id **`spotco.tokyo-silence`**; keep MIT license/attribution to y4m3; chip + future bar tweaks live in that tree.
6. **Default config:** LOCKED — replace `resources/assets/sample-config.yaml` only (first-run / fresh installs). Do **not** overwrite or auto-merge existing `%USERPROFILE%\.glzr\glazewm\config.yaml`.

## Open questions

_(none — Step 0 complete)_

## Non-goals (this plan)

- Changing focus chords (Super+Arrow without Shift).
- Rewriting the layout snapshot schema.
- Upstreaming to `glzr-io/glazewm` (spotcobuild-first).
