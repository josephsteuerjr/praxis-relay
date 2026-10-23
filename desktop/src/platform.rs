//! Native lifecycle differences, independent of account management.
use tauri::{Manager, WebviewWindow};

#[cfg(target_os = "linux")]
pub fn tray_available() -> bool {
    // Creating AppIndicator alone does not prove GNOME/a bare WM has a host.
    // Without one, minimize normally so the operator can still reach the window.
    static CACHE: std::sync::Mutex<Option<(std::time::Instant, bool)>> =
        std::sync::Mutex::new(None);
    let mut cached = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    if let Some((when, available)) = *cached {
        if when.elapsed().as_secs() < 5 {
            return available;
        }
    }
    let available = std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--timeout",
            "1",
            "--dest",
            "org.kde.StatusNotifierWatcher",
            "--object-path",
            "/StatusNotifierWatcher",
            "--method",
            "org.freedesktop.DBus.Properties.Get",
            "org.kde.StatusNotifierWatcher",
            "IsStatusNotifierHostRegistered",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .is_ok_and(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("true")
        });
    *cached = Some((std::time::Instant::now(), available));
    available
}
#[cfg(not(target_os = "linux"))]
pub fn tray_available() -> bool {
    true
}

pub fn hide(window: &WebviewWindow) -> tauri::Result<()> {
    if !tray_available() {
        return window.minimize();
    }
    #[cfg(not(target_os = "macos"))]
    window.set_skip_taskbar(true)?;
    window.hide()?;
    #[cfg(target_os = "macos")]
    window
        .app_handle()
        .set_activation_policy(tauri::ActivationPolicy::Accessory)?;
    Ok(())
}
pub fn show(app: &tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(window) = app.get_webview_window("main") {
        #[cfg(not(target_os = "macos"))]
        let _ = window.set_skip_taskbar(false);
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}
