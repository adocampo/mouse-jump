/// Wayland xdg-toplevel window for displaying the screenshot thumbnail
/// and handling pointer/keyboard input.
/// Positioned via KWin scripting since Wayland doesn't allow client-side positioning.
use anyhow::{Context, Result};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm, delegate_xdg_shell, delegate_xdg_window,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        WaylandSurface,
        xdg::{
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
            XdgShell,
        },
    },
    shm::{
        slot::{Buffer, SlotPool},
        Shm, ShmHandler,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

use crate::coords::CoordMapper;

/// Result of the overlay interaction.
#[derive(Debug)]
pub enum OverlayResult {
    /// User clicked at these overlay-local coordinates.
    Click { x: f64, y: f64 },
    /// User dismissed the overlay (Escape pressed).
    Dismissed,
}

/// State for the overlay Wayland client.
struct OverlayState {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    #[allow(dead_code)]
    compositor_state: CompositorState,
    shm: Shm,
    #[allow(dead_code)]
    xdg_shell: XdgShell,

    window: Option<Window>,
    pool: Option<SlotPool>,
    buffer: Option<Buffer>,

    // Input objects from seat
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,

    // Pixel data for the thumbnail
    pixel_data: Vec<u8>,
    width: u32,
    height: u32,

    // Interaction state
    result: Option<OverlayResult>,
    running: bool,
    configured: bool,
    pointer_x: f64,
    pointer_y: f64,
    needs_redraw: bool,

    // Coordinate mapper for crosshair preview
    #[allow(dead_code)]
    coord_mapper: Option<CoordMapper>,
}

impl OverlayState {
    fn draw(&mut self, _qh: &QueueHandle<Self>) {
        let Some(ref window) = self.window else {
            return;
        };
        let Some(ref mut pool) = self.pool else {
            return;
        };

        let width = self.width;
        let height = self.height;
        let stride = width as i32 * 4;
        let buf_size = (stride * height as i32) as usize;

        let (buffer, canvas) = pool
            .create_buffer(
                width as i32,
                height as i32,
                stride,
                wl_shm::Format::Argb8888,
            )
            .expect("Failed to create shm buffer");

        // Copy pixel data to the buffer
        if self.pixel_data.len() == buf_size {
            canvas[..buf_size].copy_from_slice(&self.pixel_data);
        } else {
            // Fill with dark background if sizes don't match
            for chunk in canvas.chunks_exact_mut(4) {
                chunk[0] = 0x20; // B
                chunk[1] = 0x20; // G
                chunk[2] = 0x20; // R
                chunk[3] = 0xFF; // A
            }
        }

        // Draw crosshair at current pointer position
        if self.pointer_x >= 0.0 && self.pointer_y >= 0.0 {
            let cx = self.pointer_x as u32;
            let cy = self.pointer_y as u32;
            crate::render::draw_crosshair(canvas, width, height, cx, cy);
        }

        let surface = window.wl_surface();
        surface.attach(Some(buffer.wl_buffer()), 0, 0);
        surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.commit();

        self.buffer = Some(buffer);
        self.needs_redraw = false;
    }
}

// === Handler implementations ===

impl CompositorHandler for OverlayState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        if self.needs_redraw {
            self.draw(qh);
        }
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for OverlayState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for OverlayState {
    fn request_close(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &Window,
    ) {
        self.running = false;
        self.result = Some(OverlayResult::Dismissed);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _window: &Window,
        _configure: WindowConfigure,
        _serial: u32,
    ) {
        self.configured = true;
        self.draw(qh);
    }
}

impl SeatHandler for OverlayState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
    ) {
    }

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            log::debug!("Acquiring keyboard from seat");
            let keyboard = self
                .seat_state
                .get_keyboard(qh, &seat, None)
                .expect("Failed to get keyboard");
            self.keyboard = Some(keyboard);
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            log::debug!("Acquiring pointer from seat");
            let pointer = self
                .seat_state
                .get_pointer(qh, &seat)
                .expect("Failed to get pointer");
            self.pointer = Some(pointer);
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            self.keyboard.take();
        }
        if capability == Capability::Pointer {
            self.pointer.take();
        }
    }

    fn remove_seat(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
    ) {
    }
}

impl KeyboardHandler for OverlayState {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
        // Lost keyboard focus → dismiss
        self.result = Some(OverlayResult::Dismissed);
        self.running = false;
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        if event.keysym == Keysym::Escape {
            log::debug!("Escape pressed, dismissing overlay");
            self.result = Some(OverlayResult::Dismissed);
            self.running = false;
        }
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: Modifiers,
        _layout: u32,
    ) {
    }
}

impl PointerHandler for OverlayState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            match event.kind {
                PointerEventKind::Motion { .. } => {
                    self.pointer_x = event.position.0;
                    self.pointer_y = event.position.1;
                    self.needs_redraw = true;
                }
                PointerEventKind::Press {
                    button,
                    ..
                } => {
                    // Left mouse button (BTN_LEFT = 272 = 0x110)
                    if button == 272 {
                        let click_x = if event.position.0 != 0.0 || event.position.1 != 0.0 {
                            event.position.0
                        } else {
                            self.pointer_x
                        };
                        let click_y = if event.position.0 != 0.0 || event.position.1 != 0.0 {
                            event.position.1
                        } else {
                            self.pointer_y
                        };
                        log::info!(
                            "Click at overlay position: ({:.1}, {:.1})",
                            click_x, click_y
                        );
                        self.result = Some(OverlayResult::Click {
                            x: click_x,
                            y: click_y,
                        });
                        self.running = false;
                    }
                }
                PointerEventKind::Leave { .. } => {
                    // Pointer left the surface → dismiss
                    self.result = Some(OverlayResult::Dismissed);
                    self.running = false;
                }
                _ => {}
            }
        }
    }
}

impl ShmHandler for OverlayState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for OverlayState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

// Delegate macro invocations
delegate_compositor!(OverlayState);
delegate_output!(OverlayState);
delegate_shm!(OverlayState);
delegate_seat!(OverlayState);
delegate_keyboard!(OverlayState);
delegate_pointer!(OverlayState);
delegate_xdg_shell!(OverlayState);
delegate_xdg_window!(OverlayState);
delegate_registry!(OverlayState);

/// Show the overlay window and wait for user interaction.
/// The window is positioned via KWin scripting (must be pre-loaded before calling this).
/// Returns the overlay-local click coordinates or a dismissal.
pub fn show_overlay(
    pixel_data: Vec<u8>,
    width: u32,
    height: u32,
    coord_mapper: CoordMapper,
) -> Result<OverlayResult> {
    let conn = Connection::connect_to_env().context("Failed to connect to Wayland display")?;
    let (globals, mut event_queue) =
        registry_queue_init(&conn).context("Failed to initialize Wayland registry")?;
    let qh = event_queue.handle();

    let compositor_state =
        CompositorState::bind(&globals, &qh).context("wl_compositor not available")?;
    let xdg_shell = XdgShell::bind(&globals, &qh).context("xdg_wm_base not available")?;
    let shm = Shm::bind(&globals, &qh).context("wl_shm not available")?;

    let surface = compositor_state.create_surface(&qh);

    // Create xdg_toplevel window (no server-side decorations = borderless)
    let window = xdg_shell.create_window(surface, WindowDecorations::None, &qh);
    window.set_title("Mouse Jump");
    window.set_app_id("mouse-jump");
    window.set_min_size(Some((width, height)));

    // Initial commit to trigger configure
    window.commit();

    // Create SHM pool for the buffer
    let pool = SlotPool::new((width * height * 4) as usize, &shm)
        .context("Failed to create SHM pool")?;

    let mut state = OverlayState {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor_state,
        shm,
        xdg_shell,
        window: Some(window),
        pool: Some(pool),
        buffer: None,
        keyboard: None,
        pointer: None,
        pixel_data,
        width,
        height,
        result: None,
        running: true,
        configured: false,
        pointer_x: -1.0,
        pointer_y: -1.0,
        needs_redraw: false,
        coord_mapper: Some(coord_mapper),
    };

    // Event loop: process events until we get a result
    while state.running {
        event_queue
            .blocking_dispatch(&mut state)
            .context("Wayland dispatch error")?;
    }

    state
        .result
        .ok_or_else(|| anyhow::anyhow!("Overlay closed without result"))
}
