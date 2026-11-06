# Issue: managed windows can retain app-set topmost state

Date: 2026-11-06

Status: implementation and local automated tests pass; VM verification is pending.

## Symptoms

A managed floating window configured with `shown_on_top: false` can set the
Windows `WS_EX_TOPMOST` style itself. When another managed window is activated,
focus changes but the floating window can remain visually above the active
window.

The related suspended-window regression covers a managed HWND that stops
pumping messages beyond the initial recovery window. After it resumes, the
z-order chain must converge through bounded retries. Its helper processes and
output reads must also have cleanup and time limits so a failure cannot hang
the test runner.

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

- The WM identifies managed windows that belong in the normal band and are
  currently topmost, then passes those IDs to the platform reconciler.
- The reconciler keeps those IDs in the request and its retry snapshot,
  demotes them with `HWND_NOTOPMOST`, and then applies the requested chain.
- Explicit `shown_on_top: true` windows and unrelated topmost HWNDs remain
  outside the demote set.
- A request is not considered complete while a requested demotion remains
  pending or the requested order has not been reached.
- The suspended-window recovery remains asynchronous and bounded, and its
  test verifies convergence after the helper resumes.
- Test child processes are terminated and reaped on every exit path; helper
  output reads use a deadline.

## Coverage and validation

Platform tests cover a managed normal-band HWND that has set
`WS_EX_TOPMOST`, an unrelated topmost HWND outside the demote set, and recovery
of a suspended HWND after it resumes. The display test also handles sessions
with no visible windows without leaving its event loop running.

Local validation completed:

- `cargo test --workspace -- --test-threads=1` — passed (266 tests).
- `cargo test -p wm-platform --test test hung_hwnd_rejoins_after_suspend_past_early_recovery_window -- --exact --nocapture --test-threads=1` — passed.

VM validation has not been run yet.
