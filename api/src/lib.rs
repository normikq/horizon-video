//! A video screen in the world, as a trait mod (docs/native-libraries.md).
//!
//! Features:
//! - `/video create <id>`: creates a new screen with the specified ID in front of you and opens the interactive editor;
//! - `/video edit <id>`: opens the interactive editor for an existing saved screen;
//! - `/video save [id]`: saves current screen position and dimensions to video_screens.json;
//! - `/video list`: lists all saved screens with clickable buttons;
//! - `/video play <id> <file>`: plays a video on the specified saved screen;
//! - `/video remove <id>`: deletes a saved screen configuration;
//! - `/video cancel`: closes the interactive editor;
//! - Surface snapping:
//!   - `/video snap wall` (or `/video attach`): snaps screen flush onto the targeted block surface/wall;
//!   - `/video grid [step|off]`: toggles block grid snapping (e.g. 0.5 or 1.0 block increments);
//! - Curved screen support:
//!   - `/video curve <degrees>`: sets cylindrical curvature (0° flat, up to 120° for immersive curved/IMAX display);
//! - Interactive editor: red wireframe grid in the 3D world with 4 corner handles and 4 border handles;
//!   drag corners via open chat to resize while preserving aspect ratio; drag borders to adjust width/height freely;
//! - Clickable chat buttons: [💾 Сохранить], [🧲 К стене], [📐 0°], [📐 30°], [📐 60°], [16:9], [4:3], [21:9], [1:1], [Auto], [❌ Закрыть];
//! - Sizing and aspect commands: `/video size <diag>`, `/video grow [step]`, `/video shrink [step]`,
//!   `/video aspect <ratio>`, `/video distance <dist>`, `/video reset`;
//! - Playback commands: `/video open <path>`, `/video pause`, `/video play`, `/video seek <sec>`,
//!   `/video volume <0-100>`, `/video close`, `/video info`.

use horizon_client::mod_api::{
    ClickEvent, ClientLevelUnloadEvent, CommandEvent, CommandsEvent, Flow, FrameEvent, Hooks,
    Host, NativeCamera,
    NativeColoredLight, NativeHit, PrimitiveShape, ScreenMouseButtonPressedPreEvent,
    ScreenMouseButtonReleasedPreEvent, ScreenMouseDraggedPreV2Event, ScreenRenderPostEvent, Style,
    Text, VisualsEvent, WorldPrimitivesEvent, WorldSubmitEvent, serde_json,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

const SCREENS_FILE: &str = "video_screens.json";
/// The mod's id, as the mixer keys its audio sources by.
const MOD_ID: &str = "horizon_video";
/// The sample rate when the game does not say (`video_audio_rate`).
const FALLBACK_RATE: u32 = 48_000;
/// The most a video may have in its 7.1 layout: one audio source each.
const MAX_CHANNELS: u128 = 8;
const CURVE_SLICES: usize = 24;

/// The default screen diagonal in blocks (~4.589 blocks corresponds to 4.0 x 2.25 at 16:9).
const DEFAULT_DIAGONAL: f32 = 4.58939;
/// Default aspect ratio (16:9).
const DEFAULT_ASPECT: f32 = 16.0 / 9.0;
const DEFAULT_HEIGHT: f32 = 2.25;
/// How far in front of the player it is put by default.
const DEFAULT_DISTANCE: f64 = 4.0;

/// Aspect ratio for the screen geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AspectRatio {
    Auto,
    Ratio(f32),
}

impl AspectRatio {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("auto") {
            return Some(Self::Auto);
        }
        if let Some((w, h)) = s.split_once(':') {
            let w: f32 = w.trim().parse().ok()?;
            let h: f32 = h.trim().parse().ok()?;
            if w > 0.0 && h > 0.0 {
                return Some(Self::Ratio(w / h));
            }
        } else if let Ok(val) = s.parse::<f32>()
            && val > 0.0 {
                return Some(Self::Ratio(val));
            }
        None
    }

    pub fn value(&self, native_size: Option<(u32, u32)>) -> f32 {
        match self {
            Self::Auto => {
                if let Some((w, h)) = native_size
                    && h > 0 {
                        return w as f32 / h as f32;
                    }
                DEFAULT_ASPECT
            }
            Self::Ratio(r) => *r,
        }
    }

    pub fn as_string(&self) -> String {
        match self {
            Self::Auto => "auto".to_string(),
            Self::Ratio(r) => {
                if (*r - 16.0 / 9.0).abs() < 0.02 {
                    "16:9".to_string()
                } else if (*r - 4.0 / 3.0).abs() < 0.02 {
                    "4:3".to_string()
                } else if (*r - 21.0 / 9.0).abs() < 0.02 {
                    "21:9".to_string()
                } else if (*r - 1.0).abs() < 0.02 {
                    "1:1".to_string()
                } else {
                    format!("{:.2}", r)
                }
            }
        }
    }
}

/// A screen configuration saved on disk.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SavedScreen {
    pub id: String,
    pub at: [f64; 3],
    pub camera_pos: [f64; 3],
    pub yaw: f32,
    pub diagonal: f32,
    pub aspect: String,
    pub distance: f64,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub curvature: f32,
    #[serde(default)]
    pub speakers: HashMap<SpeakerId, [f64; 3]>,
    /// The audio mode it plays with; none in files from before it was kept.
    #[serde(default)]
    pub audio_mode: Option<AudioMode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragHandle {
    CornerTL,
    CornerTR,
    CornerBL,
    CornerBR,
    EdgeLeft,
    EdgeRight,
    EdgeTop,
    EdgeBottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SpeakerId {
    FL,
    FR,
    C,
    LFE,
    SL,
    SR,
    BL,
    BR,
}

impl SpeakerId {
    pub fn name(self) -> &'static str {
        match self {
            Self::FL => "Левый фронт (FL)",
            Self::FR => "Правый фронт (FR)",
            Self::C => "Центральный (C)",
            Self::LFE => "Сабвуфер (LFE)",
            Self::SL => "Боковой/тыловой левый (SL)",
            Self::SR => "Боковой/тыловой правый (SR)",
            Self::BL => "Задний левый (BL)",
            Self::BR => "Задний правый (BR)",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DragTarget {
    Handle(DragHandle),
    ScreenCenter { initial_dist: f64 },
    Speaker(SpeakerId, f64),
}

impl DragTarget {
    pub fn is_handle(&self, h: DragHandle) -> bool {
        matches!(self, DragTarget::Handle(x) if *x == h)
    }
    pub fn is_speaker(&self, s: SpeakerId) -> bool {
        matches!(self, DragTarget::Speaker(x, _) if *x == s)
    }
    pub fn is_center(&self) -> bool {
        matches!(self, DragTarget::ScreenCenter { .. })
    }
}

pub struct EditState {
    pub id: String,
    pub active_handle: Option<DragHandle>,
}

pub struct Screen {
    video: Option<u32>,
    camera_pos: [f64; 3],
    yaw: f32,
    diagonal: f32,
    aspect: AspectRatio,
    distance: f64,
    /// Cylindrical curvature in degrees (0.0 = flat, up to 120.0).
    pub curvature: f32,
    applied_native_size: Option<(u32, u32)>,
    at: [f64; 3],
    corners: [[f32; 3]; 4],
    pub custom_speakers: HashMap<SpeakerId, [f64; 3]>,
}

impl Screen {
    pub fn new(video: Option<u32>, camera_pos: [f64; 3], yaw: f32) -> Self {
        let (sin, cos) = yaw.to_radians().sin_cos();
        let [x, y, z] = camera_pos;
        let mut screen = Self {
            video,
            camera_pos,
            yaw,
            diagonal: DEFAULT_DIAGONAL,
            aspect: AspectRatio::Auto,
            distance: DEFAULT_DISTANCE,
            curvature: 0.0,
            applied_native_size: None,
            at: [
                x - f64::from(sin) * DEFAULT_DISTANCE,
                y - f64::from(DEFAULT_HEIGHT) / 2.0,
                z + f64::from(cos) * DEFAULT_DISTANCE,
            ],
            corners: [[0.0; 3]; 4],
            custom_speakers: HashMap::new(),
        };
        screen.recalculate(None);
        screen
    }

    pub fn from_saved(saved: &SavedScreen, video: Option<u32>) -> Self {
        let mut screen = Self {
            video,
            camera_pos: saved.camera_pos,
            yaw: saved.yaw,
            diagonal: saved.diagonal,
            aspect: AspectRatio::parse(&saved.aspect).unwrap_or(AspectRatio::Auto),
            distance: saved.distance,
            curvature: saved.curvature,
            applied_native_size: None,
            at: saved.at,
            corners: [[0.0; 3]; 4],
            custom_speakers: saved.speakers.clone(),
        };
        screen.recalculate(None);
        screen
    }

    pub fn dimensions(&self, native_size: Option<(u32, u32)>) -> (f32, f32) {
        let r = self.aspect.value(native_size);
        let height = self.diagonal / (r * r + 1.0).sqrt();
        let width = r * height;
        (width, height)
    }

    pub fn recalculate(&mut self, native_size: Option<(u32, u32)>) {
        self.applied_native_size = native_size;
        let (width, height) = self.dimensions(native_size);
        let (sin, cos) = self.yaw.to_radians().sin_cos();

        let right = [-cos, -sin];
        let side = |sign: f32, y_val: f32| {
            [
                right[0] * sign * width / 2.0,
                y_val,
                right[1] * sign * width / 2.0,
            ]
        };

        self.corners = [
            side(-1.0, height),
            side(-1.0, 0.0),
            side(1.0, 0.0),
            side(1.0, height),
        ];
    }

    /// Computes mesh slices for curved or flat screen rendering.
    /// Each slice returns `(corners, uv)` where `corners` are in world-pose relative to `self.at`.
    pub fn slices(&self, native_size: Option<(u32, u32)>) -> Vec<([[f32; 3]; 4], [f32; 4])> {
        let (width, height) = self.dimensions(native_size);
        if self.curvature <= 0.5 {
            return vec![(self.corners, [0.0, 0.0, 1.0, 1.0])];
        }

        let (sin, cos) = self.yaw.to_radians().sin_cos();
        let right = [-cos, -sin];
        let normal = [sin, -cos]; // Normal facing towards camera

        let alpha = self.curvature.to_radians();
        let radius = width / alpha;
        let n = CURVE_SLICES;
        let mut slices = Vec::with_capacity(n);

        let pt_at = |x_rel: f32, y_val: f32, z_rel: f32| -> [f32; 3] {
            [
                right[0] * x_rel + normal[0] * z_rel,
                y_val,
                right[1] * x_rel + normal[1] * z_rel,
            ]
        };

        for i in 0..n {
            let f0 = i as f32 / n as f32;
            let f1 = (i + 1) as f32 / n as f32;
            let theta0 = -alpha * 0.5 + alpha * f0;
            let theta1 = -alpha * 0.5 + alpha * f1;

            let x0 = radius * theta0.sin();
            let z0 = radius * (1.0 - theta0.cos());
            let x1 = radius * theta1.sin();
            let z1 = radius * (1.0 - theta1.cos());

            let corners = [
                pt_at(x0, height, z0),
                pt_at(x0, 0.0, z0),
                pt_at(x1, 0.0, z1),
                pt_at(x1, height, z1),
            ];
            slices.push((corners, [f0, 0.0, f1, 1.0]));
        }

        slices
    }
}

/// Calculated positions for 3D surround sound speakers in the room.
#[derive(Clone, Copy, Debug)]
pub struct SpeakerLayout {
    pub fl: [f64; 3],
    pub fr: [f64; 3],
    pub c: [f64; 3],
    pub lfe: [f64; 3],
    pub sl: [f64; 3],
    pub sr: [f64; 3],
    pub bl: [f64; 3],
    pub br: [f64; 3],
}

impl SpeakerLayout {
    pub fn compute(
        screen_at: [f64; 3],
        yaw: f32,
        width: f32,
        height: f32,
        custom: &HashMap<SpeakerId, [f64; 3]>,
    ) -> Self {
        let (sin, cos) = yaw.to_radians().sin_cos();
        let right = [f64::from(-cos), 0.0, f64::from(-sin)];
        let normal = [f64::from(sin), 0.0, f64::from(-cos)];
        let up = [0.0, 1.0, 0.0];

        let hw = f64::from(width) * 0.5;
        let h = f64::from(height);
        // Room depth estimated from screen width
        let room_depth = (hw * 2.4).clamp(5.0, 18.0);

        let pt = |r: f64, u: f64, n: f64| -> [f64; 3] {
            [
                screen_at[0] + right[0] * r + up[0] * u + normal[0] * n,
                screen_at[1] + right[1] * r + up[1] * u + normal[1] * n,
                screen_at[2] + right[2] * r + up[2] * u + normal[2] * n,
            ]
        };

        // Front Center: behind screen center at dialogue level
        let mut c = pt(0.0, h * 0.45, -0.15);
        // Front Left: outside left screen edge, ear level
        let mut fl = pt(-(hw + 0.6), h * 0.4, 0.2);
        // Front Right: outside right screen edge, ear level
        let mut fr = pt(hw + 0.6, h * 0.4, 0.2);
        // Subwoofer (LFE): on floor near front corner
        let mut lfe = pt(-(hw * 0.7), 0.25, 0.4);
        // Side Left: along left room wall at viewer row height
        let mut sl = pt(-(hw + 1.8), 1.5, room_depth * 0.5);
        // Side Right: along right room wall at viewer row height
        let mut sr = pt(hw + 1.8, 1.5, room_depth * 0.5);
        // Back Left: rear room wall
        let mut bl = pt(-(hw + 0.8), 1.7, room_depth * 0.95);
        // Back Right: rear room wall
        let mut br = pt(hw + 0.8, 1.7, room_depth * 0.95);

        if let Some(&p) = custom.get(&SpeakerId::FL) { fl = p; }
        if let Some(&p) = custom.get(&SpeakerId::FR) { fr = p; }
        if let Some(&p) = custom.get(&SpeakerId::C) { c = p; }
        if let Some(&p) = custom.get(&SpeakerId::LFE) { lfe = p; }
        if let Some(&p) = custom.get(&SpeakerId::SL) { sl = p; }
        if let Some(&p) = custom.get(&SpeakerId::SR) { sr = p; }
        if let Some(&p) = custom.get(&SpeakerId::BL) { bl = p; }
        if let Some(&p) = custom.get(&SpeakerId::BR) { br = p; }

        Self { fl, fr, c, lfe, sl, sr, bl, br }
    }
}

/// Audio output modes for screen playback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AudioMode {
    /// 3D Spatial Audio: positioned at the screen center with 3D HRTF panning,
    /// distance attenuation up to 48 blocks, and wall occlusion muffling.
    Game3D,
    /// Dual 3D Speakers: separate left and right spatial emitters placed at the
    /// edges of wide cinema screens for realistic room stereo separation.
    StereoSpeakers,
    /// 5.1 Surround Sound: 6 discrete/upmixed 3D speaker sources in world:
    /// FL (Front Left), FR (Front Right), C (Center screen), LFE (Subwoofer),
    /// SL (Surround Left), SR (Surround Right).
    Surround51,
    /// 7.1 Surround Sound: 8 discrete/upmixed 3D speaker sources in world:
    /// FL, FR, C, LFE, BL (Back Left), BR (Back Right), SL (Side Left), SR (Side Right).
    Surround71,
    /// Non-spatial direct audio routed through Horizon's game mixer (controlled by
    /// in-game master volume and pause menu attenuation).
    Direct,
    /// Mute audio.
    Mute,
}

struct State {
    screen: Option<Screen>,
    editing: Option<EditState>,
    saved_screens: HashMap<String, SavedScreen>,
    demo: Option<String>,
    reported: Option<Instant>,
    screen_size: [f32; 2],
    chat_open: bool,
    /// Optional grid snapping increment in blocks (e.g. Some(0.5)).
    grid_snap: Option<f32>,
    /// Dynamic room lighting enabled (default: true).
    light_enabled: bool,
    /// Light brightness multiplier (default: 1.0, 0.0..=2.0).
    light_brightness: f32,
    /// Smoothed ambient light RGB for transitions.
    current_light_rgb: [f32; 3],
    current_light_intensity: f32,
    /// In-game audio mode (default: 5.1 surround sound).
    audio_mode: AudioMode,
    /// Show virtual 3D speaker boxes and tags in world.
    show_speakers: bool,
    /// Currently hovered target by player's crosshair in first-person
    hovered_target: Option<DragTarget>,
    /// Target actively being dragged by holding Right Mouse Button (ПКМ)
    active_drag: Option<DragTarget>,
    /// Previous frame's RMB state to detect initial press transition
    prev_right_down: bool,
    /// Where the saved screens are kept; none until a hook has asked the game.
    screens_path: Option<PathBuf>,
    /// When sound was last taken, for how much to take this frame.
    audio_at: Option<Instant>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            screen: None,
            editing: None,
            saved_screens: HashMap::new(),
            demo: None,
            reported: None,
            screen_size: [0.0, 0.0],
            chat_open: false,
            grid_snap: None,
            light_enabled: true,
            light_brightness: 1.0,
            current_light_rgb: [0.0, 0.0, 0.0],
            current_light_intensity: 0.0,
            audio_mode: AudioMode::Surround51,
            show_speakers: false,
            hovered_target: None,
            active_drag: None,
            prev_right_down: false,
            screens_path: None,
            audio_at: None,
        }
    }
}

/// Where the screens are kept: the game's shared config folder
/// (`config/video_screens.json`), not wherever the game was started from.
fn screens_path(host: &dyn Host) -> PathBuf {
    host.config_dir()
        .map(|dir| dir.join(SCREENS_FILE))
        .unwrap_or_else(|_| PathBuf::from(SCREENS_FILE))
}

/// Where earlier versions kept the screens: the working directory, then
/// `%APPDATA%\horizon`. Read once while the config folder has none.
fn legacy_screens_paths() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from(SCREENS_FILE)];
    if let Ok(appdata) = std::env::var("APPDATA") {
        paths.push(PathBuf::from(appdata).join("horizon").join(SCREENS_FILE));
    }
    paths
}

/// The screens saved at `path`, or at an earlier version's place while there are none.
fn load_saved_screens(path: &Path) -> HashMap<String, SavedScreen> {
    let read = |path: &Path| std::fs::read_to_string(path).ok();
    let Some(content) = read(path).or_else(|| legacy_screens_paths().iter().find_map(|p| read(p)))
    else {
        return HashMap::new();
    };
    let content = content.trim_start_matches('\u{feff}');
    serde_json::from_str(content).unwrap_or_default()
}

/// Reads the saved screens the first time a hook can ask the game where they are.
fn ensure_screens(host: &dyn Host, state: &mut State) {
    if state.screens_path.is_none() {
        let path = screens_path(host);
        state.saved_screens = load_saved_screens(&path);
        state.screens_path = Some(path);
    }
}

fn save_saved_screens(state: &State) -> Result<(), String> {
    let path = state
        .screens_path
        .clone()
        .unwrap_or_else(|| PathBuf::from(SCREENS_FILE));
    let json = serde_json::to_string_pretty(&state.saved_screens)
        .map_err(|e| format!("failed to serialize screens: {e}"))?;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("failed to create {}: {e}", dir.display()))?;
    }
    std::fs::write(&path, json)
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    Ok(())
}

/// The first of `video`'s audio sources in the mixer; a channel's is this plus its index.
fn voice_base(video: u32) -> u128 {
    u128::from(video) << 4
}

/// Every audio source `video` may use.
fn voices(video: u32) -> Vec<u128> {
    (0..MAX_CHANNELS).map(|i| voice_base(video) + i).collect()
}

/// Closes `video` and drops what it still had waiting in the mixer.
fn stop_video(host: &mut dyn Host, video: u32) {
    host.video_close(video);
    host.clear_voices(MOD_ID, &voices(video));
}

/// A frame's worth of a video's sound, found under the lock and taken and
/// played after it, so the host calls never run while the lock is held.
struct AudioJob {
    video: u32,
    mode: AudioMode,
    speakers: SpeakerLayout,
    /// Seconds since sound was last taken.
    elapsed: f64,
}

impl AudioJob {
    fn play(self, host: &mut dyn Host) {
        let rate = host.video_audio_rate(self.video).unwrap_or(FALLBACK_RATE);
        // Twice what played since the last frame, and at least a 30 FPS
        // frame's: a slow frame takes its whole share and nothing piles up.
        let elapsed = self.elapsed.clamp(0.0, 0.25);
        let max = ((f64::from(rate) * elapsed * 2.0).ceil() as u32).max(rate / 30);
        let (video, spk) = (self.video, &self.speakers);
        let (channels, places): (Vec<Vec<f32>>, Vec<Option<[f64; 3]>>) = match self.mode {
            AudioMode::Surround71 => match host.video_audio_71(video, max) {
                // FL, FR, FC, LFE, BL, BR, SL, SR
                Some(ch) => (
                    Vec::from(ch),
                    [spk.fl, spk.fr, spk.c, spk.lfe, spk.bl, spk.br, spk.sl, spk.sr]
                        .map(Some)
                        .to_vec(),
                ),
                None => return,
            },
            AudioMode::Surround51 => match host.video_audio_51(video, max) {
                // FL, FR, FC, LFE, SL, SR
                Some(ch) => (
                    Vec::from(ch),
                    [spk.fl, spk.fr, spk.c, spk.lfe, spk.sl, spk.sr]
                        .map(Some)
                        .to_vec(),
                ),
                None => return,
            },
            AudioMode::StereoSpeakers => match host.video_audio_stereo(video, max) {
                Some((left, right)) => (vec![left, right], vec![Some(spk.fl), Some(spk.fr)]),
                None => return,
            },
            AudioMode::Game3D => match host.video_audio(video, max) {
                Some(mono) => (vec![mono], vec![Some(spk.c)]),
                None => return,
            },
            AudioMode::Direct => match host.video_audio(video, max) {
                Some(mono) => (vec![mono], vec![None]),
                None => return,
            },
            AudioMode::Mute => return,
        };
        // Whatever number of channels came, each has its own source and place;
        // all are queued together so they stay lined up.
        let count = channels.len().min(places.len());
        let sources: Vec<u128> = (0..count as u128).map(|i| voice_base(video) + i).collect();
        host.push_voices(MOD_ID, &sources, &channels[..count], rate, &places[..count]);
    }
}

pub struct VideoScreen(Mutex<State>);

impl Default for VideoScreen {
    fn default() -> Self {
        Self(Mutex::new(State {
            demo: std::env::var("HORIZON_VIDEO_DEMO").ok(),
            ..State::default()
        }))
    }
}

impl VideoScreen {
    /// The mod's state. A panic in an earlier hook leaves it as it was, not
    /// poisoned: the next hook goes on with it rather than failing too.
    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A frame's work on the state: the screen, the editor, the light. Gives
    /// the sound to take and play, which `frame` does after the lock.
    fn frame_locked(&self, host: &mut dyn Host, state: &mut State) -> Option<AudioJob> {
        ensure_screens(&*host, state);
        // A video the game closed (the world was left) is gone: forget its id.
        if let Some(screen) = state.screen.as_mut()
            && let Some(video) = screen.video
            && host.video_state(video).is_none()
        {
            screen.video = None;
            host.clear_voices(MOD_ID, &voices(video));
        }
        if let Some(path) = state.demo.take() {
            match self.open(host, state, &path) {
                Ok(()) => host.log("info", &format!("video: playing {path}")),
                Err(_) if host.camera().is_none() => state.demo = Some(path),
                Err(why) => host.log("warn", &format!("video: {why}")),
            }
        }

        // Automatic aspect ratio detection from video stream metadata
        if let Some(screen) = state.screen.as_mut()
            && screen.aspect == AspectRatio::Auto
                && let Some(video) = screen.video
                    && let Some(native_size) = host.video_state(video).and_then(|s| s.video_size)
                        && Some(native_size) != screen.applied_native_size {
                            screen.recalculate(Some(native_size));
                            let (w, h) = screen.dimensions(Some(native_size));
                            let (nw, nh) = native_size;
                            host.log(
                                "info",
                                &format!(
                                    "video: auto-detected video stream resolution {}x{} -> screen adjusted to {:.2}x{:.2} blocks",
                                    nw, nh, w, h
                                ),
                            );
                        }

        // The video's sound for the game's mixer: what to take is decided
        // here, the taking and playing happen once the lock is let go.
        let now = Instant::now();
        let elapsed = state
            .audio_at
            .replace(now)
            .map_or(1.0 / 60.0, |at| now.duration_since(at).as_secs_f64());
        let mut audio = None;
        if let Some(screen) = &state.screen
            && let Some(video) = screen.video
            && state.audio_mode != AudioMode::Mute
        {
            let native_size = host.video_state(video).and_then(|s| s.video_size);
            let (w, h) = screen.dimensions(native_size);
            audio = Some(AudioJob {
                video,
                mode: state.audio_mode,
                speakers: SpeakerLayout::compute(screen.at, screen.yaw, w, h, &screen.custom_speakers),
                elapsed,
            });
        }

        // 3D crosshair raycasting and RMB (ПКМ) dragging of screen handles, center, and speakers
        let right_down = host.has_right_down();
        let prev_right_down = state.prev_right_down;
        state.prev_right_down = right_down;

        let is_editing = state.editing.is_some();
        let show_speakers = state.show_speakers || is_editing;
        let grid_snap = state.grid_snap;
        let audio_mode = state.audio_mode;
        let mut hovered_target = state.hovered_target;
        let mut active_drag = state.active_drag;

        let camera_opt = host.camera();
        if let (Some(camera), Some(screen)) = (camera_opt.as_ref(), state.screen.as_mut()) {
            let ray_dir = Self::crosshair_ray(camera);
            let native_size = screen.video.and_then(|v| host.video_state(v)).and_then(|s| s.video_size);

            if right_down {
                if !prev_right_down && active_drag.is_none() {
                    // Pressed RMB on this frame: grab hovered target
                    if let Some(target) = hovered_target {
                        active_drag = Some(target);
                    }
                }

                if let Some(active) = active_drag {
                    match active {
                        DragTarget::Handle(handle) => {
                            if let Some((u, v)) = Self::ray_intersect_screen_plane(screen, camera.position, ray_dir) {
                                Self::apply_drag(screen, handle, u as f32, v as f32, native_size, grid_snap);
                            }
                        }
                        DragTarget::Speaker(spk_id, dist) => {
                            let mut new_pos = [
                                camera.position[0] + ray_dir[0] * dist,
                                camera.position[1] + ray_dir[1] * dist,
                                camera.position[2] + ray_dir[2] * dist,
                            ];
                            if let Some(step) = grid_snap {
                                let s = f64::from(step);
                                new_pos[0] = (new_pos[0] / s).round() * s;
                                new_pos[1] = (new_pos[1] / s).round() * s;
                                new_pos[2] = (new_pos[2] / s).round() * s;
                            }
                            screen.custom_speakers.insert(spk_id, new_pos);
                        }
                        DragTarget::ScreenCenter { initial_dist } => {
                            let (_w, h) = screen.dimensions(native_size);
                            let new_center = [
                                camera.position[0] + ray_dir[0] * initial_dist,
                                camera.position[1] + ray_dir[1] * initial_dist,
                                camera.position[2] + ray_dir[2] * initial_dist,
                            ];
                            let mut new_at = [
                                new_center[0],
                                new_center[1] - f64::from(h) * 0.5,
                                new_center[2],
                            ];
                            if let Some(step) = grid_snap {
                                let s = f64::from(step);
                                new_at[0] = (new_at[0] / s).round() * s;
                                new_at[1] = (new_at[1] / s).round() * s;
                                new_at[2] = (new_at[2] / s).round() * s;
                            }
                            screen.at = new_at;
                        }
                    }
                }
            } else {
                // Released RMB
                if let Some(finished) = active_drag.take() {
                    match finished {
                        DragTarget::Handle(_) => {
                            let (w, h) = screen.dimensions(native_size);
                            host.feedback(Text::colored(
                                format!("§6[Horizon Video] §aРазмер экрана зафиксирован: §b{:.2}x{:.2} бл §7(диагональ {:.2}).", w, h, screen.diagonal),
                                0x55FF55,
                            ));
                        }
                        DragTarget::Speaker(id, _) => {
                            if let Some(pos) = screen.custom_speakers.get(&id) {
                                host.feedback(Text::colored(
                                    format!("§6[Horizon Video] §aПозиция колонки §e{} §aзафиксирована: §7[{:.2}, {:.2}, {:.2}].", id.name(), pos[0], pos[1], pos[2]),
                                    0x55FF55,
                                ));
                            }
                        }
                        DragTarget::ScreenCenter { .. } => {
                            host.feedback(Text::colored(
                                format!("§6[Horizon Video] §aПозиция экрана зафиксирована: §7[{:.2}, {:.2}, {:.2}].", screen.at[0], screen.at[1], screen.at[2]),
                                0x55FF55,
                            ));
                        }
                    }
                }

                // Update hovered target for visual highlighting
                hovered_target = Self::find_crosshair_target(
                    screen,
                    camera.position,
                    ray_dir,
                    is_editing,
                    show_speakers,
                    audio_mode,
                );
            }
        }
        state.active_drag = active_drag;
        state.hovered_target = hovered_target;

        let due = state
            .reported
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(5));
        if let (true, Some(screen)) = (due, &state.screen) {
            host.log("info", &Self::info(host, screen));
            state.reported = Some(Instant::now());
        }
        audio
    }


    fn open(&self, host: &mut dyn Host, state: &mut State, path: &str) -> Result<(), String> {
        let camera = host.camera().ok_or("no world is shown yet")?;
        let video = host.video_open(path, 1280, 720, true)?;
        if let Some(old) = state.screen.take()
            && let Some(v) = old.video {
                stop_video(host, v);
            }
        state.screen = Some(Screen::new(Some(video), camera.position, camera.yaw));
        Ok(())
    }

    fn open_on_screen(
        &self,
        host: &mut dyn Host,
        state: &mut State,
        saved: &SavedScreen,
        path: &str,
    ) -> Result<(), String> {
        let video = host.video_open(path, 1280, 720, true)?;
        if let Some(old) = state.screen.take()
            && let Some(v) = old.video {
                stop_video(host, v);
            }
        state.screen = Some(Screen::from_saved(saved, Some(video)));
        Ok(())
    }

    fn info(host: &dyn Host, screen: &Screen) -> String {
        let native_size = screen.video.and_then(|v| host.video_state(v)).and_then(|s| s.video_size);
        let (w, h) = screen.dimensions(native_size);
        let aspect_mode = match screen.aspect {
            AspectRatio::Auto => format!("auto ({:.2})", screen.aspect.value(native_size)),
            AspectRatio::Ratio(r) => format!("{:.2}", r),
        };
        let curve_str = if screen.curvature > 0.5 {
            format!(", curve {:.0}°", screen.curvature)
        } else {
            "".to_string()
        };
        let size_str = format!("{:.2}x{:.2} blocks (diag {:.2}, aspect {}{})", w, h, screen.diagonal, aspect_mode, curve_str);
        let video_info = screen.video.and_then(|v| host.video_state(v));
        match video_info {
            Some(s) => format!(
                "video: {:.1}/{:.1} s, size {}, dist {:.1} blk, {}{}{} frames; render {} us, game thread {} us taking + {} us staging",
                s.position.unwrap_or(0.0),
                s.duration.unwrap_or(0.0),
                size_str,
                screen.distance,
                if s.paused { "paused, " } else { "" },
                if s.failed { "FAILED, " } else { "" },
                s.frames,
                s.render_micros,
                s.pump_micros,
                s.upload_micros,
            ),
            None => format!("screen: size {}, dist {:.1} blk (no video playing)", size_str, screen.distance),
        }
    }

    fn send_chat_editor_prompt(
        host: &mut dyn Host,
        id: &str,
        screen: &Screen,
        light_enabled: bool,
        audio_mode: AudioMode,
        show_speakers: bool,
    ) {
        let (w, h) = screen.dimensions(screen.applied_native_size);
        let curve_info = if screen.curvature > 0.5 {
            format!(" | изгиб {:.0}°", screen.curvature)
        } else {
            "".to_string()
        };

        host.feedback(Text::colored(
            format!("§6[Horizon Video] §eРежим настройки экрана: §b{id} §7({:.2}x{:.2} бл{})", w, h, curve_info),
            0xFFAA00,
        ));
        host.feedback(Text::colored(
            "§eУправление в мире: §fНаведите прицел на угол экрана или колонку и §6зажмите ПКМ§f, чтобы перемещать в 3D!",
            0xFFFF55,
        ));
        host.feedback(Text::colored(
            "§7(Тяните за угол — размер экрана; за край — ширина/высота; за центр — экран целиком; за колонку — перенос звука).",
            0xAAAAAA,
        ));

        // Row 1: Actions & Snapping
        let mut row1 = Text::literal("  ");
        row1 = row1.append(Text::styled("[💾 Сохранить] ", Style {
            color: Some(0x55FF55),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand(format!("/video save {id}"))),
            ..Default::default()
        }));
        row1 = row1.append(Text::styled("[🧲 К стене] ", Style {
            color: Some(0xFFAA00),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand("/video snap wall".into())),
            ..Default::default()
        }));
        row1 = row1.append(Text::styled("[📐 0°] ", Style {
            color: Some(0xAAAAAA),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand("/video curve 0".into())),
            ..Default::default()
        }));
        row1 = row1.append(Text::styled("[📐 30°] ", Style {
            color: Some(0xFFFF55),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand("/video curve 30".into())),
            ..Default::default()
        }));
        row1 = row1.append(Text::styled("[📐 60°] ", Style {
            color: Some(0xFFFF55),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand("/video curve 60".into())),
            ..Default::default()
        }));
        row1 = row1.append(Text::styled("[❌ Закрыть]", Style {
            color: Some(0xFF5555),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand("/video cancel".into())),
            ..Default::default()
        }));
        host.feedback(row1);

        // Row 2: Aspect Ratios, Lighting, Audio
        let mut row2 = Text::literal("  §7Пропорции: ");
        row2 = row2.append(Text::styled("[16:9] ", Style {
            color: Some(0x55FFFF),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand("/video aspect 16:9".into())),
            ..Default::default()
        }));
        row2 = row2.append(Text::styled("[4:3] ", Style {
            color: Some(0x55FFFF),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand("/video aspect 4:3".into())),
            ..Default::default()
        }));
        row2 = row2.append(Text::styled("[21:9] ", Style {
            color: Some(0x55FFFF),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand("/video aspect 21:9".into())),
            ..Default::default()
        }));
        row2 = row2.append(Text::styled("[1:1] ", Style {
            color: Some(0x55FFFF),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand("/video aspect 1:1".into())),
            ..Default::default()
        }));
        row2 = row2.append(Text::styled("[Auto]", Style {
            color: Some(0xFFFF55),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand("/video aspect auto".into())),
            ..Default::default()
        }));
        let (light_text, light_col, light_cmd) = if light_enabled {
            (" [💡 Свет: Вкл]", 0xFFFF55, "/video light off")
        } else {
            (" [💡 Свет: Выкл]", 0x888888, "/video light on")
        };
        row2 = row2.append(Text::styled(light_text, Style {
            color: Some(light_col),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand(light_cmd.into())),
            ..Default::default()
        }));

        let (audio_text, audio_col, audio_cmd) = match audio_mode {
            AudioMode::Game3D => (" [🔊 3D]", 0x55FF55, "/video audio stereo"),
            AudioMode::StereoSpeakers => (" [🔊 Стерео]", 0x55FFFF, "/video audio 5.1"),
            AudioMode::Surround51 => (" [🔊 5.1]", 0xFFAA00, "/video audio 7.1"),
            AudioMode::Surround71 => (" [🔊 7.1]", 0xFF55FF, "/video audio direct"),
            AudioMode::Direct => (" [🔊 Direct]", 0xFFFF55, "/video audio mute"),
            AudioMode::Mute => (" [🔇 Выкл]", 0xFF5555, "/video audio 3d"),
        };
        row2 = row2.append(Text::styled(audio_text, Style {
            color: Some(audio_col),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand(audio_cmd.into())),
            ..Default::default()
        }));

        let (speakers_text, speakers_col) = if show_speakers {
            (" [🔈 Колонки: ВКЛ]", 0x55FF55)
        } else {
            (" [🔈 Колонки: ВЫКЛ]", 0x888888)
        };
        row2 = row2.append(Text::styled(speakers_text, Style {
            color: Some(speakers_col),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand("/video speakers".into())),
            ..Default::default()
        }));
        host.feedback(row2);
    }

    fn attach_to_surface(screen: &mut Screen, camera: &NativeCamera, hit: &NativeHit) -> String {
        let (_scr_w, scr_h) = screen.dimensions(screen.applied_native_size);
        let bx = hit.block_x as f64;
        let by = hit.block_y as f64;
        let bz = hit.block_z as f64;

        let d_west = (hit.x - bx).abs();
        let d_east = (hit.x - (bx + 1.0)).abs();
        let d_north = (hit.z - bz).abs();
        let d_south = (hit.z - (bz + 1.0)).abs();
        let d_down = (hit.y - by).abs();
        let d_up = (hit.y - (by + 1.0)).abs();

        let min_d = d_west.min(d_east).min(d_north).min(d_south).min(d_down).min(d_up);
        let offset = 0.02; // Small gap to prevent z-fighting with block surface

        let (normal, yaw, face_name) = if min_d == d_west {
            ([-1.0, 0.0, 0.0], 270.0, "западная стена (West)")
        } else if min_d == d_east {
            ([1.0, 0.0, 0.0], 90.0, "восточная стена (East)")
        } else if min_d == d_north {
            ([0.0, 0.0, -1.0], 0.0, "северная стена (North)")
        } else if min_d == d_south {
            ([0.0, 0.0, 1.0], 180.0, "южная стена (South)")
        } else if min_d == d_up {
            // Looking at floor: place facing player's horizontal yaw
            ([0.0, 1.0, 0.0], (camera.yaw / 90.0).round() * 90.0, "пол (Floor)")
        } else {
            ([0.0, -1.0, 0.0], (camera.yaw / 90.0).round() * 90.0, "потолок (Ceiling)")
        };

        screen.yaw = yaw;
        if min_d == d_up {
            // Standing on floor
            screen.at = [hit.x, hit.y + offset, hit.z];
        } else if min_d == d_down {
            // Hanging from ceiling
            screen.at = [hit.x, hit.y - f64::from(scr_h) - offset, hit.z];
        } else {
            // Mounted flush on wall
            let wall_x = hit.x + normal[0] * offset;
            let wall_y = hit.y - f64::from(scr_h) * 0.5;
            let wall_z = hit.z + normal[2] * offset;
            screen.at = [wall_x, wall_y, wall_z];
        }

        // Keep distance and camera_pos consistent
        let dx = camera.position[0] - screen.at[0];
        let dy = camera.position[1] - (screen.at[1] + f64::from(scr_h) * 0.5);
        let dz = camera.position[2] - screen.at[2];
        screen.distance = (dx * dx + dy * dy + dz * dz).sqrt().clamp(1.0, 100.0);
        screen.camera_pos = camera.position;
        screen.recalculate(screen.applied_native_size);

        format!("блок {} [{:.0}, {:.0}, {:.0}] - {}", hit.block, bx, by, bz, face_name)
    }

    fn raycast_screen_plane(
        screen: &Screen,
        camera: &NativeCamera,
        mouse: [f32; 2],
        gui_size: [f32; 2],
    ) -> Option<(f32, f32)> {
        let w = gui_size[0].max(1.0);
        let h = gui_size[1].max(1.0);
        let nx = 2.0 * (mouse[0] / w) - 1.0;
        let ny = 1.0 - 2.0 * (mouse[1] / h);

        let fov_rad = camera.fov_degrees.to_radians();
        let half_tan = (fov_rad * 0.5).tan();
        let aspect = w / h;

        let yaw_rad = camera.yaw.to_radians();
        let pitch_rad = camera.pitch.to_radians();
        let (sin_y, cos_y) = yaw_rad.sin_cos();
        let (sin_p, cos_p) = pitch_rad.sin_cos();

        // Forward vector in Minecraft camera space
        let f_x = -cos_p * sin_y;
        let f_y = -sin_p;
        let f_z = cos_p * cos_y;

        // Right vector
        let r_x = -cos_y;
        let r_y = 0.0;
        let r_z = -sin_y;

        // Up vector: R x F
        let u_x = -sin_y * sin_p;
        let u_y = cos_p;
        let u_z = cos_y * sin_p;

        // Ray direction
        let dx = f_x + r_x * (nx * half_tan * aspect) + u_x * (ny * half_tan);
        let dy = f_y + r_y * (nx * half_tan * aspect) + u_y * (ny * half_tan);
        let dz = f_z + r_z * (nx * half_tan * aspect) + u_z * (ny * half_tan);
        let len = (dx * dx + dy * dy + dz * dz).sqrt().max(1e-6);
        let (dir_x, dir_y, dir_z) = (dx / len, dy / len, dz / len);

        // Screen center
        let (_scr_w, scr_h) = screen.dimensions(screen.applied_native_size);
        let center = [
            screen.at[0],
            screen.at[1] + f64::from(scr_h) * 0.5,
            screen.at[2],
        ];

        let (scr_sin, scr_cos) = screen.yaw.to_radians().sin_cos();
        // Screen normal facing the camera
        let norm_x = f64::from(scr_sin);
        let norm_y = 0.0;
        let norm_z = f64::from(-scr_cos);

        let denom = f64::from(dir_x) * norm_x + f64::from(dir_y) * norm_y + f64::from(dir_z) * norm_z;
        if denom >= -1e-4 {
            return None;
        }

        let to_center_x = center[0] - camera.position[0];
        let to_center_y = center[1] - camera.position[1];
        let to_center_z = center[2] - camera.position[2];
        let numer = to_center_x * norm_x + to_center_y * norm_y + to_center_z * norm_z;
        let t = numer / denom;
        if t <= 0.0 {
            return None;
        }

        let hit_x = camera.position[0] + t * f64::from(dir_x);
        let hit_y = camera.position[1] + t * f64::from(dir_y);
        let hit_z = camera.position[2] + t * f64::from(dir_z);

        let rel_x = (hit_x - center[0]) as f32;
        let rel_y = (hit_y - center[1]) as f32;
        let rel_z = (hit_z - center[2]) as f32;

        let scr_r_x = -scr_cos;
        let scr_r_z = -scr_sin;

        let u = rel_x * scr_r_x + rel_z * scr_r_z;
        let v = rel_y;

        Some((u, v))
    }

    fn find_hit_handle(
        screen: &Screen,
        camera: &NativeCamera,
        mouse: [f32; 2],
        gui_size: [f32; 2],
    ) -> Option<(DragHandle, (f32, f32))> {
        let (u, v) = Self::raycast_screen_plane(screen, camera, mouse, gui_size)?;
        let (w, h) = screen.dimensions(screen.applied_native_size);
        let hw = w * 0.5;
        let hh = h * 0.5;

        let r_corner = (w.min(h) * 0.25).clamp(0.3, 0.9);
        let r_edge = (w.min(h) * 0.16).clamp(0.2, 0.6);

        // Corners:
        if (u - (-hw)).powi(2) + (v - hh).powi(2) <= r_corner.powi(2) {
            return Some((DragHandle::CornerTL, (u, v)));
        }
        if (u - hw).powi(2) + (v - hh).powi(2) <= r_corner.powi(2) {
            return Some((DragHandle::CornerTR, (u, v)));
        }
        if (u - (-hw)).powi(2) + (v - (-hh)).powi(2) <= r_corner.powi(2) {
            return Some((DragHandle::CornerBL, (u, v)));
        }
        if (u - hw).powi(2) + (v - (-hh)).powi(2) <= r_corner.powi(2) {
            return Some((DragHandle::CornerBR, (u, v)));
        }

        // Edges:
        if (u - (-hw)).abs() <= r_edge && v.abs() <= hh + r_edge {
            return Some((DragHandle::EdgeLeft, (u, v)));
        }
        if (u - hw).abs() <= r_edge && v.abs() <= hh + r_edge {
            return Some((DragHandle::EdgeRight, (u, v)));
        }
        if (v - hh).abs() <= r_edge && u.abs() <= hw + r_edge {
            return Some((DragHandle::EdgeTop, (u, v)));
        }
        if (v - (-hh)).abs() <= r_edge && u.abs() <= hw + r_edge {
            return Some((DragHandle::EdgeBottom, (u, v)));
        }

        None
    }

    pub fn crosshair_ray(camera: &NativeCamera) -> [f64; 3] {
        let yaw_rad = f64::from(camera.yaw).to_radians();
        let pitch_rad = f64::from(camera.pitch).to_radians();
        let (sin_y, cos_y) = yaw_rad.sin_cos();
        let (sin_p, cos_p) = pitch_rad.sin_cos();

        let dx = -cos_p * sin_y;
        let dy = -sin_p;
        let dz = cos_p * cos_y;
        let len = (dx * dx + dy * dy + dz * dz).sqrt().max(1e-6);
        [dx / len, dy / len, dz / len]
    }

    pub fn point_ray_dist(point: [f64; 3], ray_orig: [f64; 3], ray_dir: [f64; 3]) -> (f64, f64) {
        let vx = point[0] - ray_orig[0];
        let vy = point[1] - ray_orig[1];
        let vz = point[2] - ray_orig[2];
        let t = vx * ray_dir[0] + vy * ray_dir[1] + vz * ray_dir[2];
        if t <= 0.0 {
            return (f64::MAX, t);
        }
        let qx = ray_orig[0] + ray_dir[0] * t;
        let qy = ray_orig[1] + ray_dir[1] * t;
        let qz = ray_orig[2] + ray_dir[2] * t;
        let dist_sq = (point[0] - qx).powi(2) + (point[1] - qy).powi(2) + (point[2] - qz).powi(2);
        (dist_sq.sqrt(), t)
    }

    pub fn ray_intersect_screen_plane(
        screen: &Screen,
        cam_pos: [f64; 3],
        ray_dir: [f64; 3],
    ) -> Option<(f64, f64)> {
        let (_scr_w, scr_h) = screen.dimensions(screen.applied_native_size);
        let center = [
            screen.at[0],
            screen.at[1] + f64::from(scr_h) * 0.5,
            screen.at[2],
        ];

        let (scr_sin, scr_cos) = screen.yaw.to_radians().sin_cos();
        let norm = [f64::from(scr_sin), 0.0, f64::from(-scr_cos)];

        let denom = ray_dir[0] * norm[0] + ray_dir[1] * norm[1] + ray_dir[2] * norm[2];
        if denom.abs() <= 1e-4 {
            return None;
        }

        let to_center = [
            center[0] - cam_pos[0],
            center[1] - cam_pos[1],
            center[2] - cam_pos[2],
        ];
        let numer = to_center[0] * norm[0] + to_center[1] * norm[1] + to_center[2] * norm[2];
        let t = numer / denom;
        if t <= 0.0 {
            return None;
        }

        let hit = [
            cam_pos[0] + t * ray_dir[0],
            cam_pos[1] + t * ray_dir[1],
            cam_pos[2] + t * ray_dir[2],
        ];

        let rel = [
            hit[0] - center[0],
            hit[1] - center[1],
            hit[2] - center[2],
        ];

        let scr_r = [f64::from(-scr_cos), 0.0, f64::from(-scr_sin)];
        let u = rel[0] * scr_r[0] + rel[1] * scr_r[1] + rel[2] * scr_r[2];
        let v = rel[1];

        Some((u, v))
    }

    pub fn find_crosshair_target(
        screen: &Screen,
        cam_pos: [f64; 3],
        ray_dir: [f64; 3],
        is_editing: bool,
        show_speakers: bool,
        audio_mode: AudioMode,
    ) -> Option<DragTarget> {
        let (w, h) = screen.dimensions(screen.applied_native_size);
        let hw = f64::from(w) * 0.5;
        let hh = f64::from(h) * 0.5;
        let center = [screen.at[0], screen.at[1] + hh, screen.at[2]];

        let mut candidates: Vec<(DragTarget, f64)> = Vec::new();

        // 1. Speakers
        if show_speakers || is_editing {
            let spk = SpeakerLayout::compute(screen.at, screen.yaw, w, h, &screen.custom_speakers);
            let speaker_list: &[(SpeakerId, [f64; 3])] = match audio_mode {
                AudioMode::Surround71 => &[
                    (SpeakerId::FL, spk.fl),
                    (SpeakerId::FR, spk.fr),
                    (SpeakerId::C, spk.c),
                    (SpeakerId::LFE, spk.lfe),
                    (SpeakerId::SL, spk.sl),
                    (SpeakerId::SR, spk.sr),
                    (SpeakerId::BL, spk.bl),
                    (SpeakerId::BR, spk.br),
                ],
                AudioMode::Surround51 => &[
                    (SpeakerId::FL, spk.fl),
                    (SpeakerId::FR, spk.fr),
                    (SpeakerId::C, spk.c),
                    (SpeakerId::LFE, spk.lfe),
                    (SpeakerId::SL, spk.sl),
                    (SpeakerId::SR, spk.sr),
                ],
                AudioMode::StereoSpeakers => &[
                    (SpeakerId::FL, spk.fl),
                    (SpeakerId::FR, spk.fr),
                ],
                AudioMode::Game3D => &[
                    (SpeakerId::C, spk.c),
                ],
                AudioMode::Direct | AudioMode::Mute => &[],
            };

            for &(id, pos) in speaker_list {
                let (perp, t) = Self::point_ray_dist(pos, cam_pos, ray_dir);
                if t > 0.1 && t < 60.0 {
                    let max_perp = (0.50 + t * 0.035).min(1.2);
                    if perp <= max_perp {
                        let score = perp / max_perp;
                        candidates.push((DragTarget::Speaker(id, t), score));
                    }
                }
            }
        }

        // 2. Handles & screen center (in edit mode)
        if is_editing {
            let (sin, cos) = screen.yaw.to_radians().sin_cos();
            let right = [f64::from(-cos), 0.0, f64::from(-sin)];
            let normal = [f64::from(sin), 0.0, f64::from(-cos)];
            let up = [0.0, 1.0, 0.0];

            let point_at = |x: f64, y: f64, z: f64| -> [f64; 3] {
                [
                    center[0] + right[0] * x + up[0] * y + normal[0] * z,
                    center[1] + right[1] * x + up[1] * y + normal[1] * z,
                    center[2] + right[2] * x + up[2] * y + normal[2] * z,
                ]
            };

            let (tl, tr, bl, br, edge_top, edge_bot, edge_left, edge_right) = if screen.curvature <= 0.5 {
                (
                    point_at(-hw, hh, 0.0),
                    point_at(hw, hh, 0.0),
                    point_at(-hw, -hh, 0.0),
                    point_at(hw, -hh, 0.0),
                    point_at(0.0, hh, 0.0),
                    point_at(0.0, -hh, 0.0),
                    point_at(-hw, 0.0, 0.0),
                    point_at(hw, 0.0, 0.0),
                )
            } else {
                let alpha = f64::from(screen.curvature).to_radians();
                let radius = f64::from(w) / alpha;
                let arc_pt = |f: f64, y: f64| -> [f64; 3] {
                    let theta = -alpha * 0.5 + alpha * f;
                    let x = radius * theta.sin();
                    let z = radius * (1.0 - theta.cos());
                    point_at(x, y, z)
                };
                (
                    arc_pt(0.0, hh),
                    arc_pt(1.0, hh),
                    arc_pt(0.0, -hh),
                    arc_pt(1.0, -hh),
                    arc_pt(0.5, hh),
                    arc_pt(0.5, -hh),
                    arc_pt(0.0, 0.0),
                    arc_pt(1.0, 0.0),
                )
            };

            let handle_list = [
                (DragHandle::CornerTL, tl),
                (DragHandle::CornerTR, tr),
                (DragHandle::CornerBL, bl),
                (DragHandle::CornerBR, br),
                (DragHandle::EdgeTop, edge_top),
                (DragHandle::EdgeBottom, edge_bot),
                (DragHandle::EdgeLeft, edge_left),
                (DragHandle::EdgeRight, edge_right),
            ];

            for (handle, pos) in handle_list {
                let (perp, t) = Self::point_ray_dist(pos, cam_pos, ray_dir);
                if t > 0.1 && t < 60.0 {
                    let max_perp = (0.45 + t * 0.035).min(1.1);
                    if perp <= max_perp {
                        let score = perp / max_perp;
                        candidates.push((DragTarget::Handle(handle), score));
                    }
                }
            }

            // Screen center
            let (perp, t) = Self::point_ray_dist(center, cam_pos, ray_dir);
            if t > 0.1 && t < 60.0 {
                let max_perp = (0.50 + t * 0.035).min(1.5);
                if perp <= max_perp {
                    let score = (perp / max_perp) + 0.15;
                    candidates.push((DragTarget::ScreenCenter { initial_dist: t }, score));
                }
            }
        }

        candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        candidates.first().map(|c| c.0)
    }

    fn apply_drag(
        screen: &mut Screen,
        handle: DragHandle,
        u: f32,
        v: f32,
        native_size: Option<(u32, u32)>,
        grid_snap: Option<f32>,
    ) {
        let (cur_w, cur_h) = screen.dimensions(native_size);
        match handle {
            DragHandle::CornerTL
            | DragHandle::CornerTR
            | DragHandle::CornerBL
            | DragHandle::CornerBR => {
                let half_diag = (u * u + v * v).sqrt();
                let mut new_diag = (2.0 * half_diag).clamp(0.5, 250.0);
                if let Some(step) = grid_snap {
                    new_diag = (new_diag / step).round() * step;
                }
                screen.diagonal = new_diag.clamp(0.5, 250.0);
                screen.recalculate(native_size);
            }
            DragHandle::EdgeLeft | DragHandle::EdgeRight => {
                let mut new_w = (2.0 * u.abs()).clamp(0.5, 250.0);
                if let Some(step) = grid_snap {
                    new_w = (new_w / step).round() * step;
                }
                let new_aspect = new_w / cur_h.max(0.1);
                screen.aspect = AspectRatio::Ratio(new_aspect);
                screen.diagonal = (new_w * new_w + cur_h * cur_h).sqrt().clamp(0.5, 250.0);
                screen.recalculate(native_size);
            }
            DragHandle::EdgeTop | DragHandle::EdgeBottom => {
                let mut new_h = (2.0 * v.abs()).clamp(0.5, 250.0);
                if let Some(step) = grid_snap {
                    new_h = (new_h / step).round() * step;
                }
                let new_aspect = cur_w / new_h.max(0.1);
                screen.aspect = AspectRatio::Ratio(new_aspect);
                screen.diagonal = (cur_w * cur_w + new_h * new_h).sqrt().clamp(0.5, 250.0);
                screen.recalculate(native_size);
            }
        }
    }
}

impl Hooks for VideoScreen {
    fn commands(&self, _: &mut dyn Host, event: &mut CommandsEvent<'_>) {
        event.commands.push("video");
    }

    fn command(&self, host: &mut dyn Host, event: &mut CommandEvent<'_>) -> Flow {
        let Some(rest) = event.command.strip_prefix("video") else {
            return Flow::Continue;
        };
        if !rest.is_empty() && !rest.starts_with(' ') {
            return Flow::Continue;
        }
        let rest = rest.trim();
        let (verb, argument) = rest.split_once(' ').unwrap_or((rest, ""));
        let argument = argument.trim().trim_matches('"');
        let mut state = self.state();
        ensure_screens(&*host, &mut state);

        let done = match verb {
            "create" => (|| -> Result<(), String> {
                let id = if argument.is_empty() {
                    format!("screen_{}", state.saved_screens.len() + 1)
                } else {
                    argument.to_string()
                };
                let camera = host.camera().ok_or("no world is shown yet")?;
                let mut screen = Screen::new(None, camera.position, camera.yaw);
                screen.recalculate(None);
                Self::send_chat_editor_prompt(host, &id, &screen, state.light_enabled, state.audio_mode, state.show_speakers);
                state.screen = Some(screen);
                state.editing = Some(EditState { id, active_handle: None });
                Ok(())
            })(),
            "edit" => {
                let id = if argument.is_empty() {
                    state.editing.as_ref().map(|e| e.id.clone()).unwrap_or_else(|| "main".into())
                } else {
                    argument.to_string()
                };
                if let Some(saved) = state.saved_screens.get(&id) {
                    let mut screen = Screen::from_saved(saved, state.screen.as_ref().and_then(|s| s.video));
                    screen.recalculate(None);
                    Self::send_chat_editor_prompt(host, &id, &screen, state.light_enabled, state.audio_mode, state.show_speakers);
                    state.screen = Some(screen);
                    state.editing = Some(EditState { id, active_handle: None });
                    Ok(())
                } else if let Some(screen) = state.screen.as_ref() {
                    Self::send_chat_editor_prompt(host, &id, screen, state.light_enabled, state.audio_mode, state.show_speakers);
                    state.editing = Some(EditState { id, active_handle: None });
                    Ok(())
                } else {
                    Err(format!("screen '{id}' not found: use /video create {id}"))
                }
            },
            "save" => (|| -> Result<(), String> {
                let id = if !argument.is_empty() {
                    argument.to_string()
                } else if let Some(e) = &state.editing {
                    e.id.clone()
                } else {
                    "main".to_string()
                };
                let screen = state.screen.as_mut().ok_or("no screen to save: use /video create <id>")?;
                let native_size = screen.video.and_then(|v| host.video_state(v)).and_then(|s| s.video_size);
                let (w, h) = screen.dimensions(native_size);
                let saved = SavedScreen {
                    id: id.clone(),
                    at: screen.at,
                    camera_pos: screen.camera_pos,
                    yaw: screen.yaw,
                    diagonal: screen.diagonal,
                    aspect: screen.aspect.as_string(),
                    distance: screen.distance,
                    width: w,
                    height: h,
                    curvature: screen.curvature,
                    speakers: screen.custom_speakers.clone(),
                    audio_mode: Some(state.audio_mode),
                };
                state.saved_screens.insert(id.clone(), saved);
                save_saved_screens(&state)?;
                state.editing = None;
                host.feedback(Text::colored(
                    format!("§a[Horizon Video] Экран '§e{}§a' сохранен! ({:.2}x{:.2} бл). Запуск: §b/video play {} <файл>", id, w, h, id),
                    0x55FF55,
                ));
                Ok(())
            })(),
            "snap" | "attach" => (|| -> Result<(), String> {
                let s = state.screen.as_mut().ok_or("no screen active: use /video create <id> or /video open <file>")?;
                let camera = host.camera().ok_or("no camera available")?;
                let hit = host.raycast(64.0).ok_or("no block in view within 64 blocks")?;
                let details = Self::attach_to_surface(s, &camera, &hit);
                host.feedback(Text::colored(
                    format!("§a[Horizon Video] Экран привязан к поверхности: §e{details}"),
                    0x55FF55,
                ));
                Ok(())
            })(),
            "curve" | "curvature" => (|| -> Result<(), String> {
                let s = state.screen.as_mut().ok_or("no screen active")?;
                if argument.is_empty() {
                    host.feedback(Text::colored(
                        format!("§a[Horizon Video] Текущее искривление экрана: §e{:.1}°{}", s.curvature, if s.curvature <= 0.1 { " (плоский)" } else { " (цилиндрический)" }),
                        0x55FF55,
                    ));
                    return Ok(());
                }
                let degrees: f32 = argument
                    .parse()
                    .map_err(|_| "usage: /video curve <degrees> (0 to 120)")?;
                s.curvature = degrees.clamp(0.0, 120.0);
                let native_size = s.video.and_then(|v| host.video_state(v)).and_then(|st| st.video_size);
                s.recalculate(native_size);
                host.feedback(Text::colored(
                    format!("§a[Horizon Video] Искривление экрана: §e{:.1}°{}", s.curvature, if s.curvature <= 0.1 { " (плоский)" } else { " (цилиндрический)" }),
                    0x55FF55,
                ));
                Ok(())
            })(),
            "grid" => (|| -> Result<(), String> {
                if argument.eq_ignore_ascii_case("off") {
                    state.grid_snap = None;
                    host.feedback(Text::colored("§7[Horizon Video] Привязка к сетке выключена.", 0xAAAAAA));
                } else {
                    let step: f32 = if argument.is_empty() {
                        0.5
                    } else {
                        argument.parse().map_err(|_| "usage: /video grid [step|off] (e.g. 0.5, 1.0, off)")?
                    };
                    state.grid_snap = Some(step.clamp(0.1, 5.0));
                    host.feedback(Text::colored(
                        format!("§a[Horizon Video] Привязка к сетке блоков: §e{:.2} бл.", step),
                        0x55FF55,
                    ));
                }
                Ok(())
            })(),
            "cancel" => {
                state.editing = None;
                host.feedback(Text::colored("§7[Horizon Video] Режим редактирования завершен.", 0xAAAAAA));
                Ok(())
            }
            "list" => {
                if state.saved_screens.is_empty() {
                    host.feedback(Text::colored("§7[Horizon Video] Нет сохраненных экранов. Создать: §b/video create <id>", 0xAAAAAA));
                } else {
                    host.feedback(Text::colored(format!("§6[Horizon Video] Сохраненные экраны ({}):", state.saved_screens.len()), 0xFFAA00));
                    for (id, s) in &state.saved_screens {
                        let curve_str = if s.curvature > 0.5 { format!(", curve {:.0}°", s.curvature) } else { "".into() };
                        let mut line = Text::literal(format!("  • §e{}§7: {:.2}x{:.2} бл (диаг {:.2}, {}{}) ", id, s.width, s.height, s.diagonal, s.aspect, curve_str));
                        line = line.append(Text::styled("[✏ Редакт] ", Style {
                            color: Some(0x55FFFF),
                            bold: Some(true),
                            click: Some(ClickEvent::RunCommand(format!("/video edit {id}"))),
                            ..Default::default()
                        }));
                        line = line.append(Text::styled("[🗑 Удалить]", Style {
                            color: Some(0xFF5555),
                            bold: Some(true),
                            click: Some(ClickEvent::RunCommand(format!("/video remove {id}"))),
                            ..Default::default()
                        }));
                        host.feedback(line);
                    }
                }
                Ok(())
            }
            "remove" | "delete" => (|| -> Result<(), String> {
                if argument.is_empty() {
                    return Err("usage: /video remove <id>".into());
                }
                if state.saved_screens.remove(argument).is_some() {
                    save_saved_screens(&state)?;
                    host.feedback(Text::colored(format!("§a[Horizon Video] Экран '{}' удален.", argument), 0x55FF55));
                    Ok(())
                } else {
                    Err(format!("screen '{argument}' not found"))
                }
            })(),
            "play" => (|| -> Result<(), String> {
                if let Some((id, path)) = argument.split_once(' ') {
                    let path = path.trim().trim_matches('"');
                    let id = id.trim();
                    if let Some(saved) = state.saved_screens.get(id).cloned() {
                        if let Some(mode) = saved.audio_mode {
                            state.audio_mode = mode;
                        }
                        return self.open_on_screen(host, &mut state, &saved, path);
                    } else {
                        return Err(format!("экран '{id}' не найден. Сохраните его командой /video save {id}"));
                    }
                }
                if let Some(screen) = state.screen.as_mut()
                    && let Some(v) = screen.video {
                        host.video_pause(v, false);
                        return Ok(());
                    }
                Err("usage: /video play <screen_id> <path> | /video play (unpause)".into())
            })(),
            "open" => self.open(host, &mut state, argument),
            "size" => (|| -> Result<(), String> {
                let s = state.screen.as_mut().ok_or("no screen active: use /video create <id> or /video open <file>")?;
                let diag: f32 = argument
                    .parse()
                    .map_err(|_| "usage: /video size <diagonal in blocks> (e.g. 5.0)")?;
                if !(0.5..=50.0).contains(&diag) {
                    return Err("diagonal must be between 0.5 and 50.0 blocks".into());
                }
                s.diagonal = diag;
                let native_size = s.video.and_then(|v| host.video_state(v)).and_then(|st| st.video_size);
                s.recalculate(native_size);
                Ok(())
            })(),
            "grow" => (|| -> Result<(), String> {
                let s = state.screen.as_mut().ok_or("no screen active")?;
                let step: f32 = if argument.is_empty() {
                    0.5
                } else {
                    argument
                        .parse()
                        .map_err(|_| "usage: /video grow [step in blocks]")?
                };
                s.diagonal = (s.diagonal + step).clamp(0.5, 50.0);
                let native_size = s.video.and_then(|v| host.video_state(v)).and_then(|st| st.video_size);
                s.recalculate(native_size);
                Ok(())
            })(),
            "shrink" => (|| -> Result<(), String> {
                let s = state.screen.as_mut().ok_or("no screen active")?;
                let step: f32 = if argument.is_empty() {
                    0.5
                } else {
                    argument
                        .parse()
                        .map_err(|_| "usage: /video shrink [step in blocks]")?
                };
                s.diagonal = (s.diagonal - step).clamp(0.5, 50.0);
                let native_size = s.video.and_then(|v| host.video_state(v)).and_then(|st| st.video_size);
                s.recalculate(native_size);
                Ok(())
            })(),
            "distance" => (|| -> Result<(), String> {
                let s = state.screen.as_mut().ok_or("no screen active")?;
                let dist: f64 = argument
                    .parse()
                    .map_err(|_| "usage: /video distance <blocks> (e.g. 5.0)")?;
                if !(1.0..=100.0).contains(&dist) {
                    return Err("distance must be between 1.0 and 100.0 blocks".into());
                }
                s.distance = dist;
                if let Some(camera) = host.camera() {
                    let (_, h) = s.dimensions(s.applied_native_size);
                    let (sin, cos) = camera.yaw.to_radians().sin_cos();
                    let [x, y, z] = camera.position;
                    s.yaw = camera.yaw;
                    s.camera_pos = camera.position;
                    s.at = [
                        x - f64::from(sin) * dist,
                        y - f64::from(h) / 2.0,
                        z + f64::from(cos) * dist,
                    ];
                }
                let native_size = s.video.and_then(|v| host.video_state(v)).and_then(|st| st.video_size);
                s.recalculate(native_size);
                Ok(())
            })(),
            "aspect" => (|| -> Result<(), String> {
                let s = state.screen.as_mut().ok_or("no screen active")?;
                if argument.is_empty() {
                    let native_size = s.video.and_then(|v| host.video_state(v)).and_then(|st| st.video_size);
                    let (w, h) = s.dimensions(native_size);
                    host.feedback(Text::colored(
                        format!("§a[Horizon Video] Текущие пропорции: §e{} ({:.2}x{:.2} бл)", s.aspect.as_string(), w, h),
                        0x55FF55,
                    ));
                    return Ok(());
                }
                let aspect = AspectRatio::parse(argument)
                    .ok_or("usage: /video aspect <16:9 | 4:3 | 21:9 | 1:1 | auto | W:H>")?;
                s.aspect = aspect;
                let native_size = s.video.and_then(|v| host.video_state(v)).and_then(|st| st.video_size);
                s.recalculate(native_size);
                Ok(())
            })(),
            "reset" => (|| -> Result<(), String> {
                let s = state.screen.as_mut().ok_or("no screen active")?;
                match argument.to_ascii_lowercase().as_str() {
                    "speakers" | "spk" | "audio" => {
                        s.custom_speakers.clear();
                        host.feedback(Text::colored(
                            "§6[Horizon Video] §aПозиции колонок сброшены к стандартной расстановке.",
                            0x55FF55,
                        ));
                    }
                    _ => {
                        s.diagonal = DEFAULT_DIAGONAL;
                        s.aspect = AspectRatio::Auto;
                        s.distance = DEFAULT_DISTANCE;
                        s.curvature = 0.0;
                        s.custom_speakers.clear();
                        let native_size = s.video.and_then(|v| host.video_state(v)).and_then(|st| st.video_size);
                        s.recalculate(native_size);
                        host.feedback(Text::colored(
                            "§6[Horizon Video] §aЭкран и колонки сброшены к начальным настройкам.",
                            0x55FF55,
                        ));
                    }
                }
                Ok(())
            })(),
            "pause" => (|| -> Result<(), String> {
                let s = state.screen.as_mut().ok_or("no video playing")?;
                let v = s.video.ok_or("no video playing")?;
                host.video_pause(v, true).then_some(()).ok_or("failed".into())
            })(),
            "play_resume" => (|| -> Result<(), String> {
                let s = state.screen.as_mut().ok_or("no video playing")?;
                let v = s.video.ok_or("no video playing")?;
                host.video_pause(v, false).then_some(()).ok_or("failed".into())
            })(),
            "seek" => (|| -> Result<(), String> {
                let s = state.screen.as_mut().ok_or("no video playing")?;
                let v = s.video.ok_or("no video playing")?;
                argument
                    .parse()
                    .ok()
                    .filter(|&seconds| {
                        let jumped = host.video_seek(v, seconds);
                        if jumped {
                            // What the mixer still holds is from before the jump.
                            host.clear_voices(MOD_ID, &voices(v));
                        }
                        jumped
                    })
                    .map(|_| ())
                    .ok_or("usage: /video seek <seconds>".into())
            })(),
            "volume" => (|| -> Result<(), String> {
                let s = state.screen.as_mut().ok_or("no video playing")?;
                let v = s.video.ok_or("no video playing")?;
                argument
                    .parse()
                    .ok()
                    .filter(|&volume| host.video_volume(v, volume))
                    .map(|_| ())
                    .ok_or("usage: /video volume <0-100>".into())
            })(),
            "close" => {
                if let Some(s) = state.screen.take()
                    && let Some(v) = s.video {
                        stop_video(host, v);
                    }
                state.editing = None;
                Ok(())
            }
            "light" | "lighting" => (|| -> Result<(), String> {
                if argument.is_empty() {
                    let status = if state.light_enabled { "включено" } else { "выключено" };
                    let pct = (state.light_brightness * 100.0).round() as u32;
                    host.feedback(Text::colored(
                        format!("§6[Horizon Video] §eДинамическое освещение: §b{} §7(яркость §f{}%§7). Используйте §f/video light on|off|<0-200>", status, pct),
                        0xFFAA00,
                    ));
                    return Ok(());
                }
                match argument.to_ascii_lowercase().as_str() {
                    "on" | "true" | "1" | "вкл" => {
                        state.light_enabled = true;
                        host.feedback(Text::colored("§6[Horizon Video] §aДинамическое освещение комнаты включено.", 0x55FF55));
                    }
                    "off" | "false" | "0" | "выкл" => {
                        state.light_enabled = false;
                        state.current_light_intensity = 0.0;
                        host.feedback(Text::colored("§6[Horizon Video] §cДинамическое освещение комнаты выключено.", 0xFF5555));
                    }
                    other => {
                        if let Ok(pct) = other.trim_end_matches('%').parse::<f32>() {
                            let b = (pct / 100.0).clamp(0.0, 2.0);
                            state.light_brightness = b;
                            state.light_enabled = b > 0.0;
                            host.feedback(Text::colored(
                                format!("§6[Horizon Video] §eЯркость динамического освещения: §b{:.0}%§e.", b * 100.0),
                                0xFFAA00,
                            ));
                        } else {
                            return Err("используйте /video light on, /video light off или /video light <0-200>".into());
                        }
                    }
                }
                Ok(())
            })(),
            "speakers" | "visuals" => {
                let show = match argument.to_ascii_lowercase().as_str() {
                    "on" | "1" | "true" | "вкл" => true,
                    "off" | "0" | "false" | "выкл" => false,
                    _ => !state.show_speakers,
                };
                state.show_speakers = show;
                let status_msg = if show {
                    "§aВКЛЮЧЕНА (в комнате отображаются виртуальные 3D-динамики и сабвуфер)."
                } else {
                    "§cВЫКЛЮЧЕНА."
                };
                host.feedback(Text::colored(
                    format!("§6[Horizon Video] §eВизуализация виртуальных колонок: {status_msg}"),
                    0xFFAA00,
                ));
                Ok(())
            },
            "audio" | "sound" => (|| -> Result<(), String> {
                if argument.is_empty() {
                    let mode_str = match state.audio_mode {
                        AudioMode::Game3D => "3D (центральный динамик в мире)",
                        AudioMode::StereoSpeakers => "Стерео (левая и правая 3D колонки)",
                        AudioMode::Surround51 => "5.1 Surround (6 виртуальных 3D колонок)",
                        AudioMode::Surround71 => "7.1 IMAX Surround (8 виртуальных 3D колонок)",
                        AudioMode::Direct => "Direct (стерео без 3D-затухания)",
                        AudioMode::Mute => "Без звука (Mute)",
                    };
                    host.feedback(Text::colored(
                        format!("§6[Horizon Video] §eРежим звука: §b{}§e. Используйте §f/video audio 3d | stereo | 5.1 | 7.1 | direct | mute", mode_str),
                        0xFFAA00,
                    ));
                    return Ok(());
                }
                match argument.to_ascii_lowercase().as_str() {
                    "3d" | "spatial" | "center" => {
                        state.audio_mode = AudioMode::Game3D;
                        host.feedback(Text::colored(
                            "§6[Horizon Video] §aВключен 3D-звук в микшере игры (позиционирование в центре экрана, затухание по расстоянию, приглушение за стенами).",
                            0x55FF55,
                        ));
                    }
                    "stereo" | "speakers" | "2ch" => {
                        state.audio_mode = AudioMode::StereoSpeakers;
                        host.feedback(Text::colored(
                            "§6[Horizon Video] §bВключены раздельные 3D-динамики (стерео-база по краям экрана с пространственным позиционированием).",
                            0x55FFFF,
                        ));
                    }
                    "5.1" | "surround" | "surround51" => {
                        state.audio_mode = AudioMode::Surround51;
                        host.feedback(Text::colored(
                            "§6[Horizon Video] §aВключен звук 5.1 Surround (6 виртуальных 3D-колонок: Левая, Правая, Центр, Сабвуфер, Тыловые).",
                            0x55FF55,
                        ));
                    }
                    "7.1" | "surround71" | "imax" => {
                        state.audio_mode = AudioMode::Surround71;
                        host.feedback(Text::colored(
                            "§6[Horizon Video] §dВключен звук 7.1 IMAX Surround (8 виртуальных 3D-колонок: Фронтальные, Центр, Сабвуфер, Боковые и Задние).",
                            0xFF55FF,
                        ));
                    }
                    "direct" | "flat" => {
                        state.audio_mode = AudioMode::Direct;
                        host.feedback(Text::colored(
                            "§6[Horizon Video] §eВключен прямой звук игры (без 3D-затухания по расстоянию, через общий микшер игры).",
                            0xFFFF55,
                        ));
                    }
                    "mute" | "off" | "0" | "выкл" => {
                        state.audio_mode = AudioMode::Mute;
                        host.feedback(Text::colored("§6[Horizon Video] §cЗвук видео выключен.", 0xFF5555));
                    }
                    _ => {
                        return Err("используйте /video audio 3d | stereo | 5.1 | 7.1 | direct | mute".into());
                    }
                }
                Ok(())
            })(),
            "info" => Ok(()),
            _ => Err("usage: /video create <id> | edit <id> | save [id] | play <id> <file> | snap wall | curve <deg> | grid [step|off] | light [on|off|0-200] | audio [3d|stereo|5.1|7.1|direct|mute] | speakers [on|off] | list | remove <id> | cancel | open <file> | size <diag> | grow | shrink | aspect <ratio> | distance <dist> | reset | pause | seek | volume | close | info".into()),
        };

        match (&done, &state.screen) {
            (Ok(()), Some(screen)) => match verb {
                "size" | "grow" | "shrink" | "aspect" => {
                    let native_size = screen.video.and_then(|v| host.video_state(v)).and_then(|st| st.video_size);
                    let (w, h) = screen.dimensions(native_size);
                    host.log(
                        "info",
                        &format!(
                            "video: resized to {:.2}x{:.2} blocks (diagonal {:.2})",
                            w, h, screen.diagonal
                        ),
                    );
                }
                "distance" => {
                    host.log(
                        "info",
                        &format!("video: distance set to {:.1} blocks", screen.distance),
                    );
                }
                "reset" => {
                    host.log(
                        "info",
                        "video: screen reset to default size and distance",
                    );
                }
                "info" => host.log("info", &Self::info(host, screen)),
                _ => {}
            },
            (Ok(()), None) => host.log("info", "video: closed"),
            (Err(why), _) => host.log("warn", &format!("video: {why}")),
        }
        Flow::Cancel
    }

    fn frame(&self, host: &mut dyn Host, _: &mut FrameEvent) {
        let audio = self.frame_locked(host, &mut self.state());
        if let Some(audio) = audio {
            audio.play(host);
        }
    }

    /// Once the world is left the screen goes with it; its video is closed
    /// and its sound dropped, the saved screens stay.
    fn client_level_unload(&self, host: &mut dyn Host, _: &mut ClientLevelUnloadEvent<'_>) {
        let mut state = self.state();
        if let Some(video) = state.screen.take().and_then(|s| s.video) {
            stop_video(host, video);
        }
        state.editing = None;
        state.hovered_target = None;
        state.active_drag = None;
        state.audio_at = None;
    }

    fn world_submit(&self, host: &dyn Host, event: &mut WorldSubmitEvent<'_>) {
        let state = self.state();
        let Some(screen) = &state.screen else { return };
        if let Some(video) = screen.video {
            event.draw.push();
            if event.draw.translate_world(screen.at) {
                if screen.curvature <= 0.5 {
                    host.draw_video(event.draw, video, screen.corners);
                } else {
                    let native_size = host.video_state(video).and_then(|s| s.video_size);
                    for (slice_corners, uv) in screen.slices(native_size) {
                        host.draw_video_part(event.draw, video, slice_corners, uv);
                    }
                }
            }
            event.draw.pop();
        }
    }

    fn world_primitives(&self, host: &dyn Host, event: &mut WorldPrimitivesEvent<'_>) {
        let state = self.state();
        let Some(screen) = &state.screen else { return };

        let native_size = screen.video.and_then(|v| host.video_state(v)).and_then(|s| s.video_size);
        let (w, h) = screen.dimensions(native_size);

        // Draw 3D Surround Speakers in world when editing or when speaker visualization is toggled on
        if state.editing.is_some() || state.show_speakers {
            let spk = SpeakerLayout::compute(screen.at, screen.yaw, w, h, &screen.custom_speakers);
            let is_spk_active = |id: SpeakerId| -> bool {
                matches!(state.active_drag, Some(DragTarget::Speaker(spk_id, _)) if spk_id == id)
            };
            let is_spk_hovered = |id: SpeakerId| -> bool {
                matches!(state.hovered_target, Some(DragTarget::Speaker(spk_id, _)) if spk_id == id)
            };
            let mut draw_spk = |id: SpeakerId, pos: [f64; 3], label: &str, color: u32, is_sub: bool| {
                let active = is_spk_active(id);
                let hovered = is_spk_hovered(id);
                let sz = if active {
                    if is_sub { 0.30 } else { 0.22 }
                } else if hovered {
                    if is_sub { 0.26 } else { 0.19 }
                } else {
                    if is_sub { 0.22 } else { 0.16 }
                };
                let (outline_col, outline_w) = if active {
                    (0xFF55FF55, 4.0)
                } else if hovered {
                    (0xFFFFFF00, 3.5)
                } else {
                    (color, 2.0)
                };

                let min = [pos[0] - sz, pos[1] - sz, pos[2] - sz];
                let max = [pos[0] + sz, pos[1] + sz, pos[2] + sz];
                event.primitives.box_outline(min, max, outline_col, outline_w).through_walls = true;
                if active || hovered {
                    event.primitives.filled_box(min, max, (outline_col & 0x00FFFFFF) | 0x35000000).through_walls = true;
                }
                event.primitives.line(screen.at, pos, (outline_col & 0x00FFFFFF) | 0x35000000, 1.0).through_walls = true;
                let text_pos = [pos[0], pos[1] + sz + 0.15, pos[2]];
                let text = if active {
                    format!("§a[Тяните {}] ({:.1}, {:.1}, {:.1})", id.name(), pos[0], pos[1], pos[2])
                } else if hovered {
                    format!("§e[Зажмите ПКМ: переместить {}]", id.name())
                } else {
                    label.to_string()
                };
                event.primitives.text(text_pos, &text, outline_col, 0.8).through_walls = true;
            };

            match state.audio_mode {
                AudioMode::Surround71 => {
                    draw_spk(SpeakerId::FL, spk.fl, "[FL] Левый фронт", 0xFF00FFCC, false);
                    draw_spk(SpeakerId::FR, spk.fr, "[FR] Правый фронт", 0xFF00FFCC, false);
                    draw_spk(SpeakerId::C, spk.c, "[C] Центр (Голос)", 0xFFFFDD33, false);
                    draw_spk(SpeakerId::LFE, spk.lfe, "[LFE] Сабвуфер", 0xFFFF6600, true);
                    draw_spk(SpeakerId::SL, spk.sl, "[SL] Боковой левый", 0xFF3399FF, false);
                    draw_spk(SpeakerId::SR, spk.sr, "[SR] Боковой правый", 0xFF3399FF, false);
                    draw_spk(SpeakerId::BL, spk.bl, "[BL] Задний левый", 0xFFCC33FF, false);
                    draw_spk(SpeakerId::BR, spk.br, "[BR] Задний правый", 0xFFCC33FF, false);
                }
                AudioMode::Surround51 => {
                    draw_spk(SpeakerId::FL, spk.fl, "[FL] Левый фронт", 0xFF00FFCC, false);
                    draw_spk(SpeakerId::FR, spk.fr, "[FR] Правый фронт", 0xFF00FFCC, false);
                    draw_spk(SpeakerId::C, spk.c, "[C] Центр (Голос)", 0xFFFFDD33, false);
                    draw_spk(SpeakerId::LFE, spk.lfe, "[LFE] Сабвуфер", 0xFFFF6600, true);
                    draw_spk(SpeakerId::SL, spk.sl, "[SL] Тыловой левый", 0xFF3399FF, false);
                    draw_spk(SpeakerId::SR, spk.sr, "[SR] Тыловой правый", 0xFF3399FF, false);
                }
                AudioMode::StereoSpeakers => {
                    draw_spk(SpeakerId::FL, spk.fl, "[L] Левая колонка", 0xFF00FFCC, false);
                    draw_spk(SpeakerId::FR, spk.fr, "[R] Правая колонка", 0xFF00FFCC, false);
                }
                AudioMode::Game3D => {
                    draw_spk(SpeakerId::C, spk.c, "[3D] Центр экрана", 0xFF55FF55, false);
                }
                _ => {}
            }
        }

        let Some(editing) = &state.editing else { return };
        let hw = f64::from(w) * 0.5;
        let hh = f64::from(h) * 0.5;

        let center = [
            screen.at[0],
            screen.at[1] + f64::from(h) * 0.5,
            screen.at[2],
        ];

        let (sin, cos) = screen.yaw.to_radians().sin_cos();
        let right = [f64::from(-cos), 0.0, f64::from(-sin)];
        let normal = [f64::from(sin), 0.0, f64::from(-cos)];
        let up = [0.0, 1.0, 0.0];

        let point_at = |x: f64, y: f64, z: f64| -> [f64; 3] {
            [
                center[0] + right[0] * x + up[0] * y + normal[0] * z,
                center[1] + right[1] * x + up[1] * y + normal[1] * z,
                center[2] + right[2] * x + up[2] * y + normal[2] * z,
            ]
        };

        let is_handle_active = |h: DragHandle| -> bool {
            matches!(state.active_drag, Some(DragTarget::Handle(handle)) if handle == h)
        };
        let is_handle_hovered = |h: DragHandle| -> bool {
            matches!(state.hovered_target, Some(DragTarget::Handle(handle)) if handle == h)
        };
        let has_any_active_screen_drag = matches!(state.active_drag, Some(DragTarget::Handle(_) | DragTarget::ScreenCenter { .. }));
        let wireframe_color = if has_any_active_screen_drag { 0xFF55FF55 } else { 0xFFFF2222 };

        if screen.curvature <= 0.5 {
            // Flat screen wireframe
            let tl = point_at(-hw, hh, 0.0);
            let tr = point_at(hw, hh, 0.0);
            let br = point_at(hw, -hh, 0.0);
            let bl = point_at(-hw, -hh, 0.0);

            // 1. Outer wireframe border
            event.primitives.line(tl, tr, wireframe_color, 3.0).through_walls = true;
            event.primitives.line(tr, br, wireframe_color, 3.0).through_walls = true;
            event.primitives.line(br, bl, wireframe_color, 3.0).through_walls = true;
            event.primitives.line(bl, tl, wireframe_color, 3.0).through_walls = true;

            // 2. Interior alignment grid (4x4 subdivisions)
            for i in 1..4 {
                let f = f64::from(i) / 4.0;
                let u = -hw + f64::from(w) * f;
                event.primitives.line(point_at(u, hh, 0.0), point_at(u, -hh, 0.0), (wireframe_color & 0x00FFFFFF) | 0x60000000, 1.5).through_walls = true;

                let v = -hh + f64::from(h) * f;
                event.primitives.line(point_at(-hw, v, 0.0), point_at(hw, v, 0.0), (wireframe_color & 0x00FFFFFF) | 0x60000000, 1.5).through_walls = true;
            }

            // 3. Corner handles
            let corner_s = (w.min(h) * 0.035).clamp(0.08, 0.35) as f64;
            for (handle, p) in [(DragHandle::CornerTL, tl), (DragHandle::CornerTR, tr), (DragHandle::CornerBL, bl), (DragHandle::CornerBR, br)] {
                let active = is_handle_active(handle);
                let hovered = is_handle_hovered(handle);
                let color = if active { 0xFF55FF55 } else if hovered { 0xFFFFFF00 } else { 0xFFFF1111 };
                let s = if active { corner_s * 1.4 } else if hovered { corner_s * 1.2 } else { corner_s };
                let min = [p[0] - s, p[1] - s, p[2] - s];
                let max = [p[0] + s, p[1] + s, p[2] + s];
                event.primitives.filled_box(min, max, color).through_walls = true;
                event.primitives.box_outline(min, max, 0xFFFFFFFF, 2.0).through_walls = true;
                if active {
                    event.primitives.text([p[0], p[1] + s + 0.2, p[2]], format!("§a[Тяните размер] {:.2}x{:.2} бл", w, h), 0xFF55FF55, 0.85).through_walls = true;
                } else if hovered {
                    event.primitives.text([p[0], p[1] + s + 0.2, p[2]], "§e[Зажмите ПКМ: изменить размер]", 0xFFFFFF00, 0.85).through_walls = true;
                }
            }

            // 4. Edge handles
            let edge_s = (w.min(h) * 0.025).clamp(0.06, 0.28) as f64;
            for (handle, p) in [
                (DragHandle::EdgeTop, point_at(0.0, hh, 0.0)),
                (DragHandle::EdgeBottom, point_at(0.0, -hh, 0.0)),
                (DragHandle::EdgeLeft, point_at(-hw, 0.0, 0.0)),
                (DragHandle::EdgeRight, point_at(hw, 0.0, 0.0)),
            ] {
                let active = is_handle_active(handle);
                let hovered = is_handle_hovered(handle);
                let color = if active { 0xFF55FF55 } else if hovered { 0xFFFFFF00 } else { 0xFFFF6622 };
                let s = if active { edge_s * 1.4 } else if hovered { edge_s * 1.2 } else { edge_s };
                let min = [p[0] - s, p[1] - s, p[2] - s];
                let max = [p[0] + s, p[1] + s, p[2] + s];
                event.primitives.filled_box(min, max, color).through_walls = true;
                event.primitives.box_outline(min, max, 0xFFFFFFFF, 1.5).through_walls = true;
                if active {
                    event.primitives.text([p[0], p[1] + s + 0.2, p[2]], format!("§a[Тяните пропорции] {:.2}x{:.2} бл", w, h), 0xFF55FF55, 0.85).through_walls = true;
                } else if hovered {
                    event.primitives.text([p[0], p[1] + s + 0.2, p[2]], "§e[Зажмите ПКМ: изменить пропорции]", 0xFFFFFF00, 0.85).through_walls = true;
                }
            }
        } else {
            // Curved cylindrical wireframe
            let alpha = f64::from(screen.curvature).to_radians();
            let radius = f64::from(w) / alpha;
            let n = CURVE_SLICES;

            let arc_pt = |f: f64, y: f64| -> [f64; 3] {
                let theta = -alpha * 0.5 + alpha * f;
                let x = radius * theta.sin();
                let z = radius * (1.0 - theta.cos());
                point_at(x, y, z)
            };

            // Draw curved top and bottom arcs
            for i in 0..n {
                let f0 = i as f64 / n as f64;
                let f1 = (i + 1) as f64 / n as f64;
                let top0 = arc_pt(f0, hh);
                let top1 = arc_pt(f1, hh);
                let bot0 = arc_pt(f0, -hh);
                let bot1 = arc_pt(f1, -hh);

                event.primitives.line(top0, top1, wireframe_color, 3.0).through_walls = true;
                event.primitives.line(bot0, bot1, wireframe_color, 3.0).through_walls = true;

                // Interior vertical ribs (every 6 slices)
                if i > 0 && i % (n / 4) == 0 {
                    event.primitives.line(top0, bot0, (wireframe_color & 0x00FFFFFF) | 0x60000000, 1.5).through_walls = true;
                }
            }

            // Left and right vertical borders
            let tl = arc_pt(0.0, hh);
            let bl = arc_pt(0.0, -hh);
            let tr = arc_pt(1.0, hh);
            let br = arc_pt(1.0, -hh);
            event.primitives.line(tl, bl, wireframe_color, 3.0).through_walls = true;
            event.primitives.line(tr, br, wireframe_color, 3.0).through_walls = true;

            // Curved corner handles
            let corner_s = (w.min(h) * 0.035).clamp(0.08, 0.35) as f64;
            for (handle, p) in [(DragHandle::CornerTL, tl), (DragHandle::CornerTR, tr), (DragHandle::CornerBL, bl), (DragHandle::CornerBR, br)] {
                let active = is_handle_active(handle);
                let hovered = is_handle_hovered(handle);
                let color = if active { 0xFF55FF55 } else if hovered { 0xFFFFFF00 } else { 0xFFFF1111 };
                let s = if active { corner_s * 1.4 } else if hovered { corner_s * 1.2 } else { corner_s };
                let min = [p[0] - s, p[1] - s, p[2] - s];
                let max = [p[0] + s, p[1] + s, p[2] + s];
                event.primitives.filled_box(min, max, color).through_walls = true;
                event.primitives.box_outline(min, max, 0xFFFFFFFF, 2.0).through_walls = true;
                if active {
                    event.primitives.text([p[0], p[1] + s + 0.2, p[2]], format!("§a[Тяните размер] {:.2}x{:.2} бл", w, h), 0xFF55FF55, 0.85).through_walls = true;
                } else if hovered {
                    event.primitives.text([p[0], p[1] + s + 0.2, p[2]], "§e[Зажмите ПКМ: изменить размер]", 0xFFFFFF00, 0.85).through_walls = true;
                }
            }

            // Edge handles
            let edge_s = (w.min(h) * 0.025).clamp(0.06, 0.28) as f64;
            for (handle, p) in [
                (DragHandle::EdgeTop, arc_pt(0.5, hh)),
                (DragHandle::EdgeBottom, arc_pt(0.5, -hh)),
                (DragHandle::EdgeLeft, arc_pt(0.0, 0.0)),
                (DragHandle::EdgeRight, arc_pt(1.0, 0.0)),
            ] {
                let active = is_handle_active(handle);
                let hovered = is_handle_hovered(handle);
                let color = if active { 0xFF55FF55 } else if hovered { 0xFFFFFF00 } else { 0xFFFF6622 };
                let s = if active { edge_s * 1.4 } else if hovered { edge_s * 1.2 } else { edge_s };
                let min = [p[0] - s, p[1] - s, p[2] - s];
                let max = [p[0] + s, p[1] + s, p[2] + s];
                event.primitives.filled_box(min, max, color).through_walls = true;
                event.primitives.box_outline(min, max, 0xFFFFFFFF, 1.5).through_walls = true;
                if active {
                    event.primitives.text([p[0], p[1] + s + 0.2, p[2]], format!("§a[Тяните пропорции] {:.2}x{:.2} бл", w, h), 0xFF55FF55, 0.85).through_walls = true;
                } else if hovered {
                    event.primitives.text([p[0], p[1] + s + 0.2, p[2]], "§e[Зажмите ПКМ: изменить пропорции]", 0xFFFFFF00, 0.85).through_walls = true;
                }
            }
        }

        // 5. Screen center handle
        let center_active = matches!(state.active_drag, Some(DragTarget::ScreenCenter { .. }));
        let center_hovered = matches!(state.hovered_target, Some(DragTarget::ScreenCenter { .. }));
        let cs = if center_active { 0.16 } else if center_hovered { 0.12 } else { 0.08 };
        let ccol = if center_active { 0xFF55FF55 } else if center_hovered { 0xFFFFFF00 } else { 0x80FFFFFF };
        let min = [center[0] - cs, center[1] - cs, center[2] - cs];
        let max = [center[0] + cs, center[1] + cs, center[2] + cs];
        event.primitives.filled_box(min, max, ccol).through_walls = true;
        event.primitives.box_outline(min, max, 0xFFFFFFFF, 1.5).through_walls = true;
        if center_active {
            event.primitives.text([center[0], center[1] + 0.35, center[2]], "§a[Тяните: перемещение экрана]", 0xFF55FF55, 0.85).through_walls = true;
        } else if center_hovered {
            event.primitives.text([center[0], center[1] + 0.35, center[2]], "§e[Зажмите ПКМ: переместить экран]", 0xFFFFFF00, 0.85).through_walls = true;
        }

        // Overhead floating label
        let label_pos = point_at(0.0, hh + 0.35, 0.0);
        let id_str = &editing.id;
        let aspect_str = screen.aspect.as_string();
        let curve_label = if screen.curvature > 0.5 { format!(" | изгиб {:.0}°", screen.curvature) } else { "".into() };
        event.primitives.push(PrimitiveShape::Text {
            at: label_pos,
            text: format!("[{}: {:.2}x{:.2} бл | {}{}]", id_str, w, h, aspect_str, curve_label),
            color: 0xFFFFFFFF,
            scale: 1.0,
            scale_with_distance: false,
            background: true,
        }).through_walls = true;

        let hint_pos = point_at(0.0, hh + 0.70, 0.0);
        event.primitives.push(PrimitiveShape::Text {
            at: hint_pos,
            text: "§e[Наведите прицел на угол или колонку и зажмите ПКМ, чтобы двигать в 3D]".into(),
            color: 0xFFFFFF55,
            scale: 0.85,
            scale_with_distance: false,
            background: true,
        }).through_walls = true;
    }

    fn visuals(&self, host: &dyn Host, event: &mut VisualsEvent<'_>) {
        let mut state = self.state();
        if !state.light_enabled || state.light_brightness <= 0.001 {
            return;
        }
        let Some(screen) = state.screen.as_ref() else {
            return;
        };
        let Some(video) = screen.video else {
            return;
        };
        let screen_at = screen.at;
        let screen_yaw = screen.yaw;
        let screen_diagonal = screen.diagonal;
        let screen_curvature = screen.curvature;
        let (width, height) = screen.dimensions(screen.applied_native_size);

        let Some(color) = host.video_color(video) else {
            return;
        };
        let zone_colors = host.video_colors(video);

        let r = color[0] as f32;
        let g = color[1] as f32;
        let b = color[2] as f32;
        let max_val = r.max(g).max(b);

        if max_val < 3.0 {
            // Dark scene: smoothly decay ambient light to dark
            state.current_light_intensity *= 0.75;
            if state.current_light_intensity < 0.01 {
                state.current_light_intensity = 0.0;
                return;
            }
        } else {
            // Perceptual color tuning: slight saturation boost to prevent grayish washout
            let mean = (r + g + b) / 3.0;
            let sat_boost = 1.35;
            let br = (mean + (r - mean) * sat_boost).clamp(0.0, 255.0);
            let bg = (mean + (g - mean) * sat_boost).clamp(0.0, 255.0);
            let bb = (mean + (b - mean) * sat_boost).clamp(0.0, 255.0);
            let top = br.max(bg).max(bb).max(1.0);
            let target_rgb = [br / top * 255.0, bg / top * 255.0, bb / top * 255.0];

            // Perceptual intensity with gamma response (lum^0.5)
            let lum = (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0;
            let target_intensity = (lum.sqrt() * (top / 255.0) * state.light_brightness).clamp(0.0, 1.0);

            // Responsive exponential smoothing
            let diff = (target_rgb[0] - state.current_light_rgb[0]).abs()
                .max((target_rgb[1] - state.current_light_rgb[1]).abs())
                .max((target_rgb[2] - state.current_light_rgb[2]).abs());
            let int_diff = (target_intensity - state.current_light_intensity).abs();

            let alpha = if diff > 70.0 || int_diff > 0.35 {
                0.85 // rapid reaction for explosions, lightning, cuts
            } else {
                0.35 // smooth transitions for continuous movie scenes
            };

            state.current_light_rgb[0] += (target_rgb[0] - state.current_light_rgb[0]) * alpha;
            state.current_light_rgb[1] += (target_rgb[1] - state.current_light_rgb[1]) * alpha;
            state.current_light_rgb[2] += (target_rgb[2] - state.current_light_rgb[2]) * alpha;
            state.current_light_intensity += (target_intensity - state.current_light_intensity) * alpha;
        }

        let intensity = state.current_light_intensity;
        if intensity < 0.01 {
            return;
        }

        let (sin, cos) = screen_yaw.to_radians().sin_cos();
        let right = [-cos as f64, -sin as f64];
        let normal = [sin as f64, -cos as f64]; // Normal facing towards viewers/room

        let forward_offset = (0.8_f64).max(width as f64 * 0.03).min(2.0_f64);
        let pack_rgb = |cr: f32, cg: f32, cb: f32| -> u32 {
            let r = (cr.round() as u32).min(255);
            let g = (cg.round() as u32).min(255);
            let b = (cb.round() as u32).min(255);
            (r << 16) | (g << 8) | b
        };

        let tune_color = |cr: f32, cg: f32, cb: f32| -> u32 {
            let m = (cr + cg + cb) / 3.0;
            let sat_boost = 1.35;
            let br = (m + (cr - m) * sat_boost).clamp(0.0, 255.0);
            let bg = (m + (cg - m) * sat_boost).clamp(0.0, 255.0);
            let bb = (m + (cb - m) * sat_boost).clamp(0.0, 255.0);
            let top = br.max(bg).max(bb).max(1.0);
            pack_rgb(br / top * 255.0, bg / top * 255.0, bb / top * 255.0)
        };

        // Screen area light simulation: distribute lights across the screen surface
        // to produce a smooth, continuous, uniform ambient wash without localized hotspots.
        let is_tall = height >= 3.0;
        let n_cols: usize = if is_tall { 6 } else { 7 };
        let rows: Vec<(f64, f32)> = if is_tall {
            // Lower row near floor and upper row for ceiling/room depth
            vec![
                ((height as f64 * 0.10).max(0.4), 1.15),
                (height as f64 * 0.60, 0.85),
            ]
        } else {
            vec![(height as f64 * 0.45, 1.0)]
        };

        let light_radius = (screen_diagonal * 1.5).max(width * 1.2).clamp(24.0, 64.0);

        let norm_factor = if is_tall { 2.4 } else { 1.8 };
        let base_light_intensity = (intensity / norm_factor).clamp(0.01, 1.0);

        let alpha = screen_curvature.to_radians() as f64;
        let radius_arc = if screen_curvature > 0.5 && alpha > 0.001 {
            (width as f64) / alpha
        } else {
            0.0
        };

        for (y_offset, row_weight) in rows {
            for col in 0..n_cols {
                let t = -0.92 + 1.84 * (col as f32 / (n_cols - 1) as f32);
                let edge_boost = if col == 0 || col == n_cols - 1 { 1.25 } else { 1.0 };
                let col_intensity = (base_light_intensity * row_weight * edge_boost).clamp(0.0, 1.0);

                // Compute exact 3D position along the flat or curved screen
                let (pos_x, pos_z, norm_x, norm_z) = if radius_arc > 0.0 {
                    let phi = (t as f64) * (alpha * 0.5);
                    let arc_x = right[0] * (radius_arc * phi.sin()) + normal[0] * (radius_arc * (1.0 - phi.cos()));
                    let arc_z = right[1] * (radius_arc * phi.sin()) + normal[1] * (radius_arc * (1.0 - phi.cos()));
                    let n_x = normal[0] * phi.cos() - right[0] * phi.sin();
                    let n_z = normal[1] * phi.cos() - right[1] * phi.sin();
                    (screen_at[0] + arc_x, screen_at[2] + arc_z, n_x, n_z)
                } else {
                    let half_w = (width as f64) * 0.5;
                    (
                        screen_at[0] + right[0] * (t as f64 * half_w),
                        screen_at[2] + right[1] * (t as f64 * half_w),
                        normal[0],
                        normal[1],
                    )
                };

                let light_pos = [
                    pos_x + norm_x * forward_offset,
                    screen_at[1] + y_offset,
                    pos_z + norm_z * forward_offset,
                ];

                // Smooth color interpolation across screen zones
                let light_rgb = if let Some(zones) = zone_colors {
                    let (c1, c2, factor) = if t <= -0.33 {
                        let f = ((t + 1.0) / 0.67).clamp(0.0, 1.0);
                        (zones[0], zones[0], f)
                    } else if t < 0.0 {
                        let f = ((t + 0.33) / 0.33).clamp(0.0, 1.0);
                        (zones[0], zones[1], f)
                    } else if t <= 0.33 {
                        let f = (t / 0.33).clamp(0.0, 1.0);
                        (zones[1], zones[2], f)
                    } else {
                        (zones[2], zones[2], 1.0)
                    };
                    let r = c1[0] as f32 * (1.0 - factor) + c2[0] as f32 * factor;
                    let g = c1[1] as f32 * (1.0 - factor) + c2[1] as f32 * factor;
                    let b = c1[2] as f32 * (1.0 - factor) + c2[2] as f32 * factor;
                    tune_color(r, g, b)
                } else {
                    pack_rgb(
                        state.current_light_rgb[0],
                        state.current_light_rgb[1],
                        state.current_light_rgb[2],
                    )
                };

                event.lights.push(NativeColoredLight {
                    at: light_pos,
                    rgb: light_rgb,
                    radius: light_radius,
                    intensity: col_intensity,
                });
            }
        }
    }

    fn screen_render_post(&self, _host: &dyn Host, event: &mut ScreenRenderPostEvent<'_>) {
        let mut state = self.state();
        state.screen_size = [event.screen.rect[2], event.screen.rect[3]];
        state.chat_open = event.screen.id == "chat";
    }

    fn screen_mouse_button_pressed_pre(
        &self,
        host: &dyn Host,
        event: &mut ScreenMouseButtonPressedPreEvent<'_>,
    ) -> Flow {
        if event.button != 0 {
            return Flow::Continue;
        }
        let mut state = self.state();
        let Some(screen) = &state.screen else {
            return Flow::Continue;
        };
        let Some(camera) = host.camera() else {
            return Flow::Continue;
        };
        let gui_size = [event.screen.rect[2], event.screen.rect[3]];

        if let Some((handle, _)) = Self::find_hit_handle(screen, &camera, event.screen.mouse, gui_size)
            && let Some(editing) = state.editing.as_mut() {
                editing.active_handle = Some(handle);
                return Flow::Cancel;
            }
        Flow::Continue
    }

    fn screen_mouse_dragged_pre_v2(
        &self,
        host: &mut dyn Host,
        event: &mut ScreenMouseDraggedPreV2Event<'_>,
    ) -> Flow {
        let mut state = self.state();
        let Some(editing) = &state.editing else {
            return Flow::Continue;
        };
        let Some(active_handle) = editing.active_handle else {
            return Flow::Continue;
        };
        let gui_size = state.screen_size;
        let grid_snap = state.grid_snap;
        let Some(screen) = state.screen.as_mut() else {
            return Flow::Continue;
        };
        let Some(camera) = host.camera() else {
            return Flow::Continue;
        };

        if let Some((u, v)) = Self::raycast_screen_plane(screen, &camera, event.mouse, gui_size) {
            let native_size = screen.video.and_then(|v| host.video_state(v)).and_then(|s| s.video_size);
            Self::apply_drag(screen, active_handle, u, v, native_size, grid_snap);
            return Flow::Cancel;
        }
        Flow::Continue
    }

    fn screen_mouse_button_released_pre(
        &self,
        _host: &dyn Host,
        event: &mut ScreenMouseButtonReleasedPreEvent<'_>,
    ) -> Flow {
        if event.button != 0 {
            return Flow::Continue;
        }
        let mut state = self.state();
        if let Some(editing) = state.editing.as_mut()
            && editing.active_handle.take().is_some() {
                return Flow::Cancel;
            }
        Flow::Continue
    }
}

#[unsafe(no_mangle)]
pub fn hooks() -> Box<dyn Hooks> {
    let state = State {
        demo: std::env::var("HORIZON_VIDEO_DEMO").ok(),
        ..State::default()
    };
    Box::new(VideoScreen(Mutex::new(state)))
}
