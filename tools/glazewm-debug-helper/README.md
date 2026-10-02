# GlazeWM debug helper

Native window used to reproduce a foreign GUI thread that stops pumping, hung
HWND z-order issues, and **owned modal / non-modal child focus** (Alt-Tab to
an app that has detached parent + popup).

Open `GlazeWmDebugHelper.sln` in Visual Studio 2022 and start the Debug x64
target. The project toolset is v143, so it builds there without retargeting.

Or from a Developer / MSBuild shell:

```
"F:\Microsoft Visual Studio\2022\Community\MSBuild\Current\Bin\MSBuild.exe" GlazeWmDebugHelper.sln /p:Configuration=Debug /p:Platform=x64
```

The process has a main window, an owned tool window (`WS_EX_TOOLWINDOW`,
ignored by GlazeWM), optional owned caption children, and a worker thread
that sleeps. Break All suspends every one of those threads. `IsHungAppWindow`
stays false.

## Buttons

Hung / style:

- **Suspend UI thread** calls `SuspendThread` on the GUI thread. This is the Break All state.
- **Break here, then Continue** hits `DebugBreak()` and then sleeps. That only stops the pump.
- **Stall style changes** holds the next cross-thread sent message, including `WM_STYLECHANGING`, until **Release style stall**. `WM_NULL` still returns.

Owned children (same process / `GW_OWNER` = main HWND):

- **Open non-modal owned (APPWINDOW + caption)** — managed by GlazeWM (has caption + not tool window). Shows in Alt-Tab.
- **Open non-modal owned (caption, no APPWINDOW)** — still managed (caption); Alt-Tab grouping may differ.
- **Open modal owned dialog (disables parent)** — `EnableWindow(parent, FALSE)` + `WS_EX_DLGMODALFRAME | WS_EX_APPWINDOW`. Closing re-enables the parent.
- **Close all owned children**

## Unattended

```
GlazeWmDebugHelper.exe --suspend
GlazeWmDebugHelper.exe --suspend --layered
GlazeWmDebugHelper.exe --stall-style --event Local\GlazeWmDebugStall
GlazeWmDebugHelper.exe --open-non-modal
GlazeWmDebugHelper.exe --open-modal
GlazeWmDebugHelper.exe --open-non-modal-no-app
```

`--suspend` pumps for 2 seconds, writes `repro-hwnd.txt` beside the exe
(`main_hwnd owned_hwnd`), prints `GLAZEWM_DEBUG_HWND:` when a console is
attached, then suspends the GUI thread. `--open-*` pumps 500ms so GlazeWM can
manage the main window, then opens the matching owned child and emits hwnds.

## Owned Alt-Tab repro (detached parent + popup)

1. Run the helper under GlazeWM; **float/detach** the main "GlazeWM debug helper" window.
2. Put another app above it (or focus a tiled window so the floater is covered).
3. Click one of the **Open … owned** buttons so a popup appears above the parent.
4. Alt-Tab away, then Alt-Tab back to the helper app / popup.
5. Expected: parent + popup both come forward (popup above parent).
6. Observed (bug): often only the popup rises; parent stays buried under tiles/peers.
