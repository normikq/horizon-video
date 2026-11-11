//! Horizon Video Screen: in-world video playback via libmpv.
//!
//! Provides the `/video` command:
//! - `/video open <path>`: spawns a screen in front of the player and plays the file;
//! - `/video size <diagonal>`: sets screen diagonal in blocks (0.5 to 50.0);
//! - `/video grow [step]`: increases screen diagonal by step blocks (default 0.5);
//! - `/video shrink [step]`: decreases screen diagonal by step blocks (default 0.5);
//! - `/video aspect <ratio>`: sets aspect ratio (e.g. 16:9, 4:3, 21:9, 1:1, or W:H);
//! - `/video distance <blocks>`: sets screen distance in front of the player (1.0 to 100.0);
//! - `/video reset`: resets screen dimensions and distance to default;
//! - `/video pause`, `/video play`, `/video seek <seconds>`, `/video volume <0-100>`, `/video close`;
//! - `/video info`: playback position, screen dimensions, and frame timing statistics.

use horizon_client::mod_api::{
    CommandEvent, CommandsEvent, Flow, FrameEvent, Hooks, Host, WorldSubmitEvent,
};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The default screen diagonal in blocks (~4.589 blocks corresponds to 4.0 x 2.25 at 16:9).
const DEFAULT_DIAGONAL: f32 = 4.58939;
/// Default aspect ratio (16:9).
const DEFAULT_ASPECT: f32 = 16.0 / 9.0;
/// How far in front of the player it is put by default.
const DEFAULT_DISTANCE: f64 = 4.0;

/// Aspect ratio for the screen geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AspectRatio {
    Ratio(f32),
}

impl AspectRatio {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
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

    pub fn value(&self) -> f32 {
        match self {
            Self::Ratio(r) => *r,
        }
    }
}

pub struct Screen {
    video: u32,
    /// Player camera position when placed or updated.
    camera_pos: [f64; 3],
    yaw: f32,
    diagonal: f32,
    aspect: AspectRatio,
    distance: f64,
    /// The middle of the screen's bottom edge, in the world.
    at: [f64; 3],
    /// The quad's corners from `at`: top left, bottom left, bottom right,
    /// top right as the player who opened it sees them.
    corners: [[f32; 3]; 4],
}

impl Screen {
    pub fn new(video: u32, camera_pos: [f64; 3], yaw: f32) -> Self {
        let mut screen = Self {
            video,
            camera_pos,
            yaw,
            diagonal: DEFAULT_DIAGONAL,
            aspect: AspectRatio::Ratio(DEFAULT_ASPECT),
            distance: DEFAULT_DISTANCE,
            at: [0.0; 3],
            corners: [[0.0; 3]; 4],
        };
        screen.recalculate();
        screen
    }

    pub fn dimensions(&self) -> (f32, f32) {
        let r = self.aspect.value();
        let height = self.diagonal / (r * r + 1.0).sqrt();
        let width = r * height;
        (width, height)
    }

    pub fn recalculate(&mut self) {
        let (width, height) = self.dimensions();
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
    demo: Option<String>,
    reported: Option<Instant>,
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
    fn open(&self, host: &mut dyn Host, state: &mut State, path: &str) -> Result<(), String> {
        let camera = host.camera().ok_or("no world is shown yet")?;
        let video = host.video_open(path, 1280, 720, true)?;
        if let Some(old) = state.screen.take() {
            host.video_close(old.video);
        }
        state.screen = Some(Screen::new(video, camera.position, camera.yaw));
        Ok(())
    }

    fn info(host: &dyn Host, screen: &Screen) -> String {
        let (w, h) = screen.dimensions();
        let size_str = format!("{:.2}x{:.2} blocks (diag {:.2})", w, h, screen.diagonal);
        match host.video_state(screen.video) {
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
            None => "video: closed".into(),
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

        let done = match (verb, state.screen.as_mut()) {
            ("open", _) => self.open(host, &mut state, argument),
            (_, None) => Err("no video is open: /video open <file>".into()),
            ("size", Some(s)) => (|| -> Result<(), String> {
                let diag: f32 = argument
                    .parse()
                    .map_err(|_| "usage: /video size <diagonal in blocks> (e.g. 5.0)")?;
                if !(0.5..=50.0).contains(&diag) {
                    return Err("diagonal must be between 0.5 and 50.0 blocks".into());
                }
                s.diagonal = diag;
                s.recalculate();
                Ok(())
            })(),
            ("grow", Some(s)) => (|| -> Result<(), String> {
                let step: f32 = if argument.is_empty() {
                    0.5
                } else {
                    argument
                        .parse()
                        .map_err(|_| "usage: /video grow [step in blocks]")?
                };
                s.diagonal = (s.diagonal + step).clamp(0.5, 50.0);
                s.recalculate();
                Ok(())
            })(),
            ("shrink", Some(s)) => (|| -> Result<(), String> {
                let step: f32 = if argument.is_empty() {
                    0.5
                } else {
                    argument
                        .parse()
                        .map_err(|_| "usage: /video shrink [step in blocks]")?
                };
                s.diagonal = (s.diagonal - step).clamp(0.5, 50.0);
                s.recalculate();
                Ok(())
            })(),
            ("distance", Some(s)) => (|| -> Result<(), String> {
                let dist: f64 = argument
                    .parse()
                    .map_err(|_| "usage: /video distance <blocks> (e.g. 5.0)")?;
                if !(1.0..=100.0).contains(&dist) {
                    return Err("distance must be between 1.0 and 100.0 blocks".into());
                }
                s.distance = dist;
                s.recalculate();
                Ok(())
            })(),
            ("aspect", Some(s)) => (|| -> Result<(), String> {
                let aspect = AspectRatio::parse(argument)
                    .ok_or_else(|| "usage: /video aspect <16:9 | 4:3 | 21:9 | 1:1 | W:H>")?;
                s.aspect = aspect;
                s.recalculate();
                Ok(())
            })(),
            ("reset", Some(s)) => {
                s.diagonal = DEFAULT_DIAGONAL;
                s.aspect = AspectRatio::Ratio(DEFAULT_ASPECT);
                s.distance = DEFAULT_DISTANCE;
                s.recalculate();
                Ok(())
            }
            ("pause", Some(s)) => host.video_pause(s.video, true).then_some(()).ok_or("failed".into()),
            ("play", Some(s)) => host.video_pause(s.video, false).then_some(()).ok_or("failed".into()),
            ("seek", Some(s)) => argument
                .parse()
                .ok()
                .filter(|&seconds| host.video_seek(s.video, seconds))
                .map(|_| ())
                .ok_or("usage: /video seek <seconds>".into()),
            ("volume", Some(s)) => argument
                .parse()
                .ok()
                .filter(|&volume| host.video_volume(s.video, volume))
                .map(|_| ())
                .ok_or("usage: /video volume <0-100>".into()),
            ("close", Some(s)) => {
                let video = s.video;
                host.video_close(video);
                state.screen = None;
                Ok(())
            }
            ("info", Some(_)) => Ok(()),
            _ => Err("usage: /video open <file> | size <diag> | grow [step] | shrink [step] | aspect <ratio> | distance <dist> | reset | pause | play | seek <seconds> | volume <0-100> | close | info".into()),
        };

        match (&done, &state.screen) {
            (Ok(()), Some(screen)) => match verb {
                "size" | "grow" | "shrink" | "aspect" => {
                    let (w, h) = screen.dimensions();
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
                        "video: screen reset to default size (4.0x2.25) and distance (4.0)",
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
        event.draw.push();
        if event.draw.translate_world(screen.at) {
            host.draw_video(event.draw, screen.video, screen.corners);
        }
        event.draw.pop();
    }
}

/// Entry point called by Horizon's launcher when linking trait mods.
pub fn hooks() -> Box<dyn Hooks> {
    Box::new(VideoScreen::default())
}
