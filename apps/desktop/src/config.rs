//! User settings in `<data dir>/settings.json`: parsing, creation, live-reload polling and the
//! font and colors the file names. The color keys and defaults live in `theme.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

use crate::keymap::{self, Keymap};
use crate::theme::Palette;

pub const DEFAULT_FONT_SIZE: f32 = 13.5;
/// The family shipped inside the app (JetBrains Mono, SIL OFL); it needs no install.
pub const BUNDLED_FONT: &str = "JetBrains Mono";
const BUNDLED_FONT_DATA: &[u8] = include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf");
/// The prompt a fork asks the source provider to write its state hand-off with (shipped in the
/// app, copied to `DEFAULT_FORK_PROMPT_FILE` on first run).
pub const BUNDLED_FORK_PROMPT: &str = include_str!("../../../assets/prompts/fork-handoff.md");
/// Where the prompt file lives by default, relative to the data dir.
pub const DEFAULT_FORK_PROMPT_FILE: &str = "prompts/fork-handoff.md";
/// The preamble after a fork's hand-off file that makes the new provider continue the
/// conversation instead of discussing the hand-off (same handling as the fork prompt).
pub const BUNDLED_FORK_RESUME_PROMPT: &str = include_str!("../../../assets/prompts/fork-resume.md");
pub const DEFAULT_FORK_RESUME_PROMPT_FILE: &str = "prompts/fork-resume.md";
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
    /// When true, plain Enter sends the prompt and Shift+Enter inserts a newline.
    pub send_on_enter: bool,
    /// Whether the status bar shows the frame rate.
    pub show_fps: bool,
    /// Whether file windows mark every space and tab (`theme.whitespace` colors them).
    pub show_whitespace: bool,
    /// Whether the session text box breaks long words anywhere (CSS `word-break: break-all`).
    pub input_break_all: bool,
    /// Whether a plain launch reopens the windows left open (they are saved only while true).
    pub restore_windows: bool,
    /// File with the prompt a fork asks the source provider; relative to the data dir unless
    /// absolute.
    pub fork_handoff_prompt: PathBuf,
    /// File with the preamble a fork's first prompt ends its hand-off with; same path rules.
    pub fork_resume_prompt: PathBuf,
    pub shortcuts: Arc<Keymap>,
    /// The `theme` block: hex colors by name; missing ones keep their defaults.
    pub theme: Palette,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            font_family: Some(BUNDLED_FONT.to_string()),
            font_size: DEFAULT_FONT_SIZE,
            send_while_running: SendMode::default(),
            send_on_enter: true,
            show_fps: false,
            show_whitespace: true,
            input_break_all: true,
            restore_windows: true,
            fork_handoff_prompt: PathBuf::from(DEFAULT_FORK_PROMPT_FILE),
            fork_resume_prompt: PathBuf::from(DEFAULT_FORK_RESUME_PROMPT_FILE),
            shortcuts: Arc::default(),
            theme: Palette::default(),
        }
    }
}

impl Config {
    /// Parses schema v1. Missing keys keep their defaults, unknown keys are ignored and
    /// `font_size` is clamped; anything else invalid (including a bad `shortcuts` block) is an
    /// error.
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
        match obj.get("send_on_enter") {
            None => {}
            Some(Value::Bool(b)) => cfg.send_on_enter = *b,
            Some(_) => return Err("send_on_enter must be true or false".into()),
        }
        match obj.get("show_fps") {
            None => {}
            Some(Value::Bool(b)) => cfg.show_fps = *b,
            Some(_) => return Err("show_fps must be true or false".into()),
        }
        match obj.get("show_whitespace") {
            None => {}
            Some(Value::Bool(b)) => cfg.show_whitespace = *b,
            Some(_) => return Err("show_whitespace must be true or false".into()),
        }
        match obj.get("input_break_all") {
            None => {}
            Some(Value::Bool(b)) => cfg.input_break_all = *b,
            Some(_) => return Err("input_break_all must be true or false".into()),
        }
        match obj.get("restore_windows") {
            None => {}
            Some(Value::Bool(b)) => cfg.restore_windows = *b,
            Some(_) => return Err("restore_windows must be true or false".into()),
        }
        match obj.get("fork_handoff_prompt") {
            None => {}
            Some(Value::String(s)) if !s.trim().is_empty() => {
                cfg.fork_handoff_prompt = PathBuf::from(s.trim());
            }
            Some(_) => return Err("fork_handoff_prompt must be a file path".into()),
        }
        match obj.get("fork_resume_prompt") {
            None => {}
            Some(Value::String(s)) if !s.trim().is_empty() => {
                cfg.fork_resume_prompt = PathBuf::from(s.trim());
            }
            Some(_) => return Err("fork_resume_prompt must be a file path".into()),
        }
        match obj.get("shortcuts") {
            None => {}
            Some(Value::Object(map)) => cfg.shortcuts = Arc::new(Keymap::parse(map)?),
            Some(_) => return Err("shortcuts must be an object".into()),
        }
        match obj.get("theme") {
            // A string was an old, unused key; it is ignored.
            None | Some(Value::String(_)) => {}
            Some(Value::Object(map)) => cfg.theme = Palette::parse(map)?,
            Some(_) => return Err("theme must be an object of hex colors".into()),
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

/// The settings file as first written: every key, with its default value.
fn defaults_json() -> String {
    serde_json::to_string_pretty(&json!({
        "font_family": BUNDLED_FONT,
        "font_size": DEFAULT_FONT_SIZE,
        "send_while_running": "queue",
        "send_on_enter": true,
        "show_fps": false,
        "show_whitespace": true,
        "input_break_all": true,
        "restore_windows": true,
        "fork_handoff_prompt": DEFAULT_FORK_PROMPT_FILE,
        "fork_resume_prompt": DEFAULT_FORK_RESUME_PROMPT_FILE,
        "shortcuts": keymap::default_shortcuts(),
        "theme": Palette::defaults_json(),
    }))
    .unwrap_or_default()
}

/// Creates the default fork prompt files from the bundled text unless they already exist (an
/// edited or deleted-and-recreated file is never overwritten). Blocking.
pub fn ensure_default_fork_prompt(data_dir: &Path) {
    for (file, text) in [
        (DEFAULT_FORK_PROMPT_FILE, BUNDLED_FORK_PROMPT),
        (DEFAULT_FORK_RESUME_PROMPT_FILE, BUNDLED_FORK_RESUME_PROMPT),
    ] {
        let path = data_dir.join(file);
        let made = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
            })
            .and_then(|mut f| std::io::Write::write_all(&mut f, text.as_bytes()));
        match made {
            Ok(()) => tracing::info!(file, "fork prompt file created"),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => tracing::warn!(error = %e, file, "could not create a fork prompt file"),
        }
    }
}

/// The fork prompt in `path`, or the bundled one plus a warning when it is missing, empty or
/// unreadable. Blocking.
pub fn read_fork_prompt(path: Option<&Path>) -> (String, Option<String>) {
    read_prompt(path, BUNDLED_FORK_PROMPT, "Fork prompt")
}

/// The fork resume preamble in `path`, with the same fallback as `read_fork_prompt`. Blocking.
pub fn read_fork_resume_prompt(path: Option<&Path>) -> (String, Option<String>) {
    read_prompt(path, BUNDLED_FORK_RESUME_PROMPT, "Fork resume prompt")
}

fn read_prompt(path: Option<&Path>, bundled: &str, what: &str) -> (String, Option<String>) {
    let Some(path) = path else {
        return (bundled.to_string(), None);
    };
    let why = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => return (text, None),
        Ok(_) => "is empty".to_string(),
        Err(e) => format!("cannot be read ({e})"),
    };
    (
        bundled.to_string(),
        Some(format!(
            "{what} {} {why}; using the bundled one",
            path.display()
        )),
    )
}

/// Replaces `path` with the defaults, keeping the old file as `<name>.bak`. The live reload
/// applies them.
pub fn restore_defaults(path: &Path) -> Result<(), String> {
    if path.exists() {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        std::fs::copy(path, &bak).map_err(|e| format!("could not back up settings ({e})"))?;
    }
    std::fs::write(path, defaults_json() + "\n")
        .map_err(|e| format!("could not write settings ({e})"))
}

/// Reads `path`, writing the defaults first when it does not exist. A bad file yields the
/// defaults and the reason.
pub fn load(path: &Path) -> (Config, Option<String>) {
    if !path.exists() {
        let defaults = defaults_json();
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
    fn theme_block_overrides_colors_and_a_bad_one_is_an_error() {
        let cfg = Config::parse(r##"{"theme":{"bg_window":"#342c2c"}}"##).unwrap();
        assert_eq!(
            cfg.theme.bg_window,
            egui::Color32::from_rgb(0x34, 0x2c, 0x2c)
        );
        assert!(Config::parse(r##"{"theme":{"bg_window":"nope"}}"##)
            .unwrap_err()
            .contains("theme.bg_window"));
        assert!(Config::parse(r#"{"theme":5}"#).is_err());
        assert_eq!(
            Config::parse(&defaults_json()).unwrap(),
            Config::default(),
            "the file as first written means the defaults"
        );
    }

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
    fn send_on_enter_defaults_to_true_and_must_be_a_bool() {
        assert!(Config::default().send_on_enter);
        assert!(
            !Config::parse(r#"{"send_on_enter":false}"#)
                .unwrap()
                .send_on_enter
        );
        assert!(Config::parse(r#"{"send_on_enter":"yes"}"#).is_err());
        assert!(Config::parse(r#"{"send_on_enter":null}"#).is_err());
    }

    #[test]
    fn show_fps_defaults_to_false_and_must_be_a_bool() {
        assert!(!Config::default().show_fps);
        assert!(Config::parse(r#"{"show_fps":true}"#).unwrap().show_fps);
        assert!(Config::parse(r#"{"show_fps":"no"}"#).is_err());
        assert!(Config::parse(r#"{"show_fps":null}"#).is_err());
    }

    #[test]
    fn input_break_all_defaults_to_true_and_must_be_a_bool() {
        assert!(Config::default().input_break_all);
        assert!(
            !Config::parse(r#"{"input_break_all":false}"#)
                .unwrap()
                .input_break_all
        );
        assert!(Config::parse(r#"{"input_break_all":"yes"}"#).is_err());
    }

    #[test]
    fn show_whitespace_defaults_to_true_and_must_be_a_bool() {
        assert!(Config::default().show_whitespace);
        assert!(
            !Config::parse(r#"{"show_whitespace":false}"#)
                .unwrap()
                .show_whitespace
        );
        assert!(Config::parse(r#"{"show_whitespace":1}"#).is_err());
        assert!(Config::parse(r#"{"show_whitespace":null}"#).is_err());
    }

    #[test]
    fn restore_windows_defaults_to_true_and_must_be_a_bool() {
        assert!(Config::default().restore_windows);
        assert!(
            !Config::parse(r#"{"restore_windows":false}"#)
                .unwrap()
                .restore_windows
        );
        assert!(Config::parse(r#"{"restore_windows":"no"}"#).is_err());
        assert!(Config::parse(r#"{"restore_windows":null}"#).is_err());
    }

    #[test]
    fn fork_handoff_prompt_defaults_to_the_data_dir_file_and_must_be_a_path() {
        assert_eq!(
            Config::default().fork_handoff_prompt,
            PathBuf::from("prompts/fork-handoff.md")
        );
        let cfg = Config::parse(r#"{"fork_handoff_prompt":"/tmp/mine.md"}"#).unwrap();
        assert_eq!(cfg.fork_handoff_prompt, PathBuf::from("/tmp/mine.md"));
        for bad in [
            r#"{"fork_handoff_prompt":""}"#,
            r#"{"fork_handoff_prompt":3}"#,
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_default_fork_prompt_is_created_once_and_never_overwritten() {
        let dir = temp_dir();
        ensure_default_fork_prompt(&dir);
        let file = dir.join(DEFAULT_FORK_PROMPT_FILE);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), BUNDLED_FORK_PROMPT);
        std::fs::write(&file, "mine").unwrap();
        ensure_default_fork_prompt(&dir);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "mine");
        assert_eq!(read_fork_prompt(Some(&file)), ("mine".to_string(), None));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_empty_or_unreadable_fork_prompt_falls_back_with_a_warning() {
        let dir = temp_dir();
        let missing = dir.join("none.md");
        let (text, warning) = read_fork_prompt(Some(&missing));
        assert_eq!(text, BUNDLED_FORK_PROMPT);
        assert!(warning.unwrap().contains("cannot be read"));
        let empty = dir.join("empty.md");
        std::fs::write(&empty, " \n").unwrap();
        let (text, warning) = read_fork_prompt(Some(&empty));
        assert_eq!(text, BUNDLED_FORK_PROMPT);
        assert!(warning.unwrap().contains("is empty"));
        // A folder cannot be read as text.
        assert!(read_fork_prompt(Some(&dir)).1.is_some());
        assert_eq!(
            read_fork_prompt(None),
            (BUNDLED_FORK_PROMPT.to_string(), None)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_fork_resume_prompt_has_its_own_key_file_and_fallback() {
        assert_eq!(
            Config::default().fork_resume_prompt,
            PathBuf::from("prompts/fork-resume.md")
        );
        let cfg = Config::parse(r#"{"fork_resume_prompt":"/tmp/r.md"}"#).unwrap();
        assert_eq!(cfg.fork_resume_prompt, PathBuf::from("/tmp/r.md"));
        for bad in [
            r#"{"fork_resume_prompt":""}"#,
            r#"{"fork_resume_prompt":3}"#,
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
        let dir = temp_dir();
        ensure_default_fork_prompt(&dir);
        let file = dir.join(DEFAULT_FORK_RESUME_PROMPT_FILE);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            BUNDLED_FORK_RESUME_PROMPT
        );
        std::fs::write(&file, "mine").unwrap();
        ensure_default_fork_prompt(&dir);
        assert_eq!(
            read_fork_resume_prompt(Some(&file)),
            ("mine".to_string(), None)
        );
        let (text, warning) = read_fork_resume_prompt(Some(&dir.join("none.md")));
        assert_eq!(text, BUNDLED_FORK_RESUME_PROMPT);
        assert!(warning.unwrap().starts_with("Fork resume prompt"));
        assert!(BUNDLED_FORK_RESUME_PROMPT.starts_with("The attached file is the state"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_bundled_fork_prompt_is_the_shipped_asset() {
        assert!(BUNDLED_FORK_PROMPT.starts_with("Generate a dense, machine-efficient"));
        assert!(BUNDLED_FORK_PROMPT.contains("## NEXT ACTION"));
    }

    #[test]
    fn shortcuts_default_and_override() {
        let km = |text| Config::parse(text).unwrap().shortcuts;
        assert_eq!(km("{}"), Arc::new(Keymap::default()));
        let custom = km(r#"{"shortcuts":{"new_session":"cmd+shift+j"}}"#);
        let binding = custom.find(keymap::Action::NewSession).unwrap();
        assert_eq!(binding.keys.len(), 1);
        assert_eq!(
            binding.keys[0].mods,
            egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT)
        );
        assert_eq!(binding.keys[0].key, egui::Key::J);
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
            r#"{"shortcuts":[]}"#,
            r#"{"shortcuts":{"new_session":"cmd+"}}"#,
            r#"{"shortcuts":{"no_such_action":"cmd+j"}}"#,
            r#"{"shortcuts":{"new_session":"cmd+n"}}"#,
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
        assert!(written.contains("\"new_session\": \"cmd+t\""));

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

    #[test]
    fn restore_defaults_backs_up_the_old_file_and_parses_back_to_defaults() {
        let dir = std::env::temp_dir().join(format!("xade-cfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("settings.json");
        std::fs::write(&file, r#"{"font_size": 20}"#).unwrap();
        restore_defaults(&file).unwrap();
        let bak = std::fs::read_to_string(dir.join("settings.json.bak")).unwrap();
        assert!(bak.contains("20"));
        let cfg = Config::parse(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(cfg, Config::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
