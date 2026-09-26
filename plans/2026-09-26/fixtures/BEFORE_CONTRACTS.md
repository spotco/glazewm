# Step 1 — Current contracts (before global tiling direction)

Date: 2026-09-26
Branch: glazewm-spotcobuild

## Commands / IPC (today)

| Surface | Name | Behavior |
|---------|------|----------|
| Invoke | `toggle-tiling-direction` | Flip focused window's direction container (wrap in inverse split or flatten / flip workspace) |
| Invoke | `set-tiling-direction --tiling-direction <h\|v>` | Toggle if focused container direction differs |
| Invoke | `move --direction <left\|right\|up\|down>` | Swap / insert using **parent** tiling direction vs arrow axis; else walk ancestors; else `invert_workspace_tiling_direction` |
| Query | `tiling-direction` | Focused container's direction container + its `tilingDirection` |
| Event | `tiling-direction-change` / `TilingDirectionChanged` | Emitted after local toggle/set; payload `directionContainer` + `newTilingDirection` |

Key sources:
- `packages/wm/src/commands/window/move_window_in_direction.rs`
- `packages/wm/src/commands/container/toggle_tiling_direction.rs`
- `packages/wm/src/ipc_server.rs` (`QueryCommand::TilingDirection`)
- `packages/wm-common/src/wm_event.rs`, `app_command.rs`, `ipc.rs` (`TilingDirectionData`)

## Zebar (today)

- Provider: `create-glazewm-provider.ts` exposes `tilingDirection` from focused-container query / events.
- Pack in use: marketplace `y4m3.tokyo-silence` (`settings.json` → widget `bar`).
- Chip in WorkspacesSection binds icon + click to `glazewm.tilingDirection` / `toggle-tiling-direction`.

## Fixture: `13/23` (focus on 3)

ASCII (workspace horizontal):

```
┌─┬─┐
│1│3│
├─┤ │
│2│ │
└─┴─┘
```

Tree (DFS): `H[ V[1 2] 3 ]` — workspace `tilingDirection=horizontal`, left child vertical split of 1+2, right child window 3.

### Before matrix (parent-local move — approximate current)

Local parent of 3 is the workspace (H). Super+Shift+Left matches H → swap/move with left sibling `V[1 2]`. Current Split-sibling path **dives into** the split (not a pure sibling reorder), so outcomes depend on nest depth — this inconsistency is why we are changing the algorithm.

### After matrix (LOCKED target)

| Global stack | Chord | Result tree (equalized) | ASCII |
|--------------|-------|-------------------------|-------|
| Horizontal | Super+Shift+Left | `H[ 3 V[1 2] ]` | `31/32` |
| Vertical | Super+Shift+Left | `V[ 1 2 3 ]` | `1/2/3` |
| Horizontal | Super+Shift+Ctrl+Left (opposite) | same as global Vertical + Left | `V[1 2 3]` |
| Vertical | Super+Shift+Ctrl+Left (opposite) | same as global Horizontal + Left | `H[3 V[1 2]]` |

Notes:
- Opposite-move must **not** mutate stored `globalTilingDirection`.
- Super+J toggles global only (no wrap/flatten).
- On restructure, equalize tiling shares (invert 0.5 spirit / equal siblings).
