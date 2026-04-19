# glint

Small Hyprland-friendly charger effect without GTK.

## Design

- One `zwlr_layer_surface_v1` overlay instead of a full GTK app.
- `wl_shm` buffers with ARGB8888 pixels, because the effect is short-lived and does not justify EGL/wgpu complexity.
- Software rendering via `tiny-skia` to avoid Cairo/GTK overhead.
- Lower default particle counts and simpler sprites instead of per-particle radial gradients.

## Target behavior

- Spawn on charger plug-in.
- Show for about `1.2s` to `1.5s`.
- Prefer one monitor by default.
- Exit immediately after the last frame.
- Use `--primary-color` to override the sparkle tint, e.g. `--primary-color '#7dd3fc'`.

## Runtime model

1. Connect to Wayland.
2. Discover the compositor globals and available outputs.
3. Create one layer-shell surface anchored fullscreen on the selected output, or one per output when `--all-monitors` is set.
4. Allocate a small rotating set of SHM buffers.
5. Simulate particles and render each frame from Wayland frame callbacks.
6. Fade out near the end of the configured lifetime and exit after the final frame is committed.

## Why this should be lighter than the Python version

- No `python`, `gi`, `gtk4`, or `cairo` startup.
- No PyGObject import overhead.
- No full widget tree or GTK scene graph.
- The only long-lived state is the Wayland connection, one surface, a couple of buffers, and a particle array.

## Next steps

- Tweak the particle palette and motion if the effect needs to be more or less subtle.
- Adjust the default duration or particle count if the runtime feel is too short or too dense.
