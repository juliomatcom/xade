//! The single source of keyboard shortcuts: the dispatcher (`App::run_action`), the keymaps
//! modal and the button tooltips all read the current `Keymap`, built from the defaults below
//! and the `shortcuts` block of `settings.json`.

use std::sync::{Arc, LazyLock, PoisonError, RwLock};
use std::time::{Duration, Instant};

use egui::{Event, Key, Modifiers};

/// How long the second key of a chord may take.
pub const CHORD_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    NewSession,
    NewMultiSession,
    NewFile,
    OpenFile,
    Save,
    SaveAs,
    OpenPicker,
    SearchSessions,
    QuickOpen,
    Find,
    Send,
    /// While a turn runs: queue instead of steer, or the reverse.
    SendOther,
    Stop,
    CloseWindow,
    FocusWindow(u8),
    NextWindow,
    PrevWindow,
    Maximize,
    ToggleSidebar,
    Retile,
    ToggleView,
    ToggleAuto,
    Shortcuts,
    OpenSettings,
    CloseModal,
    /// Debug builds only: panics in the UI handler, to try the crash safety net.
    #[cfg(debug_assertions)]
    DebugPanic,
    /// Debug builds only: kills the store writer thread.
    #[cfg(debug_assertions)]
    DebugKillWriter,
}

impl Action {
    /// Handled where the key is read (composer, modals), so the dispatcher leaves it alone.
    fn is_local(self) -> bool {
        matches!(self, Action::Send | Action::SendOther | Action::CloseModal)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    Sessions,
    Files,
    Chat,
    Layout,
    General,
}

pub const GROUPS: [(Group, &str); 5] = [
    (Group::Sessions, "Sessions"),
    (Group::Files, "Files"),
    (Group::Chat, "Chat"),
    (Group::Layout, "Layout"),
    (Group::General, "General"),
];

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Stroke {
    pub mods: Modifiers,
    pub key: Key,
}

/// How the modal lists a binding.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Show {
    Row,
    /// One row for a run of bindings, shown as "first ... last" (the last is this action's).
    Through(Action),
    Hidden,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Binding {
    pub action: Action,
    pub group: Group,
    pub title: &'static str,
    /// The `settings.json` name; `None` for the fixed bindings.
    pub name: Option<&'static str>,
    /// One stroke, or two for a chord.
    pub keys: Vec<Stroke>,
    pub show: Show,
}

/// A binding as shipped: static metadata plus the default keys as `settings.json` text.
struct Def {
    name: Option<&'static str>,
    action: Action,
    group: Group,
    title: &'static str,
    keys: &'static str,
    show: Show,
}

const fn row(
    name: &'static str,
    action: Action,
    group: Group,
    title: &'static str,
    keys: &'static str,
) -> Def {
    Def {
        name: Some(name),
        action,
        group,
        title,
        keys,
        show: Show::Row,
    }
}

const fn focus(name: &'static str, n: u8, keys: &'static str) -> Def {
    Def {
        name: Some(name),
        action: Action::FocusWindow(n),
        group: Group::Layout,
        title: "Focus window 1-9",
        keys,
        show: if n == 1 {
            Show::Through(Action::FocusWindow(9))
        } else {
            Show::Hidden
        },
    }
}

/// A binding `settings.json` cannot change.
const fn fixed(action: Action, group: Group, title: &'static str, keys: &'static str) -> Def {
    Def {
        name: None,
        action,
        group,
        title,
        keys,
        show: Show::Row,
    }
}

/// A fixed binding the keymaps modal never lists.
#[cfg(debug_assertions)]
const fn hidden(action: Action, keys: &'static str) -> Def {
    Def {
        show: Show::Hidden,
        ..fixed(action, Group::General, "Debug", keys)
    }
}

static DEFS: &[Def] = &[
    row(
        "new_session",
        Action::NewSession,
        Group::Sessions,
        "New session in the selected workspace",
        "cmd+t",
    ),
    row(
        "new_multi_session",
        Action::NewMultiSession,
        Group::Sessions,
        "New multi-session in the selected workspace",
        "cmd+shift+t",
    ),
    row(
        "new_file",
        Action::NewFile,
        Group::Files,
        "New file",
        "cmd+n",
    ),
    row(
        "open_file",
        Action::OpenFile,
        Group::Files,
        "Open file",
        "cmd+o",
    ),
    row(
        "save",
        Action::Save,
        Group::Files,
        "Save the focused file",
        "cmd+s",
    ),
    row(
        "save_as",
        Action::SaveAs,
        Group::Files,
        "Save the focused file as",
        "cmd+shift+s",
    ),
    row(
        "open_picker",
        Action::OpenPicker,
        Group::Sessions,
        "Open session picker",
        "cmd+shift+p",
    ),
    row(
        "search_sessions",
        Action::SearchSessions,
        Group::Sessions,
        "Fuzzy search sessions",
        "cmd+shift+f",
    ),
    row(
        "quick_open",
        Action::QuickOpen,
        Group::Files,
        "Open file by name or path",
        "cmd+p",
    ),
    row(
        "close_window",
        Action::CloseWindow,
        Group::Sessions,
        "Close the focused window",
        "cmd+w",
    ),
    row(
        "send",
        Action::Send,
        Group::Chat,
        "Send prompt",
        "cmd+enter",
    ),
    row(
        "send_other",
        Action::SendOther,
        Group::Chat,
        "Send the other way while running (queue / steer)",
        "cmd+shift+enter",
    ),
    row(
        "stop",
        Action::Stop,
        Group::Chat,
        "Stop the current turn",
        "cmd+period",
    ),
    row(
        "toggle_auto",
        Action::ToggleAuto,
        Group::Chat,
        "Toggle Auto mode (focused session)",
        "cmd+shift+a",
    ),
    focus("focus_window_1", 1, "cmd+1"),
    focus("focus_window_2", 2, "cmd+2"),
    focus("focus_window_3", 3, "cmd+3"),
    focus("focus_window_4", 4, "cmd+4"),
    focus("focus_window_5", 5, "cmd+5"),
    focus("focus_window_6", 6, "cmd+6"),
    focus("focus_window_7", 7, "cmd+7"),
    focus("focus_window_8", 8, "cmd+8"),
    focus("focus_window_9", 9, "cmd+9"),
    row(
        "next_window",
        Action::NextWindow,
        Group::Layout,
        "Next window",
        "ctrl+tab",
    ),
    row(
        "prev_window",
        Action::PrevWindow,
        Group::Layout,
        "Previous window",
        "ctrl+shift+tab",
    ),
    row(
        "maximize",
        Action::Maximize,
        Group::Layout,
        "Maximize / restore the focused window",
        "cmd+k cmd+m",
    ),
    row(
        "retile",
        Action::Retile,
        Group::Layout,
        "Re-tile windows",
        "cmd+r",
    ),
    row(
        "toggle_sidebar",
        Action::ToggleSidebar,
        Group::Layout,
        "Toggle the sidebar",
        "cmd+b",
    ),
    row(
        "find",
        Action::Find,
        Group::General,
        "Find in the focused window",
        "cmd+f",
    ),
    row(
        "toggle_view",
        Action::ToggleView,
        Group::General,
        "Home / Workspace",
        "cmd+shift+h",
    ),
    row(
        "shortcuts",
        Action::Shortcuts,
        Group::General,
        "Keyboard shortcuts",
        "cmd+k cmd+s",
    ),
    row(
        "open_settings",
        Action::OpenSettings,
        Group::General,
        "Open settings (settings.json)",
        "cmd+comma",
    ),
    fixed(
        Action::CloseModal,
        Group::General,
        "Close a modal",
        "escape",
    ),
    #[cfg(debug_assertions)]
    hidden(Action::DebugPanic, "cmd+shift+alt+p"),
    #[cfg(debug_assertions)]
    hidden(Action::DebugKillWriter, "cmd+shift+alt+w"),
];

/// The default keys of every configurable action, as the `shortcuts` block of a new settings.json.
pub fn default_shortcuts() -> serde_json::Value {
    DEFS.iter()
        .filter_map(|d| Some((d.name?.to_string(), serde_json::Value::from(d.keys))))
        .collect::<serde_json::Map<_, _>>()
        .into()
}

fn parse_stroke(text: &str) -> Result<Stroke, String> {
    let parts: Vec<&str> = text.split('+').map(str::trim).collect();
    let (key, mod_parts) = parts.split_last().ok_or("empty shortcut")?;
    let mut mods = Modifiers::NONE;
    for m in mod_parts {
        mods = mods.plus(match m.to_ascii_lowercase().as_str() {
            "cmd" => Modifiers::COMMAND,
            "ctrl" => Modifiers::CTRL,
            "alt" => Modifiers::ALT,
            "shift" => Modifiers::SHIFT,
            _ => return Err(format!("unknown modifier \"{m}\" in \"{text}\"")),
        });
    }
    let mut capitalized = key.to_ascii_lowercase();
    if let Some(first) = capitalized.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    let key = Key::from_name(key)
        .or_else(|| Key::from_name(&capitalized))
        .ok_or_else(|| format!("unknown key \"{key}\" in \"{text}\""))?;
    Ok(Stroke { mods, key })
}

/// "cmd+t", or two space-separated strokes for a chord ("cmd+k cmd+m").
fn parse_keys(text: &str) -> Result<Vec<Stroke>, String> {
    let keys = text
        .split_whitespace()
        .map(parse_stroke)
        .collect::<Result<Vec<_>, _>>()?;
    match keys.len() {
        1 | 2 => Ok(keys),
        0 => Err("empty shortcut".into()),
        _ => Err(format!("\"{text}\" has more than two keys")),
    }
}

/// Every binding with its effective keys.
#[derive(Clone, PartialEq, Debug)]
pub struct Keymap {
    bindings: Vec<Binding>,
}

impl Default for Keymap {
    fn default() -> Self {
        Keymap::parse(&serde_json::Map::new()).unwrap_or(Keymap {
            bindings: Vec::new(),
        })
    }
}

impl Keymap {
    /// The defaults with `overrides` (action name -> keys text) applied. An unknown action, an
    /// invalid shortcut, two actions on the same keys, or a single key that also starts a
    /// chord is an error.
    pub fn parse(overrides: &serde_json::Map<String, serde_json::Value>) -> Result<Keymap, String> {
        if let Some(name) = overrides
            .keys()
            .find(|n| !DEFS.iter().any(|d| d.name == Some(n.as_str())))
        {
            return Err(format!("shortcuts: unknown action \"{name}\""));
        }
        let mut bindings = Vec::with_capacity(DEFS.len());
        for d in DEFS {
            let who = d.name.unwrap_or(d.title);
            let text = match d.name.and_then(|n| overrides.get(n)) {
                None => d.keys,
                Some(serde_json::Value::String(s)) => s,
                Some(_) => return Err(format!("shortcuts.{who}: expected a string")),
            };
            let keys = parse_keys(text).map_err(|e| format!("shortcuts.{who}: {e}"))?;
            if d.action.is_local() && keys.len() != 1 {
                return Err(format!("shortcuts.{who}: must be a single key"));
            }
            bindings.push(Binding {
                action: d.action,
                group: d.group,
                title: d.title,
                name: d.name,
                keys,
                show: d.show,
            });
        }
        for (i, a) in bindings.iter().enumerate() {
            for b in &bindings[i + 1..] {
                let clash = a.keys == b.keys
                    || matches!((a.keys.as_slice(), b.keys.as_slice()),
                        ([s], [f, _]) | ([f, _], [s]) if s == f);
                if clash {
                    return Err(format!(
                        "shortcuts: {} and {} clash (same keys, or one is the start of the other's chord)",
                        a.name.unwrap_or(a.title),
                        b.name.unwrap_or(b.title)
                    ));
                }
            }
        }
        Ok(Keymap { bindings })
    }

    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    pub fn find(&self, action: Action) -> Option<&Binding> {
        self.bindings.iter().find(|b| b.action == action)
    }

    /// "⌘N", "⌘K ⌘S" (or "Ctrl+N", "Ctrl+K Ctrl+S" off macOS).
    pub fn label(&self, b: &Binding, mac: bool) -> String {
        let mut text = keys_label(&b.keys, mac);
        if let Show::Through(last) = b.show {
            if let Some(last) = self.find(last) {
                text.push_str(" \u{2026} ");
                text.push_str(&keys_label(&last.keys, mac));
            }
        }
        text
    }
}

/// The keymap in effect, replaced when `settings.json` is (re)applied.
static CURRENT: LazyLock<RwLock<Arc<Keymap>>> =
    LazyLock::new(|| RwLock::new(Arc::new(Keymap::default())));

pub fn current() -> Arc<Keymap> {
    Arc::clone(&CURRENT.read().unwrap_or_else(PoisonError::into_inner))
}

pub fn set_current(keymap: Arc<Keymap>) {
    *CURRENT.write().unwrap_or_else(PoisonError::into_inner) = keymap;
}

fn hits(s: &Stroke, mods: Modifiers, key: Key) -> bool {
    s.key == key && mods.matches_exact(s.mods)
}

/// Consumes this frame's press of the local Send or SendOther key (the composer reads these
/// itself); the stroke with more modifiers goes first because `consume_key` ignores extra
/// Shift and Alt.
pub fn take_send(input: &mut egui::InputState) -> Option<Action> {
    let km = current();
    let mut strokes: Vec<(Action, Stroke)> = [Action::Send, Action::SendOther]
        .into_iter()
        .filter_map(|a| Some((a, *km.find(a)?.keys.first()?)))
        .collect();
    let weight = |m: Modifiers| [m.shift, m.alt, m.ctrl, m.command, m.mac_cmd].map(u8::from);
    strokes.sort_by_key(|(_, s)| std::cmp::Reverse(weight(s.mods).iter().sum::<u8>()));
    strokes
        .into_iter()
        .find(|(_, s)| input.consume_key(s.mods, s.key))
        .map(|(a, _)| a)
}

/// What one key press did to the chord state.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Feed {
    Ignored,
    /// The first key of a chord; waiting for the second.
    Armed,
    Fired(Action),
    /// A chord was pending and this key (or the timeout) ended it.
    Cancelled,
}

/// Chord state: the first stroke and when it was pressed.
#[derive(Default)]
pub struct Chord {
    armed: Option<(Instant, Stroke)>,
}

impl Chord {
    /// Whether a chord is waiting for its second key.
    pub fn armed(&self, now: Instant) -> bool {
        self.remaining(now).is_some()
    }

    /// The first stroke of the pending chord, for the status bar hint.
    pub fn pending(&self, now: Instant) -> Option<Stroke> {
        self.remaining(now).and(self.armed).map(|(_, s)| s)
    }

    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        let (at, _) = self.armed?;
        CHORD_TIMEOUT
            .checked_sub(now.saturating_duration_since(at))
            .filter(|d| !d.is_zero())
    }

    /// Drops a chord whose time ran out.
    pub fn expire(&mut self, now: Instant) {
        if !self.armed(now) {
            self.armed = None;
        }
    }

    /// Feeds one key press (not a repeat).
    pub fn feed(&mut self, km: &Keymap, now: Instant, mods: Modifiers, key: Key) -> Feed {
        if let Some((at, first)) = self.armed.take() {
            if now.saturating_duration_since(at) < CHORD_TIMEOUT {
                return second(km, &first, mods, key);
            }
        }
        for b in km.bindings.iter().filter(|b| !b.action.is_local()) {
            match b.keys.as_slice() {
                [s] if hits(s, mods, key) => return Feed::Fired(b.action),
                [s, _] if hits(s, mods, key) => {
                    self.armed = Some((now, *s));
                    return Feed::Armed;
                }
                _ => {}
            }
        }
        Feed::Ignored
    }
}

/// The chord starting with `first` that `key` completes; Cmd may already be released.
fn second(km: &Keymap, first: &Stroke, mods: Modifiers, key: Key) -> Feed {
    km.bindings
        .iter()
        .find_map(|b| match b.keys.as_slice() {
            [f, s]
                if f == first && s.key == key && (mods.matches_exact(s.mods) || mods.is_none()) =>
            {
                Some(Feed::Fired(b.action))
            }
            _ => None,
        })
        .unwrap_or(Feed::Cancelled)
}

/// Reads this frame's key presses, consuming the ones that fire or arm a shortcut so text
/// fields never see them.
pub fn poll(input: &mut egui::InputState, chord: &mut Chord, now: Instant) -> Vec<Action> {
    let presses: Vec<(Key, Modifiers)> = input
        .events
        .iter()
        .filter_map(|e| match e {
            Event::Key {
                key,
                pressed: true,
                repeat: false,
                modifiers,
                ..
            } => Some((*key, *modifiers)),
            _ => None,
        })
        .collect();
    let km = current();
    let mut out = Vec::new();
    for (key, mods) in presses {
        let was_armed = chord.armed(now);
        match chord.feed(&km, now, mods, key) {
            Feed::Fired(action) => {
                input.consume_key(mods, key);
                if was_armed {
                    // With Cmd already released the second key also arrives as text.
                    input.events.retain(
                        |e| !matches!(e, Event::Text(t) if t.eq_ignore_ascii_case(key.name())),
                    );
                }
                out.push(action);
            }
            Feed::Armed => {
                input.consume_key(mods, key);
            }
            Feed::Ignored | Feed::Cancelled => {}
        }
    }
    out
}

// ----- labels -----

fn key_text(key: Key) -> &'static str {
    match key {
        Key::Escape => "Esc",
        k => k.symbol_or_name(),
    }
}

/// Only ⌘ exists in egui's default fonts (checked by `cmd_glyph_exists`); the other
/// modifiers are spelled out.
pub fn stroke_label(s: &Stroke, mac: bool) -> String {
    let m = s.mods;
    let cmd = m.command || m.mac_cmd;
    let mut out = String::new();
    for (on, name) in [
        (m.ctrl || (cmd && !mac), "Ctrl+"),
        (m.alt, "Alt+"),
        (m.shift, "Shift+"),
        (cmd && mac, MAC_CMD),
    ] {
        if on {
            out.push_str(name);
        }
    }
    out + key_text(s.key)
}

pub const MAC_CMD: &str = "\u{2318}";

fn keys_label(keys: &[Stroke], mac: bool) -> String {
    keys.iter()
        .map(|s| stroke_label(s, mac))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The label of the first binding of `action`, for tooltips.
pub fn shortcut_text(action: Action) -> String {
    let km = current();
    km.find(action)
        .map(|b| km.label(b, cfg!(target_os = "macos")))
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    const CMD: Modifiers = Modifiers::COMMAND;
    const CMD_SHIFT: Modifiers = Modifiers::COMMAND.plus(Modifiers::SHIFT);

    fn press(c: &mut Chord, now: Instant, mods: Modifiers, key: Key) -> Feed {
        c.feed(&Keymap::default(), now, mods, key)
    }

    fn with(overrides: serde_json::Value) -> Result<Keymap, String> {
        Keymap::parse(overrides.as_object().unwrap())
    }

    #[test]
    fn defaults_parse_and_do_not_clash() {
        let km = Keymap::default();
        assert_eq!(km.bindings().len(), DEFS.len());
        assert_eq!(with(default_shortcuts()).unwrap(), km);
        assert_eq!(
            km.find(Action::NewSession).unwrap().keys,
            [Stroke {
                mods: CMD,
                key: Key::T
            }]
        );
        assert_eq!(km.find(Action::Maximize).unwrap().keys.len(), 2);
    }

    #[test]
    fn file_and_session_shortcuts_are_distinct() {
        let now = Instant::now();
        let mut c = Chord::default();
        assert_eq!(
            press(&mut c, now, CMD, Key::N),
            Feed::Fired(Action::NewFile)
        );
        assert_eq!(
            press(&mut c, now, CMD, Key::T),
            Feed::Fired(Action::NewSession)
        );
        assert_eq!(press(&mut c, now, CMD, Key::S), Feed::Fired(Action::Save));
        assert_eq!(
            press(&mut c, now, CMD_SHIFT, Key::S),
            Feed::Fired(Action::SaveAs)
        );
    }

    #[test]
    fn every_binding_fires_its_action() {
        let now = Instant::now();
        let km = Keymap::default();
        for b in km.bindings().iter().filter(|b| !b.action.is_local()) {
            let mut c = Chord::default();
            let mut last = Feed::Ignored;
            for s in &b.keys {
                last = c.feed(&km, now, s.mods, s.key);
            }
            assert_eq!(last, Feed::Fired(b.action));
        }
    }

    #[test]
    fn chord_fires_within_timeout_and_cancels_otherwise() {
        let t0 = Instant::now();
        let mut c = Chord::default();
        assert_eq!(press(&mut c, t0, CMD, Key::K), Feed::Armed);
        assert!(c.armed(t0));
        let soon = t0 + Duration::from_millis(1000);
        assert_eq!(
            press(&mut c, soon, CMD, Key::S),
            Feed::Fired(Action::Shortcuts)
        );
        assert!(!c.armed(soon));

        // Second key without Cmd still counts.
        press(&mut c, t0, CMD, Key::K);
        assert_eq!(
            press(&mut c, soon, Modifiers::NONE, Key::S),
            Feed::Fired(Action::Shortcuts)
        );

        // Timeout: the late key is a fresh press (plain Cmd+S saves), not the chord's second key.
        press(&mut c, t0, CMD, Key::K);
        let late = t0 + CHORD_TIMEOUT + Duration::from_millis(1);
        assert!(!c.armed(late));
        assert_eq!(press(&mut c, late, CMD, Key::S), Feed::Fired(Action::Save));

        // Another key cancels.
        press(&mut c, t0, CMD, Key::K);
        assert_eq!(press(&mut c, soon, CMD, Key::Q), Feed::Cancelled);
        assert_eq!(press(&mut c, soon, CMD, Key::S), Feed::Fired(Action::Save));

        // expire() drops a stale chord.
        press(&mut c, t0, CMD, Key::K);
        c.expire(late);
        assert!(c.remaining(late).is_none());
    }

    #[test]
    fn shift_variants_do_not_cross_fire() {
        let now = Instant::now();
        let mut c = Chord::default();
        assert_eq!(press(&mut c, now, CMD, Key::F), Feed::Fired(Action::Find));
        assert_eq!(
            press(&mut c, now, CMD_SHIFT, Key::F),
            Feed::Fired(Action::SearchSessions)
        );
        assert_eq!(
            press(&mut c, now, CMD, Key::T),
            Feed::Fired(Action::NewSession)
        );
        assert_eq!(
            press(&mut c, now, CMD_SHIFT, Key::T),
            Feed::Fired(Action::NewMultiSession)
        );
        assert_eq!(press(&mut c, now, CMD, Key::H), Feed::Ignored);
        assert_eq!(
            press(&mut c, now, CMD_SHIFT, Key::H),
            Feed::Fired(Action::ToggleView)
        );
        assert_eq!(
            press(&mut c, now, Modifiers::CTRL, Key::Tab),
            Feed::Fired(Action::NextWindow)
        );
        assert_eq!(
            press(
                &mut c,
                now,
                Modifiers::CTRL.plus(Modifiers::SHIFT),
                Key::Tab
            ),
            Feed::Fired(Action::PrevWindow)
        );
    }

    #[test]
    fn labels_read_naturally() {
        let km = Keymap::default();
        let label = |a, mac| km.label(km.find(a).expect("binding"), mac);
        assert_eq!(label(Action::NewFile, true), "\u{2318}N");
        assert_eq!(label(Action::NewSession, true), "\u{2318}T");
        assert_eq!(label(Action::Shortcuts, true), "\u{2318}K \u{2318}S");
        assert_eq!(label(Action::NextWindow, true), "Ctrl+Tab");
        assert_eq!(label(Action::ToggleView, true), "Shift+\u{2318}H");
        assert_eq!(label(Action::Shortcuts, false), "Ctrl+K Ctrl+S");
        assert_eq!(
            label(Action::FocusWindow(1), true),
            "\u{2318}1 \u{2026} \u{2318}9"
        );
    }

    #[test]
    fn strokes_parse_with_modifiers_and_key_names() {
        let stroke = |t| parse_stroke(t).unwrap();
        assert_eq!(
            stroke("cmd+t"),
            Stroke {
                mods: CMD,
                key: Key::T
            }
        );
        assert_eq!(stroke("Cmd+Shift+A").mods, CMD_SHIFT);
        assert_eq!(stroke("ctrl+tab").key, Key::Tab);
        assert_eq!(stroke("alt+f5").mods, Modifiers::ALT);
        assert_eq!(stroke("cmd+f12").key, Key::F12);
        assert_eq!(stroke("cmd+period").key, Key::Period);
        assert_eq!(stroke("escape").mods, Modifiers::NONE);
        assert_eq!(stroke("cmd+0").key, Key::Num0);
        assert_eq!(parse_keys("cmd+k  cmd+m").unwrap().len(), 2);
        for bad in ["", "cmd+", "cmd+nokey", "hyper+t", "cmd+k cmd+m cmd+t"] {
            assert!(parse_keys(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn overrides_replace_only_the_named_actions() {
        let km = with(json!({"new_session": "cmd+j", "maximize": "cmd+k cmd+x"})).unwrap();
        let now = Instant::now();
        let mut c = Chord::default();
        let mut feed = |mods, key| c.feed(&km, now, mods, key);
        assert_eq!(feed(CMD, Key::J), Feed::Fired(Action::NewSession));
        assert_eq!(feed(CMD, Key::T), Feed::Ignored);
        assert_eq!(feed(CMD, Key::N), Feed::Fired(Action::NewFile));
        assert_eq!(feed(CMD, Key::K), Feed::Armed);
        assert_eq!(feed(CMD, Key::X), Feed::Fired(Action::Maximize));
        assert_eq!(feed(CMD, Key::K), Feed::Armed);
        assert_eq!(feed(CMD, Key::M), Feed::Cancelled);
        assert_eq!(
            km.label(km.find(Action::NewSession).unwrap(), true),
            "\u{2318}J"
        );
    }

    #[test]
    fn invalid_shortcuts_are_errors() {
        for bad in [
            json!({"nope": "cmd+j"}),
            json!({"escape": "cmd+j"}),
            json!({"new_session": 3}),
            json!({"new_session": null}),
            json!({"new_session": "cmd+"}),
            json!({"new_session": "cmd+n"}),
            json!({"new_session": "escape"}),
            // A lone key that starts a chord.
            json!({"new_session": "cmd+k"}),
            json!({"toggle_sidebar": "cmd+k"}),
            json!({"send": "cmd+k cmd+enter"}),
        ] {
            assert!(with(bad.clone()).is_err(), "{bad}");
        }
        // Moving a key away frees it for another action.
        assert!(with(json!({"new_file": "cmd+j", "new_session": "cmd+n"})).is_ok());
        let err = with(json!({"new_session": "cmd+n"})).unwrap_err();
        assert!(
            err.contains("new_session") && err.contains("new_file"),
            "{err}"
        );
    }

    #[test]
    fn cmd_glyph_exists() {
        let ctx = egui::Context::default();
        ctx.run_pass(egui::RawInput::default(), |_| {})
            .drop_without_applying_deltas();
        let missing: Vec<&str> = ctx.fonts_mut(|f| {
            [MAC_CMD]
                .into_iter()
                .filter(|g| !f.has_glyphs(&egui::FontId::proportional(13.5), g))
                .collect()
        });
        assert!(missing.is_empty(), "missing glyphs: {missing:?}");
    }
}
