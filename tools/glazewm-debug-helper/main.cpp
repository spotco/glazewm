#include <windows.h>

#include <cstdio>
#include <string>
#include <vector>

namespace {

constexpr int kBreakButtonId = 1001;
constexpr int kSuspendButtonId = 1002;
constexpr int kStallButtonId = 1003;
constexpr int kReleaseButtonId = 1004;
constexpr int kOpenNonModalId = 1005;
constexpr int kOpenModalId = 1006;
constexpr int kOpenNonModalNoAppId = 1007;
constexpr int kCloseChildrenId = 1008;
constexpr int kChildCloseId = 2001;

constexpr wchar_t kWindowClassName[] = L"GlazeWmDebugHelperWindow";
constexpr wchar_t kToolClassName[] = L"GlazeWmDebugHelperTool";
constexpr wchar_t kChildClassName[] = L"GlazeWmDebugHelperChild";
constexpr wchar_t kWindowTitle[] = L"GlazeWM debug helper";
constexpr wchar_t kToolTitle[] = L"GlazeWM debug helper owned tool";
constexpr wchar_t kNonModalTitle[] = L"GlazeWM helper non-modal owned";
constexpr wchar_t kModalTitle[] = L"GlazeWM helper modal owned";
constexpr wchar_t kNonModalNoAppTitle[] =
    L"GlazeWM helper non-modal owned (no APPWINDOW)";

HWND g_main_window = nullptr;
HWND g_tool_window = nullptr;
HWND g_modal_window = nullptr;
std::vector<HWND> g_children;
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

void ForgetChild(HWND child) {
    for (auto it = g_children.begin(); it != g_children.end(); ++it) {
        if (*it == child) {
            g_children.erase(it);
            break;
        }
    }
    if (g_modal_window == child) {
        g_modal_window = nullptr;
        if (g_main_window != nullptr && IsWindow(g_main_window) != FALSE) {
            EnableWindow(g_main_window, TRUE);
            SetForegroundWindow(g_main_window);
        }
    }
}

void CloseAllChildren() {
    // Copy first — DestroyWindow triggers WM_DESTROY which mutates g_children.
    const std::vector<HWND> snapshot = g_children;
    for (HWND child : snapshot) {
        if (child != nullptr && IsWindow(child) != FALSE) {
            DestroyWindow(child);
        }
    }
    g_children.clear();
    if (g_modal_window != nullptr) {
        g_modal_window = nullptr;
        if (g_main_window != nullptr && IsWindow(g_main_window) != FALSE) {
            EnableWindow(g_main_window, TRUE);
        }
    }
}

HWND CreateOwnedChild(
    HINSTANCE instance,
    HWND owner,
    const wchar_t* title,
    DWORD ex_style,
    bool modal) {
    RECT owner_rect{};
    GetWindowRect(owner, &owner_rect);
    const int x = owner_rect.left + 40;
    const int y = owner_rect.top + 40;
    const int width = 360;
    const int height = 180;

    HWND child = CreateWindowExW(
        ex_style,
        kChildClassName,
        title,
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
        x,
        y,
        width,
        height,
        owner,
        nullptr,
        instance,
        nullptr);
    if (child == nullptr) {
        return nullptr;
    }

    CreateWindowExW(
        0,
        L"BUTTON",
        L"Close this child",
        WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON,
        24,
        24,
        200,
        28,
        child,
        reinterpret_cast<HMENU>(static_cast<INT_PTR>(kChildCloseId)),
        instance,
        nullptr);

    CreateWindowExW(
        0,
        L"STATIC",
        modal
            ? L"Modal owned window (parent disabled). Alt-Tab to this app."
            : L"Non-modal owned window. Alt-Tab to this app / popup.",
        WS_CHILD | WS_VISIBLE,
        24,
        64,
        300,
        60,
        child,
        nullptr,
        instance,
        nullptr);

    g_children.push_back(child);

    if (modal) {
        if (g_modal_window != nullptr && IsWindow(g_modal_window) != FALSE) {
            // Only one modal at a time for a clear repro.
            DestroyWindow(g_modal_window);
        }
        g_modal_window = child;
        EnableWindow(owner, FALSE);
    }

    SetForegroundWindow(child);
    return child;
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

LRESULT CALLBACK ChildProc(HWND window, UINT message, WPARAM wParam, LPARAM lParam) {
    if (g_stall_style && InSendMessage() != FALSE && message != WM_NULL &&
        g_release_event != nullptr) {
        WaitForSingleObject(g_release_event, 30000);
    }

    switch (message) {
    case WM_COMMAND:
        if (LOWORD(wParam) == kChildCloseId) {
            DestroyWindow(window);
            return 0;
        }
        break;

    case WM_CLOSE:
        DestroyWindow(window);
        return 0;

    case WM_DESTROY:
        ForgetChild(window);
        return 0;

    default:
        break;
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
    case WM_CREATE: {
        const auto* create = reinterpret_cast<LPCREATESTRUCTW>(lParam);
        HINSTANCE instance = create->hInstance;
        int y = 16;
        auto button = [&](int id, const wchar_t* label) {
            CreateWindowExW(
                0,
                L"BUTTON",
                label,
                WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON,
                24,
                y,
                400,
                28,
                window,
                reinterpret_cast<HMENU>(static_cast<INT_PTR>(id)),
                instance,
                nullptr);
            y += 36;
        };

        button(kBreakButtonId, L"Break here, then Continue");
        button(kSuspendButtonId, L"Suspend UI thread");
        button(kStallButtonId, L"Stall style changes");
        button(kReleaseButtonId, L"Release style stall");
        button(kOpenNonModalId, L"Open non-modal owned (APPWINDOW + caption)");
        button(
            kOpenNonModalNoAppId,
            L"Open non-modal owned (caption, no APPWINDOW)");
        button(kOpenModalId, L"Open modal owned dialog (disables parent)");
        button(kCloseChildrenId, L"Close all owned children");
        return 0;
    }

    case WM_COMMAND: {
        const int id = LOWORD(wParam);
        HINSTANCE instance = reinterpret_cast<HINSTANCE>(
            GetWindowLongPtrW(window, GWLP_HINSTANCE));
        if (id == kBreakButtonId) {
            DebugBreak();
            Sleep(INFINITE);
        } else if (id == kSuspendButtonId) {
            SuspendThisGuiThread();
        } else if (id == kStallButtonId) {
            g_stall_style = true;
            if (g_release_event != nullptr) {
                ResetEvent(g_release_event);
            }
        } else if (id == kReleaseButtonId && g_release_event != nullptr) {
            SetEvent(g_release_event);
        } else if (id == kOpenNonModalId) {
            CreateOwnedChild(
                instance,
                window,
                kNonModalTitle,
                WS_EX_APPWINDOW,
                false);
        } else if (id == kOpenNonModalNoAppId) {
            CreateOwnedChild(
                instance,
                window,
                kNonModalNoAppTitle,
                0,
                false);
        } else if (id == kOpenModalId) {
            CreateOwnedChild(
                instance,
                window,
                kModalTitle,
                WS_EX_DLGMODALFRAME | WS_EX_APPWINDOW,
                true);
        } else if (id == kCloseChildrenId) {
            CloseAllChildren();
        }
        return 0;
    }

    case WM_DESTROY:
        CloseAllChildren();
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
    const bool open_non_modal = HasSwitch(command_line, L"--open-non-modal");
    const bool open_modal = HasSwitch(command_line, L"--open-modal");
    const bool open_non_modal_no_app =
        HasSwitch(command_line, L"--open-non-modal-no-app");
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

    WNDCLASSEXW child_class = window_class;
    child_class.lpfnWndProc = ChildProc;
    child_class.lpszClassName = kChildClassName;
    if (RegisterClassExW(&child_class) == 0) {
        return static_cast<int>(GetLastError());
    }

    HWND window = CreateWindowExW(
        WS_EX_APPWINDOW,
        kWindowClassName,
        kWindowTitle,
        WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        470,
        360,
        nullptr,
        nullptr,
        instance,
        nullptr);
    if (window == nullptr) {
        return static_cast<int>(GetLastError());
    }
    g_main_window = window;

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

    if (open_non_modal || open_modal || open_non_modal_no_app) {
        // Pump briefly so GlazeWM can manage the main window first.
        const DWORD pump_until = GetTickCount() + 500;
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
        if (open_non_modal) {
            CreateOwnedChild(
                instance,
                window,
                kNonModalTitle,
                WS_EX_APPWINDOW,
                false);
        }
        if (open_non_modal_no_app) {
            CreateOwnedChild(
                instance,
                window,
                kNonModalNoAppTitle,
                0,
                false);
        }
        if (open_modal) {
            CreateOwnedChild(
                instance,
                window,
                kModalTitle,
                WS_EX_DLGMODALFRAME | WS_EX_APPWINDOW,
                true);
        }
        EmitHwnd(window);
    }

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
