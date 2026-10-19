mod ripple;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use memmap2::MmapMut;
use ripple::{ChargeRipple, Rect};
use std::cmp::min;
use std::fs::{File, OpenOptions};
use std::os::fd::AsFd;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use wayland_client::{
    delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool,
        wl_surface,
    },
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

const DEFAULT_DURATION_MS: u64 = 1750;
const DEFAULT_PRIMARY_COLOR: &str = "#fff0b4";
const APP_NAME: &str = "glint";
const DEFAULT_WIDTH: u32 = 1920;
const DEFAULT_HEIGHT: u32 = 1080;
const BUFFER_COUNT: usize = 3;

#[derive(Debug, Clone, Parser)]
#[command(name = APP_NAME)]
#[command(about = "Small event animation overlay for Hyprland via layer-shell")]
struct Args {
    #[arg(long, default_value_t = AnimationKind::SparkleSurge)]
    animation: AnimationKind,

    #[arg(long, default_value_t = DEFAULT_DURATION_MS)]
    duration_ms: u64,

    /// Draw on every monitor, or only the first one with `false`
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
    all_monitors: bool,

    #[arg(long, default_value = DEFAULT_PRIMARY_COLOR, value_parser = parse_color_arg)]
    primary_color: Color,

    /// Where the animation starts, e.g. the charging port side
    #[arg(long, default_value_t = Origin::Bottom)]
    origin: Origin,
}

#[derive(Debug, Clone, Copy)]
enum AnimationKind {
    SparkleSurge,
}

impl AnimationKind {
    const ALL: [Self; 1] = [Self::SparkleSurge];

    fn name(self) -> &'static str {
        match self {
            Self::SparkleSurge => "sparkle-surge",
        }
    }
}

impl std::fmt::Display for AnimationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for AnimationKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.name() == value)
            .ok_or_else(|| {
                let names: Vec<_> = Self::ALL.iter().map(|kind| kind.name()).collect();
                anyhow!(
                    "unknown animation '{value}'. Expected one of: {}",
                    names.join(", ")
                )
            })
    }
}

#[derive(Debug, Clone, Copy)]
enum Origin {
    TopLeft,
    Top,
    TopRight,
    Left,
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

impl Origin {
    const ALL: [Self; 9] = [
        Self::TopLeft,
        Self::Top,
        Self::TopRight,
        Self::Left,
        Self::Center,
        Self::Right,
        Self::BottomLeft,
        Self::Bottom,
        Self::BottomRight,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::TopLeft => "top-left",
            Self::Top => "top",
            Self::TopRight => "top-right",
            Self::Left => "left",
            Self::Center => "center",
            Self::Right => "right",
            Self::BottomLeft => "bottom-left",
            Self::Bottom => "bottom",
            Self::BottomRight => "bottom-right",
        }
    }

    /// Position as a fraction of the screen size, (0, 0) being top left.
    fn normalized(self) -> (f32, f32) {
        match self {
            Self::TopLeft => (0.0, 0.0),
            Self::Top => (0.5, 0.0),
            Self::TopRight => (1.0, 0.0),
            Self::Left => (0.0, 0.5),
            Self::Center => (0.5, 0.5),
            Self::Right => (1.0, 0.5),
            Self::BottomLeft => (0.0, 1.0),
            Self::Bottom => (0.5, 1.0),
            Self::BottomRight => (1.0, 1.0),
        }
    }
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Origin {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|origin| origin.name() == value)
            .ok_or_else(|| {
                let names: Vec<_> = Self::ALL.iter().map(|origin| origin.name()).collect();
                anyhow!(
                    "unknown origin '{value}'. Expected one of: {}",
                    names.join(", ")
                )
            })
    }
}

#[derive(Debug, Clone, Copy)]
struct Color {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

impl FromStr for Color {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let hex = value.strip_prefix('#').unwrap_or(value);
        let parse_pair = |chunk: &str| -> Result<u8> {
            u8::from_str_radix(chunk, 16)
                .with_context(|| format!("invalid hex color component: {chunk}"))
        };

        let color = match hex.len() {
            6 => Self {
                r: parse_pair(&hex[0..2])?,
                g: parse_pair(&hex[2..4])?,
                b: parse_pair(&hex[4..6])?,
                a: 255,
            },
            8 => Self {
                r: parse_pair(&hex[0..2])?,
                g: parse_pair(&hex[2..4])?,
                b: parse_pair(&hex[4..6])?,
                a: parse_pair(&hex[6..8])?,
            },
            _ => bail!("expected color in #RRGGBB or #RRGGBBAA format, got {value}"),
        };

        Ok(color)
    }
}

fn parse_color_arg(value: &str) -> Result<Color, String> {
    Color::from_str(value).map_err(|err| err.to_string())
}

struct ShmBuffer {
    _file: File,
    pixels: MmapMut,
    buffer: wl_buffer::WlBuffer,
    busy: bool,
    dirty: Option<Rect>,
}

struct Screen {
    _output: wl_output::WlOutput,
    surface: wl_surface::WlSurface,
    layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    frame_callback: Option<wl_callback::WlCallback>,
    buffers: Vec<ShmBuffer>,
    width: u32,
    height: u32,
    configured: bool,
    closed: bool,
    completed: bool,
    needs_redraw: bool,
    last_drawn: Option<Rect>,
}

impl Screen {
    fn new(
        output: wl_output::WlOutput,
        surface: wl_surface::WlSurface,
        layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    ) -> Self {
        Self {
            _output: output,
            surface,
            layer_surface,
            frame_callback: None,
            buffers: Vec::new(),
            width: 0,
            height: 0,
            configured: false,
            closed: false,
            completed: false,
            needs_redraw: false,
            last_drawn: None,
        }
    }

    fn ensure_buffer_size(
        &mut self,
        qh: &QueueHandle<App>,
        shm: &wl_shm::WlShm,
        width: u32,
        height: u32,
    ) -> Result<()> {
        if self.width == width && self.height == height && !self.buffers.is_empty() {
            return Ok(());
        }

        self.width = width;
        self.height = height;
        for old in self.buffers.drain(..) {
            old.buffer.destroy();
        }

        let stride = width.checked_mul(4).context("buffer stride overflow")?;
        let size = stride.checked_mul(height).context("buffer size overflow")? as usize;

        for _slot in 0..BUFFER_COUNT {
            let file = create_shm_file(size).context("failed to create SHM file")?;
            let pixels = unsafe { MmapMut::map_mut(&file).context("failed to map SHM file")? };

            let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
            let buffer = pool.create_buffer(
                0,
                width as i32,
                height as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
                qh,
                (),
            );
            // The buffer keeps the memory alive; the pool is no longer needed.
            pool.destroy();

            self.buffers.push(ShmBuffer {
                _file: file,
                pixels,
                buffer,
                busy: false,
                dirty: None,
            });
        }

        Ok(())
    }

    fn free_buffer_index(&self) -> Option<usize> {
        self.buffers.iter().position(|buffer| !buffer.busy)
    }
}

struct PendingOutput {
    output: wl_output::WlOutput,
    spawned: bool,
}

struct App {
    args: Args,
    animation: ChargeRipple,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    outputs: Vec<PendingOutput>,
    screens: Vec<Screen>,
    selected_output_taken: bool,
    started_at: Instant,
    should_exit: bool,
}

impl App {
    fn new(args: Args) -> Self {
        let animation = match args.animation {
            AnimationKind::SparkleSurge => {
                ChargeRipple::new(args.primary_color, args.origin.normalized())
            }
        };
        Self {
            args,
            animation,
            compositor: None,
            shm: None,
            layer_shell: None,
            outputs: Vec::new(),
            screens: Vec::new(),
            selected_output_taken: false,
            started_at: Instant::now(),
            should_exit: false,
        }
    }

    fn duration(&self) -> Duration {
        Duration::from_millis(self.args.duration_ms.max(1))
    }

    fn try_spawn_screens(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        let compositor = self.compositor.as_ref().context("missing wl_compositor")?;
        let layer_shell = self
            .layer_shell
            .as_ref()
            .context("missing zwlr_layer_shell_v1")?;

        for pending in &mut self.outputs {
            if pending.spawned {
                continue;
            }

            let output = pending.output.clone();
            let surface = compositor.create_surface(qh, ());
            let layer_surface = layer_shell.get_layer_surface(
                &surface,
                Some(&output),
                zwlr_layer_shell_v1::Layer::Overlay,
                APP_NAME.into(),
                qh,
                (),
            );
            layer_surface.set_anchor(
                zwlr_layer_surface_v1::Anchor::Top
                    | zwlr_layer_surface_v1::Anchor::Bottom
                    | zwlr_layer_surface_v1::Anchor::Left
                    | zwlr_layer_surface_v1::Anchor::Right,
            );
            layer_surface.set_exclusive_zone(-1);
            layer_surface
                .set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
            layer_surface.set_margin(0, 0, 0, 0);
            surface.set_buffer_scale(1);
            surface.commit();

            self.screens
                .push(Screen::new(output, surface, layer_surface));
            pending.spawned = true;

            if !self.args.all_monitors {
                break;
            }
        }

        Ok(())
    }

    fn render_screen(&mut self, screen_idx: usize, qh: &QueueHandle<Self>) -> Result<()> {
        if self.should_exit {
            return Ok(());
        }

        let elapsed = self.started_at.elapsed();
        let progress = (elapsed.as_secs_f32() / self.duration().as_secs_f32()).clamp(0.0, 1.0);

        let animation = &self.animation;
        let screen = self
            .screens
            .get_mut(screen_idx)
            .ok_or_else(|| anyhow!("invalid screen index {screen_idx}"))?;

        if screen.completed || screen.frame_callback.is_some() || !screen.configured {
            return Ok(());
        }

        let Some(buffer_idx) = screen.free_buffer_index() else {
            screen.needs_redraw = true;
            return Ok(());
        };

        let width = screen.width.max(1);
        let height = screen.height.max(1);
        let buffer = &mut screen.buffers[buffer_idx];
        let drawn = animation.render(
            buffer.pixels.as_mut(),
            width,
            height,
            progress,
            elapsed.as_secs_f32() * 1000.0,
            buffer.dirty,
        );
        buffer.dirty = drawn;
        buffer.busy = true;

        // Damage what the compositor last showed plus what this frame drew. A frame with
        // nothing to show still damages one pixel so frame callbacks keep coming.
        let damage = Rect::merge(screen.last_drawn, drawn).unwrap_or(Rect::full(1, 1));
        screen.last_drawn = drawn;

        screen.surface.attach(Some(&buffer.buffer), 0, 0);
        screen.surface.damage_buffer(
            damage.x0 as i32,
            damage.y0 as i32,
            (damage.x1 - damage.x0) as i32,
            (damage.y1 - damage.y0) as i32,
        );

        if progress < 1.0 {
            let callback = screen.surface.frame(qh, ());
            screen.frame_callback = Some(callback);
        } else {
            screen.completed = true;
        }

        screen.needs_redraw = false;
        screen.surface.commit();

        if self
            .screens
            .iter()
            .all(|screen| screen.completed || screen.closed)
        {
            self.should_exit = true;
        }

        Ok(())
    }

    fn render_pending(&mut self, qh: &QueueHandle<Self>) -> Result<()> {
        for idx in 0..self.screens.len() {
            if self.screens[idx].needs_redraw && !self.screens[idx].completed {
                self.render_screen(idx, qh)?;
            }
        }
        Ok(())
    }
}

fn create_shm_file(size: usize) -> Result<File> {
    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    let mut path = std::env::temp_dir();
    path.push(format!(
        "{}-{}-{}.shm",
        APP_NAME,
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ));

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("failed to open temporary SHM file at {}", path.display()))?;

    let _ = std::fs::remove_file(&path);
    file.set_len(size as u64)
        .with_context(|| format!("failed to size temporary SHM file at {}", path.display()))?;
    Ok(file)
}

fn min_version(available: u32, max_supported: u32) -> u32 {
    min(available, max_supported)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let conn = Connection::connect_to_env().context("failed to connect to Wayland")?;
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    conn.display().get_registry(&qh, ());

    let mut app = App::new(args);

    event_queue
        .roundtrip(&mut app)
        .context("initial Wayland roundtrip failed")?;
    app.try_spawn_screens(&qh)?;

    event_queue
        .roundtrip(&mut app)
        .context("configure roundtrip failed")?;

    if app.screens.is_empty() {
        bail!("no usable output was discovered");
    }

    for screen in &mut app.screens {
        screen.needs_redraw = true;
    }
    app.render_pending(&qh)?;
    conn.flush().context("failed to flush initial frame")?;

    while !app.should_exit {
        event_queue
            .blocking_dispatch(&mut app)
            .context("Wayland dispatch failed")?;
        app.render_pending(&qh)?;
        conn.flush().context("failed to flush frame")?;
    }

    Ok(())
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };

        match &interface[..] {
            "wl_compositor" => {
                if state.compositor.is_none() {
                    let compositor = registry.bind::<wl_compositor::WlCompositor, _, _>(
                        name,
                        min_version(version, 6),
                        qh,
                        (),
                    );
                    state.compositor = Some(compositor);
                }
            }
            "wl_shm" => {
                if state.shm.is_none() {
                    let shm =
                        registry.bind::<wl_shm::WlShm, _, _>(name, min_version(version, 1), qh, ());
                    state.shm = Some(shm);
                }
            }
            "zwlr_layer_shell_v1" => {
                if state.layer_shell.is_none() {
                    let layer_shell = registry.bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1, _, _>(
                        name,
                        min_version(version, 4),
                        qh,
                        (),
                    );
                    state.layer_shell = Some(layer_shell);
                }
            }
            "wl_output" => {
                if state.args.all_monitors || !state.selected_output_taken {
                    let output = registry.bind::<wl_output::WlOutput, _, _>(
                        name,
                        min_version(version, 4),
                        qh,
                        (),
                    );
                    state.outputs.push(PendingOutput {
                        output,
                        spawned: false,
                    });
                    if !state.args.all_monitors {
                        state.selected_output_taken = true;
                    }
                }
            }
            _ => {}
        }

        if state.compositor.is_some() && state.shm.is_some() && state.layer_shell.is_some() {
            let _ = state.try_spawn_screens(qh);
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for App {
    fn event(
        state: &mut Self,
        layer_surface: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let Some(screen_idx) = state
            .screens
            .iter()
            .position(|screen| screen.layer_surface.id() == layer_surface.id())
        else {
            return;
        };

        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer_surface.ack_configure(serial);
                let width = if width > 0 { width } else { DEFAULT_WIDTH };
                let height = if height > 0 { height } else { DEFAULT_HEIGHT };
                let screen = &mut state.screens[screen_idx];
                screen.configured = true;
                if let Some(shm) = state.shm.as_ref() {
                    let _ = screen.ensure_buffer_size(qh, shm, width, height);
                }
                screen.needs_redraw = true;
                let _ = state.render_screen(screen_idx, qh);
            }
            zwlr_layer_surface_v1::Event::Closed => {
                let screen = &mut state.screens[screen_idx];
                screen.completed = true;
                screen.closed = true;
                screen.needs_redraw = false;
                screen.frame_callback = None;
                if state
                    .screens
                    .iter()
                    .all(|screen| screen.completed || screen.closed)
                {
                    state.should_exit = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for App {
    fn event(
        state: &mut Self,
        callback: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let Some(screen_idx) = state.screens.iter().position(|screen| {
            screen
                .frame_callback
                .as_ref()
                .is_some_and(|pending| pending.id() == callback.id())
        }) else {
            return;
        };

        if let wl_callback::Event::Done { .. } = event {
            let screen = &mut state.screens[screen_idx];
            screen.frame_callback = None;
            if !screen.completed {
                screen.needs_redraw = true;
                let _ = state.render_screen(screen_idx, qh);
            }
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for App {
    fn event(
        state: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_buffer::Event::Release = event else {
            return;
        };

        let buffer_id = buffer.id();
        let Some(screen_idx) = state.screens.iter_mut().position(|screen| {
            screen
                .buffers
                .iter_mut()
                .find(|slot| slot.buffer.id() == buffer_id)
                .map(|slot| slot.busy = false)
                .is_some()
        }) else {
            return;
        };

        let screen = &state.screens[screen_idx];
        if screen.needs_redraw && !screen.completed {
            let _ = state.render_screen(screen_idx, qh);
        }
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_output::WlOutput);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
