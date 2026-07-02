/// Mouse Jump — a PowerToys Mouse Jump equivalent for KDE Plasma 6 Wayland.
///
/// Captures a screenshot of all monitors, shows a scaled thumbnail popup,
/// and teleports the cursor to wherever the user clicks.
mod coords;
mod dbus;
mod overlay;
mod render;

use anyhow::{Context, Result};
use coords::{CoordMapper, DesktopBounds};
use overlay::OverlayResult;

/// Maximum overlay size as a fraction of the primary monitor.
const OVERLAY_SCALE_FACTOR: f64 = 0.25;
/// Maximum overlay width in pixels.
const MAX_OVERLAY_WIDTH: u32 = 1200;
/// Maximum overlay height in pixels.
const MAX_OVERLAY_HEIGHT: u32 = 800;
/// Border width around the thumbnail.
const BORDER_WIDTH: u32 = 2;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("mouse_jump=info")).init();

    // If --delay N is passed, wait N seconds (for testing without a hotkey)
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--delay") {
        if let Some(secs) = args.get(pos + 1).and_then(|s| s.parse::<u64>().ok()) {
            log::info!("Waiting {} seconds — move cursor to desired position...", secs);
            tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
        }
    }

    log::info!("Mouse Jump starting...");

    // Connect to D-Bus session bus
    let conn = zbus::Connection::session()
        .await
        .context("Failed to connect to D-Bus session bus")?;

    // Get monitor layout
    log::info!("Querying monitor layout...");
    let monitors = dbus::get_monitors(&conn).await?;
    log::info!("Found {} monitor(s):", monitors.len());
    for m in &monitors {
        log::info!(
            "  {} @ ({},{}) {}x{} scale={}",
            m.name,
            m.x,
            m.y,
            m.width,
            m.height,
            m.scale
        );
    }

    // Compute desktop bounds
    let desktop = DesktopBounds::from_monitors(&monitors);
    log::info!(
        "Virtual desktop: ({},{}) {}x{}",
        desktop.x,
        desktop.y,
        desktop.width,
        desktop.height
    );

    // Get current cursor position EARLY (before screenshot to avoid delay)
    let (cursor_x, cursor_y) = dbus::get_cursor_position(&conn).await.unwrap_or((0, 0));
    log::info!("Cursor at global ({}, {})", cursor_x, cursor_y);

    // Determine which monitor the cursor is on
    let cursor_monitor = monitors.iter().find(|m| {
        cursor_x >= m.x
            && cursor_x < m.x + m.width as i32
            && cursor_y >= m.y
            && cursor_y < m.y + m.height as i32
    });
    let (monitor_w, monitor_h, monitor_x, monitor_y) = match cursor_monitor {
        Some(m) => (m.width, m.height, m.x, m.y),
        None => (monitors[0].width, monitors[0].height, monitors[0].x, monitors[0].y),
    };
    log::info!("Cursor is on monitor at ({},{}) {}x{}", monitor_x, monitor_y, monitor_w, monitor_h);

    // Calculate thumbnail size
    let max_w = (desktop.width as f64 * OVERLAY_SCALE_FACTOR) as u32;
    let max_h = (desktop.height as f64 * OVERLAY_SCALE_FACTOR) as u32;
    let max_w = max_w.min(MAX_OVERLAY_WIDTH);
    let max_h = max_h.min(MAX_OVERLAY_HEIGHT);
    let (thumb_w, thumb_h) = CoordMapper::compute_overlay_size(&desktop, max_w, max_h);
    log::info!("Thumbnail size: {}x{}", thumb_w, thumb_h);

    // Capture workspace screenshot (skip D-Bus, go straight to spectacle)
    log::info!("Capturing workspace screenshot...");
    let png_data = dbus::capture_workspace_fast().await?;
    log::info!("Screenshot captured: {} bytes", png_data.len());

    // Decode and scale the screenshot
    let (mut pixel_data, thumb_w, thumb_h) =
        render::decode_and_scale(&png_data, thumb_w, thumb_h)?;
    log::info!("Thumbnail rendered: {}x{}", thumb_w, thumb_h);

    // Add border
    render::add_border(&mut pixel_data, thumb_w, thumb_h, BORDER_WIDTH);

    // Compute position: center thumbnail on cursor
    let local_cursor_x = cursor_x - monitor_x;
    let local_cursor_y = cursor_y - monitor_y;
    let margin_left = (local_cursor_x - thumb_w as i32 / 2).clamp(0, monitor_w as i32 - thumb_w as i32);
    let margin_top = (local_cursor_y - thumb_h as i32 / 2).clamp(0, monitor_h as i32 - thumb_h as i32);

    // Position in global coordinates (KWin uses global coords for frameGeometry)
    let target_x = monitor_x + margin_left;
    let target_y = monitor_y + margin_top;
    log::info!("Target window position: global ({}, {})", target_x, target_y);

    // Load KWin script to position the overlay when it appears
    dbus::load_positioning_script(&conn, target_x, target_y, thumb_w, thumb_h).await?;

    // Create coordinate mapper
    let mapper = CoordMapper::new(DesktopBounds::from_monitors(&monitors), thumb_w, thumb_h);

    // Show overlay (KWin script will position it)
    log::info!("Showing overlay...");
    let result = overlay::show_overlay(pixel_data, thumb_w, thumb_h, mapper)?;

    // Cleanup positioning script
    dbus::unload_positioning_script(&conn).await;

    match result {
        OverlayResult::Click { x, y } => {
            // Map click to real desktop coordinates
            let desktop = DesktopBounds::from_monitors(&monitors);
            let mapper = CoordMapper::new(
                DesktopBounds::from_monitors(&monitors),
                thumb_w,
                thumb_h,
            );
            let (real_x, real_y) = mapper.overlay_to_desktop(x, y);
            log::info!(
                "Teleporting cursor to ({:.0}, {:.0})",
                real_x,
                real_y
            );
            dbus::warp_cursor(&conn, real_x, real_y, desktop.width, desktop.height).await?;
        }
        OverlayResult::Dismissed => {
            log::info!("Overlay dismissed");
        }
    }

    Ok(())
}
