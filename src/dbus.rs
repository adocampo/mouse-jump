/// D-Bus interactions with KWin for screenshot capture, cursor warping,
/// monitor layout queries, and global shortcut registration.
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::os::fd::FromRawFd;
use zbus::Connection;
use zbus::proxy::Proxy;
use zbus::zvariant::{self, Fd};

use crate::coords::Monitor;

/// Get the current cursor position (global desktop coordinates) via KWin scripting.
/// xdotool doesn't work on Wayland (returns stale XWayland position).
pub async fn get_cursor_position(conn: &Connection) -> Result<(i32, i32)> {
    let script_content = r#"console.log("MOUSE_JUMP_CURSOR:" + workspace.cursorPos.x + "," + workspace.cursorPos.y);"#;
    let script_path = "/tmp/mouse-jump-get-cursor.js";
    tokio::fs::write(script_path, script_content).await?;

    let proxy: Proxy<'_> = zbus::proxy::Builder::new(conn)
        .destination("org.kde.KWin")?
        .path("/Scripting")?
        .interface("org.kde.kwin.Scripting")?
        .build()
        .await?;

    let script_name = "mouse-jump-get-cursor";
    let _ = proxy.call_method("unloadScript", &(script_name,)).await;

    let script_id: i32 = proxy
        .call_method("loadScript", &(script_path, script_name))
        .await
        .context("Failed to load cursor position script")?
        .body()
        .deserialize()?;

    let script_path_dbus = format!("/Scripting/Script{}", script_id);
    let script_proxy: Proxy<'_> = zbus::proxy::Builder::new(conn)
        .destination("org.kde.KWin")?
        .path(script_path_dbus.as_str())?
        .interface("org.kde.kwin.Script")?
        .build()
        .await?;

    script_proxy.call_method("run", &()).await?;

    // Give KWin a moment to execute and journal to flush
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Read cursor position from journal
    let journal_output = tokio::process::Command::new("journalctl")
        .args(["--user", "-t", "kwin_wayland", "--since", "3 sec ago", "--no-pager", "-o", "cat"])
        .output()
        .await
        .context("Failed to read journal")?;

    let _ = proxy.call_method("unloadScript", &(script_name,)).await;
    let _ = tokio::fs::remove_file(script_path).await;

    let journal_str = String::from_utf8_lossy(&journal_output.stdout);
    for line in journal_str.lines().rev() {
        if let Some(pos) = line.strip_prefix("MOUSE_JUMP_CURSOR:") {
            let parts: Vec<&str> = pos.split(',').collect();
            if parts.len() == 2 {
                let x = parts[0].trim().parse::<i32>().unwrap_or(0);
                let y = parts[1].trim().parse::<i32>().unwrap_or(0);
                return Ok((x, y));
            }
        }
    }

    anyhow::bail!("Failed to read cursor position from KWin script output")
}

/// Capture a screenshot of the entire workspace (all monitors) via KWin's ScreenShot2 interface.
/// Returns raw PNG bytes.
pub async fn capture_workspace(conn: &Connection) -> Result<Vec<u8>> {
    // Try KWin D-Bus first, fall back to spectacle CLI
    match capture_workspace_dbus(conn).await {
        Ok(data) if !data.is_empty() => Ok(data),
        Err(e) => {
            log::debug!("D-Bus screenshot failed: {}, falling back to spectacle", e);
            capture_workspace_spectacle().await
        }
        Ok(_) => capture_workspace_spectacle().await,
    }
}

/// Fast capture: skip D-Bus attempt (NotAuthorized on this system) and go straight to spectacle.
pub async fn capture_workspace_fast() -> Result<Vec<u8>> {
    capture_workspace_spectacle().await
}

/// Capture via spectacle CLI (KDE's native screenshot tool).
async fn capture_workspace_spectacle() -> Result<Vec<u8>> {
    let tmp_path = "/tmp/mouse-jump-screenshot.png";

    let output = tokio::process::Command::new("spectacle")
        .args(["-b", "-n", "-f", "-o", tmp_path])
        .output()
        .await
        .context("Failed to run spectacle (is it installed?)")?;

    if !output.status.success() {
        anyhow::bail!(
            "spectacle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Read the PNG file
    let data = tokio::fs::read(tmp_path)
        .await
        .context("Failed to read screenshot file")?;

    // Clean up
    let _ = tokio::fs::remove_file(tmp_path).await;

    Ok(data)
}

/// Capture via KWin D-Bus ScreenShot2 interface (requires authorization).
async fn capture_workspace_dbus(conn: &Connection) -> Result<Vec<u8>> {
    let proxy: Proxy<'_> = zbus::proxy::Builder::new(conn)
        .destination("org.kde.KWin")?
        .path("/org/kde/KWin/ScreenShot2")?
        .interface("org.kde.KWin.ScreenShot2")?
        .build()
        .await?;

    // CaptureWorkspace signature: (options: a{sv}, pipe: h) -> results: a{sv}
    // We create a pipe, pass the write end, and read from the read end.
    let (read_fd, write_fd) = nix_pipe()?;

    let options: HashMap<&str, zvariant::Value<'_>> = HashMap::from([
        ("include-cursor", zvariant::Value::Bool(false)),
        ("native-resolution", zvariant::Value::Bool(true)),
    ]);

    // Pass the write end of the pipe as the fd handle
    let pipe_fd = Fd::from(&write_fd);

    let _result: HashMap<String, zvariant::OwnedValue> = proxy
        .call_method("CaptureWorkspace", &(options, pipe_fd))
        .await
        .context("Failed to call CaptureWorkspace")?
        .body()
        .deserialize()
        .context("Failed to deserialize CaptureWorkspace result")?;

    // Close the write end so we get EOF when reading
    drop(write_fd);

    // Read all PNG data from the read end
    use std::io::Read;
    let mut file = read_fd;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .context("Failed to read screenshot data from pipe")?;

    Ok(buf)
}

/// Create a Unix pipe, returning (read_end, write_end) as std::fs::File handles.
fn nix_pipe() -> Result<(std::fs::File, std::fs::File)> {
    let mut fds = [0i32; 2];
    let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    if ret != 0 {
        anyhow::bail!("pipe2 failed: {}", std::io::Error::last_os_error());
    }
    let read_file = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let write_file = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    Ok((read_file, write_file))
}

/// Warp the mouse cursor to absolute coordinates.
/// Tries multiple strategies: dotool (preferred), ydotool, kdotool, KWin scripting.
pub async fn warp_cursor(conn: &Connection, x: f64, y: f64, desktop_width: u32, desktop_height: u32) -> Result<()> {
    log::info!("Warping cursor to ({:.0}, {:.0})", x, y);

    // Strategy 1: dotool (uses libei / uinput with percentage-based absolute positioning)
    match warp_cursor_dotool(x, y, desktop_width, desktop_height).await {
        Ok(()) => {
            log::info!("Cursor warped via dotool");
            return Ok(());
        }
        Err(e) => log::debug!("dotool failed: {}", e),
    }

    // Strategy 2: ydotool (uses /dev/uinput, needs ydotoold running)
    match warp_cursor_ydotool(x, y, desktop_width, desktop_height).await {
        Ok(()) => {
            log::info!("Cursor warped via ydotool");
            return Ok(());
        }
        Err(e) => log::debug!("ydotool failed: {}", e),
    }

    // Strategy 3: kdotool (KDE-specific)
    match warp_cursor_kdotool(x, y).await {
        Ok(()) => {
            log::info!("Cursor warped via kdotool");
            return Ok(());
        }
        Err(e) => log::debug!("kdotool failed: {}", e),
    }

    // Strategy 4: KWin scripting (may not work on Wayland due to input security)
    match warp_cursor_kwin_script(conn, x, y).await {
        Ok(()) => {
            log::info!("Cursor warped via KWin scripting");
            return Ok(());
        }
        Err(e) => log::debug!("KWin scripting failed: {}", e),
    }

    anyhow::bail!(
        "Failed to warp cursor to ({:.0}, {:.0}). None of the strategies worked.", x, y
    )
}

/// Warp cursor using KWin scripting via D-Bus.
/// This is the most reliable method for KDE Plasma 6 Wayland.
async fn warp_cursor_kwin_script(conn: &Connection, x: f64, y: f64) -> Result<()> {
    // Write a small KWin script to set cursor position
    let script_content = format!(
        "workspace.cursorPos = Qt.point({}, {});",
        x as i32, y as i32
    );
    let script_path = "/tmp/mouse-jump-warp.js";
    tokio::fs::write(script_path, &script_content)
        .await
        .context("Failed to write warp script")?;

    let proxy: Proxy<'_> = zbus::proxy::Builder::new(conn)
        .destination("org.kde.KWin")?
        .path("/Scripting")?
        .interface("org.kde.kwin.Scripting")?
        .build()
        .await?;

    // Load the script
    let script_id: i32 = proxy
        .call_method("loadScript", &(script_path, "mouse-jump-warp"))
        .await
        .context("Failed to load KWin script")?
        .body()
        .deserialize()
        .context("Failed to get script ID")?;

    // Run the script
    let script_proxy: Proxy<'_> = zbus::proxy::Builder::new(conn)
        .destination("org.kde.KWin")?
        .path(format!("/Scripting/Script{}", script_id))?
        .interface("org.kde.kwin.Script")?
        .build()
        .await?;

    script_proxy
        .call_method("run", &())
        .await
        .context("Failed to run KWin warp script")?;

    // Small delay to let the script execute, then unload
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Stop/unload the script to avoid accumulating scripts
    let _ = script_proxy.call_method("stop", &()).await;
    let _ = proxy
        .call_method("unloadScript", &("mouse-jump-warp",))
        .await;

    // Clean up temp file
    let _ = tokio::fs::remove_file(script_path).await;

    Ok(())
}

async fn warp_cursor_dotool(x: f64, y: f64, desktop_width: u32, desktop_height: u32) -> Result<()> {
    // dotool mouseto uses percentages (0.0 to 1.0) of the total screen area
    let pct_x = x / desktop_width as f64;
    let pct_y = y / desktop_height as f64;
    log::info!("dotool mouseto {:.6} {:.6} (from pixel {:.0},{:.0} in {}x{})",
        pct_x, pct_y, x, y, desktop_width, desktop_height);

    let cmd = format!("mouseto {} {}", pct_x, pct_y);
    let output = tokio::process::Command::new("sh")
        .args(["-c", &format!("echo '{}' | dotool", cmd)])
        .output()
        .await
        .context("dotool not found")?;

    if !output.status.success() {
        anyhow::bail!("dotool failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

async fn warp_cursor_ydotool(x: f64, y: f64, desktop_width: u32, desktop_height: u32) -> Result<()> {
    // ydotool absolute mode uses coordinates in range 0..32767 (like a graphics tablet),
    // not pixel coordinates. Scale accordingly.
    let abs_x = ((x / desktop_width as f64) * 32767.0) as i32;
    let abs_y = ((y / desktop_height as f64) * 32767.0) as i32;
    log::debug!("ydotool abs coords: ({}, {}) from pixel ({:.0}, {:.0}) desktop {}x{}",
        abs_x, abs_y, x, y, desktop_width, desktop_height);

    let output = tokio::process::Command::new("ydotool")
        .args([
            "mousemove",
            "--absolute",
            "-x",
            &format!("{}", abs_x),
            "-y",
            &format!("{}", abs_y),
        ])
        .output()
        .await
        .context("ydotool not found")?;

    if !output.status.success() {
        anyhow::bail!("ydotool failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

async fn warp_cursor_kdotool(x: f64, y: f64) -> Result<()> {
    let output = tokio::process::Command::new("kdotool")
        .args(["mousemove", &format!("{}", x as i32), &format!("{}", y as i32)])
        .output()
        .await
        .context("kdotool not found")?;

    if !output.status.success() {
        anyhow::bail!("kdotool failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

/// Query monitor layout from KScreen via D-Bus.
/// Falls back to parsing kscreen-doctor output if D-Bus fails.
pub async fn get_monitors(conn: &Connection) -> Result<Vec<Monitor>> {
    match get_monitors_kscreen(conn).await {
        Ok(monitors) if !monitors.is_empty() => Ok(monitors),
        _ => get_monitors_cli().await,
    }
}

/// Query monitors via KScreen D-Bus interface.
async fn get_monitors_kscreen(conn: &Connection) -> Result<Vec<Monitor>> {
    let proxy: Proxy<'_> = zbus::proxy::Builder::new(conn)
        .destination("org.kde.KScreen")?
        .path("/backend")?
        .interface("org.kde.KScreen.Backend")?
        .build()
        .await?;

    // GetConfig returns a complex structure; we'll try to parse it
    let reply = proxy
        .call_method("getConfig", &())
        .await
        .context("Failed to call KScreen getConfig")?;

    let body = reply.body();
    let config: HashMap<String, zbus::zvariant::OwnedValue> = body
        .deserialize()
        .context("Failed to deserialize KScreen config")?;

    let monitors = Vec::new();

    if let Some(outputs_val) = config.get("outputs") {
        // Parse outputs array - this is compositor-specific
        // For now, log and fallback to CLI if parsing is complex
        log::debug!("KScreen outputs value: {:?}", outputs_val);
        let _ = outputs_val; // Suppress unused warning
    }

    if monitors.is_empty() {
        anyhow::bail!("No monitors found via KScreen D-Bus");
    }

    Ok(monitors)
}

/// Fallback: parse kscreen-doctor --outputs to get monitor layout.
async fn get_monitors_cli() -> Result<Vec<Monitor>> {
    let output = tokio::process::Command::new("kscreen-doctor")
        .arg("--outputs")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .output()
        .await
        .context("Failed to run kscreen-doctor")?;

    if !output.status.success() {
        anyhow::bail!(
            "kscreen-doctor failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Strip any remaining ANSI escape codes
    let clean = strip_ansi_codes(&stdout);
    log::debug!("kscreen-doctor output ({} bytes, {} lines)", clean.len(), clean.lines().count());
    parse_kscreen_doctor_output(&clean)
}

/// Strip ANSI escape sequences from a string.
fn strip_ansi_codes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip until we find a letter (the terminator of the escape sequence)
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if nc.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Parse kscreen-doctor output format:
/// Output: 1 DP-1 <uuid>
///         enabled
///         connected
///         Geometry: 0,0 3840x2160
///         Scale: 1.5
///         ...
fn parse_kscreen_doctor_output(output: &str) -> Result<Vec<Monitor>> {
    let mut monitors = Vec::new();
    let mut current_name = String::new();
    let mut current_x: i32 = 0;
    let mut current_y: i32 = 0;
    let mut current_w: u32 = 0;
    let mut current_h: u32 = 0;
    let mut current_scale: f64 = 1.0;
    let mut in_output = false;
    let mut enabled = false;

    for line in output.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("Output:") {
            // Save previous monitor if valid
            if in_output && enabled && current_w > 0 {
                monitors.push(Monitor {
                    name: current_name.clone(),
                    x: current_x,
                    y: current_y,
                    width: current_w,
                    height: current_h,
                    scale: current_scale,
                });
            }
            // Reset for new output
            in_output = true;
            enabled = false; // Will be set when we see "enabled" on a following line
            // Extract name (second token after "Output: N")
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            current_name = parts.get(2).unwrap_or(&"unknown").to_string();
            current_x = 0;
            current_y = 0;
            current_w = 0;
            current_h = 0;
            current_scale = 1.0;
        } else if in_output {
            if trimmed == "enabled" {
                enabled = true;
            } else if trimmed == "disabled" {
                enabled = false;
            } else if trimmed.starts_with("Geometry:") {
                // Format: "Geometry: X,Y WxH"
                let rest = trimmed.strip_prefix("Geometry:").unwrap().trim();
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if let Some(pos) = parts.first() {
                    let coords: Vec<&str> = pos.split(',').collect();
                    if coords.len() == 2 {
                        current_x = coords[0].parse().unwrap_or(0);
                        current_y = coords[1].parse().unwrap_or(0);
                    }
                }
                if let Some(size) = parts.get(1) {
                    let dims: Vec<&str> = size.split('x').collect();
                    if dims.len() == 2 {
                        current_w = dims[0].parse().unwrap_or(0);
                        current_h = dims[1].parse().unwrap_or(0);
                    }
                }
            } else if trimmed.starts_with("Scale:") {
                let rest = trimmed.strip_prefix("Scale:").unwrap().trim();
                current_scale = rest.parse().unwrap_or(1.0);
            }
        }
    }

    // Don't forget the last monitor
    if in_output && enabled && current_w > 0 {
        monitors.push(Monitor {
            name: current_name,
            x: current_x,
            y: current_y,
            width: current_w,
            height: current_h,
            scale: current_scale,
        });
    }

    if monitors.is_empty() {
        anyhow::bail!("No monitors found in kscreen-doctor output");
    }

    Ok(monitors)
}

const KWIN_SCRIPT_NAME: &str = "mouse-jump-pos";

/// Load a KWin script that positions the mouse-jump window at (x, y) when it appears.
/// Must be called BEFORE showing the overlay.
pub async fn load_positioning_script(conn: &Connection, x: i32, y: i32, w: u32, h: u32) -> Result<()> {
    let script_content = format!(
        r#"workspace.windowAdded.connect(function(win) {{
    if (win.resourceClass === "mouse-jump" || win.resourceName === "mouse-jump") {{
        win.noBorder = true;
        win.keepAbove = true;
        var target = {{x: {x}, y: {y}, width: {w}, height: {h}}};
        win.frameGeometry = target;
        win.frameGeometryChanged.connect(function() {{
            if (win.frameGeometry.x !== {x} || win.frameGeometry.y !== {y}) {{
                win.frameGeometry = target;
            }}
        }});
    }}
}});"#,
        x = x,
        y = y,
        w = w,
        h = h
    );

    let script_path = "/tmp/mouse-jump-position.js";
    tokio::fs::write(script_path, &script_content).await?;

    let proxy: Proxy<'_> = zbus::proxy::Builder::new(conn)
        .destination("org.kde.KWin")?
        .path("/Scripting")?
        .interface("org.kde.kwin.Scripting")?
        .build()
        .await?;

    // Unload any previous instance
    let _ = proxy
        .call_method("unloadScript", &(KWIN_SCRIPT_NAME,))
        .await;

    // Load and run
    let script_id: i32 = proxy
        .call_method("loadScript", &(script_path, KWIN_SCRIPT_NAME))
        .await
        .context("Failed to load KWin positioning script")?
        .body()
        .deserialize()?;

    let script_path_dbus = format!("/Scripting/Script{}", script_id);
    let script_proxy: Proxy<'_> = zbus::proxy::Builder::new(conn)
        .destination("org.kde.KWin")?
        .path(script_path_dbus.as_str())?
        .interface("org.kde.kwin.Script")?
        .build()
        .await?;

    script_proxy.call_method("run", &()).await?;
    log::info!("KWin positioning script loaded: target ({}, {})", x, y);
    Ok(())
}

/// Unload the positioning KWin script (cleanup after overlay is shown).
pub async fn unload_positioning_script(conn: &Connection) {
    let proxy: Proxy<'_> = match zbus::proxy::Builder::new(conn)
        .destination("org.kde.KWin")
        .and_then(|b| b.path("/Scripting"))
        .and_then(|b| b.interface("org.kde.kwin.Scripting"))
    {
        Ok(b) => match b.build().await {
            Ok(p) => p,
            Err(_) => return,
        },
        Err(_) => return,
    };
    let _ = proxy
        .call_method("unloadScript", &(KWIN_SCRIPT_NAME,))
        .await;
    let _ = tokio::fs::remove_file("/tmp/mouse-jump-position.js").await;
}

/// Register a global shortcut with KDE's kglobalaccel.
/// The shortcut activation will be received as a D-Bus signal.
#[allow(dead_code)]
pub async fn register_shortcut(conn: &Connection, shortcut: &str) -> Result<()> {
    let proxy: Proxy<'_> = zbus::proxy::Builder::new(conn)
        .destination("org.kde.kglobalaccel")?
        .path("/kglobalaccel")?
        .interface("org.kde.KGlobalAccel")?
        .build()
        .await?;

    // Register our component and action
    // The format for KGlobalAccel is:
    // setShortcut(component_unique, friendly_name, action_unique, action_friendly, default_keys, keys)
    let component_unique = "mouse-jump";
    let component_friendly = "Mouse Jump";
    let action_unique = "activate";
    let action_friendly = "Activate Mouse Jump";

    // Keys format: list of key sequences as strings
    let default_keys: Vec<String> = vec![shortcut.to_string()];
    let keys: Vec<String> = vec![shortcut.to_string()];

    // The actual D-Bus call uses a different method signature
    // doRegister with (component_unique, action_id, friendly_name, keys)
    let action_id = vec![
        component_unique.to_string(),
        action_unique.to_string(),
        component_friendly.to_string(),
        action_friendly.to_string(),
    ];

    let result = proxy
        .call_method("setShortcutKeys", &(action_id, default_keys, keys))
        .await;

    match result {
        Ok(_) => {
            log::info!("Global shortcut '{}' registered successfully", shortcut);
            Ok(())
        }
        Err(e) => {
            log::warn!("Failed to register shortcut via KGlobalAccel: {}", e);
            log::info!("You can manually set the shortcut in System Settings > Shortcuts");
            // Non-fatal: user can set shortcut manually
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_kscreen_doctor_output() {
        let output = r#"Output: 1 DP-1 some-uuid-here
        enabled
        connected
        priority 1
        Modes:  0:3840x2160@60  1:2560x1440@60  
        Geometry: 0,0 3840x2160
        Scale: 1.5
        Rotation: 1
Output: 2 HDMI-1 another-uuid
        enabled
        connected
        priority 2
        Modes:  0:1920x1080@60  
        Geometry: 3840,0 1920x1080
        Scale: 1
        Rotation: 1
Output: 3 DP-2 third-uuid
        disabled
        disconnected
"#;
        let monitors = parse_kscreen_doctor_output(output).unwrap();
        assert_eq!(monitors.len(), 2);
        assert_eq!(monitors[0].name, "DP-1");
        assert_eq!(monitors[0].x, 0);
        assert_eq!(monitors[0].y, 0);
        assert_eq!(monitors[0].width, 3840);
        assert_eq!(monitors[0].height, 2160);
        assert_eq!(monitors[0].scale, 1.5);
        assert_eq!(monitors[1].name, "HDMI-1");
        assert_eq!(monitors[1].x, 3840);
        assert_eq!(monitors[1].width, 1920);
    }
}
