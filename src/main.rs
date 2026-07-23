#![windows_subsystem = "windows"]

//! MemHover - A lightweight Windows utility for real-time memory monitoring.
//!
//! This application operates as a background process, utilizing the UI Automation API
//! to determine the application currently under the cursor and displaying its memory
//! consumption metrics via a low-latency, transparent overlay (tooltip).

use std::mem::size_of;
use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::ProcessStatus::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::System::Com::*;

// Application-defined messages and identifiers
const WM_TRAYICON: u32 = WM_APP + 1;
const HOOK_CHECK_TIMER_ID: usize = 1;

// Global state variables for UI components and COM interfaces
// Note: In a production-grade application, these could be encapsulated within a context struct
// passed via window userdata (GWLP_USERDATA), but static mut is utilized here for minimal overhead
// in a strictly single-threaded message loop paradigm.
static mut TOOLTIP_HWND: HWND = HWND(std::ptr::null_mut());
static mut MAIN_HWND: HWND = HWND(std::ptr::null_mut());
static mut UI_AUTOMATION_INSTANCE: Option<IUIAutomation> = None;

// Cached rendering data to minimize allocation overhead during the WM_PAINT cycle
static mut TOOLTIP_LINES: Vec<Vec<u16>> = Vec::new();
static mut LAST_HOVERED_PID: u32 = 0;

fn main() -> Result<()> {
    unsafe {
        // Initialize the COM library on the current thread.
        // Required for UI Automation interfaces.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // Instantiate the UI Automation COM object once during initialization
        // to prevent extreme overhead and CPU consumption in the high-frequency timer loop.
        if let Ok(automation) = CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
            UI_AUTOMATION_INSTANCE = Some(automation);
        } else {
            // Fallback: Exit gracefully if UI Automation infrastructure cannot be initialized
            CoUninitialize();
            return Ok(());
        }

        let instance = GetModuleHandleW(None)?;

        // Register and create the hidden main window responsible for message pumping and tray icon management.
        let main_class = w!("AppMemoryTooltipMainClass");
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
            w!("AppMemoryTooltipHidden"),
            WINDOW_STYLE(0),
            0, 0, 0, 0,
            None, None, instance, None,
        )?;

        // Register and create the tooltip overlay window.
        // Utilizes layered window attributes (WS_EX_LAYERED) for hardware-accelerated transparency.
        let tooltip_class = w!("AppMemoryTooltipPopupClass");
        let wc2 = WNDCLASSW {
            lpfnWndProc: Some(tooltip_wnd_proc),
            hInstance: instance.into(),
            lpszClassName: tooltip_class,
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as *mut _),
            ..Default::default()
        };
        RegisterClassW(&wc2);

        TOOLTIP_HWND = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE,
            tooltip_class,
            w!("Tooltip"),
            WS_POPUP,
            0, 0, 210, 65,
            None, None, instance, None,
        )?;
        
        // Apply alpha transparency channel to the tooltip window.
        SetLayeredWindowAttributes(TOOLTIP_HWND, COLORREF(0), 240, LWA_ALPHA)?;

        // Initialize the system tray notification icon.
        add_tray_icon(MAIN_HWND, instance)?;

        // Establish a high-resolution timer to poll cursor position periodically.
        // Interval: 200ms provides an optimal balance between visual responsiveness and CPU conservation.
        SetTimer(MAIN_HWND, HOOK_CHECK_TIMER_ID, 200, None);

        // Standard Win32 message loop paradigm
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Graceful teardown of COM resources upon exit
        UI_AUTOMATION_INSTANCE = None;
        CoUninitialize();
        Ok(())
    }
}

/// Main window procedure.
/// Dispatches system messages, tray icon interactions, and timer events.
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
            let event = lparam.0 as u32;
            if event == WM_RBUTTONUP {
                show_tray_context_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as u32;
            if id == 1001 { // Defined Exit command identifier
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

/// Tooltip window procedure.
/// Handles GDI (Graphics Device Interface) painting for the memory statistics overlay.
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
            
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);

            // Render background establishing a dark theme aesthetic
            let bg_brush = CreateSolidBrush(COLORREF(0x0026_2626));
            FillRect(hdc, &rect, bg_brush);
            let _ = DeleteObject(bg_brush);

            // Render subtle perimeter border
            let border_pen = CreatePen(PS_SOLID, 1, COLORREF(0x0040_4040));
            let old_pen = SelectObject(hdc, border_pen);
            let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
            let _ = Rectangle(hdc, 0, 0, rect.right, rect.bottom);
            SelectObject(hdc, old_pen);
            SelectObject(hdc, old_brush);
            let _ = DeleteObject(border_pen);

            SetBkMode(hdc, TRANSPARENT);

            // Configure typography parameters
            let font = CreateFontW(
                16, 0, 0, 0, FW_NORMAL.0 as i32, 0, 0, 0,
                DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32, CLIP_DEFAULT_PRECIS.0 as u32,
                CLEARTYPE_QUALITY.0 as u32, FF_DONTCARE.0 as u32,
                w!("Segoe UI"),
            );

            let old_font = SelectObject(hdc, font);
            SetTextColor(hdc, COLORREF(0x00E0_E0E0));
            
            let mut y_offset = 10;
            
            // Render pre-formatted text lines.
            // Raw pointer access is used here to comply with Rust 2024 strict aliasing rules for mutable statics.
            for line in (*std::ptr::addr_of_mut!(TOOLTIP_LINES)).iter_mut() {
                let mut render_rect = RECT { 
                    left: 12, 
                    top: y_offset, 
                    right: rect.right - 10, 
                    bottom: y_offset + 20 
                };
                DrawTextW(
                    hdc,
                    line,
                    &mut render_rect,
                    DT_LEFT | DT_SINGLELINE | DT_NOPREFIX,
                );
                y_offset += 20;
            }

            // Cleanup ephemeral GDI objects to prevent memory leaks
            SelectObject(hdc, old_font);
            let _ = DeleteObject(font);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Registers the system tray notification icon within the taskbar status area.
unsafe fn add_tray_icon(hwnd: HWND, instance: HMODULE) -> Result<()> {
    let mut nid = NOTIFYICONDATAW::default();
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAYICON;
    // Attempt to load the embedded custom icon (Resource ID 1), fallback to default if it fails
    nid.hIcon = match LoadIconW(instance, PCWSTR(1 as _)) {
        Ok(icon) => icon,
        Err(_) => LoadIconW(None, IDI_APPLICATION)?,
    };
    
    let tip = encode_wide("App Memory Monitor");
    let len = tip.len().min(nid.szTip.len());
    nid.szTip[..len].copy_from_slice(&tip[..len]);

    Shell_NotifyIconW(NIM_ADD, &nid).ok()?;
    Ok(())
}

/// Displays the context menu for the system tray icon upon a right-click event.
unsafe fn show_tray_context_menu(hwnd: HWND) {
    let menu = CreatePopupMenu().unwrap_or_default();
    if menu.is_invalid() { return; }
    
    let _ = AppendMenuW(menu, MF_STRING, 1001, w!("Exit"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    
    // Set the process to foreground to ensure the OS dismisses the menu correctly upon outside clicks
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
}

/// Determines whether the cursor is currently positioned over the Windows Taskbar
/// or the System Tray notification area.
///
/// Resolution strategy: Traverses the complete parent window chain from the window
/// directly under the cursor upward, checking each ancestor's class name against
/// well-known taskbar identifiers. This approach is robust across Windows 10 and 11,
/// where the internal taskbar window hierarchy may differ significantly.
///
/// Recognized taskbar class names:
/// - `Shell_TrayWnd`              → Primary taskbar (Win10/11)
/// - `Shell_SecondaryTrayWnd`     → Secondary monitor taskbar
/// - `NotifyIconOverflowWindow`   → System tray overflow (hidden icons)
unsafe fn is_cursor_over_taskbar_or_tray(pt: POINT) -> bool {
    let mut hwnd = WindowFromPoint(pt);
    if hwnd.is_invalid() {
        return false;
    }

    // Walk the complete ancestor chain rather than jumping directly to the root,
    // as Windows 11 introduced a multi-level taskbar window hierarchy that breaks
    // the simpler GA_ROOT approach used in prior Windows versions.
    loop {
        let mut class_buf = [0u16; 256];
        let char_count = GetClassNameW(hwnd, &mut class_buf);

        if char_count > 0 {
            let class_name = String::from_utf16_lossy(&class_buf[..char_count as usize]);
            match class_name.as_str() {
                "Shell_TrayWnd"
                | "Shell_SecondaryTrayWnd"
                | "NotifyIconOverflowWindow" => return true,
                _ => {}
            }
        }

        // Advance to the next ancestor; terminate if the chain is exhausted
        match GetParent(hwnd) {
            Ok(parent) if !parent.is_invalid() => hwnd = parent,
            _ => break,
        }
    }

    false
}



/// Core routine invoked periodically to resolve the UI element under the cursor,
/// retrieve associated process memory metrics, and update the UI overlay accordingly.
/// 
/// The routine performs an early-exit check to ensure metrics are only surfaced
/// when the cursor is positioned over the Windows Taskbar or System Tray — not
/// over arbitrary application windows.
unsafe fn poll_cursor_and_update_metrics() {
    let mut pt = POINT::default();
    if GetCursorPos(&mut pt).is_err() {
        return;
    }

    // Early-exit guard: Suppress tooltip rendering when the cursor is not
    // positioned over the taskbar or system tray notification area.
    if !is_cursor_over_taskbar_or_tray(pt) {
        hide_tooltip_overlay();
        return;
    }

    // Access the cached COM instance via a raw pointer to satisfy Rust 2024 aliasing rules.
    let automation = match &*std::ptr::addr_of!(UI_AUTOMATION_INSTANCE) {
        Some(a) => a,
        None => return, // Fail gracefully if the COM infrastructure is unavailable
    };

    // Resolve the UI Automation element corresponding to the current cursor coordinates
    let element = match automation.ElementFromPoint(pt) {
        Ok(e) => e,
        Err(_) => { hide_tooltip_overlay(); return; }
    };

    let mut target_pid: u32 = 0;
    
    // Primary resolution technique: Derive PID via Native Window Handle (HWND)
    if let Ok(hwnd) = element.CurrentNativeWindowHandle() {
        if !hwnd.is_invalid() {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32));
            if pid != 0 {
                target_pid = pid;
            }
        }
    }

    // Fallback technique: Resolve via direct UI Automation process mapping
    if target_pid == 0 {
        if let Ok(pid) = element.CurrentProcessId() {
            target_pid = pid as u32;
        }
    }

    let current_exe_pid = std::process::id();

    // Suppress rendering for the monitor itself or unresolved entities
    if target_pid == 0 || target_pid == current_exe_pid {
        hide_tooltip_overlay();
        LAST_HOVERED_PID = 0;
        return;
    }

    // Retrieve comprehensive memory telemetry for the targeted process
    if let Some((name, working_set_mb, private_mb)) = get_process_memory_telemetry(target_pid) {
        // Exclude system shell (Explorer) to reduce visual noise during arbitrary taskbar interactions
        if name.eq_ignore_ascii_case("explorer.exe") {
            hide_tooltip_overlay();
            LAST_HOVERED_PID = 0;
            return;
        }

        // Cache the formatted strings as UTF-16 once per data change to strictly optimize the WM_PAINT pipeline
        let line1 = format!("App: {}", name);
        let line2 = format!("RAM: {:.1} MB | Priv: {:.1} MB", working_set_mb, private_mb);
        
        // Mutate the static buffer through a raw pointer to comply with Rust 2024 strict aliasing rules.
        let lines = &mut *std::ptr::addr_of_mut!(TOOLTIP_LINES);
        lines.clear();
        lines.push(encode_wide_with_null(&line1));
        lines.push(encode_wide_with_null(&line2));
        LAST_HOVERED_PID = target_pid;

        // Reposition the overlay relative to the cursor and force a repaint invalidation
        let _ = SetWindowPos(
            TOOLTIP_HWND,
            HWND_TOPMOST,
            pt.x + 12,
            pt.y - 55,
            210,
            65,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let _ = InvalidateRect(TOOLTIP_HWND, None, true);
    } else {
        hide_tooltip_overlay();
        LAST_HOVERED_PID = 0;
    }
}

/// Suppresses the rendering of the tooltip overlay by manipulating the window state.
fn hide_tooltip_overlay() {
    unsafe {
        let _ = ShowWindow(TOOLTIP_HWND, SW_HIDE);
    }
}

/// Extracts key memory consumption metrics for a given process identifier.
/// 
/// Returns a tuple containing:
/// - Process Executable Name (String)
/// - Working Set Size in Megabytes (f64)
/// - Private Memory Usage in Megabytes (f64)
unsafe fn get_process_memory_telemetry(pid: u32) -> Option<(String, f64, f64)> {
    let process_handle = OpenProcess(
        PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
        false,
        pid,
    ).ok()?;

    let mut mem_counters = PROCESS_MEMORY_COUNTERS_EX::default();
    let success = K32GetProcessMemoryInfo(
        process_handle,
        &mut mem_counters as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
        size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
    );

    let name = resolve_process_name(process_handle).unwrap_or_else(|| "Unknown".to_string());
    
    // Deterministic release of the OS resource handle to prevent memory/handle leaks
    let _ = CloseHandle(process_handle);

    if success.as_bool() {
        let mb_divisor = 1024.0 * 1024.0;
        let working_set_mb = mem_counters.WorkingSetSize as f64 / mb_divisor;
        let private_mb = mem_counters.PrivateUsage as f64 / mb_divisor;
        Some((name, working_set_mb, private_mb))
    } else {
        None
    }
}

/// Resolves the executable filename associated with an active process handle.
unsafe fn resolve_process_name(process_handle: HANDLE) -> Option<String> {
    let mut buffer = [0u16; MAX_PATH as usize];
    let mut buffer_size = buffer.len() as u32;
    
    // Query the OS for the fully qualified path of the process image
    if QueryFullProcessImageNameW(
        process_handle, 
        PROCESS_NAME_WIN32, 
        PWSTR(buffer.as_mut_ptr()), 
        &mut buffer_size
    ).is_ok() {
        let full_path = String::from_utf16_lossy(&buffer[..buffer_size as usize]);
        // Isolate the executable filename from the absolute path string
        let file_name = full_path
            .rsplit('\\')
            .next()
            .unwrap_or(&full_path)
            .to_string();
        Some(file_name)
    } else {
        None
    }
}

/// Encodes a standard Rust string into a standard UTF-16 representation.
fn encode_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Encodes a standard Rust string into a null-terminated UTF-16 representation,
/// suitable for Win32 API interop (e.g., DrawTextW).
fn encode_wide_with_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}