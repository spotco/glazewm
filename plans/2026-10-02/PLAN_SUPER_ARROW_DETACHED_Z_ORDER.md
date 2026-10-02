# Plan: Super+arrow detached z-order flash / stuck floaters between tiles

Date: 2026-10-02
Status: PR #13 review fixes pushed (box); Windows smoke/deploy pending Asus return
Branch: `bugfix/super-arrow-z-order-hung-helper`
Prior branch work preserved from `bugfix/issue-10-skip-hung-style-writes` HEAD
Scope: document confirmed Super+arrow z-order bugs, fix tiling bring_to_front race + hung-HWND reorder failure, floating Super+arrow dual-applicator, hung recovery, TID probe cache; test; deploy with `general.verbose_z_order: true`

## Progress

- [x] Step 0 - Confirm evidence from `layout.log` (~00:33 ET / 04:33Z Oct 2) — tiling flash / floaters between tiles
- [x] Step 1 - Write this plan (issues + observations + invariants)
- [x] Step 2a - Skip per-window bring_to_front on tiling focus; queue reorder from `focus_in_direction` (`72037050`)
- [x] Step 2b - Hung tiled HWND follow-up: make chain apply skip hung anchors / avoid HWND_NOTOPMOST raise races
- [x] Step 3 - Automated tests: (a) tiling-group defer / no floater-between-tiles; (b) hung helper reorder leaves floater above until fix
- [x] Step 4 - Verify new hung test FAILS on current code, then fix, then PASS
- [x] Step 5 - Soft `wm-exit`, deploy to Program Files; leave verbose on
- [x] Step 6 - Report branch, plan updates, fail-then-pass evidence, fix approach, deploy proof
- [x] Step 7 - Commit, push, open PR for review
- [x] Step 8 - Review fix: floating Super+arrow mark targeted + defer when workspace reorder queued
- [x] Step 9 - Review fix: hung HWND bounded delayed reconcile (100/250/500ms) + generation cancel
- [x] Step 10 - Review fix: cache responsiveness probe by TID per chain apply
- [x] Step 11 - Plan: document keep `SWP_ASYNCWINDOWPOS` (do not switch responsive path to sync)
- [x] Step 12 - Rebase onto current `glazewm-spotcobuild` (PR #12 squash) when pushing
- [ ] Step 13 - Windows-only integration tests + Asus deploy/smoke on return

## Objective

Stop Super+arrow `focus --direction` from flashing (and sometimes leaving)
detached/floating windows between non-focused tiled windows. Tiled windows
must remain one contiguous z-order group; focusing a detached window must
promote only that window. Focusing a hung tiled HWND must not leave floaters
stuck above the responsive tiled group.

## Confirmed bug A (layout.log 2026-10-02 ~00:33 ET) — FIXED

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
  67552, 329284` - tile `67552` sunk below floaters; tiled block split.

User-visible result: floaters flash above non-focused tiles; last move can
leave floaters stuck above non-focused tiles while the focused tile sits on
top.

### Root cause A

Two z-order applicators fight under `SWP_ASYNCWINDOWPOS`:

| Path | What it does | Problem |
|------|----------------|---------|
| `windows_to_bring_to_front` + per-window `set_z_order` | Places each tiling peer with `AfterWindow(focused)` | Splits the tiled block relative to floaters; async SWPs can land *after* the workspace chain |
| `reorder_focused_workspace_layers` + `reorder_z_order` | Applies one top-to-bottom chain; 10ms generation-aware retry | Correct intent, but raced by earlier bring_to_front SWPs; rapid Super+arrow bumps `Z_ORDER_GENERATION` via `begin_z_order_batch` and cancels the 10ms retry |

Detached focus already bypasses the per-window path when
`focused_window_to_bring_to_front` is set. Tiling focus did not.

### Fix A (landed in `72037050`)

1. On tiling focus, **skip** per-window `bring_to_front` `set_z_order` for
   windows that are not also being geometry-redrawn (`tiling_group_defer_workspace_reorder`).
2. Rely on `reorder_focused_workspace_layers` as the sole normal-layer
   applicator after `SetForegroundWindow`.
3. Queue `workspace_to_reorder` from `focus_in_direction` so the first
   `platform_sync` after Super+arrow runs SFW and the chain apply in the
   same drain.
4. Preserve detached-focus behavior (targeted floating / ignored promote
   only the selected hwnd).

## Confirmed bug B (layout.log 2026-10-02 ~01:02 ET / 05:02Z) — FIXED

Repro: Super+arrow onto hung tiled **"GlazeWM debug helper" / non-pumping
z-order helper** (`HWND 2168898`) while Steam and other floaters exist.

Observed:

1. Focus correctly lands on hung tiled `2168898` (`SetForegroundWindow` ok).
2. `skip bring_to_front` with `tiling_group_defer_workspace_reorder` runs
   (Fix A working as designed).
3. `reorder_focused_workspace_layers` intends tiles-then-floaters with hung
   hwnd first in the chain.
4. `post-reorder` and especially `post-reorder+25ms` show
   `match_intended=false` with **floaters on top**.

Smoking-gun sample (`2026-10-02T05:02:35.628Z` / `05:02:39.830Z`):

- intended: `2168898, 265086, 196668, 399988, 198896, 330202, 788234`
- native +25ms: `330202, 788234, 2168898, 265086, ...` or
  `198896, 330202, 788234, 2168898, ...` — Steam/floaters above tiles.

### Root cause B

`apply_z_order_chain` always issues async `HWND_NOTOPMOST` then
`insert_after` for **every** hwnd, including hung and floaters:

1. Hung focused hwnd's SWPs never apply (GUI thread not pumping).
2. Responsive peers use `AfterWindow(hung)` which anchors at hung's **old**
   (low) position — tiles stay below floaters.
3. Separate async `HWND_NOTOPMOST` on floaters raises them to the top of the
   normal band; if the follow-up `insert_after` races or anchors badly,
   floaters stay on top at +25ms.
4. The 10ms generation-aware retry does not repair this when the hung hwnd
   remains an unusable anchor — pure timing dependence.

### Fix B (landed)

1. Probe each hwnd (`WM_NULL` / responsive check) before chain apply.
2. **Skip** SetWindowPos on hung/non-responsive hwnds entirely.
3. Use last **successfully placed responsive** hwnd as `insert_after`
   (or `HWND_TOP` for the first responsive window) so tiles rise above
   floaters even when the focused tile is hung.
4. Call `HWND_NOTOPMOST` only when the window is actually topmost — avoid
   raising normal floaters via a redundant NOTOPMOST.
5. **Keep `SWP_ASYNCWINDOWPOS`** for all chain apply SetWindowPos calls
   (responsive and hung). A slow-but-pumping foreign queue must never
   block the WM loop; hung skip + insert-after-last-responsive is the
   correctness fix, not sync SWP. (Earlier draft suggested sync for
   responsive hwnds — rejected; code keeps ASYNC.)
6. Keep Fix A skip-bring_to_front for normal tiling focus (no regression:
   tiles-as-one-group, detached focus only promotes selected, Super+arrow
   no flash on normal tiles).

## Invariants (must hold after fix)

1. Tiled windows = one contiguous z-order group.
2. Focusing a detached/ignored window promotes **only** that window; peers
   keep their relation to the tiled block.
3. Focusing a tiled window promotes the **entire** tiled group above
   detached windows without reshuffling detached peer order.
4. Focusing a hung tiled hwnd still raises the **responsive** tiled peers
   above floaters (hung hwnd may stay out of place; floaters must not cover
   the group).
5. Verbose z-order logging (`general.verbose_z_order` /
   `GLAZEWM_VERBOSE_Z_ORDER=1`) stays available for verification.


## Review follow-ups (PR #13)

### Bug C — floating Super+arrow dual applicators (MUST FIX)

`focus_in_direction` queues `workspace_to_reorder` for every directional
focus, but defer only covered tiling / `focused_window_to_bring_to_front`.
Super+arrow among normal floaters still ran the legacy AfterWindow swarm
*and* the workspace chain → peer floaters flash / wrong promote.

**Fix C:**

1. Mark floating directional targets via
   `queue_focused_window_to_bring_to_front` (same as native floating focus)
   so `windows_to_bring_to_front` selects only that hwnd.
2. Expand `should_defer_bring_to_front_to_workspace_reorder` to also defer
   whenever the workspace reorder is already queued / focus update pending.
3. Invariant unchanged: focusing a detached window promotes **only** that
   window. Regression coverage: 3 floaters + tiles, Super+Left/Right among
   floaters (`floating_directional_focus_chain_promotes_only_selected_floater`).

### Hung HWND eventual repair

Skipping hung anchors leaves the hung hwnd out of native order until
something re-applies. After Fix B, schedule generation-aware recovery
retries at **100 / 250 / 500 ms** when any hwnd was skipped. Cancel when
`Z_ORDER_GENERATION` advances. Stop early once a retry places the full
responsive set (no remaining skips). Test:
`hung_hwnd_eventually_rejoins_chain_after_resume`.

### TID probe cache

`apply_z_order_chain` caches `WM_NULL` responsiveness by GUI thread id for
the duration of one chain apply so N hwnds on one hung TID cost one 50ms
probe, not N×50ms.

### Pending TOPMOST timing

`reorder_z_order_retries_across_independent_foreign_gui_queues` remains the
coverage for delayed TOPMOST foreign queues under ASYNC. Hung recovery
retries may also help convergence; further TOPMOST-specific changes deferred
unless Windows CI flakes.

## Constraints

- Commit/push/PR approved by user.
- Quote `"C:\Program Files\glzr.io\GlazeWM\..."` paths; soft `wm-exit`
  before any `taskkill`.
- Do not overwrite unrelated user config; keep `general.verbose_z_order: true`.

## Smoke-test (after deploy)

1. Several tiled windows + ≥2 floaters (e.g. Grok Bot, Steam).
2. Rapid Super+arrow around **normal** tiles: no floater flash between tiles.
3. Focus a floater (click / Alt-Tab): only that floater rises; other
   floaters and tile block order preserved as before.
4. Return focus to a tile: whole tile group rises; floaters stay below as
   a group without splitting tiles.
5. If a hung debug/non-pumping helper is tiled: Super+arrow onto it must
   not leave Steam/floaters stuck above the other tiles.
6. Optional: watch `layout.log` for `skip bring_to_front` + improved
   `match_intended` after hung-aware chain apply.
