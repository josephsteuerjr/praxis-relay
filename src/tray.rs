//! Windows system-tray presence for the running relay server.
//!
//! A console window that must stay open reads as fragile — one stray click on
//! its X kills the relay.  Once the server starts, the console hides into a
//! tray icon by the clock: double-click toggles the window, the menu opens
//! the health page or quits deliberately.  The console (with its live log)
//! stays one click away instead of one click from death.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::System::Console::GetConsoleWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, IsWindowVisible, PeekMessageW, SetForegroundWindow, ShowWindow,
    TranslateMessage, MSG, PM_REMOVE, SW_HIDE, SW_SHOW,
};

static SPAWNED: AtomicBool = AtomicBool::new(false);

/// Start the tray thread once; safe to call again after a failed server start.
pub fn spawn(port: u16) {
    if SPAWNED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("tray".to_string())
        .spawn(move || run(port));
}

fn run(port: u16) {
    let menu = Menu::new();
    let toggle = MenuItem::new("Show / hide the console window", true, None);
    let health = MenuItem::new("Open the health page in a browser", true, None);
    let quit = MenuItem::new("Quit (stop the relay)", true, None);
    let _ = menu.append(&toggle);
    let _ = menu.append(&health);
    let _ = menu.append(&quit);

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(format!("Praxis Relay — 127.0.0.1:{port}"))
        .with_icon(icon())
        .build();
    let _tray = match tray {
        Ok(tray) => tray,
        Err(error) => {
            // No tray (odd shells, remote sessions) → the console simply stays
            // visible; the relay itself is unaffected.
            eprintln!("tray unavailable ({error}); the console window stays visible.");
            return;
        }
    };

    set_console_visible(false);

    loop {
        // tray-icon delivers its events only while this thread pumps Windows
        // messages; PeekMessage keeps the loop responsive without a WndProc.
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id() == toggle.id() {
                toggle_console();
            } else if event.id() == health.id() {
                open_in_browser(&format!("http://127.0.0.1:{port}/health"));
            } else if event.id() == quit.id() {
                std::process::exit(0);
            }
        }
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                toggle_console();
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn set_console_visible(visible: bool) {
    unsafe {
        let window = GetConsoleWindow();
        if window as usize == 0 {
            return;
        }
        ShowWindow(window, if visible { SW_SHOW } else { SW_HIDE });
        if visible {
            SetForegroundWindow(window);
        }
    }
}

fn toggle_console() {
    unsafe {
        let window = GetConsoleWindow();
        if window as usize == 0 {
            return;
        }
        let visible = IsWindowVisible(window) != 0;
        set_console_visible(!visible);
    }
}

fn open_in_browser(url: &str) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

/// A generated 32×32 icon (teal disc, white core) — no asset files to ship.
fn icon() -> Icon {
    const SIZE: usize = 32;
    let mut rgba = vec![0u8; SIZE * SIZE * 4];
    let center = (SIZE as f32 - 1.0) / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let distance = (dx * dx + dy * dy).sqrt();
            let index = (y * SIZE + x) * 4;
            let pixel: Option<[u8; 3]> = if distance <= 5.0 {
                Some([236, 244, 243])
            } else if distance <= 12.5 {
                Some([16, 163, 152])
            } else if distance <= 14.5 {
                Some([10, 90, 84])
            } else {
                None
            };
            if let Some([r, g, b]) = pixel {
                rgba[index] = r;
                rgba[index + 1] = g;
                rgba[index + 2] = b;
                rgba[index + 3] = 255;
            }
        }
    }
    Icon::from_rgba(rgba, SIZE as u32, SIZE as u32).expect("static icon dimensions are valid")
}
