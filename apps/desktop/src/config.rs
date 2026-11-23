//! User settings in `<data dir>/settings.json`: parsing, creation, live-reload polling and the
//! font the file names. Colors and sizes stay in `theme.rs`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

pub const DEFAULT_FONT_SIZE: f32 = 13.5;
/// The family shipped inside the app (JetBrains Mono, SIL OFL); it needs no install.
pub const BUNDLED_FONT: &str = "JetBrains Mono";
const BUNDLED_FONT_DATA: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf");
const MIN_FONT_SIZE: f32 = 9.0;
const MAX_FONT_SIZE: f32 = 28.0;
/// How often the file's mtime is checked.
pub const POLL: Duration = Duration::from_secs(2);

/// What sending while a turn runs does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SendMode {
    /// Hold the message and send it when the turn ends.
    #[default]
    Queue,
    /// Send it now; the CLI folds it into the running turn at the next tool boundary.
    Steer,
}

impl SendMode {
    pub fn other(self) -> SendMode {
        match self {
            SendMode::Queue => SendMode::Steer,
            SendMode::Steer => SendMode::Queue,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// The bundled family or an installed one; `None` keeps egui's default font.
    pub font_family: Option<String>,
    pub font_size: f32,
    pub send_while_running: SendMode,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            font_family: Some(BUNDLED_FONT.to_string()),
            font_size: DEFAULT_FONT_SIZE,
            send_while_running: SendMode::default(),
        }
    }
}

impl Config {
    /// Parses schema v1. Missing keys keep their defaults, unknown keys are ignored and
    /// `font_size` is clamped; anything else invalid is an error.
    pub fn parse(text: &str) -> Result<Config, String> {
        let value: Value = serde_json::from_str(text).map_err(|e| format!("invalid JSON ({e})"))?;
        let obj = value.as_object().ok_or("expected a JSON object")?;
        let mut cfg = Config::default();
        match obj.get("font_family") {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) if !s.trim().is_empty() => {
                cfg.font_family = Some(s.trim().to_string());
            }
            Some(_) => return Err("font_family must be null or a family name".into()),
        }
        match obj.get("font_size") {
            None => {}
            Some(Value::Number(n)) => {
                let size = n.as_f64().ok_or("font_size must be a number")? as f32;
                cfg.font_size = size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
            }
            Some(_) => return Err("font_size must be a number".into()),
        }
        match obj.get("send_while_running").and_then(Value::as_str) {
            None if !obj.contains_key("send_while_running") => {}
            Some("queue") => cfg.send_while_running = SendMode::Queue,
            Some("steer") => cfg.send_while_running = SendMode::Steer,
            _ => return Err("send_while_running must be \"queue\" or \"steer\"".into()),
        }
        Ok(cfg)
    }
}

/// The settings file; a pre-rename `config.json` is moved to it when `settings.json` is absent.
pub fn path(data_dir: &Path) -> PathBuf {
    let new = data_dir.join("settings.json");
    let old = data_dir.join("config.json");
    if old.exists() && !new.exists() {
        match std::fs::rename(&old, &new) {
            Ok(()) => tracing::info!("config.json renamed to settings.json"),
            Err(e) => tracing::warn!(error = %e, "could not rename config.json"),
        }
    }
    new
}

/// Reads `path`, writing the defaults first when it does not exist. A bad file yields the
/// defaults and the reason.
pub fn load(path: &Path) -> (Config, Option<String>) {
    if !path.exists() {
        let defaults = serde_json::to_string_pretty(&json!({
            "font_family": BUNDLED_FONT,
            "font_size": DEFAULT_FONT_SIZE,
            "send_while_running": "queue",
        }))
        .unwrap_or_default();
        if let Err(e) = std::fs::write(path, defaults + "\n") {
            tracing::warn!(error = %e, "could not write settings.json");
        }
        return (Config::default(), None);
    }
    let parsed = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read ({e})"))
        .and_then(|text| Config::parse(&text));
    match parsed {
        Ok(cfg) => (cfg, None),
        Err(reason) => {
            tracing::warn!(%reason, "settings.json ignored");
            (Config::default(), Some(format!("settings.json: {reason}")))
        }
    }
}

/// Detects changes of the file's mtime, at most once per `POLL`.
pub struct Watch {
    path: PathBuf,
    mtime: Option<SystemTime>,
    checked: Instant,
}

impl Watch {
    pub fn new(path: PathBuf) -> Self {
        let mtime = mtime(&path);
        Watch {
            path,
            mtime,
            checked: Instant::now(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the file changed (or appeared or vanished) since the last check.
    pub fn changed(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.checked) < POLL {
            return false;
        }
        self.checked = now;
        let current = mtime(&self.path);
        let changed = current != self.mtime;
        self.mtime = current;
        changed
    }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Egui's default fonts with `family` (bundled or installed) in front of the proportional list.
pub fn font_definitions(family: &str) -> Result<egui::FontDefinitions, String> {
    if family.eq_ignore_ascii_case(BUNDLED_FONT) {
        return Ok(with_front(
            BUNDLED_FONT,
            egui::FontData::from_static(BUNDLED_FONT_DATA),
        ));
    }
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let name = canonical_family(&db, family).unwrap_or_else(|| family.to_string());
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(&name)],
        ..Default::default()
    };
    let id = db
        .query(&query)
        .ok_or_else(|| format!("font \"{family}\" is not installed"))?;
    let (bytes, index) = db
        .with_face_data(id, |data, index| (data.to_vec(), index))
        .ok_or_else(|| format!("font \"{family}\" could not be read"))?;
    let mut data = egui::FontData::from_owned(bytes);
    data.index = index;
    tracing::info!(family, "font resolved");
    Ok(with_front(family, data))
}

fn with_front(family: &str, data: egui::FontData) -> egui::FontDefinitions {
    let mut defs = egui::FontDefinitions::default();
    defs.font_data
        .insert(family.to_string(), std::sync::Arc::new(data));
    defs.families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, family.to_string());
    defs
}

/// The database's spelling of `family`, matched ignoring case.
fn canonical_family(db: &fontdb::Database, family: &str) -> Option<String> {
    db.faces()
        .flat_map(|f| f.families.iter())
        .find(|(name, _)| name.eq_ignore_ascii_case(family))
        .map(|(name, _)| name.clone())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn defaults_partial_and_unknown_keys() {
        assert_eq!(Config::parse("{}").unwrap(), Config::default());
        let full = Config::parse(r#"{"font_family":"Menlo","font_size":16}"#).unwrap();
        assert_eq!(full.font_family.as_deref(), Some("Menlo"));
        assert_eq!(full.font_size, 16.0);
        let partial = Config::parse(r#"{"font_size":15,"theme":"light","extra":[1]}"#).unwrap();
        assert_eq!(partial.font_family.as_deref(), Some(BUNDLED_FONT));
        assert_eq!(partial.font_size, 15.0);
        let null = Config::parse(r#"{"font_family":null}"#).unwrap();
        assert_eq!(null, Config::default());
    }

    #[test]
    fn send_while_running_defaults_to_queue_and_rejects_unknown_values() {
        assert_eq!(Config::default().send_while_running, SendMode::Queue);
        let steer = Config::parse(r#"{"send_while_running":"steer"}"#).unwrap();
        assert_eq!(steer.send_while_running, SendMode::Steer);
        assert_eq!(steer.send_while_running.other(), SendMode::Queue);
        assert!(Config::parse(r#"{"send_while_running":"now"}"#).is_err());
        assert!(Config::parse(r#"{"send_while_running":null}"#).is_err());
    }

    #[test]
    fn font_size_is_clamped() {
        assert_eq!(Config::parse(r#"{"font_size":2}"#).unwrap().font_size, 9.0);
        assert_eq!(
            Config::parse(r#"{"font_size":99}"#).unwrap().font_size,
            28.0
        );
    }

    #[test]
    fn invalid_content_is_an_error() {
        for bad in [
            "",
            "{",
            "[]",
            r#"{"font_size":"big"}"#,
            r#"{"font_family":3}"#,
            r#"{"font_family":" "}"#,
        ] {
            assert!(Config::parse(bad).is_err(), "{bad:?}");
        }
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xade-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_creates_defaults_and_reports_bad_files() {
        let dir = temp_dir();
        let file = path(&dir);
        let (cfg, err) = load(&file);
        assert_eq!((cfg, err), (Config::default(), None));
        let written = std::fs::read_to_string(&file).unwrap();
        assert_eq!(Config::parse(&written).unwrap(), Config::default());
        assert!(written.contains('\n') && written.contains("\"font_family\": \"JetBrains Mono\""));

        std::fs::write(&file, "{ nope").unwrap();
        let (cfg, err) = load(&file);
        assert_eq!(cfg, Config::default());
        assert!(err.unwrap().starts_with("settings.json: invalid JSON"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_json_migrates_unless_settings_json_exists() {
        let dir = temp_dir();
        std::fs::write(dir.join("config.json"), "{\"font_size\":15}").unwrap();
        let file = path(&dir);
        assert!(file.ends_with("settings.json") && !dir.join("config.json").exists());
        assert_eq!(load(&file).0.font_size, 15.0);
        std::fs::write(dir.join("config.json"), "{\"font_size\":20}").unwrap();
        path(&dir);
        assert!(dir.join("config.json").exists());
        assert_eq!(load(&file).0.font_size, 15.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn watch_reports_only_real_changes_once_per_interval() {
        let dir = temp_dir();
        let file = path(&dir);
        std::fs::write(&file, "{}").unwrap();
        let mut w = Watch::new(file.clone());
        let t0 = w.checked;
        let tick = |n: u64| t0 + POLL * n as u32;
        assert!(!w.changed(tick(1)));
        // Too soon after a check: not even looked at.
        let later = std::fs::File::options().write(true).open(&file).unwrap();
        later
            .set_modified(SystemTime::now() + Duration::from_secs(60))
            .unwrap();
        assert!(!w.changed(tick(1) + Duration::from_millis(500)));
        assert!(w.changed(tick(2)));
        assert!(!w.changed(tick(3)));
        std::fs::remove_file(&file).unwrap();
        assert!(w.changed(tick(4)));
        assert!(!w.changed(tick(5)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_default_font_ships_with_the_app() {
        let defs = font_definitions("jetbrains mono").unwrap();
        assert!(defs.font_data.contains_key(BUNDLED_FONT));
        assert_eq!(
            defs.families[&egui::FontFamily::Proportional][0],
            BUNDLED_FONT
        );
    }

    #[test]
    fn unknown_font_is_reported() {
        let err = font_definitions("No Such Family \u{1F680}").unwrap_err();
        assert!(err.contains("not installed"));
    }
}
