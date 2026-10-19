mod ripple;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use memmap2::MmapMut;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use ripple::{ChargeRipple, Rect};
use std::cmp::min;
use std::fs::{File, OpenOptions};
use std::os::fd::AsFd;
use std::str::FromStr;
use std::time::{Duration, Instant};
use tiny_skia::{FillRule, LineCap, Paint, PathBuilder, PixmapMut, Stroke, Transform};
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
const RIPPLE_DURATION_MS: u64 = 1750;
const DEFAULT_PARTICLE_COUNT: usize = 260;
const DEFAULT_PRIMARY_COLOR: &str = "#fff0b4";
const APP_NAME: &str = "glint";
const DEFAULT_WIDTH: u32 = 1920;
const DEFAULT_HEIGHT: u32 = 1080;
const BUFFER_COUNT: usize = 3;
const FADE_START: f32 = 0.78;
const DEFAULT_ANIMATION: AnimationKind = AnimationKind::SparkleSurge;
const REACTOR_PALETTE_SIZE: usize = 7;

#[derive(Debug, Clone, Parser)]
#[command(name = APP_NAME)]
#[command(about = "Small event animation overlay for Hyprland via layer-shell")]
struct Args {
    #[arg(long, default_value_t = DEFAULT_ANIMATION)]
    animation: AnimationKind,

    /// Animation length [default: 1750 for sparkle-surge, 1200 for the others]
    #[arg(long)]
    duration_ms: Option<u64>,

    #[arg(long, default_value_t = DEFAULT_PARTICLE_COUNT)]
    particle_count: usize,

    #[arg(long, default_value_t = true)]
    all_monitors: bool,

    #[arg(long, default_value = DEFAULT_PRIMARY_COLOR, value_parser = parse_color_arg)]
    primary_color: SparkleColor,

    /// Where sparkle-surge starts, e.g. the charging port side
    #[arg(long, default_value_t = Origin::Bottom)]
    origin: Origin,
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
enum AnimationKind {
    SparkleSurge,
    ReactorRing,
    ArcBloom,
    BatteryFill,
    SonicRipple,
    ScreenshotFlash,
}

impl AnimationKind {
    fn default_duration_ms(self) -> u64 {
        match self {
            Self::SparkleSurge => RIPPLE_DURATION_MS,
            _ => DEFAULT_DURATION_MS,
        }
    }

    fn names() -> &'static str {
        "sparkle-surge, reactor-ring, arc-bloom, battery-fill, sonic-ripple, screenshot-flash"
    }
}

impl std::fmt::Display for AnimationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::SparkleSurge => "sparkle-surge",
            Self::ReactorRing => "reactor-ring",
            Self::ArcBloom => "arc-bloom",
            Self::BatteryFill => "battery-fill",
            Self::SonicRipple => "sonic-ripple",
            Self::ScreenshotFlash => "screenshot-flash",
        };
        f.write_str(name)
    }
}

impl FromStr for AnimationKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "sparkle-surge" | "sparkle" | "surge" => Ok(Self::SparkleSurge),
            "reactor-ring" | "reactor" | "ring" => Ok(Self::ReactorRing),
            "arc-bloom" | "arc" | "bloom" => Ok(Self::ArcBloom),
            "battery-fill" | "battery" => Ok(Self::BatteryFill),
            "sonic-ripple" | "sonic" | "ripple" => Ok(Self::SonicRipple),
            "screenshot-flash" | "screenshot" | "flash" => Ok(Self::ScreenshotFlash),
            _ => bail!(
                "unknown animation '{value}'. Expected one of: {}",
                Self::names()
            ),
        }
    }
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

    fn with_alpha(self, alpha: u8) -> Self {
        Self { a: alpha, ..self }
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
struct HslColor {
    h: f32,
    s: f32,
    l: f32,
}

#[derive(Debug, Clone)]
struct ReactorPalette {
    colors: [SparkleColor; REACTOR_PALETTE_SIZE],
}

impl ReactorPalette {
    fn from_seed(seed: SparkleColor) -> Self {
        let hsl = rgb_to_hsl(seed);
        let seed_alpha = seed.a;
        let colors = [
            hsl_to_rgb(HslColor {
                h: hsl.h,
                s: (hsl.s * 1.08).clamp(0.3, 1.0),
                l: (hsl.l + 0.10).clamp(0.48, 0.84),
            })
            .with_alpha(seed_alpha),
            hsl_to_rgb(HslColor {
                h: hsl.h + 0.08,
                s: (hsl.s * 0.96).clamp(0.28, 0.96),
                l: (hsl.l + 0.24).clamp(0.58, 0.92),
            })
            .with_alpha(seed_alpha),
            hsl_to_rgb(HslColor {
                h: hsl.h - 0.10,
                s: (hsl.s * 1.12).clamp(0.32, 1.0),
                l: (hsl.l + 0.18).clamp(0.54, 0.9),
            })
            .with_alpha(seed_alpha),
            hsl_to_rgb(HslColor {
                h: hsl.h + 0.28,
                s: (hsl.s * 0.86).clamp(0.24, 0.92),
                l: (hsl.l + 0.16).clamp(0.52, 0.88),
            })
            .with_alpha(seed_alpha),
            hsl_to_rgb(HslColor {
                h: hsl.h + 0.52,
                s: (hsl.s * 0.64).clamp(0.18, 0.78),
                l: (hsl.l + 0.30).clamp(0.66, 0.96),
            })
            .with_alpha(seed_alpha),
            hsl_to_rgb(HslColor {
                h: hsl.h - 0.22,
                s: (hsl.s * 0.72).clamp(0.2, 0.86),
                l: (hsl.l + 0.26).clamp(0.62, 0.94),
            })
            .with_alpha(seed_alpha),
            SparkleColor {
                r: 255,
                g: 255,
                b: 255,
                a: seed_alpha,
            },
        ];
        Self { colors }
    }

    fn color_for(&self, idx: usize) -> SparkleColor {
        self.colors[idx % self.colors.len()]
    }
}

#[derive(Debug, Clone, Copy)]
struct OrbitParticle {
    angle: f32,
    radius: f32,
    speed: f32,
    size: f32,
    alpha: f32,
    palette_idx: usize,
    phase: f32,
}

#[derive(Debug, Clone)]
struct ArcPath {
    points: Vec<(f32, f32)>,
    delay: f32,
    alpha: f32,
}

#[derive(Debug, Clone, Copy)]
struct BurstParticle {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    size: f32,
    alpha: f32,
}

#[derive(Debug, Clone, Copy)]
struct Shard {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    angle: f32,
    size: f32,
    alpha: f32,
}

#[derive(Debug, Clone)]
enum AnimationState {
    SparkleSurge {
        ripple: ChargeRipple,
    },
    ReactorRing {
        orbiters: Vec<OrbitParticle>,
        palette: ReactorPalette,
    },
    ArcBloom {
        arcs: Vec<ArcPath>,
        sparks: Vec<BurstParticle>,
    },
    BatteryFill {
        particles: Vec<BurstParticle>,
    },
    SonicRipple {
        dots: Vec<OrbitParticle>,
    },
    ScreenshotFlash {
        shards: Vec<Shard>,
    },
}

impl AnimationState {
    fn new(
        kind: AnimationKind,
        width: u32,
        height: u32,
        particle_count: usize,
        primary_color: SparkleColor,
        origin: Origin,
    ) -> Self {
        let mut rng = SmallRng::from_entropy();
        let width = width.max(DEFAULT_WIDTH) as f32;
        let height = height.max(DEFAULT_HEIGHT) as f32;
        let count = particle_count.max(1);

        match kind {
            AnimationKind::SparkleSurge => Self::SparkleSurge {
                ripple: ChargeRipple::new(primary_color, origin.normalized()),
            },
            AnimationKind::ReactorRing => {
                let orbiters = (0..(count / 2).max(72))
                    .map(|_| OrbitParticle {
                        angle: rng.gen_range(0.0..std::f32::consts::TAU),
                        radius: rng.gen_range(20.0..height.min(width) * 0.25),
                        speed: rng.gen_range(-5.5..5.5),
                        size: rng.gen_range(1.0..4.2),
                        alpha: rng.gen_range(0.35..1.0),
                        palette_idx: rng.gen_range(0..REACTOR_PALETTE_SIZE),
                        phase: rng.gen_range(0.0..std::f32::consts::TAU),
                    })
                    .collect();
                Self::ReactorRing {
                    orbiters,
                    palette: ReactorPalette::from_seed(primary_color),
                }
            }
            AnimationKind::ArcBloom => {
                let center = (width * 0.5, height * 0.5);
                let mut arcs = Vec::with_capacity(18);
                for idx in 0..18 {
                    let side = idx % 4;
                    let start = match side {
                        0 => (rng.gen_range(0.0..width), -20.0),
                        1 => (width + 20.0, rng.gen_range(0.0..height)),
                        2 => (rng.gen_range(0.0..width), height + 20.0),
                        _ => (-20.0, rng.gen_range(0.0..height)),
                    };
                    let mut points = Vec::with_capacity(9);
                    for step in 0..9 {
                        let t = step as f32 / 8.0;
                        let wobble = (1.0 - (t - 0.5).abs() * 1.4).max(0.0);
                        let x = lerp(start.0, center.0, t) + rng.gen_range(-70.0..70.0) * wobble;
                        let y = lerp(start.1, center.1, t) + rng.gen_range(-70.0..70.0) * wobble;
                        points.push((x, y));
                    }
                    arcs.push(ArcPath {
                        points,
                        delay: rng.gen_range(0.0..0.35),
                        alpha: rng.gen_range(0.45..1.0),
                    });
                }
                let sparks = (0..count)
                    .map(|_| BurstParticle {
                        x: center.0 + rng.gen_range(-42.0..42.0),
                        y: center.1 + rng.gen_range(-42.0..42.0),
                        vx: rng.gen_range(-620.0..620.0),
                        vy: rng.gen_range(-620.0..620.0),
                        size: rng.gen_range(0.8..3.5),
                        alpha: rng.gen_range(0.25..1.0),
                    })
                    .collect();
                Self::ArcBloom { arcs, sparks }
            }
            AnimationKind::BatteryFill => {
                let particles = (0..count)
                    .map(|_| BurstParticle {
                        x: width * 0.5 + rng.gen_range(-160.0..160.0),
                        y: height * 0.5 + rng.gen_range(-50.0..50.0),
                        vx: rng.gen_range(-360.0..360.0),
                        vy: rng.gen_range(-760.0..-120.0),
                        size: rng.gen_range(1.0..4.0),
                        alpha: rng.gen_range(0.25..1.0),
                    })
                    .collect();
                Self::BatteryFill { particles }
            }
            AnimationKind::SonicRipple => {
                let dots = (0..(count / 2).max(96))
                    .map(|_| OrbitParticle {
                        angle: rng.gen_range(0.0..std::f32::consts::TAU),
                        radius: rng.gen_range(40.0..height.min(width) * 0.4),
                        speed: rng.gen_range(-1.2..1.2),
                        size: rng.gen_range(1.0..3.0),
                        alpha: rng.gen_range(0.25..0.8),
                        palette_idx: 0,
                        phase: rng.gen_range(0.0..std::f32::consts::TAU),
                    })
                    .collect();
                Self::SonicRipple { dots }
            }
            AnimationKind::ScreenshotFlash => {
                let shards = (0..(count / 2).max(90))
                    .map(|_| {
                        let edge = rng.gen_range(0..4);
                        let (x, y) = match edge {
                            0 => (rng.gen_range(0.0..width), 0.0),
                            1 => (width, rng.gen_range(0.0..height)),
                            2 => (rng.gen_range(0.0..width), height),
                            _ => (0.0, rng.gen_range(0.0..height)),
                        };
                        let dx = x - width * 0.5;
                        let dy = y - height * 0.5;
                        let len = (dx * dx + dy * dy).sqrt().max(1.0);
                        Shard {
                            x,
                            y,
                            vx: dx / len * rng.gen_range(160.0..620.0),
                            vy: dy / len * rng.gen_range(160.0..620.0),
                            angle: rng.gen_range(0.0..std::f32::consts::TAU),
                            size: rng.gen_range(8.0..28.0),
                            alpha: rng.gen_range(0.2..0.8),
                        }
                    })
                    .collect();
                Self::ScreenshotFlash { shards }
            }
        }
    }

    fn update(&mut self, dt: f32) {
        match self {
            Self::SparkleSurge { .. } => {}
            Self::ReactorRing { orbiters, .. } | Self::SonicRipple { dots: orbiters } => {
                for orbiter in orbiters {
                    orbiter.angle += orbiter.speed * dt;
                    orbiter.radius += 55.0 * dt;
                    orbiter.phase += 4.2 * dt;
                }
            }
            Self::ArcBloom { sparks, .. } | Self::BatteryFill { particles: sparks } => {
                for spark in sparks {
                    spark.x += spark.vx * dt;
                    spark.y += spark.vy * dt;
                    spark.vy += 420.0 * dt;
                }
            }
            Self::ScreenshotFlash { shards } => {
                for shard in shards {
                    shard.x += shard.vx * dt;
                    shard.y += shard.vy * dt;
                    shard.angle += 2.8 * dt;
                }
            }
        }
    }
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
    animation: Option<AnimationState>,
    last_tick: Instant,
    animation_kind: AnimationKind,
    particle_count: usize,
    primary_color: SparkleColor,
    origin: Origin,
}

impl Screen {
    fn new(
        output: wl_output::WlOutput,
        surface: wl_surface::WlSurface,
        layer_surface: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        animation_kind: AnimationKind,
        particle_count: usize,
        primary_color: SparkleColor,
        origin: Origin,
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
            animation: None,
            last_tick: Instant::now(),
            animation_kind,
            particle_count,
            primary_color,
            origin,
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
                dirty: None,
            });
        }

        if self.animation.is_none() {
            self.seed_animation();
        }

        Ok(())
    }

    fn seed_animation(&mut self) {
        self.animation = Some(AnimationState::new(
            self.animation_kind,
            self.width,
            self.height,
            self.particle_count,
            self.primary_color,
            self.origin,
        ));
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
        let millis = self
            .args
            .duration_ms
            .unwrap_or_else(|| self.args.animation.default_duration_ms());
        Duration::from_millis(millis.max(1))
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
                self.args.animation,
                self.args.particle_count,
                self.args.primary_color,
                self.args.origin,
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
            if let Some(animation) = screen.animation.as_mut() {
                animation.update(dt);
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
        let primary_color = screen.primary_color;
        let elapsed_ms = elapsed.as_secs_f32() * 1000.0;
        let (wl_buffer, damage) = {
            let Screen {
                animation,
                buffers,
                last_drawn,
                ..
            } = &mut *screen;
            let animation = animation
                .as_ref()
                .ok_or_else(|| anyhow!("screen animation was not initialized"))?;
            let buffer = buffers
                .get_mut(buffer_idx)
                .ok_or_else(|| anyhow!("invalid buffer index {buffer_idx}"))?;

            let drawn = draw_frame(
                buffer.pixels.as_mut(),
                width,
                height,
                animation,
                progress,
                elapsed_ms,
                fade,
                primary_color,
                buffer.dirty,
            )?;
            // Damage what the compositor last showed plus what this frame drew. A frame
            // with nothing to show still damages one pixel so frame callbacks keep coming.
            let damage = Rect::merge(*last_drawn, drawn).unwrap_or(Rect::full(1, 1));
            *last_drawn = drawn;
            buffer.dirty = drawn;
            buffer.busy = true;
            (buffer.buffer.clone(), damage)
        };

        screen.surface.attach(Some(&wl_buffer), 0, 0);
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
            screen.needs_redraw = false;
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
    animation: &AnimationState,
    progress: f32,
    elapsed_ms: f32,
    fade: f32,
    color: SparkleColor,
    dirty: Option<Rect>,
) -> Result<Option<Rect>> {
    if let AnimationState::SparkleSurge { ripple } = animation {
        return Ok(ripple.render(pixels, width, height, progress, elapsed_ms, dirty));
    }

    let mut pixmap = PixmapMut::from_bytes(pixels, width, height)
        .ok_or_else(|| anyhow!("failed to create pixmap view"))?;

    pixmap.fill(tiny_skia::Color::from_rgba8(0, 0, 0, 0));

    match animation {
        AnimationState::SparkleSurge { .. } => unreachable!("rendered by ChargeRipple"),
        AnimationState::ReactorRing { orbiters, palette } => {
            draw_reactor_ring(
                &mut pixmap,
                width,
                height,
                orbiters,
                progress,
                fade,
                palette,
            );
        }
        AnimationState::ArcBloom { arcs, sparks } => {
            draw_arc_bloom(
                &mut pixmap,
                width,
                height,
                arcs,
                sparks,
                progress,
                fade,
                color,
            );
        }
        AnimationState::BatteryFill { particles } => {
            draw_battery_fill(&mut pixmap, width, height, particles, progress, fade, color);
        }
        AnimationState::SonicRipple { dots } => {
            draw_sonic_ripple(&mut pixmap, width, height, dots, progress, fade, color);
        }
        AnimationState::ScreenshotFlash { shards } => {
            draw_screenshot_flash(&mut pixmap, width, height, shards, progress, fade, color);
        }
    }

    Ok(Some(Rect::full(width, height)))
}

fn draw_radial_segment(
    pixmap: &mut PixmapMut<'_>,
    cx: f32,
    cy: f32,
    inner: f32,
    outer: f32,
    angle: f32,
    color: SparkleColor,
    alpha: f32,
    width: f32,
) {
    let mut builder = PathBuilder::new();
    builder.move_to(cx + angle.cos() * inner, cy + angle.sin() * inner * 0.62);
    builder.line_to(cx + angle.cos() * outer, cy + angle.sin() * outer * 0.62);
    if let Some(path) = builder.finish() {
        stroke_path(pixmap, &path, color, alpha, width, LineCap::Round);
    }
}

fn draw_reactor_ring(
    pixmap: &mut PixmapMut<'_>,
    width: u32,
    height: u32,
    orbiters: &[OrbitParticle],
    progress: f32,
    fade: f32,
    palette: &ReactorPalette,
) {
    let cx = width as f32 * 0.5;
    let cy = height as f32 * 0.5;
    let max_radius = width.min(height) as f32 * 0.42;
    let pulse = ease_out(progress);
    let white_hot = palette.color_for(REACTOR_PALETTE_SIZE - 1);

    draw_circle(
        pixmap,
        cx,
        cy,
        72.0 + 80.0 * pulse,
        palette.color_for(4),
        fade * (1.0 - progress * 0.45).max(0.0) * 0.08,
    );

    for idx in 0..4 {
        let ring_t = ((pulse + idx as f32 * 0.18) % 1.0).clamp(0.0, 1.0);
        let radius = lerp(32.0, max_radius, ring_t);
        let alpha = fade * (1.0 - ring_t).powf(1.4) * 0.9;
        let color = palette.color_for(idx);
        stroke_circle(
            pixmap,
            cx,
            cy,
            radius + 5.0,
            7.0 + 14.0 * (1.0 - ring_t),
            palette.color_for(idx + 3),
            alpha * 0.11,
        );
        stroke_circle(
            pixmap,
            cx,
            cy,
            radius,
            2.0 + 8.0 * (1.0 - ring_t),
            color,
            alpha,
        );
        stroke_circle(
            pixmap,
            cx,
            cy,
            radius * 0.985,
            1.0 + 2.0 * (1.0 - ring_t),
            white_hot,
            alpha * 0.38,
        );
    }

    for idx in 0..10 {
        let spoke_phase = progress * std::f32::consts::TAU * 1.25 + idx as f32 * 0.628;
        let inner = 34.0 + (idx % 3) as f32 * 7.0;
        let outer = lerp(92.0, max_radius * 0.72, pulse) + (idx % 2) as f32 * 18.0;
        let alpha = fade * (0.22 + 0.16 * (spoke_phase * 2.0).sin().abs());
        draw_radial_segment(
            pixmap,
            cx,
            cy,
            inner,
            outer,
            spoke_phase,
            palette.color_for(idx + 1),
            alpha,
            1.0 + (idx % 3) as f32 * 0.45,
        );
    }

    draw_circle(
        pixmap,
        cx,
        cy,
        42.0 + 22.0 * (1.0 - pulse),
        palette.color_for(1),
        fade * 0.22,
    );
    stroke_circle(
        pixmap,
        cx,
        cy,
        34.0 + 18.0 * (1.0 - pulse),
        3.0,
        white_hot,
        fade * 0.8,
    );

    for orbiter in orbiters {
        let swell = 1.0 + progress * 1.7;
        let wobble = 1.0 + 0.08 * orbiter.phase.sin();
        let x = cx + orbiter.angle.cos() * orbiter.radius * swell * wobble;
        let y = cy + orbiter.angle.sin() * orbiter.radius * swell * 0.62 / wobble;
        let alpha = fade * orbiter.alpha * (1.0 - progress * 0.45).max(0.0);
        let color = palette.color_for(orbiter.palette_idx);
        let halo = palette.color_for(orbiter.palette_idx + 2);
        draw_circle(pixmap, x, y, orbiter.size * 3.5, halo, alpha * 0.08);
        draw_circle(pixmap, x, y, orbiter.size * 1.55, color, alpha * 0.38);
        draw_circle(pixmap, x, y, orbiter.size, white_hot, alpha * 0.7);
    }
}

fn draw_arc_bloom(
    pixmap: &mut PixmapMut<'_>,
    width: u32,
    height: u32,
    arcs: &[ArcPath],
    sparks: &[BurstParticle],
    progress: f32,
    fade: f32,
    color: SparkleColor,
) {
    let cx = width as f32 * 0.5;
    let cy = height as f32 * 0.5;
    for arc in arcs {
        let local = ((progress - arc.delay) / 0.55).clamp(0.0, 1.0);
        if local <= 0.0 {
            continue;
        }
        let visible = (arc.points.len() as f32 * local).ceil() as usize;
        if visible < 2 {
            continue;
        }
        let mut builder = PathBuilder::new();
        builder.move_to(arc.points[0].0, arc.points[0].1);
        for point in arc.points.iter().take(visible) {
            builder.line_to(point.0, point.1);
        }
        if let Some(path) = builder.finish() {
            let alpha = fade * arc.alpha * (1.0 - (progress - 0.62).max(0.0) * 1.8).clamp(0.0, 1.0);
            stroke_path(
                pixmap,
                &path,
                color,
                alpha,
                2.0 + 4.0 * (1.0 - local),
                LineCap::Round,
            );
        }
    }

    draw_circle(
        pixmap,
        cx,
        cy,
        28.0 + 90.0 * progress,
        color,
        fade * (1.0 - progress) * 0.7,
    );
    for spark in sparks {
        let alpha = fade * spark.alpha * (progress * 1.6).clamp(0.0, 1.0);
        draw_circle(pixmap, spark.x, spark.y, spark.size, color, alpha);
    }
}

fn draw_battery_fill(
    pixmap: &mut PixmapMut<'_>,
    width: u32,
    height: u32,
    particles: &[BurstParticle],
    progress: f32,
    fade: f32,
    color: SparkleColor,
) {
    let w = (width as f32 * 0.24).clamp(220.0, 420.0);
    let h = (height as f32 * 0.11).clamp(82.0, 150.0);
    let x = width as f32 * 0.5 - w * 0.5;
    let y = height as f32 * 0.5 - h * 0.5;
    let cap_w = (w * 0.08).max(18.0);
    let fill_t = (progress / 0.72).clamp(0.0, 1.0);

    stroke_rect(pixmap, x, y, w, h, color, fade * 0.88, 4.0);
    stroke_rect(
        pixmap,
        x + w + 7.0,
        y + h * 0.28,
        cap_w,
        h * 0.44,
        color,
        fade * 0.7,
        4.0,
    );

    let fill_w = (w - 18.0) * ease_out(fill_t);
    fill_rect(
        pixmap,
        x + 9.0,
        y + 9.0,
        fill_w.max(0.0),
        h - 18.0,
        color,
        fade * 0.55,
    );

    if progress > 0.58 {
        let burst = ((progress - 0.58) / 0.42).clamp(0.0, 1.0);
        for particle in particles {
            let alpha = fade * particle.alpha * (1.0 - burst * 0.25);
            draw_circle(pixmap, particle.x, particle.y, particle.size, color, alpha);
        }
    }
}

fn draw_sonic_ripple(
    pixmap: &mut PixmapMut<'_>,
    width: u32,
    height: u32,
    dots: &[OrbitParticle],
    progress: f32,
    fade: f32,
    color: SparkleColor,
) {
    let cx = width as f32 * 0.5;
    let cy = height as f32 * 0.5;
    let max_radius = width.min(height) as f32 * 0.45;
    for idx in 0..6 {
        let t = ((progress * 1.35 + idx as f32 * 0.16) % 1.0).clamp(0.0, 1.0);
        let radius = lerp(28.0, max_radius, ease_out(t));
        let alpha = fade * (1.0 - t).powf(1.2) * 0.75;
        stroke_circle(
            pixmap,
            cx,
            cy,
            radius,
            2.0 + idx as f32 * 0.45,
            color,
            alpha,
        );
    }

    for dot in dots {
        let wave = (dot.angle * 5.0 + progress * 18.0).sin();
        let x = cx + dot.angle.cos() * dot.radius;
        let y = cy + dot.angle.sin() * dot.radius * 0.55 + wave * 16.0;
        let alpha = fade * dot.alpha * (1.0 - progress * 0.35);
        draw_circle(pixmap, x, y, dot.size, color, alpha);
    }
}

fn draw_screenshot_flash(
    pixmap: &mut PixmapMut<'_>,
    width: u32,
    height: u32,
    shards: &[Shard],
    progress: f32,
    fade: f32,
    color: SparkleColor,
) {
    let flash = (1.0 - (progress / 0.24).clamp(0.0, 1.0)).powf(1.8);
    if flash > 0.01 {
        fill_rect(
            pixmap,
            0.0,
            0.0,
            width as f32,
            height as f32,
            color,
            flash * 0.18,
        );
    }

    let inset = 12.0 + progress * 42.0;
    stroke_rect(
        pixmap,
        inset,
        inset,
        width as f32 - inset * 2.0,
        height as f32 - inset * 2.0,
        color,
        fade * (1.0 - progress * 0.75).max(0.0),
        5.0,
    );

    for shard in shards {
        let alpha = fade * shard.alpha * (1.0 - progress * 0.5).max(0.0);
        draw_shard(pixmap, shard, color, alpha);
    }
}

fn draw_circle(
    pixmap: &mut PixmapMut<'_>,
    x: f32,
    y: f32,
    radius: f32,
    color: SparkleColor,
    alpha: f32,
) {
    if alpha <= 0.0 || radius <= 0.0 {
        return;
    }
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color(color.as_tiny_skia(alpha.clamp(0.0, 1.0)));
    if let Some(path) = PathBuilder::from_circle(x, y, radius) {
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

fn stroke_circle(
    pixmap: &mut PixmapMut<'_>,
    x: f32,
    y: f32,
    radius: f32,
    width: f32,
    color: SparkleColor,
    alpha: f32,
) {
    if alpha <= 0.0 || radius <= 0.0 {
        return;
    }
    if let Some(path) = PathBuilder::from_circle(x, y, radius) {
        stroke_path(pixmap, &path, color, alpha, width, LineCap::Butt);
    }
}

fn fill_rect(
    pixmap: &mut PixmapMut<'_>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    color: SparkleColor,
    alpha: f32,
) {
    if alpha <= 0.0 || width <= 0.0 || height <= 0.0 {
        return;
    }
    let Some(path) = rect_path(x, y, width, height) else {
        return;
    };
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color(color.as_tiny_skia(alpha.clamp(0.0, 1.0)));
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

fn stroke_rect(
    pixmap: &mut PixmapMut<'_>,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    color: SparkleColor,
    alpha: f32,
    stroke_width: f32,
) {
    if alpha <= 0.0 || width <= 0.0 || height <= 0.0 {
        return;
    }
    if let Some(path) = rect_path(x, y, width, height) {
        stroke_path(pixmap, &path, color, alpha, stroke_width, LineCap::Butt);
    }
}

fn stroke_path(
    pixmap: &mut PixmapMut<'_>,
    path: &tiny_skia::Path,
    color: SparkleColor,
    alpha: f32,
    width: f32,
    line_cap: LineCap,
) {
    if alpha <= 0.0 || width <= 0.0 {
        return;
    }
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color(color.as_tiny_skia(alpha.clamp(0.0, 1.0)));
    let stroke = Stroke {
        width,
        line_cap,
        ..Stroke::default()
    };
    pixmap.stroke_path(path, &paint, &stroke, Transform::identity(), None);
}

fn draw_shard(pixmap: &mut PixmapMut<'_>, shard: &Shard, color: SparkleColor, alpha: f32) {
    if alpha <= 0.0 {
        return;
    }
    let dx = shard.angle.cos() * shard.size;
    let dy = shard.angle.sin() * shard.size;
    let px = -dy * 0.35;
    let py = dx * 0.35;
    let mut builder = PathBuilder::new();
    builder.move_to(shard.x + dx, shard.y + dy);
    builder.line_to(shard.x - dx + px, shard.y - dy + py);
    builder.line_to(shard.x - dx - px, shard.y - dy - py);
    builder.close();
    if let Some(path) = builder.finish() {
        let mut paint = Paint::default();
        paint.anti_alias = true;
        paint.set_color(color.as_tiny_skia(alpha.clamp(0.0, 1.0)));
        pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

fn rect_path(x: f32, y: f32, width: f32, height: f32) -> Option<tiny_skia::Path> {
    let mut builder = PathBuilder::new();
    builder.move_to(x, y);
    builder.line_to(x + width, y);
    builder.line_to(x + width, y + height);
    builder.line_to(x, y + height);
    builder.close();
    builder.finish()
}

fn rgb_to_hsl(color: SparkleColor) -> HslColor {
    let r = color.r as f32 / 255.0;
    let g = color.g as f32 / 255.0;
    let b = color.b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) * 0.5;

    if (max - min).abs() <= f32::EPSILON {
        return HslColor { h: 0.12, s: 0.0, l };
    }

    let delta = max - min;
    let s = if l > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };

    let h = if (max - r).abs() <= f32::EPSILON {
        ((g - b) / delta + if g < b { 6.0 } else { 0.0 }) / 6.0
    } else if (max - g).abs() <= f32::EPSILON {
        ((b - r) / delta + 2.0) / 6.0
    } else {
        ((r - g) / delta + 4.0) / 6.0
    };

    HslColor { h, s, l }
}

fn hsl_to_rgb(color: HslColor) -> SparkleColor {
    let h = color.h.rem_euclid(1.0);
    let s = color.s.clamp(0.0, 1.0);
    let l = color.l.clamp(0.0, 1.0);

    if s <= f32::EPSILON {
        let value = channel_from_unit(l);
        return SparkleColor {
            r: value,
            g: value,
            b: value,
            a: 255,
        };
    }

    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    SparkleColor {
        r: channel_from_unit(hue_to_rgb(p, q, h + 1.0 / 3.0)),
        g: channel_from_unit(hue_to_rgb(p, q, h)),
        b: channel_from_unit(hue_to_rgb(p, q, h - 1.0 / 3.0)),
        a: 255,
    }
}

fn hue_to_rgb(p: f32, q: f32, t: f32) -> f32 {
    let t = t.rem_euclid(1.0);
    if t < 1.0 / 6.0 {
        p + (q - p) * 6.0 * t
    } else if t < 1.0 / 2.0 {
        q
    } else if t < 2.0 / 3.0 {
        p + (q - p) * (2.0 / 3.0 - t) * 6.0
    } else {
        p
    }
}

fn channel_from_unit(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn lerp(start: f32, end: f32, t: f32) -> f32 {
    start + (end - start) * t
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
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
