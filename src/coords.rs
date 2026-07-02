/// Coordinate mapping between the overlay thumbnail and real desktop space.

/// Represents a single monitor in the virtual desktop layout.
#[derive(Debug, Clone)]
pub struct Monitor {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

/// The bounding box of the entire virtual desktop (union of all monitors).
#[derive(Debug, Clone)]
pub struct DesktopBounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl DesktopBounds {
    /// Compute the bounding box that contains all monitors.
    pub fn from_monitors(monitors: &[Monitor]) -> Self {
        if monitors.is_empty() {
            return Self {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            };
        }

        let min_x = monitors.iter().map(|m| m.x).min().unwrap();
        let min_y = monitors.iter().map(|m| m.y).min().unwrap();
        let max_x = monitors
            .iter()
            .map(|m| m.x + m.width as i32)
            .max()
            .unwrap();
        let max_y = monitors
            .iter()
            .map(|m| m.y + m.height as i32)
            .max()
            .unwrap();

        Self {
            x: min_x,
            y: min_y,
            width: (max_x - min_x) as u32,
            height: (max_y - min_y) as u32,
        }
    }
}

/// Maps a click position on the overlay to real desktop coordinates.
pub struct CoordMapper {
    pub desktop: DesktopBounds,
    pub overlay_width: u32,
    pub overlay_height: u32,
}

impl CoordMapper {
    pub fn new(desktop: DesktopBounds, overlay_width: u32, overlay_height: u32) -> Self {
        Self {
            desktop,
            overlay_width,
            overlay_height,
        }
    }

    /// Convert a click at (overlay_x, overlay_y) to real desktop coordinates.
    pub fn overlay_to_desktop(&self, overlay_x: f64, overlay_y: f64) -> (f64, f64) {
        let real_x =
            (overlay_x / self.overlay_width as f64) * self.desktop.width as f64 + self.desktop.x as f64;
        let real_y =
            (overlay_y / self.overlay_height as f64) * self.desktop.height as f64 + self.desktop.y as f64;
        (real_x, real_y)
    }

    /// Calculate the overlay dimensions that fit within max_width x max_height
    /// while maintaining the desktop aspect ratio.
    pub fn compute_overlay_size(desktop: &DesktopBounds, max_width: u32, max_height: u32) -> (u32, u32) {
        let aspect = desktop.width as f64 / desktop.height as f64;
        let (w, h) = if max_width as f64 / max_height as f64 > aspect {
            // Height is the limiting factor
            let h = max_height;
            let w = (h as f64 * aspect) as u32;
            (w, h)
        } else {
            // Width is the limiting factor
            let w = max_width;
            let h = (w as f64 / aspect) as u32;
            (w, h)
        };
        (w.max(1), h.max(1))
    }
}
