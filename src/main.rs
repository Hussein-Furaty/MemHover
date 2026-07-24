#![windows_subsystem = "windows"]

//! MemHover — Real-time memory monitoring utility for Windows.
//!
//! Sits silently in the system tray. When the cursor hovers over any application
//! icon on the Windows Taskbar, a sleek overlay displays the combined memory usage
//! (Working Set + Private Commit) for all instances of that process — matching
//! what Task Manager reports.

use std::mem::size_of;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::ProcessStatus::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// ── Application-defined message / timer identifiers ──────────────────────────
const WM_TRAYICON: u32 = WM_APP + 1;
const HOOK_CHECK_TIMER_ID: usize = 1;

// ── Global singletons (single-threaded Win32 message loop) ───────────────────
static mut TOOLTIP_HWND: HWND = HWND(std::ptr::null_mut());
static mut MAIN_HWND: HWND = HWND(std::ptr::null_mut());
static mut UI_AUTOMATION_INSTANCE: Option<IUIAutomation> = None;

// Pre-encoded UTF-16 strings written once per data change and read on every
// WM_PAINT to avoid repeated allocations inside the paint routine.
static mut TOOLTIP_LINES: Vec<Vec<u16>> = Vec::new();

// ── Entry point ──────────────────────────────────────────────────────────────
fn main() -> Result<()> {
    unsafe {
        // COM must be initialized before any UI-Automation calls.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // Create the IUIAutomation COM object once; reuse it on every timer tick.
        match CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
            Ok(automation) => UI_AUTOMATION_INSTANCE = Some(automation),
            Err(_) => {
                CoUninitialize();
                return Ok(());
            }
        }

        let instance = GetModuleHandleW(None)?;

        // ── Hidden main window (message pump + tray host) ─────────────────
        let main_class = w!("MemHoverMainClass");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(main_wnd_proc),
            hInstance: instance.into(),
            lpszClassName: main_class,
            ..Default::default()
        };
        RegisterClassW(&wc);

        MAIN_HWND = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            main_class,
            w!("MemHoverHidden"),
            WINDOW_STYLE(0),
            0, 0, 0, 0,
            None, None, instance, None,
        )?;

        // ── Tooltip overlay window ────────────────────────────────────────
        let tooltip_class = encode_wide_with_null("MemHoverTooltipClass");
        let wc2 = WNDCLASSW {
            style: CS_DROPSHADOW,
            lpfnWndProc: Some(tooltip_wnd_proc),
            hInstance: instance.into(),
            lpszClassName: PCWSTR(tooltip_class.as_ptr()),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0 as *mut core::ffi::c_void),
            ..Default::default()
        };
        RegisterClassW(&wc2);

        TOOLTIP_HWND = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            PCWSTR(tooltip_class.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0, 0, 0, 0,
            None, None, instance, None,
        )?;

        // 90 % opaque — subtle translucency gives a premium look.
        SetLayeredWindowAttributes(TOOLTIP_HWND, COLORREF(0), 230, LWA_ALPHA)?;

        // Windows 11 rounded corners via DWM.
        let preference: u32 = DWMWCP_ROUND.0 as u32;
        let _ = DwmSetWindowAttribute(
            TOOLTIP_HWND,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &preference as *const u32 as *const core::ffi::c_void,
            size_of::<u32>() as u32,
        );

        add_tray_icon(MAIN_HWND, instance)?;

        // Poll the cursor position every 200 ms — fast enough to feel instant,
        // cheap enough to be undetectable in Task Manager.
        SetTimer(MAIN_HWND, HOOK_CHECK_TIMER_ID, 200, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        UI_AUTOMATION_INSTANCE = None;
        CoUninitialize();
        Ok(())
    }
}

// ── Main window procedure ─────────────────────────────────────────────────────
unsafe extern "system" fn main_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TIMER => {
            if wparam.0 == HOOK_CHECK_TIMER_ID {
                poll_cursor_and_update_metrics();
            }
            LRESULT(0)
        }
        WM_TRAYICON => {
            if lparam.0 as u32 == WM_RBUTTONUP {
                show_tray_context_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            if (wparam.0 & 0xFFFF) as u32 == 1001 {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let mut nid = NOTIFYICONDATAW::default();
            nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = 1;
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ── Tooltip window procedure ──────────────────────────────────────────────────
unsafe extern "system" fn tooltip_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);

            // Dark background
            let mut rect = RECT { left: 0, top: 0, right: 300, bottom: 70 };
            let bg_brush = CreateSolidBrush(COLORREF(0x001C_1C1C));
            let _ = FillRect(hdc, &rect, bg_brush);
            let _ = DeleteObject(bg_brush);

            // Subtle border
            let border_brush = CreateSolidBrush(COLORREF(0x0033_3333));
            let _ = FrameRect(hdc, &rect, border_brush);
            let _ = DeleteObject(border_brush);

            let _ = SetBkMode(hdc, TRANSPARENT);

            // Line 1 — App name, bold white
            let hfont_bold = CreateFontW(
                16, 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0,
                DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32, CLEARTYPE_QUALITY.0 as u32,
                DEFAULT_PITCH.0 as u32,
                PCWSTR(encode_wide_with_null("Segoe UI").as_ptr()),
            );

            // Line 2 — Memory stats, normal gray
            let hfont_normal = CreateFontW(
                14, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0,
                DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32, CLEARTYPE_QUALITY.0 as u32,
                DEFAULT_PITCH.0 as u32,
                PCWSTR(encode_wide_with_null("Segoe UI").as_ptr()),
            );

            let lines = &*std::ptr::addr_of!(TOOLTIP_LINES);
            if lines.len() >= 2 {
                let old_font = SelectObject(hdc, hfont_bold);
                let _ = SetTextColor(hdc, COLORREF(0x00FF_FFFF));
                rect.left = 15;
                rect.top = 10;
                let mut line1 = lines[0].clone();
                let _ = DrawTextW(hdc, &mut line1, &mut rect, DT_SINGLELINE | DT_NOPREFIX);

                SelectObject(hdc, hfont_normal);
                let _ = SetTextColor(hdc, COLORREF(0x00AA_AAAA));
                rect.top = 35;
                let mut line2 = lines[1].clone();
                let _ = DrawTextW(hdc, &mut line2, &mut rect, DT_SINGLELINE | DT_NOPREFIX);

                SelectObject(hdc, old_font);
            }

            let _ = DeleteObject(hfont_bold);
            let _ = DeleteObject(hfont_normal);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ── System tray helpers ───────────────────────────────────────────────────────
unsafe fn add_tray_icon(hwnd: HWND, instance: HMODULE) -> Result<()> {
    let mut nid = NOTIFYICONDATAW::default();
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAYICON;
    nid.hIcon = match LoadIconW(instance, PCWSTR(1 as _)) {
        Ok(icon) => icon,
        Err(_) => LoadIconW(None, IDI_APPLICATION)?,
    };
    let tip = encode_wide("MemHover — Memory Monitor");
    let len = tip.len().min(nid.szTip.len());
    nid.szTip[..len].copy_from_slice(&tip[..len]);
    Shell_NotifyIconW(NIM_ADD, &nid).ok()?;
    Ok(())
}

unsafe fn show_tray_context_menu(hwnd: HWND) {
    let menu = CreatePopupMenu().unwrap_or_default();
    if menu.is_invalid() {
        return;
    }
    let _ = AppendMenuW(menu, MF_STRING, 1001, w!("Exit"));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, 0, hwnd, None);
    let _ = DestroyMenu(menu);
}

// ── Taskbar detection ─────────────────────────────────────────────────────────
unsafe fn is_cursor_over_taskbar_or_tray(pt: POINT) -> bool {
    let hwnd = WindowFromPoint(pt);
    if hwnd.is_invalid() {
        return false;
    }
    let root = GetAncestor(hwnd, GA_ROOT);
    if root.is_invalid() {
        return false;
    }
    let mut buf = [0u16; 256];
    let n = GetClassNameW(root, &mut buf);
    if n > 0 {
        match String::from_utf16_lossy(&buf[..n as usize]).as_str() {
            "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "NotifyIconOverflowWindow"
            | "XamlExplorerHostIslandWindow" => return true,
            "Progman" | "WorkerW" => return false,
            _ => {}
        }
    }
    false
}

// ── Core polling logic ────────────────────────────────────────────────────────
unsafe fn poll_cursor_and_update_metrics() {
    let mut pt = POINT::default();
    if GetCursorPos(&mut pt).is_err() {
        return;
    }
    if !is_cursor_over_taskbar_or_tray(pt) {
        hide_tooltip();
        return;
    }

    let automation = match &*std::ptr::addr_of!(UI_AUTOMATION_INSTANCE) {
        Some(a) => a,
        None => return,
    };

    let element = match automation.ElementFromPoint(pt) {
        Ok(e) => e,
        Err(_) => {
            hide_tooltip();
            return;
        }
    };

    // Resolve PID — prefer HWND-based lookup, fall back to UIA direct PID.
    let mut target_pid: u32 = 0;
    if let Ok(hwnd) = element.CurrentNativeWindowHandle() {
        if !hwnd.is_invalid() {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32));
            if pid != 0 {
                target_pid = pid;
            }
        }
    }
    if target_pid == 0 {
        if let Ok(pid) = element.CurrentProcessId() {
            target_pid = pid as u32;
        }
    }

    // If PID resolves to explorer.exe (the Taskbar host), resolve the real
    // application by matching the UIA element name against visible window titles.
    if target_pid != 0 && process_name_for_pid(target_pid).eq_ignore_ascii_case("explorer.exe") {
        if let Ok(bstr) = element.CurrentName() {
            let uia_name = bstr.to_string();
            if !uia_name.is_empty() {
                let resolved = find_pid_by_window_title(&uia_name);
                if resolved != 0 {
                    target_pid = resolved;
                }
            }
        }
    }

    if target_pid == 0 || target_pid == std::process::id() {
        hide_tooltip();
        return;
    }

    match get_process_memory_telemetry(target_pid) {
        Some((name, ws_mb, commit_mb)) if !name.eq_ignore_ascii_case("explorer.exe") => {
            let line1 = format!("App: {}", name);
            let line2 = format!("Total WS: {:.1} MB | Commit: {:.1} MB", ws_mb, commit_mb);
            let lines = &mut *std::ptr::addr_of_mut!(TOOLTIP_LINES);
            lines.clear();
            lines.push(encode_wide_with_null(&line1));
            lines.push(encode_wide_with_null(&line2));
            let _ = SetWindowPos(
                TOOLTIP_HWND, HWND_TOPMOST,
                pt.x + 12, pt.y - 70, 300, 70,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            let _ = InvalidateRect(TOOLTIP_HWND, None, true);
        }
        _ => hide_tooltip(),
    }
}

fn hide_tooltip() {
    unsafe {
        let _ = ShowWindow(TOOLTIP_HWND, SW_HIDE);
    }
}

// ── Window enumeration for Taskbar icon → PID resolution ─────────────────────
struct EnumState {
    target_name: String,
    best_pid: u32,
    best_score: u32,
}

unsafe extern "system" fn enum_windows_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = &mut *(lparam.0 as *mut EnumState);
    let mut buf = [0u16; 512];
    let len = GetWindowTextW(hwnd, &mut buf);
    if len < 3 {
        return BOOL(1);
    }
    let title = String::from_utf16_lossy(&buf[..len as usize]).to_lowercase();

    // Strip Windows 11 Taskbar suffix like " - 1 running window"
    let mut target = state.target_name.to_lowercase();
    if let Some(idx) = target.rfind(" - ") {
        target = target[..idx].trim().to_string();
    }

    let mut score: u32 = 0;
    if title == target {
        score = 100;
    } else if title.starts_with(&target) || target.starts_with(title.trim()) {
        score = 80;
    } else if title.contains(&target) || target.contains(title.trim()) {
        score = 50;
    }

    if score > 0 {
        if IsWindowVisible(hwnd).as_bool() {
            score += 500;
        }
        if score > state.best_score {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32));
            if pid != 0 {
                state.best_pid = pid;
                state.best_score = score;
            }
        }
    }
    BOOL(1)
}

fn find_pid_by_window_title(name: &str) -> u32 {
    let mut state = EnumState { target_name: name.to_string(), best_pid: 0, best_score: 0 };
    unsafe {
        let _ = EnumWindows(Some(enum_windows_callback), LPARAM(&mut state as *mut _ as isize));
    }
    state.best_pid
}

// ── Memory telemetry ──────────────────────────────────────────────────────────

/// Returns just the executable file name for a PID, or an empty string on failure.
unsafe fn process_name_for_pid(pid: u32) -> String {
    if let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        if QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            let _ = CloseHandle(handle);
            return path.rsplit('\\').next().unwrap_or("").to_string();
        }
        let _ = CloseHandle(handle);
    }
    String::new()
}

/// Sums Working Set and Private Commit across **all** processes that share the
/// same executable name — matching the grouped view in Task Manager.
unsafe fn get_process_memory_telemetry(pid: u32) -> Option<(String, f64, f64)> {
    // 1. Identify the target executable name.
    let target_name = process_name_for_pid(pid);
    if target_name.is_empty() {
        return None;
    }

    // 2. Enumerate every running PID and accumulate memory for matching processes.
    let mut total_ws: usize = 0;
    let mut total_commit: usize = 0;
    let mut pids = [0u32; 2048];
    let mut bytes_returned = 0u32;

    if K32EnumProcesses(
        pids.as_mut_ptr(),
        (pids.len() * size_of::<u32>()) as u32,
        &mut bytes_returned,
    ).as_bool() {
        let count = bytes_returned as usize / size_of::<u32>();
        for &p in &pids[..count] {
            if p == 0 {
                continue;
            }
            if let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, false, p) {
                let name = {
                    let mut buf = [0u16; 1024];
                    let mut len = buf.len() as u32;
                    if QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
                        let path = String::from_utf16_lossy(&buf[..len as usize]);
                        path.rsplit('\\').next().unwrap_or("").to_string()
                    } else {
                        String::new()
                    }
                };

                if name.eq_ignore_ascii_case(&target_name) {
                    let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
                    if K32GetProcessMemoryInfo(
                        handle,
                        &mut counters as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
                        size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
                    ).as_bool() {
                        total_ws += counters.WorkingSetSize;
                        total_commit += counters.PrivateUsage;
                    }
                }
                let _ = CloseHandle(handle);
            }
        }
    }

    Some((
        target_name,
        total_ws as f64 / 1_048_576.0,
        total_commit as f64 / 1_048_576.0,
    ))
}

// ── UTF-16 helpers ────────────────────────────────────────────────────────────
fn encode_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

fn encode_wide_with_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}