#include <windows.h>

#include <cstdio>
#include <string>

namespace {

constexpr int kBreakButtonId = 1001;
constexpr int kSuspendButtonId = 1002;
constexpr int kStallButtonId = 1003;
constexpr int kReleaseButtonId = 1004;
constexpr wchar_t kWindowClassName[] = L"GlazeWmDebugHelperWindow";
constexpr wchar_t kToolClassName[] = L"GlazeWmDebugHelperTool";
constexpr wchar_t kWindowTitle[] = L"GlazeWM debug helper";
constexpr wchar_t kToolTitle[] = L"GlazeWM debug helper owned tool";

HWND g_tool_window = nullptr;
HANDLE g_release_event = nullptr;
bool g_stall_style = false;

DWORD WINAPI WorkerThread(void*) {
    // Extra non-GUI thread. Break All suspends this thread too.
    Sleep(INFINITE);
    return 0;
}

void SuspendThisGuiThread() {
    // Matches Visual Studio Break All: the GUI thread is suspended,
    // not merely sleeping, so IsHungAppWindow stays false.
    SuspendThread(GetCurrentThread());
}

bool HasSwitch(const wchar_t* command_line, const wchar_t* name) {
    return command_line != nullptr && std::wcsstr(command_line, name) != nullptr;
}

// Copies the argument after `--event ` into `name`.
bool EventName(const wchar_t* command_line, std::wstring* name) {
    const wchar_t* found = command_line == nullptr
        ? nullptr
        : std::wcsstr(command_line, L"--event ");
    if (found == nullptr) {
        return false;
    }
    found += std::wcslen(L"--event ");
    while (*found == L' ') {
        ++found;
    }
    const wchar_t* end = found;
    while (*end != L'\0' && *end != L' ') {
        ++end;
    }
    if (end == found) {
        return false;
    }
    name->assign(found, end);
    return true;
}

void EmitHwnd(HWND main_window) {
    wchar_t exe_path[MAX_PATH]{};
    GetModuleFileNameW(nullptr, exe_path, MAX_PATH);
    std::wstring path(exe_path);
    const auto slash = path.find_last_of(L"\\/");
    if (slash != std::wstring::npos) {
        path.resize(slash + 1);
    }
    path += L"repro-hwnd.txt";
    FILE* file = nullptr;
    if (_wfopen_s(&file, path.c_str(), L"w") == 0 && file != nullptr) {
        std::fwprintf(
            file,
            L"%lld %lld\n",
            static_cast<long long>(reinterpret_cast<intptr_t>(main_window)),
            static_cast<long long>(reinterpret_cast<intptr_t>(g_tool_window)));
        std::fclose(file);
    }

    if (AttachConsole(ATTACH_PARENT_PROCESS) != FALSE) {
        FILE* console = nullptr;
        if (_wfreopen_s(&console, L"CONOUT$", L"w", stdout) == 0 && console != nullptr) {
            std::wprintf(
                L"GLAZEWM_DEBUG_HWND:%lld %lld\n",
                static_cast<long long>(reinterpret_cast<intptr_t>(main_window)),
                static_cast<long long>(reinterpret_cast<intptr_t>(g_tool_window)));
            std::fflush(stdout);
        }
    }
}

LRESULT CALLBACK ToolProc(HWND window, UINT message, WPARAM wParam, LPARAM lParam) {
    if (g_stall_style && InSendMessage() != FALSE && message != WM_NULL &&
        g_release_event != nullptr) {
        WaitForSingleObject(g_release_event, 30000);
    }
    if (message == WM_DESTROY) {
        return 0;
    }
    return DefWindowProcW(window, message, wParam, lParam);
}

LRESULT CALLBACK WindowProc(HWND window, UINT message, WPARAM wParam, LPARAM lParam) {
    // SetLayeredWindowAttributes does not arrive here. Style changes do.
    if (g_stall_style && InSendMessage() != FALSE && message != WM_NULL &&
        g_release_event != nullptr) {
        WaitForSingleObject(g_release_event, 30000);
    }

    switch (message) {
    case WM_CREATE:
        CreateWindowExW(
            0,
            L"BUTTON",
            L"Break here, then Continue",
            WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON,
            24,
            16,
            280,
            28,
            window,
            reinterpret_cast<HMENU>(static_cast<INT_PTR>(kBreakButtonId)),
            reinterpret_cast<LPCREATESTRUCTW>(lParam)->hInstance,
            nullptr);
        CreateWindowExW(
            0,
            L"BUTTON",
            L"Suspend UI thread",
            WS_CHILD | WS_VISIBLE | BS_DEFPUSHBUTTON,
            24,
            52,
            280,
            28,
            window,
            reinterpret_cast<HMENU>(static_cast<INT_PTR>(kSuspendButtonId)),
            reinterpret_cast<LPCREATESTRUCTW>(lParam)->hInstance,
            nullptr);
        CreateWindowExW(
            0,
            L"BUTTON",
            L"Stall style changes",
            WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON,
            24,
            88,
            280,
            28,
            window,
            reinterpret_cast<HMENU>(static_cast<INT_PTR>(kStallButtonId)),
            reinterpret_cast<LPCREATESTRUCTW>(lParam)->hInstance,
            nullptr);
        CreateWindowExW(
            0,
            L"BUTTON",
            L"Release style stall",
            WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON,
            24,
            124,
            280,
            28,
            window,
            reinterpret_cast<HMENU>(static_cast<INT_PTR>(kReleaseButtonId)),
            reinterpret_cast<LPCREATESTRUCTW>(lParam)->hInstance,
            nullptr);
        return 0;

    case WM_COMMAND:
        if (LOWORD(wParam) == kBreakButtonId) {
            DebugBreak();
            Sleep(INFINITE);
        } else if (LOWORD(wParam) == kSuspendButtonId) {
            SuspendThisGuiThread();
        } else if (LOWORD(wParam) == kStallButtonId) {
            g_stall_style = true;
            if (g_release_event != nullptr) {
                ResetEvent(g_release_event);
            }
        } else if (LOWORD(wParam) == kReleaseButtonId && g_release_event != nullptr) {
            SetEvent(g_release_event);
        }
        return 0;

    case WM_DESTROY:
        PostQuitMessage(0);
        return 0;

    default:
        return DefWindowProcW(window, message, wParam, lParam);
    }
}

}  // namespace

int WINAPI wWinMain(HINSTANCE instance, HINSTANCE, PWSTR command_line, int show_command) {
    const bool suspend_after_pump = HasSwitch(command_line, L"--suspend");
    const bool layered = HasSwitch(command_line, L"--layered");
    const bool stall_after_pump = HasSwitch(command_line, L"--stall-style");
    // Stay unarmed through the first pump so create-time style calls
    // from a running window manager finish before the stall begins.
    g_stall_style = false;

    std::wstring event_name;
    if (EventName(command_line, &event_name)) {
        g_release_event = CreateEventW(nullptr, TRUE, FALSE, event_name.c_str());
    } else {
        g_release_event = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    }
    if (g_release_event == nullptr) {
        return static_cast<int>(GetLastError());
    }

    WNDCLASSEXW window_class{};
    window_class.cbSize = sizeof(WNDCLASSEXW);
    window_class.style = CS_HREDRAW | CS_VREDRAW;
    window_class.lpfnWndProc = WindowProc;
    window_class.hInstance = instance;
    window_class.hCursor = LoadCursorW(nullptr, IDC_ARROW);
    window_class.hbrBackground = reinterpret_cast<HBRUSH>(COLOR_WINDOW + 1);
    window_class.lpszClassName = kWindowClassName;
    if (RegisterClassExW(&window_class) == 0) {
        return static_cast<int>(GetLastError());
    }

    WNDCLASSEXW tool_class = window_class;
    tool_class.lpfnWndProc = ToolProc;
    tool_class.lpszClassName = kToolClassName;
    if (RegisterClassExW(&tool_class) == 0) {
        return static_cast<int>(GetLastError());
    }

    HWND window = CreateWindowExW(
        WS_EX_APPWINDOW,
        kWindowClassName,
        kWindowTitle,
        WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        460,
        240,
        nullptr,
        nullptr,
        instance,
        nullptr);
    if (window == nullptr) {
        return static_cast<int>(GetLastError());
    }

    g_tool_window = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        kToolClassName,
        kToolTitle,
        WS_OVERLAPPED | WS_CAPTION | WS_VISIBLE,
        80,
        80,
        280,
        120,
        window,
        nullptr,
        instance,
        nullptr);

    if (layered) {
        LONG_PTR style = GetWindowLongPtrW(window, GWL_EXSTYLE);
        SetWindowLongPtrW(window, GWL_EXSTYLE, style | WS_EX_LAYERED);
        SetLayeredWindowAttributes(window, 0, 200, LWA_ALPHA);
    }

    ShowWindow(window, show_command);
    UpdateWindow(window);
    HANDLE worker = CreateThread(nullptr, 0, WorkerThread, nullptr, 0, nullptr);

    if (suspend_after_pump || stall_after_pump) {
        const DWORD pump_until = GetTickCount() + (stall_after_pump ? 300 : 2000);
        MSG message{};
        while (GetTickCount() < pump_until) {
            while (PeekMessageW(&message, nullptr, 0, 0, PM_REMOVE) != FALSE) {
                if (message.message == WM_QUIT) {
                    return static_cast<int>(message.wParam);
                }
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            Sleep(10);
        }
        EmitHwnd(window);
        if (stall_after_pump) {
            g_stall_style = true;
        }
        if (suspend_after_pump) {
            SuspendThisGuiThread();
        }
    }

    MSG message{};
    while (GetMessageW(&message, nullptr, 0, 0) > 0) {
        TranslateMessage(&message);
        DispatchMessageW(&message);
    }
    if (worker != nullptr) {
        CloseHandle(worker);
    }
    CloseHandle(g_release_event);
    return static_cast<int>(message.wParam);
}
