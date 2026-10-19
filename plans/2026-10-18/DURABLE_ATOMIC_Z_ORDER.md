# Durable atomic z-order reconciliation

## Baseline and failure

The Windows branch starts from `ed3bcef` (`origin/glazewm-spotcobuild`).
Normal z-order is currently stored as a flat `Vec<WindowId>` per workspace.
Although reconciliation packs tiled HWNDs into a contiguous block, it infers
promotion from the focused window's state. A lifecycle fallback to a tile is
therefore indistinguishable from explicit tile activation. The Windows native
writer then replays the workspace chain from `HWND_TOP`, lifting unrelated
windows and omitted native windows.

The PowerToys reproduction has an ownerless `EditShortcutsWindow`. On close,
the saved logical focus does not identify Settings as its owner. A recent
unmanage time window discards Settings' valid native activation, then the
tiling focus path promotes the complete tile band over Settings.

The guest baseline reproduction log confirms the dialog enters the workspace
stack before it is removed. In `F:\dev\glazewm\Temp\repro-km-click-ok.log`,
the live dialog HWND `329266` appears in the desired chain at 02:26:16.831.
At 02:26:17.495 it is removed with `owner=None`; 17 ms later, reconciliation
uses `layer=Some(Tiling)` and requests `[198168, 66804, 67018, 197896]`.
The immediate native sample is `[198168, 67018, 66804, 197896]`: the tiled
chain splits around PowerToys Settings. The Asus state dump independently
shows the event order: sequence 1194 discards Settings' focus as
`override_recent_unmanage`, sequence 1195 selects the tiled terminal, and
sequence 1196 reconciles at the tiling layer. The dialog is ownerless in both
the guest log and the state dump, so the saved atom below it is the correct
fallback source.

## Invariants

1. A workspace stack contains ordered atoms: one tile-group atom and one atom
   for each live detached or ignored normal window. An atom is never duplicated.
2. Removing or inserting a detached atom leaves all pre-existing surviving
   atoms in the same pairwise order. Reclassification changes only the atom
   representing that window (or the existing tile-group membership).
3. Focus recovery changes focus intent only. It does not enqueue stack
   activation. Explicit user/native or WM activation requests promotion.
4. Tiled HWND order is internal to its group. Native reconciliation materializes
   the group contiguously while preserving the group's external atom position.
5. Reconciliation does no native write when the observed participants already
   satisfy the desired order. Geometry-only SetWindowPos calls use
   `SWP_NOZORDER`.
6. Win32 owner and topmost behavior is an explicit constraint. An activation of
   a detached owned window may promote its smallest managed owner family;
   reconciliation does not invent activation.

## Intended design

- Add a platform-independent `NormalWindowStack` reducer with explicit
  `InsertDetached`, `Remove`, `Reclassify`, `Activate`, and `Reconcile`
  operations. The WM state owns one ordered atom stack per workspace; current
  live HWND membership is materialized into the tile atom at reconciliation.
- Keep focus intent (`ExplicitActivation`, `ObservedActivation`,
  `LifecycleRecovery`, `Reconcile`) separate from stack operations. Replace the
  broad recent-unmanage focus override with an expected recovery target tied to
  that transition. Resolve fallback in this order: verified live owner, nearest
  eligible atom below the removed atom, then existing workspace focus history.
- Make Windows stack synchronization consume a desired atom order. Apply
  explicit activation separately from preservation reconciliation. Compare
  observed participant ranks first and skip already-satisfied orders. Delayed
  workers carry a generation and live HWND identity, and only repair the same
  desired operation while its activation/reconciliation context remains valid.
- Keep topmost transitions separate from normal-window order. Geometry-only
  repositioning must not pass a normal z-order request.
- Emit operation reason, atom order and observed/converged native order in the
  existing z-order diagnostic path, with verbose details gated as today.

## Validation plan

- Pure reducer unit and randomized state-machine tests for lifecycle,
  reclassification, owner-family activation, duplicate notifications,
  idempotence and pairwise preservation.
- Existing Windows HWND tests extended for relative reconciliation, no-op
  preservation, owner groups, delayed generation cancellation, and foreign
  threads. A synthetic dialog/owner E2E exercises nested and independent
  detached windows without PowerToys.
- Run formatting, workspace tests and Windows tests on `GLAZEWIN11PRO` when the
  guest is available. Record unexecuted host-specific checks explicitly.

## Win32 constraints

`SetWindowPos` insert-after operations can move a window across unrelated
native windows, while owner windows are kept above their owned windows by the
system and topmost windows occupy a distinct band. Preserve-mode reconciliation
must therefore avoid writes when only the WM participant order is already
correct, and should use native anchors rather than treating `HWND_TOP` as a
generic chain head. Genuine owner/topmost ordering may prevent exact atom order;
such cases must be logged as a constraint rather than repeatedly fought.

Windows identity checks use HWND, process ID, GUI thread ID, and class name.
This rejects ordinary handle reuse before focus recovery, geometry sync, or
stack membership accepts the stale wrapper. Windows does not expose a durable
generation token for the rare case where the same process and GUI thread
recreates the same class on a recycled HWND; delayed native retries also
validate owner identity and their current workspace generation.

## Candidate Windows regression run

On 2026-10-19, the release build from this branch was launched in the
interactive `GLAZEWIN11PRO` session from the isolated
`F:\dev\glazewm-atomic-verify` worktree. The PowerToys Keyboard Manager
"Remap a shortcut" dialog was opened and closed with OK three times on the
initial candidate, then once more after rebuilding the final source, using the
existing UI Automation reproduction script. All four runs completed with an
ownerless `EditShortcutsWindow` removal followed by a
`ZOrderIntent::Preserve` reconciliation. On the final rebuilt candidate,
Settings was native z-order row 0 and the PowerShell, Brave, and Windows
Terminal tile HWNDs occupied consecutive rows 1–3. The recovery target was
the Settings window, `dialog-still=0`, and GlazeWM remained alive at the same
candidate PID. Captures are
`F:\dev\glazewm\Temp\repro-km-atomic-cycle-1.log` through
`repro-km-atomic-cycle-3.log` and
`F:\dev\glazewm\Temp\repro-km-atomic-final-cycle.log`.

The live native `windows-debug` snapshot on the initial candidate showed the
explicit activation contract too: focusing a tiled window put both tiled HWNDs
together above PowerToys Settings; explicitly focusing Settings put it above
both tiled HWNDs again. The candidate and its temporary scheduled tasks were
removed afterward. The guest is back on its installed GlazeWM build with one
watcher process.

The Asus state-dump host was unavailable over SSH during this session. The
baseline event-order evidence above comes from the user-provided state dump
summary and the accessible `GLAZEWIN11PRO` repro log.

## Validation results

All commands below ran on `GLAZEWIN11PRO` against the final branch source unless
specified otherwise.

- `cargo fmt --all -- --check`, `cargo check --workspace --all-targets`, and
  `cargo clippy --workspace --all-targets` passed. Remaining warnings are
  existing workspace manifest and unrelated unused/dead-code warnings.
- `cargo test -p wm --bin glazewm normal_window_stack::tests --
  --test-threads=1`: 11 passed. The new focus recovery owner/stack selection
  test also passed (1 passed).
- Native HWND tests passed for the synthetic ownerless nested-dialog close and
  atom preservation (1 passed), native z-order convergence/foreign queues
  (10 passed), workspace retry generation isolation (1 passed), and HWND
  identity reuse checks (1 passed).
- `cargo test -p wm-platform --test test -- --skip test_nearest_display
  --test-threads=1`: 67 passed, 1 failed, 1 filtered. The failure is the
  existing `layered_attributes_stay_bounded_during_style_stall` test; it also
  failed on the unmodified base commit `ed3bcef`. `test_nearest_display` needs
  a monitor and is unavailable in the non-interactive test session.
- `cargo test -p wm --bin glazewm -- --test-threads=1`: 72 passed, 19 failed.
  The failures require a monitor and report `Display device not found` in the
  SSH/session-0 test environment; the new stack and recovery tests passed.
- A full `cargo test --workspace --no-fail-fast` run was attempted but did not
  complete: parallel native GUI tests exposed timing-sensitive failures in
  existing hung-window/style-stall tests, and the runner stopped making
  progress after printing its results. It was terminated and is not counted as
  passing.
- PowerToys E2E passed on the final release candidate in the interactive
  guest, as recorded above. Asus state-dump access was unavailable over SSH,
  so no new dump was captured from that host.
