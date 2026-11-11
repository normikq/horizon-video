//! Horizon Video Screen: in-world video playback via libmpv.
//!
//! Features:
//! - `/video create <id>`: creates a new screen with the specified ID in front of you and opens the interactive editor;
//! - `/video edit <id>`: opens the interactive editor for an existing saved screen;
//! - `/video save [id]`: saves current screen position and dimensions to video_screens.json;
//! - `/video list`: lists all saved screens with clickable buttons;
//! - `/video play <id> <file>`: plays a video on the specified saved screen;
//! - `/video remove <id>`: deletes a saved screen configuration;
//! - `/video cancel`: closes the interactive editor;
//! - Interactive editor: red wireframe grid in the 3D world with 4 corner handles and 4 border handles;
//!   drag corners via open chat to resize while preserving aspect ratio; drag borders to adjust width/height freely;
//! - Clickable chat buttons: [💾 Сохранить], [16:9], [4:3], [21:9], [1:1], [Auto], [❌ Закрыть];
//! - Sizing and aspect commands: `/video size <diag>`, `/video grow [step]`, `/video shrink [step]`,
//!   `/video aspect <ratio>`, `/video distance <dist>`, `/video reset`;
//! - Playback commands: `/video open <path>`, `/video pause`, `/video play`, `/video seek <sec>`,
//!   `/video volume <0-100>`, `/video close`, `/video info`.

use horizon_client::mod_api::{
    ClickEvent, CommandEvent, CommandsEvent, Flow, FrameEvent, Hooks, Host, NativeCamera,
    PrimitiveShape, ScreenMouseButtonPressedPreEvent, ScreenMouseButtonReleasedPreEvent,
    ScreenMouseDraggedPreV2Event, ScreenRenderPostEvent, Style, Text, WorldPrimitivesEvent,
    WorldSubmitEvent, serde_json,
};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const SCREENS_FILE: &str = "video_screens.json";

/// The default screen diagonal in blocks (~4.589 blocks corresponds to 4.0 x 2.25 at 16:9).
const DEFAULT_DIAGONAL: f32 = 4.58939;
/// Default aspect ratio (16:9).
const DEFAULT_ASPECT: f32 = 16.0 / 9.0;
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
        } else if let Ok(val) = s.parse::<f32>() {
            if val > 0.0 {
                return Some(Self::Ratio(val));
            }
        }
        None
    }

    pub fn value(&self, native_size: Option<(u32, u32)>) -> f32 {
        match self {
            Self::Auto => {
                if let Some((w, h)) = native_size {
                    if h > 0 {
                        return w as f32 / h as f32;
                    }
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

pub struct EditState {
    pub id: String,
    pub active_handle: Option<DragHandle>,
}

pub struct Screen {
    video: Option<u32>,
    /// Player camera position when placed or updated.
    camera_pos: [f64; 3],
    yaw: f32,
    diagonal: f32,
    aspect: AspectRatio,
    distance: f64,
    /// The last detected video native size applied to corners.
    applied_native_size: Option<(u32, u32)>,
    /// The middle of the screen's bottom edge, in the world.
    at: [f64; 3],
    /// The quad's corners from `at`: top left, bottom left, bottom right,
    /// top right as the player who opened it sees them.
    corners: [[f32; 3]; 4],
}

impl Screen {
    pub fn new(video: Option<u32>, camera_pos: [f64; 3], yaw: f32) -> Self {
        let mut screen = Self {
            video,
            camera_pos,
            yaw,
            diagonal: DEFAULT_DIAGONAL,
            aspect: AspectRatio::Auto,
            distance: DEFAULT_DISTANCE,
            applied_native_size: None,
            at: [0.0; 3],
            corners: [[0.0; 3]; 4],
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
            applied_native_size: None,
            at: saved.at,
            corners: [[0.0; 3]; 4],
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
        let [x, y, z] = self.camera_pos;

        self.at = [
            x - f64::from(sin) * self.distance,
            y - f64::from(height) / 2.0,
            z + f64::from(cos) * self.distance,
        ];

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
}

#[derive(Default)]
struct State {
    screen: Option<Screen>,
    editing: Option<EditState>,
    saved_screens: HashMap<String, SavedScreen>,
    demo: Option<String>,
    reported: Option<Instant>,
    screen_size: [f32; 2],
    chat_open: bool,
}

fn load_saved_screens() -> HashMap<String, SavedScreen> {
    if let Ok(content) = std::fs::read_to_string(SCREENS_FILE) {
        let content = content.trim_start_matches('\u{feff}');
        serde_json::from_str(content).unwrap_or_default()
    } else {
        HashMap::new()
    }
}

fn save_saved_screens(screens: &HashMap<String, SavedScreen>) -> Result<(), String> {
    let json = serde_json::to_string_pretty(screens)
        .map_err(|e| format!("failed to serialize screens: {e}"))?;
    std::fs::write(SCREENS_FILE, json)
        .map_err(|e| format!("failed to write {SCREENS_FILE}: {e}"))?;
    Ok(())
}

pub struct VideoScreen(Mutex<State>);

impl Default for VideoScreen {
    fn default() -> Self {
        Self(Mutex::new(State {
            saved_screens: load_saved_screens(),
            demo: std::env::var("HORIZON_VIDEO_DEMO").ok(),
            ..State::default()
        }))
    }
}

impl VideoScreen {
    fn open(&self, host: &mut dyn Host, state: &mut State, path: &str) -> Result<(), String> {
        let camera = host.camera().ok_or("no world is shown yet")?;
        let video = host.video_open(path, 1280, 720, true)?;
        if let Some(old) = state.screen.take() {
            if let Some(v) = old.video {
                host.video_close(v);
            }
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
        if let Some(old) = state.screen.take() {
            if let Some(v) = old.video {
                host.video_close(v);
            }
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
        let size_str = format!("{:.2}x{:.2} blocks (diag {:.2}, aspect {})", w, h, screen.diagonal, aspect_mode);
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

    fn send_chat_editor_prompt(host: &mut dyn Host, id: &str, screen: &Screen) {
        let (w, h) = screen.dimensions(screen.applied_native_size);

        host.feedback(Text::colored(
            format!("§6[Horizon Video] §eРежим настройки экрана: §b{id} §7({:.2}x{:.2} бл)", w, h),
            0xFFAA00,
        ));
        host.feedback(Text::colored(
            "§7Тяните курсором за §cуглы §7(сохраняет пропорции) или §cрамки §7(свободный размер).",
            0xAAAAAA,
        ));

        let mut row = Text::literal("  ");
        row = row.append(Text::styled("[💾 Сохранить] ", Style {
            color: Some(0x55FF55),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand(format!("/video save {id}"))),
            ..Default::default()
        }));
        row = row.append(Text::styled("[16:9] ", Style {
            color: Some(0x55FFFF),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand("/video aspect 16:9".into())),
            ..Default::default()
        }));
        row = row.append(Text::styled("[4:3] ", Style {
            color: Some(0x55FFFF),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand("/video aspect 4:3".into())),
            ..Default::default()
        }));
        row = row.append(Text::styled("[21:9] ", Style {
            color: Some(0x55FFFF),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand("/video aspect 21:9".into())),
            ..Default::default()
        }));
        row = row.append(Text::styled("[1:1] ", Style {
            color: Some(0x55FFFF),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand("/video aspect 1:1".into())),
            ..Default::default()
        }));
        row = row.append(Text::styled("[Auto] ", Style {
            color: Some(0xFFFF55),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand("/video aspect auto".into())),
            ..Default::default()
        }));
        row = row.append(Text::styled("[❌ Закрыть]", Style {
            color: Some(0xFF5555),
            bold: Some(true),
            click: Some(ClickEvent::RunCommand("/video cancel".into())),
            ..Default::default()
        }));
        host.feedback(row);
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

        // Screen Right vector: [-scr_cos, 0, -scr_sin]
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

    fn apply_drag(
        screen: &mut Screen,
        handle: DragHandle,
        u: f32,
        v: f32,
        native_size: Option<(u32, u32)>,
    ) {
        let (cur_w, cur_h) = screen.dimensions(native_size);
        match handle {
            DragHandle::CornerTL
            | DragHandle::CornerTR
            | DragHandle::CornerBL
            | DragHandle::CornerBR => {
                // Preserves aspect ratio: scale diagonal based on distance from center
                let half_diag = (u * u + v * v).sqrt();
                let new_diag = (2.0 * half_diag).clamp(0.5, 50.0);
                screen.diagonal = new_diag;
                screen.recalculate(native_size);
            }
            DragHandle::EdgeLeft | DragHandle::EdgeRight => {
                // Free width adjustment
                let new_w = (2.0 * u.abs()).clamp(0.5, 50.0);
                let new_aspect = new_w / cur_h.max(0.1);
                screen.aspect = AspectRatio::Ratio(new_aspect);
                screen.diagonal = (new_w * new_w + cur_h * cur_h).sqrt().clamp(0.5, 50.0);
                screen.recalculate(native_size);
            }
            DragHandle::EdgeTop | DragHandle::EdgeBottom => {
                // Free height adjustment
                let new_h = (2.0 * v.abs()).clamp(0.5, 50.0);
                let new_aspect = cur_w / new_h.max(0.1);
                screen.aspect = AspectRatio::Ratio(new_aspect);
                screen.diagonal = (cur_w * cur_w + new_h * new_h).sqrt().clamp(0.5, 50.0);
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
        let mut state = self.0.lock().unwrap();

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
                Self::send_chat_editor_prompt(host, &id, &screen);
                state.screen = Some(screen);
                state.editing = Some(EditState { id, active_handle: None });
                Ok(())
            })(),
            "edit" => (|| -> Result<(), String> {
                let id = if argument.is_empty() {
                    state.editing.as_ref().map(|e| e.id.clone()).unwrap_or_else(|| "main".into())
                } else {
                    argument.to_string()
                };
                if let Some(saved) = state.saved_screens.get(&id) {
                    let mut screen = Screen::from_saved(saved, state.screen.as_ref().and_then(|s| s.video));
                    screen.recalculate(None);
                    Self::send_chat_editor_prompt(host, &id, &screen);
                    state.screen = Some(screen);
                    state.editing = Some(EditState { id, active_handle: None });
                    Ok(())
                } else if let Some(screen) = state.screen.as_ref() {
                    Self::send_chat_editor_prompt(host, &id, screen);
                    state.editing = Some(EditState { id, active_handle: None });
                    Ok(())
                } else {
                    Err(format!("screen '{id}' not found: use /video create {id}"))
                }
            })(),
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
                };
                state.saved_screens.insert(id.clone(), saved);
                save_saved_screens(&state.saved_screens)?;
                state.editing = None;
                host.feedback(Text::colored(
                    format!("§a[Horizon Video] Экран '§e{}§a' сохранен! ({:.2}x{:.2} бл). Запуск: §b/video play {} <файл>", id, w, h, id),
                    0x55FF55,
                ));
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
                        let mut line = Text::literal(format!("  • §e{}§7: {:.2}x{:.2} бл (диаг {:.2}, {}) ", id, s.width, s.height, s.diagonal, s.aspect));
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
                    save_saved_screens(&state.saved_screens)?;
                    host.feedback(Text::colored(format!("§a[Horizon Video] Экран '{}' удален.", argument), 0x55FF55));
                    Ok(())
                } else {
                    Err(format!("screen '{argument}' not found"))
                }
            })(),
            "play" => (|| -> Result<(), String> {
                if let Some((id, path)) = argument.split_once(' ') {
                    let path = path.trim().trim_matches('"');
                    if let Some(saved) = state.saved_screens.get(id).cloned() {
                        return self.open_on_screen(host, &mut state, &saved, path);
                    }
                }
                // Fallback: if a screen is already open, unpause
                if let Some(screen) = state.screen.as_mut() {
                    if let Some(v) = screen.video {
                        host.video_pause(v, false);
                        return Ok(());
                    }
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
                let native_size = s.video.and_then(|v| host.video_state(v)).and_then(|st| st.video_size);
                s.recalculate(native_size);
                Ok(())
            })(),
            "aspect" => (|| -> Result<(), String> {
                let s = state.screen.as_mut().ok_or("no screen active")?;
                let aspect = AspectRatio::parse(argument)
                    .ok_or_else(|| "usage: /video aspect <16:9 | 4:3 | 21:9 | 1:1 | auto | W:H>")?;
                s.aspect = aspect;
                let native_size = s.video.and_then(|v| host.video_state(v)).and_then(|st| st.video_size);
                s.recalculate(native_size);
                Ok(())
            })(),
            "reset" => (|| -> Result<(), String> {
                let s = state.screen.as_mut().ok_or("no screen active")?;
                s.diagonal = DEFAULT_DIAGONAL;
                s.aspect = AspectRatio::Auto;
                s.distance = DEFAULT_DISTANCE;
                let native_size = s.video.and_then(|v| host.video_state(v)).and_then(|st| st.video_size);
                s.recalculate(native_size);
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
                    .filter(|&seconds| host.video_seek(v, seconds))
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
                if let Some(s) = state.screen.take() {
                    if let Some(v) = s.video {
                        host.video_close(v);
                    }
                }
                state.editing = None;
                Ok(())
            }
            "info" => Ok(()),
            _ => Err("usage: /video create <id> | edit <id> | save [id] | play <id> <file> | list | remove <id> | cancel | open <file> | size <diag> | grow | shrink | aspect <ratio> | distance <dist> | reset | pause | seek | volume | close | info".into()),
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
        let mut state = self.0.lock().unwrap();
        if let Some(path) = state.demo.take() {
            match self.open(host, &mut state, &path) {
                Ok(()) => host.log("info", &format!("video: playing {path}")),
                Err(_) if host.camera().is_none() => state.demo = Some(path),
                Err(why) => host.log("warn", &format!("video: {why}")),
            }
        }

        // Automatic aspect ratio detection from video stream metadata
        if let Some(screen) = state.screen.as_mut() {
            if screen.aspect == AspectRatio::Auto {
                if let Some(video) = screen.video {
                    if let Some(native_size) = host.video_state(video).and_then(|s| s.video_size) {
                        if Some(native_size) != screen.applied_native_size {
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
                    }
                }
            }
        }

        let due = state
            .reported
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(5));
        if let (true, Some(screen)) = (due, &state.screen) {
            host.log("info", &Self::info(host, screen));
            state.reported = Some(Instant::now());
        }
    }

    fn world_submit(&self, host: &dyn Host, event: &mut WorldSubmitEvent<'_>) {
        let state = self.0.lock().unwrap();
        let Some(screen) = &state.screen else { return };
        if let Some(video) = screen.video {
            event.draw.push();
            if event.draw.translate_world(screen.at) {
                host.draw_video(event.draw, video, screen.corners);
            }
            event.draw.pop();
        }
    }

    fn world_primitives(&self, host: &dyn Host, event: &mut WorldPrimitivesEvent<'_>) {
        let state = self.0.lock().unwrap();
        let Some(screen) = &state.screen else { return };
        let editing = state.editing.is_some() || (state.chat_open && state.screen.is_some());
        if !editing {
            return;
        }

        let native_size = screen.video.and_then(|v| host.video_state(v)).and_then(|s| s.video_size);
        let (w, h) = screen.dimensions(native_size);
        let hw = f64::from(w) * 0.5;
        let hh = f64::from(h) * 0.5;

        let center = [
            screen.at[0],
            screen.at[1] + f64::from(h) * 0.5,
            screen.at[2],
        ];

        let (sin, cos) = screen.yaw.to_radians().sin_cos();
        let right = [f64::from(-cos), 0.0, f64::from(-sin)];
        let up = [0.0, 1.0, 0.0];

        let point_at = |u: f64, v: f64| -> [f64; 3] {
            [
                center[0] + right[0] * u + up[0] * v,
                center[1] + right[1] * u + up[1] * v,
                center[2] + right[2] * u + up[2] * v,
            ]
        };

        let tl = point_at(-hw, hh);
        let tr = point_at(hw, hh);
        let br = point_at(hw, -hh);
        let bl = point_at(-hw, -hh);

        // 1. Outer wireframe border (Bright Red)
        event.primitives.line(tl, tr, 0xFFFF2222, 3.0).through_walls = true;
        event.primitives.line(tr, br, 0xFFFF2222, 3.0).through_walls = true;
        event.primitives.line(br, bl, 0xFFFF2222, 3.0).through_walls = true;
        event.primitives.line(bl, tl, 0xFFFF2222, 3.0).through_walls = true;

        // 2. Interior alignment grid (4x4 subdivisions)
        for i in 1..4 {
            let f = f64::from(i) / 4.0;
            // Vertical lines
            let u = -hw + f64::from(w) * f;
            let top_p = point_at(u, hh);
            let bot_p = point_at(u, -hh);
            event.primitives.line(top_p, bot_p, 0x80FF4444, 1.5).through_walls = true;

            // Horizontal lines
            let v = -hh + f64::from(h) * f;
            let left_p = point_at(-hw, v);
            let right_p = point_at(hw, v);
            event.primitives.line(left_p, right_p, 0x80FF4444, 1.5).through_walls = true;
        }

        // Active handle highlight
        let active = state.editing.as_ref().and_then(|e| e.active_handle);

        // 3. Corner handles (drag to resize preserving aspect ratio)
        let corner_s = 0.08;
        let corners = [
            (DragHandle::CornerTL, tl),
            (DragHandle::CornerTR, tr),
            (DragHandle::CornerBL, bl),
            (DragHandle::CornerBR, br),
        ];

        for (handle, p) in corners {
            let is_active = active == Some(handle);
            let color = if is_active { 0xFFFFDD00 } else { 0xFFFF1111 };
            let min = [p[0] - corner_s, p[1] - corner_s, p[2] - corner_s];
            let max = [p[0] + corner_s, p[1] + corner_s, p[2] + corner_s];
            event.primitives.filled_box(min, max, color).through_walls = true;
            event.primitives.box_outline(min, max, 0xFFFFFFFF, 2.0).through_walls = true;
        }

        // 4. Border/edge handles (drag to adjust width/height freely)
        let edge_s = 0.06;
        let edges = [
            (DragHandle::EdgeTop, point_at(0.0, hh)),
            (DragHandle::EdgeBottom, point_at(0.0, -hh)),
            (DragHandle::EdgeLeft, point_at(-hw, 0.0)),
            (DragHandle::EdgeRight, point_at(hw, 0.0)),
        ];

        for (handle, p) in edges {
            let is_active = active == Some(handle);
            let color = if is_active { 0xFFFFDD00 } else { 0xFFFF6622 };
            let min = [p[0] - edge_s, p[1] - edge_s, p[2] - edge_s];
            let max = [p[0] + edge_s, p[1] + edge_s, p[2] + edge_s];
            event.primitives.filled_box(min, max, color).through_walls = true;
            event.primitives.box_outline(min, max, 0xFFFFFFFF, 1.5).through_walls = true;
        }

        // 5. Overhead floating label
        let label_pos = point_at(0.0, hh + 0.35);
        let id_str = state.editing.as_ref().map(|e| e.id.as_str()).unwrap_or("screen");
        let aspect_str = screen.aspect.as_string();
        event.primitives.push(PrimitiveShape::Text {
            at: label_pos,
            text: format!("[{}: {:.2}x{:.2} бл | {}]", id_str, w, h, aspect_str),
            color: 0xFFFFFFFF,
            scale: 1.0,
            scale_with_distance: false,
            background: true,
        }).through_walls = true;
    }

    fn screen_render_post(&self, _host: &dyn Host, event: &mut ScreenRenderPostEvent<'_>) {
        let mut state = self.0.lock().unwrap();
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
        let mut state = self.0.lock().unwrap();
        let Some(screen) = &state.screen else {
            return Flow::Continue;
        };
        let Some(camera) = host.camera() else {
            return Flow::Continue;
        };
        let gui_size = [event.screen.rect[2], event.screen.rect[3]];

        if let Some((handle, _)) = Self::find_hit_handle(screen, &camera, event.screen.mouse, gui_size) {
            if let Some(editing) = state.editing.as_mut() {
                editing.active_handle = Some(handle);
                return Flow::Cancel;
            }
        }
        Flow::Continue
    }

    fn screen_mouse_dragged_pre_v2(
        &self,
        host: &mut dyn Host,
        event: &mut ScreenMouseDraggedPreV2Event<'_>,
    ) -> Flow {
        let mut state = self.0.lock().unwrap();
        let Some(editing) = &state.editing else {
            return Flow::Continue;
        };
        let Some(active_handle) = editing.active_handle else {
            return Flow::Continue;
        };
        let gui_size = state.screen_size;
        let Some(screen) = state.screen.as_mut() else {
            return Flow::Continue;
        };
        let Some(camera) = host.camera() else {
            return Flow::Continue;
        };

        if let Some((u, v)) = Self::raycast_screen_plane(screen, &camera, event.mouse, gui_size) {
            let native_size = screen.video.and_then(|v| host.video_state(v)).and_then(|s| s.video_size);
            Self::apply_drag(screen, active_handle, u, v, native_size);
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
        let mut state = self.0.lock().unwrap();
        if let Some(editing) = state.editing.as_mut() {
            if editing.active_handle.take().is_some() {
                return Flow::Cancel;
            }
        }
        Flow::Continue
    }
}

#[no_mangle]
pub fn hooks() -> Box<dyn Hooks> {
    Box::new(VideoScreen::default())
}
