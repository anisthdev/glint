# glint

Small Hyprland-friendly event animation overlay without GTK.

## Design

- One `zwlr_layer_surface_v1` overlay per output instead of a full GTK app.
- `wl_shm` buffers with ARGB8888 pixels, because the effect is short-lived and does not justify EGL/wgpu complexity.
- Software rendering tuned for a full-screen effect: per-frame lookup tables, transparent block skipping, dirty-region damage and multithreaded shading.

## Target behavior

- Spawn when another tool detects an event and launches this binary.
- Play once and exit immediately after the last frame.
- Stay animation-only: no charger, battery, DBus, udev, Hyprland, or other event detection lives here.

## Usage

```sh
glint --origin bottom-right --primary-color '#7dd3fc'
```

| Flag | Default | Meaning |
|---|---|---|
| `--animation` | `sparkle-surge` | Effect to play |
| `--duration-ms` | `1750` | Animation length |
| `--primary-color` | `#fff0b4` | Tint, `#RRGGBB` or `#RRGGBBAA` |
| `--origin` | `bottom` | Start point: `top-left`, `top`, `top-right`, `left`, `center`, `right`, `bottom-left`, `bottom`, `bottom-right` |

## Animations

- `sparkle-surge`: a CPU port of Android's wired-charging ripple (upstream source in `reference/aosp-charging-ripple/`). A soft glow and sparkle ring expand from `--origin` until they reach the farthest screen corner.

Event mapping belongs in the caller. For example, sysaud can run `glint --origin bottom-right` on `power.ac-plugged`.

## Runtime model

1. Connect to Wayland.
2. Discover the compositor globals and available outputs.
3. Create one fullscreen layer-shell overlay surface per output.
4. Allocate a small rotating set of SHM buffers.
5. Render each frame from Wayland frame callbacks, damaging only the regions that changed.
6. Exit after the final, fully faded frame is committed.

## Install

Arch Linux, from a checkout of this repository:

```sh
makepkg -si
```

Or with Cargo (Rust 1.85+):

```sh
cargo install --locked --path .
```

glint needs a Wayland compositor with the wlr layer-shell protocol, such as Hyprland or Sway. See `man glint` for all options.

## License

MIT (see `LICENSE`), except `src/ripple.rs` and `reference/aosp-charging-ripple/`, which are derived from the Android Open Source Project and licensed under Apache 2.0 (see `LICENSE-APACHE` and `NOTICE`).
