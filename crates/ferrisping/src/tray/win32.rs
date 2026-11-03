//! The tray icon on Windows, and the mutex that keeps one normal copy running.
//!
//! The icon is found by its window and a number rather than by a GUID: Windows ties
//! a GUID to the exe's path, and a portable exe gets moved.

use std::cell::{Cell, OnceCell};
use std::ptr::{null, null_mut, without_provenance};
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_LARGE_ICON, NIIF_NOSOUND, NIIF_USER, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_BALLOONUSERCLICK, NIN_SELECT, NINF_KEY, NOTIFYICON_VERSION_4,
    NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, FindWindowW, GetMessageW, GetSystemMetrics, GetWindowThreadProcessId, HICON,
    HMENU, IMAGE_ICON, KillTimer, LR_DEFAULTCOLOR, LoadImageW, MENU_ITEM_FLAGS, MF_CHECKED, MF_SEPARATOR, MF_STRING,
    MF_UNCHECKED, MSG, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SM_CXICON, SM_CXSMICON,
    SM_CYICON, SM_CYSMICON, SM_MENUDROPALIGNMENT, SetForegroundWindow, SetMenuDefaultItem, SetTimer, TPM_BOTTOMALIGN,
    TPM_LEFTALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTALIGN, TPM_RIGHTBUTTON, TrackPopupMenuEx, TranslateMessage,
    WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_NULL, WM_TIMER, WNDCLASSW, WS_OVERLAPPED,
};

use super::{Banner, Shared, TrayCommand, banner, lock};

/// Held by the copy of FerrisPing that was started normally, while it runs.
pub const MUTEX: &str = r"Local\FerrisPing";
/// The hidden window's class.
const CLASS: &str = "FerrisPingTray";
/// The normally started copy's window title, which a second start looks for.
/// Sessions' windows have none.
const MAIN_TITLE: &str = "FerrisPing";
/// The exe's icon, as winresource numbers it.
const ICON_RESOURCE: usize = 1;
const ICON_ID: u32 = 1;

/// The icon's clicks and banners.
const WM_ICON: u32 = WM_APP + 1;
/// Alerts are waiting.
const WM_ALERTS: u32 = WM_APP + 2;
/// The tooltip changed.
const WM_TIP: u32 = WM_APP + 3;
/// A banner that isn't about alerts is waiting.
const WM_NOTE: u32 = WM_APP + 4;
/// FerrisPing was started again, and that copy wants this one's window open.
const WM_OPEN: u32 = WM_APP + 5;
/// Enter or the space bar on the icon.
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

const BATCH_TIMER: usize = 1;
/// Alerts that come this close together share a banner...
const BATCH_WAIT: Duration = Duration::from_secs(2);
/// ...but a steady stream of them still gets one this often.
const BATCH_MOST: Duration = Duration::from_secs(6);

const MENU_OPEN: usize = 1;
const MENU_PAUSE: usize = 2;
const MENU_NOTIFICATIONS: usize = 3;
const MENU_QUIT: usize = 4;

/// The tray's window, to post to from any thread.
#[derive(Clone, Copy)]
pub struct Window(HWND);

// A window handle is a number for Windows to look up, not memory, and any thread
// may post to it.
unsafe impl Send for Window {}
unsafe impl Sync for Window {}

impl Window {
    pub fn alerts(self) {
        self.post(WM_ALERTS);
    }

    pub fn tip(self) {
        self.post(WM_TIP);
    }

    pub fn note(self) {
        self.post(WM_NOTE);
    }

    fn post(self, message: u32) {
        unsafe { PostMessageW(self.0, message, 0, 0) };
    }
}

/// The icon, and the thread it belongs to. Dropping it takes the icon away.
pub struct Icon {
    window: Window,
    thread: Option<JoinHandle<()>>,
}

impl Icon {
    pub fn start(shared: Arc<Shared>, main: bool) -> Option<Self> {
        let (ready, made) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("tray".to_string())
            .spawn(move || run(shared, main, &ready))
            .ok()?;
        match made.recv() {
            Ok(Some(window)) => Some(Self {
                window,
                thread: Some(thread),
            }),
            _ => {
                let _ = thread.join();
                None
            }
        }
    }

    pub fn window(&self) -> Window {
        self.window
    }
}

impl Drop for Icon {
    fn drop(&mut self) {
        self.window.post(WM_CLOSE);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Take the mutex named `name`, unless another copy holds it. Returns whether this
/// copy holds it now, which it does until it quits.
pub fn claim(name: &str) -> bool {
    let name = wide(name);
    let handle = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
    if handle.is_null() {
        // Can't tell, so run.
        return true;
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(handle) };
        return false;
    }
    // Left open: Windows closes it when this copy quits.
    true
}

/// Ask the normally started copy to open its window. False if it can't be found,
/// after a moment for one that's just starting.
pub fn open_running_copy() -> bool {
    let (class, title) = (wide(CLASS), wide(MAIN_TITLE));
    for _ in 0..20 {
        let hwnd = unsafe { FindWindowW(class.as_ptr(), title.as_ptr()) };
        if !hwnd.is_null() {
            let mut process = 0;
            unsafe {
                GetWindowThreadProcessId(hwnd, &raw mut process);
                // Only the program in front may bring another one to the front.
                AllowSetForegroundWindow(process);
                PostMessageW(hwnd, WM_OPEN, 0, 0);
            }
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

thread_local! {
    /// The tray thread's window and icon, once made.
    static TRAY: OnceCell<TrayWindow> = const { OnceCell::new() };
}

fn run(shared: Arc<Shared>, main: bool, ready: &mpsc::Sender<Option<Window>>) {
    let Some(tray) = TrayWindow::create(shared, main) else {
        let _ = ready.send(None);
        return;
    };
    // The app goes on while the icon is added, since Explorer can be slow to answer.
    // If it isn't up yet, as when FerrisPing starts with Windows, the icon comes with
    // its TaskbarCreated message.
    let _ = ready.send(Some(Window(tray.hwnd)));
    TRAY.with(|cell| cell.get_or_init(|| tray).add());
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&raw mut msg, null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Messages that come while the window is being made go to Windows.
    let handled = TRAY.with(|cell| cell.get().is_some_and(|tray| tray.handle(msg, wparam, lparam)));
    if handled {
        0
    } else {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }
}

struct TrayWindow {
    hwnd: HWND,
    shared: Arc<Shared>,
    small_icon: HICON,
    large_icon: HICON,
    /// Explorer's message that the taskbar is back after a restart, without the icon.
    taskbar_created: u32,
    /// When the oldest alert waiting for its banner came.
    waiting_since: Cell<Option<Instant>>,
    /// The latest banner is about alerts, so a click on it opens the events.
    alerts_shown: Cell<bool>,
}

impl TrayWindow {
    fn create(shared: Arc<Shared>, main: bool) -> Option<Self> {
        let class = wide(CLASS);
        let title = wide(if main { MAIN_TITLE } else { "" });
        let taskbar_created = wide("TaskbarCreated");
        unsafe {
            let instance = GetModuleHandleW(null());
            let window_class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            // Fails harmlessly if a tray made earlier in this process registered it.
            RegisterClassW(&raw const window_class);
            // Top-level, unlike a message-only window, to hear Explorer's broadcasts.
            // It's never shown.
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            if hwnd.is_null() {
                return None;
            }
            let icon = |width, height| {
                LoadImageW(
                    instance,
                    without_provenance(ICON_RESOURCE),
                    IMAGE_ICON,
                    GetSystemMetrics(width),
                    GetSystemMetrics(height),
                    LR_DEFAULTCOLOR,
                ) as HICON
            };
            Some(Self {
                hwnd,
                shared,
                small_icon: icon(SM_CXSMICON, SM_CYSMICON),
                large_icon: icon(SM_CXICON, SM_CYICON),
                taskbar_created: RegisterWindowMessageW(taskbar_created.as_ptr()),
                waiting_since: Cell::new(None),
                alerts_shown: Cell::new(false),
            })
        }
    }

    /// Returns false for messages left to Windows.
    fn handle(&self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
        match msg {
            WM_ICON => match (lparam & 0xffff) as u32 {
                NIN_SELECT | NIN_KEYSELECT => self.shared.send(TrayCommand::Open),
                NIN_BALLOONUSERCLICK => self.shared.send(if self.alerts_shown.get() {
                    TrayCommand::OpenEvents
                } else {
                    TrayCommand::Open
                }),
                // Where to open the menu, in screen pixels.
                WM_CONTEXTMENU => self.menu(i32::from(wparam as i16), i32::from((wparam >> 16) as i16)),
                _ => {}
            },
            WM_ALERTS => self.wait_for_more(),
            WM_TIMER if wparam == BATCH_TIMER => self.show_alerts(),
            WM_TIP => self.update_tip(),
            WM_NOTE => self.show_note(),
            WM_OPEN => self.shared.send(TrayCommand::Open),
            WM_CLOSE => unsafe {
                DestroyWindow(self.hwnd);
            },
            WM_DESTROY => {
                self.remove();
                unsafe { PostQuitMessage(0) };
            }
            _ if msg == self.taskbar_created => self.add(),
            _ => return false,
        }
        true
    }

    /// What every call about the icon starts from.
    fn data(&self) -> NOTIFYICONDATAW {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.hwnd;
        data.uID = ICON_ID;
        data
    }

    fn add(&self) {
        let mut data = self.data();
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = WM_ICON;
        data.hIcon = self.small_icon;
        copy_wide(&mut data.szTip, &lock(&self.shared.tip));
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        unsafe {
            Shell_NotifyIconW(NIM_ADD, &raw const data);
            Shell_NotifyIconW(NIM_SETVERSION, &raw const data);
        }
    }

    fn remove(&self) {
        let data = self.data();
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &raw const data);
            for icon in [self.small_icon, self.large_icon] {
                if !icon.is_null() {
                    DestroyIcon(icon);
                }
            }
        }
    }

    fn update_tip(&self) {
        let mut data = self.data();
        data.uFlags = NIF_TIP | NIF_SHOWTIP;
        copy_wide(&mut data.szTip, &lock(&self.shared.tip));
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data) };
    }

    /// Give more alerts a moment to come, so they share the banner.
    fn wait_for_more(&self) {
        let now = Instant::now();
        let since = self.waiting_since.get().unwrap_or(now);
        self.waiting_since.set(Some(since));
        let wait = BATCH_WAIT.min(BATCH_MOST.saturating_sub(now - since));
        unsafe { SetTimer(self.hwnd, BATCH_TIMER, wait.as_millis() as u32, None) };
    }

    fn show_alerts(&self) {
        unsafe { KillTimer(self.hwnd, BATCH_TIMER) };
        self.waiting_since.set(None);
        let alerts = std::mem::take(&mut *lock(&self.shared.alerts));
        if !alerts.is_empty() && self.shared.notifications.load(Ordering::Relaxed) {
            self.show(&banner(&alerts), true);
        }
    }

    fn show_note(&self) {
        let note = lock(&self.shared.note).take();
        if let Some(note) = note {
            self.show(&note, false);
        }
    }

    fn show(&self, banner: &Banner, alerts: bool) {
        let mut data = self.data();
        data.uFlags = NIF_INFO | NIF_SHOWTIP;
        copy_wide(&mut data.szInfoTitle, &banner.title);
        copy_wide(&mut data.szInfo, &banner.text);
        data.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON;
        if !self.shared.sound.load(Ordering::Relaxed) {
            data.dwInfoFlags |= NIIF_NOSOUND;
        }
        data.hBalloonIcon = self.large_icon;
        if unsafe { Shell_NotifyIconW(NIM_MODIFY, &raw const data) } != 0 {
            self.alerts_shown.set(alerts);
        }
    }

    fn menu(&self, x: i32, y: i32) {
        let paused = self.shared.paused.load(Ordering::Relaxed);
        let notifications = self.shared.notifications.load(Ordering::Relaxed);
        let chosen = unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            append(menu, MF_STRING, MENU_OPEN, "&Open FerrisPing");
            append(menu, MF_SEPARATOR, 0, "");
            append(menu, MF_STRING, MENU_PAUSE, if paused { "&Resume" } else { "&Pause" });
            let checked = if notifications { MF_CHECKED } else { MF_UNCHECKED };
            append(menu, MF_STRING | checked, MENU_NOTIFICATIONS, "Show &notifications");
            append(menu, MF_SEPARATOR, 0, "");
            append(menu, MF_STRING, MENU_QUIT, "&Quit FerrisPing");
            SetMenuDefaultItem(menu, MENU_OPEN as u32, 0);
            // Without this the menu stays open when the user clicks somewhere else.
            SetForegroundWindow(self.hwnd);
            let align = if GetSystemMetrics(SM_MENUDROPALIGNMENT) != 0 {
                TPM_RIGHTALIGN
            } else {
                TPM_LEFTALIGN
            };
            let flags = TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | align;
            let chosen = TrackPopupMenuEx(menu, flags, x, y, self.hwnd, null());
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
            DestroyMenu(menu);
            chosen
        };
        let command = match chosen as usize {
            MENU_OPEN => TrayCommand::Open,
            MENU_PAUSE => TrayCommand::TogglePause,
            MENU_NOTIFICATIONS => TrayCommand::ToggleNotifications,
            MENU_QUIT => TrayCommand::Quit,
            _ => return,
        };
        self.shared.send(command);
    }
}

unsafe fn append(menu: HMENU, flags: MENU_ITEM_FLAGS, id: usize, text: &str) {
    let text = wide(text);
    let text = if flags & MF_SEPARATOR != 0 {
        null()
    } else {
        text.as_ptr()
    };
    unsafe { AppendMenuW(menu, flags, id, text) };
}

/// `text` as Windows takes it, ending in a NUL.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// Copy `text` into a fixed-size field, cut to fit and ending in a NUL.
fn copy_wide(field: &mut [u16], text: &str) {
    let mut units: Vec<u16> = text.encode_utf16().take(field.len() - 1).collect();
    // Don't leave half of a character that needs two units.
    if units.last().is_some_and(|unit| (0xD800..0xDC00).contains(unit)) {
        units.pop();
    }
    field[..units.len()].copy_from_slice(&units);
    field[units.len()] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_copy_holds_the_mutex() {
        // Never the real name, which a FerrisPing that's running may hold.
        let name = format!(r"Local\FerrisPing-test-{}", std::process::id());
        assert!(claim(&name), "the first claim takes it");
        assert!(!claim(&name), "a second one finds it taken");
    }

    #[test]
    fn text_is_cut_to_its_field() {
        let mut field = [0xffff_u16; 8];
        copy_wide(&mut field, "db-01 stopped answering");
        assert_eq!(String::from_utf16_lossy(&field[..7]), "db-01 s");
        assert_eq!(field[7], 0);
        // A crab takes two units, and doesn't fit in the one that's left.
        copy_wide(&mut field, "db-01 🦀");
        assert_eq!(String::from_utf16_lossy(&field[..6]), "db-01 ");
        assert_eq!(field[6], 0);
    }
}
