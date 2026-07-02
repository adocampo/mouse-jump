# Mouse Jump

A PowerToys Mouse Jump equivalent for **KDE Plasma 6 on Wayland**.

Shows a thumbnail of your entire virtual desktop (all monitors). Click anywhere on the thumbnail to instantly teleport your cursor there.

## How it works

1. Run `mouse-jump` (or bind it to a keyboard shortcut)
2. A scaled screenshot of all your monitors appears as an overlay popup
3. Move your mouse over the thumbnail — a crosshair shows where you'll land
4. **Click** to teleport the cursor to that position
5. **Escape** to dismiss without moving

## Requirements

- KDE Plasma 6 (Wayland session)
- `kscreen-doctor` (usually pre-installed with Plasma)
- `spectacle` (KDE screenshot tool, usually pre-installed)
- One of these for cursor teleport:
  - `ydotool` + `ydotoold` daemon running (recommended)
  - `kdotool`
  - KWin FakeInput D-Bus interface (if available on your system)

## Install

```bash
cargo build --release
cp target/release/mouse-jump ~/.local/bin/
```

## Usage

### One-shot (run manually or from a shortcut)

```bash
mouse-jump
```

### Bind to a keyboard shortcut

In KDE System Settings → Shortcuts → Custom Shortcuts:

1. Add a new shortcut
2. Set the trigger (e.g. `Super+J`)
3. Set the action to: `~/.local/bin/mouse-jump`

### Environment variable for logging

```bash
RUST_LOG=mouse_jump=debug mouse-jump
```

## Installing ydotool (Arch Linux)

```bash
paru -S ydotool
sudo systemctl enable --now ydotool
# Or run as user:
ydotoold &
```

## Architecture

```
┌───────────────────────────────────────────────────┐
│  mouse-jump (single-shot CLI)                     │
│                                                   │
│  1. Query monitors (kscreen-doctor)               │
│  2. Capture screenshot (spectacle / KWin D-Bus)   │
│  3. Show overlay (wlr_layer_shell)                │
│  4. Wait for click or Escape                      │
│  5. Teleport cursor (ydotool / KWin FakeInput)    │
└───────────────────────────────────────────────────┘
```

## License

MIT
