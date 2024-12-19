// Add tracing to dependencies in Cargo.toml
use tauri::Emitter;
use tracing::{info, warn};
#[cfg(target_os = "linux")]
use zbus::{blocking::Connection, dbus_proxy};

#[cfg(target_os = "windows")]
use windows::{
    core::*,
    Win32::Foundation::*,
    Win32::System::{
        LibraryLoader::*,
        RemoteDesktop::{WTSRegisterSessionNotification, NOTIFY_FOR_ALL_SESSIONS},
    },
    Win32::UI::Input::KeyboardAndMouse::GetActiveWindow,
    Win32::UI::WindowsAndMessaging::*,
};

#[cfg(target_os = "macos")]
extern crate core_foundation;
#[cfg(target_os = "macos")]
extern crate core_graphics;

#[cfg(target_os = "macos")]
use core_foundation::{base::TCFType, base::ToVoid, dictionary::CFDictionary, string::CFString};

use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use tauri::{
    plugin::{Builder, TauriPlugin},
    AppHandle, Runtime,
};

#[cfg(target_os = "macos")]
extern "C" {
    fn CGSessionCopyCurrentDictionary() -> core_foundation::dictionary::CFDictionaryRef;
}

#[cfg(target_os = "linux")]
#[dbus_proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1/session/auto"
)]
trait Session {
    #[dbus_proxy(property)]
    fn locked_hint(&self) -> zbus::Result<bool>;
}

#[cfg(target_os = "windows")]
fn register_session_notification(hwnd: HWND) {
    unsafe {
        let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_ALL_SESSIONS);
    }
}

#[cfg(target_os = "windows")]
extern "system" fn wndproc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match message as u32 {
            _ => DefWindowProcA(window, message, wparam, lparam),
        }
    }
}

pub static WINDOW_TAURI: OnceLock<AppHandle> = OnceLock::new();

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    #[cfg(target_os = "windows")]
    {
        thread::spawn(|| unsafe {
            info!("Starting new thread for Windows screen lock monitoring...");
            let instance = GetModuleHandleA(None).unwrap();
            debug_assert!(instance.0 != 0);

            let window_class = s!("window");

            let wc = WNDCLASSA {
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap(),
                hInstance: instance.into(),
                lpszClassName: window_class,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                ..Default::default()
            };

            let atom = RegisterClassA(&wc);
            debug_assert!(atom != 0);

            CreateWindowExA(
                WINDOW_EX_STYLE::default(),
                window_class,
                s!("Window"),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                None,
                None,
                instance,
                Some(std::ptr::null()),
            );

            let hwnd = GetActiveWindow();
            ShowWindow(*&hwnd, SW_HIDE);

            let mut message = MSG::default();
            register_session_notification(hwnd);
            while GetMessageA(&mut message, HWND(0), 0, 0).into() {
                if message.message == WM_WTSSESSION_CHANGE {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);

                    match message.wParam.0 as u32 {
                        WTS_SESSION_LOCK => match WINDOW_TAURI.get() {
                            Ok(handle) => {
                                let _ = handle.emit(
                                    "window_screen_lock_status://change_session_status",
                                    "lock",
                                );
                                info!("Screen locked");
                            }
                            Err(e) => warn!("Failed to get WINDOW_TAURI handle: {}", e),
                        },
                        WTS_SESSION_UNLOCK => match WINDOW_TAURI.get() {
                            Ok(handle) => {
                                let _ = handle.emit(
                                    "window_screen_lock_status://change_session_status",
                                    "unlock",
                                );
                                info!("Screen unlocked");
                            }
                            Err(e) => warn!("Failed to get WINDOW_TAURI handle: {}", e),
                        },
                        _ => {}
                    }
                }
                thread::sleep(Duration::from_millis(1000));
            }
        });
    }

    #[cfg(target_os = "linux")]
    {
        thread::spawn(move || {
            info!("Starting new thread for Linux screen lock monitoring...");
            let mut flg = false;
            loop {
                let conn = match Connection::system() {
                    Ok(conn) => conn,
                    Err(e) => {
                        warn!("Failed to establish system connection: {}", e);
                        break;
                    }
                };

                let proxy = match SessionProxyBlocking::new(&conn) {
                    Ok(proxy) => proxy,
                    Err(e) => {
                        warn!("Failed to create session proxy: {}", e);
                        break;
                    }
                };

                let mut property = proxy.receive_locked_hint_changed();

                match property.next() {
                    Some(pro) => {
                        let current_property = match pro.get() {
                            Ok(prop) => prop,
                            Err(e) => {
                                warn!("Failed to get property: {}", e);
                                break;
                            }
                        };

                        if flg != current_property {
                            flg = current_property;
                            match WINDOW_TAURI.get() {
                                Some(handle) => {
                                    if current_property {
                                        let _ = handle.emit(
                                            "window_screen_lock_status://change_session_status",
                                            "lock",
                                        );
                                        info!("Screen locked");
                                    } else {
                                        let _ = handle.emit(
                                            "window_screen_lock_status://change_session_status",
                                            "unlock",
                                        );
                                        info!("Screen unlocked");
                                    }
                                }
                                None => {
                                    warn!("Failed to get WINDOW_TAURI handle");
                                    break;
                                }
                            }
                        }
                    }
                    None => {
                        warn!("No property changes received");
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(1000));
            }
        });
    }

    #[cfg(target_os = "macos")]
    {
        thread::spawn(move || {
            info!("Starting new thread for macOS screen lock monitoring...");
            let mut flg = false;
            loop {
                unsafe {
                    let session_dictionary_ref = CGSessionCopyCurrentDictionary();
                    let session_dictionary: CFDictionary =
                        CFDictionary::wrap_under_create_rule(session_dictionary_ref);
                    let mut current_session_property = false;
                    match session_dictionary
                        .find(CFString::new("CGSSessionScreenIsLocked").to_void())
                    {
                        None => current_session_property = false,
                        Some(_) => current_session_property = true,
                    }
                    if flg != current_session_property {
                        flg = current_session_property;
                        match WINDOW_TAURI.get() {
                            Some(handle) => {
                                if current_session_property {
                                    let _ = handle.emit(
                                        "window_screen_lock_status://change_session_status",
                                        "lock",
                                    );
                                    info!("Screen locked");
                                } else {
                                    let _ = handle.emit(
                                        "window_screen_lock_status://change_session_status",
                                        "unlock",
                                    );
                                    info!("Screen unlocked");
                                }
                            }
                            None => {
                                warn!("Failed to get WINDOW_TAURI handle");
                                break;
                            }
                        }
                    }
                    thread::sleep(Duration::from_millis(1000));
                }
            }
        });
    }
    Builder::new("window_screen_lock_status").build()
}
