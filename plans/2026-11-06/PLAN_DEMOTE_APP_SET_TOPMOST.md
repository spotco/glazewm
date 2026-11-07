# Issue: managed windows can retain app-set topmost state

Date: 2026-11-06

Status: R1–R7 review fixes are implemented; local and remote Windows validation pass.

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
`WS_EX_TOPMOST`, its owned dialog, an explicit topmost HWND, an unrelated
topmost HWND, and recovery of a suspended HWND after it resumes. The synthetic
fixtures are hidden so a running GlazeWM instance cannot adopt or focus them.
The selector test covers tiled and regular floating windows and excludes
`shown_on_top`, ignored, stale, show-desktop-minimized, and nonvisible windows.
The real Roblox Studio scenario was not evaluated: Roblox Studio was not
running on the VM. The display test also handles sessions with no visible
windows without leaving its event loop running.

Validation completed:

- `cargo fmt --all -- --check` — passed.
- `cargo clippy --all-targets --all-features -- -D warnings` — passed.
  Log: `Temp/pr18-final-clippy.log`.
- `python -m unittest scripts.test_remote_tests` — 9 passed, including
  continuous-output and descendant-process timeout probes plus remote-marker
  disagreement cases. Log:
  `Temp/pr18-final-runner-tests.log`.
- `cargo test --workspace -- --test-threads=1` — passed locally (267 passed,
  15 ignored). Log: `Temp/pr18-final-local-workspace-tests.log`.
- Filtered remote run `hung_hwnd_rejoins_after_suspend_past_early_recovery_window`
  — 1 executable and 1 test case passed. Log: `Temp/remote-tests/20261107T032104Z-1b8788a5`.
- Full remote workspace run — 16 executable passes, 0 failures, 0 no-match
  skips; 255 test cases passed, 0 failed, and 13 ignored. The active console
  session matched the worker session and reported one monitor for every run.
  Log: `Temp/remote-tests/20261107T032218Z-4c3a851c`.
- A deliberately unmatched filter recorded `zero_match_failure: true`, skipped
  its sole executable, and made no remote run. Log:
  `Temp/remote-tests/20261107T032045Z-3ba196e0`.

Runnable Rustdoc examples ran remotely. `compile_fail` and `no_run` examples
were checked during the runner's local Rustdoc build and were not launched as
VM executables. Run logs are local ignored artifacts under `Temp/`; they are
not committed or automatically backed up.
