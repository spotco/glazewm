# Plan: Detached Super+arrow cycle after workspace move

Date: 2026-10-02
Status: done — committed locally; deployed Asus for smoke
Branch: `bugfix/detached-super-arrow-cycle-after-ws-move`
Base: `glazewm-spotcobuild` @ `3ce3e7f7`

## Objective

When a detached (floating) window is transferred between workspaces (or
detached/reattached), Super+arrow (`focus --direction` left/right) among
floating windows on the focused workspace must include it. Repro: move
detached Steam to WKS2, move back, Super+arrow cycle — Steam must be in
the cycle.

## Root cause

`floating_focus_target` built the cycle from **immediate floating
siblings** and wrapped with `floating_siblings.last()` (Left) /
`.next()` (Right). That is not a ring:

- Left from the last floater went to the second-to-last, not the first.
- Right from the first floater went to the second, not the last.

`move --workspace` appends the transferred floater at
`target_workspace.child_count()` (last). After Steam WKS2→back, Steam
sits last; Super+Right from an older floater never reached it.

## Fix

1. Collect floating windows on the **focused workspace** (direct children,
   plus any nested floating descendants as a safety net).
2. Ring wrap: Left = `(i+1)%n`, Right = `(i+n-1)%n` (same next/prev sense
   as before). Membership tracks workspace + Floating state across
   detach/reattach and transfers.

## Steps

- [x] Step 1 - Write this plan
- [x] Step 2 - Reproduce with failing unit test(s) (ring + transfer)
- [x] Step 3 - Fix floating cycle membership
- [x] Step 4 - Add detach/reattach + nested + transfer coverage; run tests
- [x] Step 5 - Soft-exit; build+deploy; commit on branch

## Tests added (`focus_in_direction.rs`)

- `floating_cycle_is_full_ring_including_appended_peer`
- `floating_cycle_includes_window_after_workspace_transfer_both_ways`
- `floating_cycle_survives_detach_reattach_and_nested_floater`

## Smoke checklist (user)

1. Detach/float Steam + ≥1 other floater on WKS1.
2. Super+arrow among floaters — Steam in cycle (both Left and Right).
3. Super+Shift+2 (move+follow) Steam to WKS2; Super+Shift+1 back.
4. Focus a floater on WKS1; Super+Left and Super+Right — Steam must appear.
5. Toggle Steam tiling then floating again; still in cycle.
6. `verbose_z_order: true` left unchanged.
