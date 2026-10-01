# GlazeWM debug helper

Native window used to reproduce a foreign GUI thread that stops pumping. Open `GlazeWmDebugHelper.sln` in Visual Studio 2022 and start the Debug x64 target. The project toolset is v143, so it builds there without retargeting.

The process has a main window, an owned tool window, and a worker thread that sleeps. Break All suspends every one of those threads. `IsHungAppWindow` stays false.

Buttons:

- **Suspend UI thread** calls `SuspendThread` on the GUI thread. This is the Break All state.
- **Break here, then Continue** hits `DebugBreak()` and then sleeps. That only stops the pump.
- **Stall style changes** holds the next cross-thread sent message, including `WM_STYLECHANGING`, until **Release style stall**. `WM_NULL` still returns. `SetLayeredWindowAttributes` does not enter this procedure, so this stall does not hold that call. A worker blocked in `SetWindowLongPtrW` is what must avoid applying a stale alpha after release.

Unattended:

```
GlazeWmDebugHelper.exe --suspend
GlazeWmDebugHelper.exe --suspend --layered
GlazeWmDebugHelper.exe --stall-style --event Local\GlazeWmDebugStall
```

`--suspend` pumps for 2 seconds, writes `repro-hwnd.txt` beside the exe (`main_hwnd owned_hwnd`), prints `GLAZEWM_DEBUG_HWND:` when a console is attached, then suspends the GUI thread. `--layered` sets `WS_EX_LAYERED` and alpha 200 on the GUI thread before that pump. `--stall-style` pumps for 300ms while unarmed, then holds sent messages other than `WM_NULL` on the named manual-reset event. Signal that event to let the blocked call finish.
