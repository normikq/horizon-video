//! Horizon Video Screen: in-world video playback via libmpv.
//!
//! Provides the `/video` command:
//! - `/video open <path>`: spawns a 16:9 screen in front of the player and plays the file;
//! - `/video pause`, `/video play`, `/video seek <seconds>`, `/video volume <0-100>`, `/video close`;
//! - `/video info`: playback position and frame timing statistics.

use horizon_client::mod_api::{
    CommandEvent, CommandsEvent, Flow, FrameEvent, Hooks, Host, WorldSubmitEvent,
};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The screen's size in blocks: 16:9.
const WIDTH: f32 = 4.0;
const HEIGHT: f32 = 2.25;
/// How far in front of the player it is put.
const DISTANCE: f64 = 4.0;

struct Screen {
    video: u32,
    /// The middle of the screen's bottom edge, in the world.
    at: [f64; 3],
    /// The quad's corners from `at`: top left, bottom left, bottom right,
    /// top right as the player who opened it sees them.
    corners: [[f32; 3]; 4],
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

/// The screen's corners for a player looking along `yaw` (degrees, 0 is
/// south): upright, facing back at the player.
fn corners(yaw: f32) -> [[f32; 3]; 4] {
    let (sin, cos) = yaw.to_radians().sin_cos();
    // The player's right-hand side: west when looking south.
    let right = [-cos, -sin];
    let side = |sign: f32, y: f32| {
        [
            right[0] * sign * WIDTH / 2.0,
            y,
            right[1] * sign * WIDTH / 2.0,
        ]
    };
    [
        side(-1.0, HEIGHT),
        side(-1.0, 0.0),
        side(1.0, 0.0),
        side(1.0, HEIGHT),
    ]
}

impl VideoScreen {
    fn open(&self, host: &mut dyn Host, state: &mut State, path: &str) -> Result<(), String> {
        let camera = host.camera().ok_or("no world is shown yet")?;
        let video = host.video_open(path, 1280, 720, true)?;
        if let Some(old) = state.screen.take() {
            host.video_close(old.video);
        }
        let (sin, cos) = camera.yaw.to_radians().sin_cos();
        let [x, y, z] = camera.position;
        state.screen = Some(Screen {
            video,
            at: [
                x - f64::from(sin) * DISTANCE,
                y - f64::from(HEIGHT) / 2.0,
                z + f64::from(cos) * DISTANCE,
            ],
            corners: corners(camera.yaw),
        });
        Ok(())
    }

    fn info(host: &dyn Host, screen: &Screen) -> String {
        match host.video_state(screen.video) {
            Some(s) => format!(
                "video: {:.1}/{:.1} s, {}{}{} frames; render {} us, game thread {} us taking + {} us staging",
                s.position.unwrap_or(0.0),
                s.duration.unwrap_or(0.0),
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
        let video = state.screen.as_ref().map(|s| s.video);
        let done = match (verb, video) {
            ("open", _) => self.open(host, &mut state, argument),
            (_, None) => Err("no video is open: /video open <file>".into()),
            ("pause", Some(v)) => host.video_pause(v, true).then_some(()).ok_or("failed".into()),
            ("play", Some(v)) => host.video_pause(v, false).then_some(()).ok_or("failed".into()),
            ("seek", Some(v)) => argument
                .parse()
                .ok()
                .filter(|&seconds| host.video_seek(v, seconds))
                .map(|_| ())
                .ok_or("usage: /video seek <seconds>".into()),
            ("volume", Some(v)) => argument
                .parse()
                .ok()
                .filter(|&volume| host.video_volume(v, volume))
                .map(|_| ())
                .ok_or("usage: /video volume <0-100>".into()),
            ("close", Some(v)) => {
                host.video_close(v);
                state.screen = None;
                Ok(())
            }
            ("info", Some(_)) => Ok(()),
            _ => Err("usage: /video open <file> | pause | play | seek <seconds> | volume <0-100> | close | info".into()),
        };
        match (&done, &state.screen) {
            (Ok(()), Some(screen)) => host.log("info", &Self::info(host, screen)),
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
