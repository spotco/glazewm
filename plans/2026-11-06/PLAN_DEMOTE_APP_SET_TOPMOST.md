# Issue: managed windows can retain app-set topmost state

Date: 2026-11-06

Status: R1–R7 and F1–F3 review follow-ups are implemented; local and remote
Windows validation pass.

## Symptoms

A managed floating window configured with `shown_on_top: false` can set the
Windows `WS_EX_TOPMOST` style itself. When another managed window is activated,
focus changes but the floating window can remain visually above the active
window.

The related suspended-window regression covers a managed HWND that stops
pumping messages beyond the initial recovery window. After it resumes, the
z-order chain must converge through retries with a one-second maximum frequency
after the early checkpoints. Helper processes and output reads must also have
cleanup and time limits so a failure cannot hang the test runner.

## Cause

The WM classifies the desired normal-band chain from managed window state. The
Windows reconciler then used to remove every topmost HWND from that chain and
from its retry snapshot. `HWND_TOP` cannot clear `WS_EX_TOPMOST`, so a managed
window that set the bit itself could not be moved back into the normal band or
recovered by later retries.

Asynchronous `SetWindowPos` calls also do not guarantee that a suspended
window's thread has processed the request. Tests need to account for delayed
processing and guarantee child-process cleanup when a timeout or assertion
occurs.

## Expected behavior

- The WM identifies every current, visible, managed window that belongs in the
  normal band, then passes those IDs to the platform reconciler. The platform
  checks each HWND's current topmost bit rather than relying on a cached style
  snapshot.
- The reconciler keeps those IDs in the request and its retry snapshot,
  demotes them with `HWND_NOTOPMOST`, and then applies the requested chain.
- Explicit `shown_on_top: true` windows and unrelated topmost HWNDs remain
  outside the demote set.
- A request is not considered complete while a requested demotion remains
  pending or the requested order has not been reached.
- The suspended-window recovery remains asynchronous. Retries are frequency-
  bounded, not lifetime-bounded; a newer generation, stale HWND identity, loss
  of the original foreground chain head, or successful convergence stops them.
  Its test verifies convergence after the helper resumes.
- Test child processes are terminated and reaped on every exit path; helper
  output reads use a deadline.
- Filtered remote runs preflight runnable test names, fail if the filter matches
  nothing, and report executable results separately from test-case results.
- A remote pass requires a successful SSH/controller process and exactly one
  valid worker exit marker.

## Coverage and validation

Platform tests cover a managed normal-band HWND that has set
`WS_EX_TOPMOST`, its owned dialog, tile promotion while a different floater is
demoted, an explicit topmost HWND, an unrelated topmost HWND, and recovery of a
suspended HWND after it resumes. The synthetic fixtures are hidden so a
running GlazeWM instance cannot adopt or focus them.
The selector test covers tiled and regular floating windows and excludes
`shown_on_top`, ignored, stale, show-desktop-minimized, and nonvisible windows.
The real Roblox Studio scenario was not evaluated: Roblox Studio was not
running on the VM. The display test also handles sessions with no visible
windows without leaving its event loop running.

## Second-review follow-up validation

The previous response recorded pre-follow-up VM runs
`20261107T032104Z-1b8788a5` and `20261107T032218Z-4c3a851c`. Those validate
the earlier R1–R7 tree, not the F1–F3 follow-up. The runs below include the
added Promote regression and are the current follow-up evidence:

- `cargo fmt --all -- --check` — passed.
- `cargo clippy --all-targets --all-features -- -D warnings` — passed with no
  Clippy errors; existing manifest/dependency warnings remain. Log:
  `Temp/pr18-second-review-clippy.log`.
- `python -m unittest scripts.test_remote_tests -v` — 9 passed, including
  continuous-output and descendant-process timeout probes. Log:
  `Temp/pr18-second-review-python-tests.log`.
- Focused local Promote regression — 1 passed:
  `cargo test -p wm-platform --test test demote_set_preserves_tile_promotion_when_floater_sets_topmost -- --exact --nocapture --test-threads=1`.
- `cargo test --workspace -- --test-threads=1` — 268 passed, 0 failed,
  15 ignored. Log: `Temp/pr18-second-review-local-workspace.log`.
- Focused VM run with `--package wm-platform --filter demote_set_` — 1
  executable passed, 2 test cases passed, 0 failed. Log:
  `Temp/remote-tests/20261107T040254Z-8eaf0094`.
- Full VM workspace run — 16/16 executables passed, 256 test cases passed,
  0 failed, 13 ignored. All 16 session checks matched the active console
  session and reported one visible monitor; all 38 transfers completed. Binaries
  were built locally and uploaded. Log:
  `Temp/remote-tests/20261107T040315Z-6a88121c`.
- The Windows test-matrix workflow now runs
  `python -m unittest scripts.test_remote_tests -v` under Python 3.12. GitHub
  CI for the follow-up commit is recorded in the response handoff.

Runnable Rustdoc examples ran remotely. `compile_fail` and `no_run` examples
were checked during the runner's local Rustdoc build and were not launched as
VM executables. Run logs are local ignored artifacts under `Temp/`; they are
not committed or automatically backed up.
