//! Foreground-window inspection for game detection. Only uses
//! PROCESS_QUERY_LIMITED_INFORMATION handles (what Task Manager uses), so it never reads
//! game memory and is safe alongside anti-cheat software.

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, RECT, WAIT_TIMEOUT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, WaitForSingleObject, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
};

use super::util::from_wide;

pub struct Foreground {
    pub hwnd: isize,
    pub pid: u32,
    pub exe: String,
    pub fullscreen: bool,
    pub monitor: HMONITOR,
}

pub fn process_exe(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit(['\\', '/']).next().map(String::from)
    }
}

pub fn foreground() -> Option<Foreground> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() || !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return None;
        }
        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4).is_ok() && cloaked != 0 {
            return None;
        }
        let mut class = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut class);
        let class = from_wide(&class[..n.max(0) as usize]);
        if matches!(class.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 || pid == GetCurrentProcessId() {
            return None;
        }
        let exe = process_exe(pid)?;
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        Some(Foreground { hwnd: hwnd.0 as isize, pid, exe, fullscreen: covers_monitor(hwnd, monitor), monitor })
    }
}

fn covers_monitor(hwnd: HWND, monitor: HMONITOR) -> bool {
    unsafe {
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_err() {
            return false;
        }
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(monitor, &mut mi).as_bool() {
            return false;
        }
        let m = mi.rcMonitor;
        r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom
    }
}

/// True if `hwnd` still exists and is shown (games often replace a splash window with
/// their main window, or recreate it when switching display modes).
pub fn window_usable(hwnd: isize) -> bool {
    let h = HWND(hwnd as *mut _);
    unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(h)).as_bool() && IsWindowVisible(h).as_bool() }
}

pub fn primary_monitor() -> HMONITOR {
    unsafe { MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY) }
}

/// Watches a process for exit without polling its state through anything heavier than a
/// wait on its handle.
pub struct ProcessWatch(HANDLE);

impl ProcessWatch {
    pub fn new(pid: u32) -> Option<ProcessWatch> {
        unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok().map(ProcessWatch) }
    }

    pub fn alive(&self) -> bool {
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_TIMEOUT }
    }
}

impl Drop for ProcessWatch {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
