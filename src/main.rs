use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use memmap2::MmapMut;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use std::cmp::min;
use std::fs::{File, OpenOptions};
use std::os::fd::AsFd;
use std::str::FromStr;
use std::time::{Duration, Instant};
use tiny_skia::{FillRule, Paint, PathBuilder, PixmapMut, Transform};
use wayland_client::{
    delegate_noop,
    protocol::{
        wl_buffer, wl_callback, wl_compositor, wl_output, wl_registry, wl_shm, wl_shm_pool,
        wl_surface,
    },
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

const DEFAULT_DURATION_MS: u64 = 1200;
const DEFAULT_PARTICLE_COUNT: usize = 260;
const DEFAULT_PRIMARY_COLOR: &str = "#fff0b4";
const APP_NAME: &str = "glint";
const DEFAULT_WIDTH: u32 = 1920;
const DEFAULT_HEIGHT: u32 = 1080;
const BUFFER_COUNT: usize = 3;
const FADE_START: f32 = 0.78;

#[derive(Debug, Clone, Parser)]
#[command(name = APP_NAME)]
#[command(about = "Small charger sparkle effect for Hyprland via layer-shell")]
struct Args {
    #[arg(long, default_value_t = DEFAULT_DURATION_MS)]
    duration_ms: u64,

    #[arg(long, default_value_t = DEFAULT_PARTICLE_COUNT)]
    particle_count: usize,

    #[arg(long, default_value_t = true)]
    all_monitors: bool,

    #[arg(long, default_value = DEFAULT_PRIMARY_COLOR, value_parser = parse_color_arg)]
    primary_color: SparkleColor,
}

#[derive(Debug, Clone, Copy)]
struct SparkleColor {
    r: u8,
    g: u8,
    b: u8,
    a: u8,
}

impl SparkleColor {
    fn as_tiny_skia(self, alpha: f32) -> tiny_skia::Color {
        let effective_alpha = alpha * (self.a as f32 / 255.0);
        tiny_skia::Color::from_rgba8(self.r, self.g, self.b, (effective_alpha * 255.0) as u8)
    }
}

impl FromStr for SparkleColor {
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
            _ => {
                return Err(anyhow!(
                    "expected color in #RRGGBB or #RRGGBBAA format, got {value}"
                ))
            }
        };

        Ok(color)
    }
}

fn parse_color_arg(value: &str) -> Result<SparkleColor, String> {
    SparkleColor::from_str(value).map_err(|err| err.to_string())
}

#[derive(Debug, Clone, Copy)]
struct Particle {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    ax: f32,
    size: f32,
    alpha: f32,
    twinkle_phase: f32,
    twinkle_speed: f32,
}

impl Particle {
    fn new(rng: &mut SmallRng, height: f32) -> Self {
        let spread = height.max(1.0);
        Self {
            x: rng.gen_range(-spread * 0.12..0.05 * spread),
            y: rng.gen_range(0.0..spread),
            vx: rng.gen_range(600.0..1800.0),
            vy: rng.gen_range(-140.0..140.0),
            ax: rng.gen_range(900.0..2600.0),
            size: rng.gen_range(0.6..3.4),
            alpha: rng.gen_range(0.35..1.0),
            twinkle_phase: rng.gen_range(0.0..std::f32::consts::TAU),
            twinkle_speed: rng.gen_range(3.5..11.5),
        }
    }

    fn update(&mut self, dt: f32) {
        self.vx += self.ax * dt;
        self.x += self.vx * dt;
        self.y += self.vy * dt;
        self.twinkle_phase += self.twinkle_speed * dt;
    }
}

struct ShmBuffer {
    _file: File,
    pixels: MmapMut,
    buffer: wl_buffer::WlBuffer,
    busy: bool,
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
    particles: Vec<Particle>,
    last_tick: Instant,
    primary_color: SparkleColor,
}

impl Screen {
    fn new(
        output: wl_output::WlOutput,
        surface: wl_surface::WlSurface,
        layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        primary_color: SparkleColor,
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
            particles: Vec::new(),
            last_tick: Instant::now(),
            primary_color,
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
        self.buffers.clear();

        let stride = width.checked_mul(4).context("buffer stride overflow")?;
        let size = stride.checked_mul(height).context("buffer size overflow")? as usize;

        for _slot in 0..BUFFER_COUNT {
            let file = create_shm_file(size).context("failed to create SHM file")?;
            file.set_len(size as u64)
                .context("failed to size SHM file")?;
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

            self.buffers.push(ShmBuffer {
                _file: file,
                pixels,
                buffer,
                busy: false,
            });
        }

        if self.particles.is_empty() {
            self.seed_particles();
        }

        Ok(())
    }

    fn seed_particles(&mut self) {
        let mut rng = SmallRng::from_entropy();
        let height = self.height.max(DEFAULT_HEIGHT) as f32;
        self.particles = (0..DEFAULT_PARTICLE_COUNT)
            .map(|_| Particle::new(&mut rng, height))
            .collect();
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
        Self {
            args,
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
        let shm = self.shm.as_ref().context("missing wl_shm")?;
        let layer_shell = self
            .layer_shell
            .as_ref()
            .context("missing zwlr_layer_shell_v1")?;

        let mut spawned_any = false;
        for pending_idx in 0..self.outputs.len() {
            if self.outputs[pending_idx].spawned {
                continue;
            }

            let output = self.outputs[pending_idx].output.clone();
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

            self.screens.push(Screen::new(
                output,
                surface,
                layer_surface,
                self.args.primary_color,
            ));
            self.outputs[pending_idx].spawned = true;
            spawned_any = true;

            if !self.args.all_monitors {
                break;
            }
        }

        let _ = (compositor, shm, layer_shell, spawned_any);
        Ok(())
    }

    fn render_screen(&mut self, screen_idx: usize, qh: &QueueHandle<Self>) -> Result<()> {
        if self.should_exit {
            return Ok(());
        }

        let now = Instant::now();
        let elapsed = now.duration_since(self.started_at);
        let progress = (elapsed.as_secs_f32() / self.duration().as_secs_f32()).clamp(0.0, 1.0);
        let fade = if progress < FADE_START {
            1.0
        } else {
            let tail = ((progress - FADE_START) / (1.0 - FADE_START)).clamp(0.0, 1.0);
            1.0 - tail
        };

        let screen = self
            .screens
            .get_mut(screen_idx)
            .ok_or_else(|| anyhow!("invalid screen index {screen_idx}"))?;

        if screen.completed || screen.frame_callback.is_some() {
            return Ok(());
        }

        if !screen.configured {
            return Ok(());
        }

        let dt = now.duration_since(screen.last_tick).as_secs_f32().min(0.05);
        if dt > 0.0 {
            for particle in &mut screen.particles {
                particle.update(dt);
            }
        }
        screen.last_tick = now;

        let buffer_idx = match screen.free_buffer_index() {
            Some(idx) => idx,
            None => {
                screen.needs_redraw = true;
                return Ok(());
            }
        };

        let width = screen.width.max(1);
        let height = screen.height.max(1);
        let buffer = screen
            .buffers
            .get_mut(buffer_idx)
            .ok_or_else(|| anyhow!("invalid buffer index {buffer_idx}"))?;

        draw_frame(
            buffer.pixels.as_mut(),
            width,
            height,
            &screen.particles,
            fade,
            screen.primary_color,
        )?;

        screen.surface.attach(Some(&buffer.buffer), 0, 0);
        screen
            .surface
            .damage_buffer(0, 0, width as i32, height as i32);

        if progress < 1.0 {
            let callback = screen.surface.frame(qh, ());
            screen.frame_callback = Some(callback);
        } else {
            screen.completed = true;
            screen.needs_redraw = false;
        }

        buffer.busy = true;
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
        let len = self.screens.len();
        for idx in 0..len {
            if self.screens[idx].needs_redraw && !self.screens[idx].completed {
                self.render_screen(idx, qh)?;
            }
        }
        Ok(())
    }
}

fn create_shm_file(size: usize) -> Result<File> {
    let mut path = std::env::temp_dir();
    let stamp = format!(
        "{}-{}-{}-{}.shm",
        APP_NAME,
        std::process::id(),
        Instant::now().elapsed().as_nanos(),
        rand::random::<u64>()
    );
    path.push(stamp);

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

fn draw_frame(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    particles: &[Particle],
    fade: f32,
    color: SparkleColor,
) -> Result<()> {
    let mut pixmap = PixmapMut::from_bytes(pixels, width, height)
        .ok_or_else(|| anyhow!("failed to create pixmap view"))?;

    pixmap.fill(tiny_skia::Color::from_rgba8(0, 0, 0, 0));

    for particle in particles {
        let phase = (particle.twinkle_phase.sin() + 1.0) * 0.5;
        let alpha = (particle.alpha * fade * (0.65 + 0.35 * phase)).clamp(0.0, 1.0);
        if alpha <= 0.0 {
            continue;
        }

        let radius = particle.size * (0.9 + 0.25 * phase);
        let mut paint = Paint::default();
        paint.anti_alias = true;
        paint.set_color(color.as_tiny_skia(alpha));

        if let Some(path) = PathBuilder::from_circle(particle.x, particle.y, radius) {
            pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }

    Ok(())
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

    for idx in 0..app.screens.len() {
        app.screens[idx].needs_redraw = true;
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
                let width = if width > 0 {
                    width as u32
                } else {
                    DEFAULT_WIDTH
                };
                let height = if height > 0 {
                    height as u32
                } else {
                    DEFAULT_HEIGHT
                };
                let mut should_render = false;
                if let Some(screen) = state.screens.get_mut(screen_idx) {
                    screen.configured = true;
                    if let Some(shm) = state.shm.as_ref() {
                        let _ = screen.ensure_buffer_size(qh, shm, width, height);
                    }
                    screen.needs_redraw = true;
                    should_render = true;
                }
                if should_render {
                    let _ = state.render_screen(screen_idx, qh);
                }
            }
            zwlr_layer_surface_v1::Event::Closed => {
                let Some(screen) = state.screens.get_mut(screen_idx) else {
                    return;
                };
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

        match event {
            wl_callback::Event::Done { callback_data: _ } => {
                let mut should_render = false;
                if let Some(screen) = state.screens.get_mut(screen_idx) {
                    screen.frame_callback = None;
                    if !screen.completed {
                        screen.needs_redraw = true;
                        should_render = true;
                    }
                }
                if should_render {
                    let _ = state.render_screen(screen_idx, qh);
                }
            }
            _ => {}
        }

        let _ = callback;
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
        if let wl_buffer::Event::Release = event {
            let buffer_id = buffer.id();
            for screen_idx in 0..state.screens.len() {
                let mut matched = false;
                let mut should_render = false;
                {
                    let screen = &mut state.screens[screen_idx];
                    for slot in &mut screen.buffers {
                        if slot.buffer.id() == buffer_id {
                            slot.busy = false;
                            matched = true;
                            break;
                        }
                    }
                    if matched && screen.needs_redraw && !screen.completed {
                        should_render = true;
                    }
                }
                if should_render {
                    let _ = state.render_screen(screen_idx, qh);
                    break;
                }
                if matched {
                    break;
                }
            }
        }

        let _ = buffer;
    }
}

delegate_noop!(App: ignore wl_compositor::WlCompositor);
delegate_noop!(App: ignore wl_output::WlOutput);
delegate_noop!(App: ignore wl_shm::WlShm);
delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
delegate_noop!(App: ignore wl_surface::WlSurface);
delegate_noop!(App: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
