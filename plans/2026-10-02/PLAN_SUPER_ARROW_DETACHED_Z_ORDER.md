# Plan: Super+arrow detached z-order flash / stuck floaters between tiles

Date: 2026-10-02
Status: implemented + deployed for user smoke-test
Branch: `bugfix/issue-10-skip-hung-style-writes` (local; do not push unless asked)
Scope: document confirmed Super+arrow z-order bug, fix tiling bring_to_front race, test, deploy with `general.verbose_z_order: true`

## Progress

- [x] Step 0 - Confirm evidence from `layout.log` (~00:33 ET / 04:33Z Oct 2)
- [x] Step 1 - Write this plan (issues + observations + invariants)
- [ ] Step 2 - Implement fix in `platform_sync` (+ same-sync reorder queue)
- [x] Step 3 - `cargo test` for wm-common / wm / wm-platform as applicable (wm 76 ok, wm-common 87 ok, wm-platform 50 ok + 3 flakes that pass alone)
- [x] Step 4 - Commit locally (no push) — `720370503ec8944410227caadaf3dca80130fd81`
- [x] Step 5 - Soft `wm-exit`, deploy to Program Files, restart; leave verbose on — PID 20684 from `"C:\Program Files\glzr.io\GlazeWM\glazewm.exe"` @ 12:55:49 ET
- [x] Step 6 - Report plan path, commit sha, test counts, deploy proof, smoke-test notes

## Objective

Stop Super+arrow `focus --direction` from flashing (and sometimes leaving)
detached/floating windows between non-focused tiled windows. Tiled windows
must remain one contiguous z-order group; focusing a detached window must
promote only that window.

## Confirmed bug (layout.log 2026-10-02 ~00:33 ET)

Repro: Super+arrow among tiled windows while floaters exist (Grok Bot,
Roblox, Steam, etc.).

Observed sequence per focus:

1. `SetForegroundWindow` on the newly focused tile.
2. Per-tile `bring_to_front` `set_z_order` with `AfterWindow(focused)` for
   every tiling peer, then `Normal` on the focused hwnd.
3. `reorder_focused_workspace_layers` intends
   `[all tiles..., then floaters...]`.
4. Native sample often has `match_intended=false` at pre-reorder,
   post-reorder, and sometimes `post-reorder+25ms`.

Smoking-gun sample (`2026-10-02T04:33:00.796Z`):

- intended: `67862, 265210, 67552, 265086, 2820358, 330202, 917752, 329284`
  (tiles then floaters)
- native post-reorder: `67862, 265210, 265086, 2820358, 330202, 917752,
  67552, 329284` — tile `67552` sunk below floaters; tiled block split.

User-visible result: floaters flash above non-focused tiles; last move can
leave floaters stuck above non-focused tiles while the focused tile sits on
top.

## Root cause

Two z-order applicators fight under `SWP_ASYNCWINDOWPOS`:

| Path | What it does | Problem |
|------|----------------|---------|
| `windows_to_bring_to_front` + per-window `set_z_order` | Places each tiling peer with `AfterWindow(focused)` | Splits the tiled block relative to floaters; async SWPs can land *after* the workspace chain |
| `reorder_focused_workspace_layers` + `reorder_z_order` | Applies one top-to-bottom chain; 10ms generation-aware retry | Correct intent, but raced by earlier bring_to_front SWPs; rapid Super+arrow bumps `Z_ORDER_GENERATION` via `begin_z_order_batch` and cancels the 10ms retry |

Detached focus already bypasses the per-window path when
`focused_window_to_bring_to_front` is set. Tiling focus does not, so the
legacy group bring_to_front still runs and undoes the layer reconcile.

## Invariants (must hold after fix)

1. Tiled windows = one contiguous z-order group.
2. Focusing a detached/ignored window promotes **only** that window; peers
   keep their relation to the tiled block.
3. Focusing a tiled window promotes the **entire** tiled group above
   detached windows without reshuffling detached peer order.
4. Verbose z-order logging (`general.verbose_z_order` /
   `GLAZEWM_VERBOSE_Z_ORDER=1`) stays available for verification.

## Intended fix

1. On tiling focus (and generally whenever workspace layer reorder will
   run), **skip** per-window `bring_to_front` `set_z_order` for windows that
   are not also being geometry-redrawn. Log the skip under verbose mode.
2. Rely on `reorder_focused_workspace_layers` as the sole normal-layer
   applicator after `SetForegroundWindow`.
3. Queue `workspace_to_reorder` from `focus_in_direction` so the first
   `platform_sync` after Super+arrow runs SFW and the chain apply in the
   same drain (no intervening bring_to_front SWPs).
4. Preserve detached-focus behavior (targeted floating / ignored promote
   only the selected hwnd).
5. Do not change live user config except keep
   `general.verbose_z_order: true`.

## Constraints

- Commit locally with a clear message; **do not push** unless asked.
- Quote `"C:\Program Files\glzr.io\GlazeWM\..."` paths; soft `wm-exit`
  before any `taskkill`.
- Do not overwrite unrelated user config.

## Smoke-test (after deploy)

1. Several tiled windows + ≥2 floaters (e.g. Grok Bot, Steam).
2. Rapid Super+arrow around tiles: no floater flash between tiles.
3. Focus a floater (click / Alt-Tab): only that floater rises; other
   floaters and tile block order preserved as before.
4. Return focus to a tile: whole tile group rises; floaters stay below as
   a group without splitting tiles.
5. Optional: watch `layout.log` for `skip bring_to_front` +
   `post-reorder+25ms: match_intended=true` on tiling focus.
