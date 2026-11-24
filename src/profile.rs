//! The user's profile: the preferences kept between runs, in `settings.json` in the per-user
//! config folder (`%APPDATA%\VibeCAD` on Windows, `~/Library/Application Support/VibeCAD` on
//! macOS, `$XDG_CONFIG_HOME/vibecad` or `~/.config/vibecad` elsewhere).
//!
//! Each field is read on its own: one missing, or one this version cannot read (a scheme a newer
//! version added), keeps its default and the rest still load. A file that is not JSON at all is
//! set aside as `settings.json.bad` rather than written over.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::navigation::Scheme;
use crate::ui_chrome::Tab;

/// Everything the profile keeps.
#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    pub navigation: Scheme,
    pub show_edges: bool,
    /// Pixels, 1 to 4.
    pub edge_width: f32,
    pub palette: bool,
    pub show_grid: bool,
    pub show_log: bool,
    /// The validators' panel (EDIT → Check) is open.
    pub show_check: bool,
    pub ortho: bool,
    pub tab: Tab,
    /// The window's inner size in points when it is not maximized.
    pub window_size: [f32; 2],
    pub maximized: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            navigation: Scheme::default(),
            show_edges: true,
            edge_width: 1.5,
            palette: true,
            show_grid: true,
            show_log: false,
            show_check: false,
            ortho: true,
            tab: Tab::default(),
            window_size: [1400.0, 900.0],
            maximized: false,
        }
    }
}

const VERSION: u32 = 1;

impl Settings {
    fn to_json(&self) -> Value {
        json!({
            "version": VERSION,
            "navigation": self.navigation,
            "show_edges": self.show_edges,
            "edge_width": self.edge_width,
            "palette": self.palette,
            "show_grid": self.show_grid,
            "show_log": self.show_log,
            "show_check": self.show_check,
            "ortho": self.ortho,
            "tab": self.tab,
            "window_size": self.window_size,
            "maximized": self.maximized,
        })
    }

    fn from_json(v: &Value) -> Settings {
        fn get<T: DeserializeOwned>(v: &Value, key: &str, default: T) -> T {
            v.get(key).and_then(|x| T::deserialize(x).ok()).unwrap_or(default)
        }
        let d = Settings::default();
        let edge_width: f32 = get(v, "edge_width", d.edge_width);
        let size: [f32; 2] = get(v, "window_size", d.window_size);
        Settings {
            navigation: get(v, "navigation", d.navigation),
            show_edges: get(v, "show_edges", d.show_edges),
            edge_width: if edge_width.is_finite() { edge_width.clamp(1.0, 4.0) } else { d.edge_width },
            palette: get(v, "palette", d.palette),
            show_grid: get(v, "show_grid", d.show_grid),
            show_log: get(v, "show_log", d.show_log),
            show_check: get(v, "show_check", d.show_check),
            ortho: get(v, "ortho", d.ortho),
            tab: get(v, "tab", d.tab),
            window_size: if size.iter().all(|s| s.is_finite() && (200.0..=20000.0).contains(s)) { size } else { d.window_size },
            maximized: get(v, "maximized", d.maximized),
        }
    }
}

/// Where the settings live, and what was last written there. Changes are written once they
/// have held still for a moment, so dragging a slider or the window's edge is one write.
pub struct Profile {
    /// `None`: nothing is read or written (the self-test, or no config folder).
    path: Option<PathBuf>,
    saved: Settings,
    pending: Option<(Settings, Instant)>,
}

const SETTLE: Duration = Duration::from_millis(500);

impl Profile {
    /// The user's profile, and the settings in it. A problem reading it is returned as text
    /// for the status line; the settings are then the defaults.
    pub fn load() -> (Profile, Settings, Option<String>) {
        match config_dir() {
            Some(dir) => Profile::load_from(&dir.join("settings.json")),
            None => (Profile::off(), Settings::default(), Some("no config folder for the settings; they will not be kept".into())),
        }
    }

    pub fn load_from(path: &Path) -> (Profile, Settings, Option<String>) {
        let (settings, problem) = match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Settings::default(), None),
            Err(e) => (Settings::default(), Some(format!("could not read {}: {e}", path.display()))),
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(v) => (Settings::from_json(&v), None),
                Err(e) => {
                    let bad = path.with_extension("json.bad");
                    let kept = std::fs::rename(path, &bad).map_or_else(|e| format!(", and could not set it aside: {e}"), |_| format!(", set aside as {}", bad.display()));
                    (Settings::default(), Some(format!("{} is not valid JSON ({e}){kept}", path.display())))
                }
            },
        };
        (Profile { path: Some(path.to_owned()), saved: settings.clone(), pending: None }, settings, problem)
    }

    /// A profile that keeps nothing.
    pub fn off() -> Profile {
        Profile { path: None, saved: Settings::default(), pending: None }
    }

    /// Note the settings as they are now; writes them once they have settled. Returns when to
    /// look again while a write waits, and a write error for the status line.
    pub fn keep(&mut self, now: Settings, at: Instant) -> (Option<Duration>, Option<String>) {
        if now == self.saved {
            self.pending = None;
            return (None, None);
        }
        match &self.pending {
            Some((p, since)) if *p == now => {
                let waited = at.duration_since(*since);
                if waited >= SETTLE {
                    return (None, self.flush());
                }
                (Some(SETTLE - waited), None)
            }
            _ => {
                self.pending = Some((now, at));
                (Some(SETTLE), None)
            }
        }
    }

    /// Write what waits now (on exit).
    pub fn flush(&mut self) -> Option<String> {
        let (settings, _) = self.pending.take()?;
        // Not retried until the settings change again, so a read-only folder is one message.
        self.saved = settings;
        let path = self.path.as_ref()?;
        write_atomically(path, &self.saved.to_json()).err().map(|e| format!("could not save the settings to {}: {e}", path.display()))
    }
}

/// Into a temporary file beside it, then renamed over it: a crash never leaves half a file.
fn write_atomically(path: &Path, v: &impl Serialize) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    std::fs::rename(&tmp, path)
}

fn config_dir() -> Option<PathBuf> {
    let env = |k| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        env("APPDATA").map(|d| d.join("VibeCAD"))
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Application Support/VibeCAD"))
    } else {
        env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config"))).map(|d| d.join("vibecad"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vibecad-profile-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("settings.json")
    }

    #[test]
    fn settings_come_back_as_they_were_saved() {
        let path = temp("roundtrip");
        let (mut p, s, problem) = Profile::load_from(&path);
        assert_eq!((s, problem), (Settings::default(), None));
        let changed = Settings { navigation: Scheme::Fusion, edge_width: 3.25, ortho: false, maximized: true, window_size: [1000.0, 700.0], ..Settings::default() };
        let t = Instant::now();
        assert_eq!(p.keep(changed.clone(), t), (Some(SETTLE), None));
        assert!(!path.exists(), "written before it settled");
        assert_eq!(p.keep(changed.clone(), t + SETTLE), (None, None));
        let (_, back, problem) = Profile::load_from(&path);
        assert_eq!((back, problem), (changed, None));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_field_it_cannot_read_keeps_its_default_and_the_rest_load() {
        let v = json!({ "navigation": "SomeFutureScheme", "edge_width": 99.0, "show_grid": false, "window_size": [10, 10], "extra": 1 });
        let s = Settings::from_json(&v);
        assert_eq!(s.navigation, Scheme::default());
        assert_eq!(s.edge_width, 4.0);
        assert!(!s.show_grid);
        assert_eq!(s.window_size, Settings::default().window_size);
    }

    #[test]
    fn a_broken_file_is_set_aside_not_written_over() {
        let path = temp("broken");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        let (mut p, s, problem) = Profile::load_from(&path);
        assert_eq!(s, Settings::default());
        assert!(problem.is_some());
        assert_eq!(std::fs::read_to_string(path.with_extension("json.bad")).unwrap(), "{ not json");
        // and the next save starts a fresh file
        p.keep(Settings { show_log: true, ..Settings::default() }, Instant::now());
        assert_eq!(p.flush(), None);
        assert!(Profile::load_from(&path).1.show_log);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
