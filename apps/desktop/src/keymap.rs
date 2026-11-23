//! The single source of keyboard shortcuts: the dispatcher (`App::run_action`), the keymaps
//! modal and the button tooltips all read `BINDINGS`. There is no user remapping.

use std::time::{Duration, Instant};

use egui::{Event, Key, Modifiers};

/// How long the second key of a chord may take.
pub const CHORD_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    NewSession,
    NewFile,
    OpenFile,
    Save,
    SaveAs,
    OpenPicker,
    Send,
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
        matches!(self, Action::Send | Action::CloseModal)
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
    /// One row for a run of bindings, shown as "first ... last".
    Through(Stroke),
    Hidden,
}

pub struct Binding {
    pub action: Action,
    pub group: Group,
    pub title: &'static str,
    /// One stroke, or two for a chord.
    pub keys: &'static [Stroke],
    pub show: Show,
}

const CMD: Modifiers = Modifiers::COMMAND;
const CMD_SHIFT: Modifiers = Modifiers::COMMAND.plus(Modifiers::SHIFT);

#[cfg(debug_assertions)]
const CMD_SHIFT_ALT: Modifiers = CMD_SHIFT.plus(Modifiers::ALT);

const fn k(mods: Modifiers, key: Key) -> Stroke {
    Stroke { mods, key }
}

const fn row(
    action: Action,
    group: Group,
    title: &'static str,
    keys: &'static [Stroke],
) -> Binding {
    Binding {
        action,
        group,
        title,
        keys,
        show: Show::Row,
    }
}

/// A binding the keymaps modal never lists.
#[cfg(debug_assertions)]
const fn hidden(action: Action, keys: &'static [Stroke]) -> Binding {
    Binding {
        action,
        group: Group::General,
        title: "Debug",
        keys,
        show: Show::Hidden,
    }
}

const fn focus(n: u8, keys: &'static [Stroke]) -> Binding {
    Binding {
        action: Action::FocusWindow(n),
        group: Group::Layout,
        title: "Focus window 1-9",
        keys,
        show: if n == 1 {
            Show::Through(k(CMD, Key::Num9))
        } else {
            Show::Hidden
        },
    }
}

pub static BINDINGS: &[Binding] = &[
    row(
        Action::NewSession,
        Group::Sessions,
        "New session in the selected workspace",
        &[k(CMD_SHIFT, Key::N)],
    ),
    row(Action::NewFile, Group::Files, "New file", &[k(CMD, Key::N)]),
    row(
        Action::OpenFile,
        Group::Files,
        "Open file",
        &[k(CMD, Key::O)],
    ),
    row(
        Action::Save,
        Group::Files,
        "Save the focused file",
        &[k(CMD, Key::S)],
    ),
    row(
        Action::SaveAs,
        Group::Files,
        "Save the focused file as",
        &[k(CMD_SHIFT, Key::S)],
    ),
    row(
        Action::OpenPicker,
        Group::Sessions,
        "Open session picker",
        &[k(CMD, Key::P)],
    ),
    row(
        Action::CloseWindow,
        Group::Sessions,
        "Close the focused window",
        &[k(CMD, Key::W)],
    ),
    row(
        Action::Send,
        Group::Chat,
        "Send prompt",
        &[k(CMD, Key::Enter)],
    ),
    row(
        Action::Stop,
        Group::Chat,
        "Stop the current turn",
        &[k(CMD, Key::Period)],
    ),
    row(
        Action::ToggleAuto,
        Group::Chat,
        "Toggle Auto mode (focused session)",
        &[k(CMD_SHIFT, Key::A)],
    ),
    focus(1, &[k(CMD, Key::Num1)]),
    focus(2, &[k(CMD, Key::Num2)]),
    focus(3, &[k(CMD, Key::Num3)]),
    focus(4, &[k(CMD, Key::Num4)]),
    focus(5, &[k(CMD, Key::Num5)]),
    focus(6, &[k(CMD, Key::Num6)]),
    focus(7, &[k(CMD, Key::Num7)]),
    focus(8, &[k(CMD, Key::Num8)]),
    focus(9, &[k(CMD, Key::Num9)]),
    row(
        Action::NextWindow,
        Group::Layout,
        "Next window",
        &[k(Modifiers::CTRL, Key::Tab)],
    ),
    row(
        Action::PrevWindow,
        Group::Layout,
        "Previous window",
        &[k(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::Tab)],
    ),
    row(
        Action::Maximize,
        Group::Layout,
        "Maximize / restore the focused window",
        &[k(CMD, Key::K), k(CMD, Key::M)],
    ),
    row(
        Action::Retile,
        Group::Layout,
        "Re-tile windows",
        &[k(CMD, Key::K), k(CMD, Key::T)],
    ),
    row(
        Action::ToggleSidebar,
        Group::Layout,
        "Toggle the sidebar",
        &[k(CMD, Key::B)],
    ),
    row(
        Action::ToggleView,
        Group::General,
        "Home / Workspace",
        &[k(CMD_SHIFT, Key::H)],
    ),
    row(
        Action::Shortcuts,
        Group::General,
        "Keyboard shortcuts",
        &[k(CMD, Key::K), k(CMD, Key::S)],
    ),
    row(
        Action::OpenSettings,
        Group::General,
        "Open settings (settings.json)",
        &[k(CMD, Key::Comma)],
    ),
    row(
        Action::CloseModal,
        Group::General,
        "Close a modal",
        &[k(Modifiers::NONE, Key::Escape)],
    ),
    #[cfg(debug_assertions)]
    hidden(Action::DebugPanic, &[k(CMD_SHIFT_ALT, Key::P)]),
    #[cfg(debug_assertions)]
    hidden(Action::DebugKillWriter, &[k(CMD_SHIFT_ALT, Key::W)]),
];

fn hits(s: &Stroke, mods: Modifiers, key: Key) -> bool {
    s.key == key && mods.matches_exact(s.mods)
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
    pub fn feed(&mut self, now: Instant, mods: Modifiers, key: Key) -> Feed {
        if let Some((at, first)) = self.armed.take() {
            if now.saturating_duration_since(at) < CHORD_TIMEOUT {
                return second(&first, mods, key);
            }
        }
        for b in BINDINGS.iter().filter(|b| !b.action.is_local()) {
            match b.keys {
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
fn second(first: &Stroke, mods: Modifiers, key: Key) -> Feed {
    BINDINGS
        .iter()
        .find_map(|b| match b.keys {
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
    let mut out = Vec::new();
    for (key, mods) in presses {
        let was_armed = chord.armed(now);
        match chord.feed(now, mods, key) {
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

/// "⌘N", "⌘K ⌘S" (or "Ctrl+N", "Ctrl+K Ctrl+S" off macOS).
pub fn binding_label(b: &Binding, mac: bool) -> String {
    let mut text = b
        .keys
        .iter()
        .map(|s| stroke_label(s, mac))
        .collect::<Vec<_>>()
        .join(" ");
    if let Show::Through(last) = b.show {
        text.push_str(" \u{2026} ");
        text.push_str(&stroke_label(&last, mac));
    }
    text
}

/// The label of the first binding of `action`, for tooltips.
pub fn shortcut_text(action: Action) -> String {
    BINDINGS
        .iter()
        .find(|b| b.action == action)
        .map(|b| binding_label(b, cfg!(target_os = "macos")))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(c: &mut Chord, now: Instant, mods: Modifiers, key: Key) -> Feed {
        c.feed(now, mods, key)
    }

    #[test]
    fn no_duplicate_bindings() {
        for (i, a) in BINDINGS.iter().enumerate() {
            for b in &BINDINGS[i + 1..] {
                assert!(
                    a.keys != b.keys,
                    "{:?} and {:?} share keys",
                    a.action,
                    b.action
                );
            }
            // A lone stroke may not also start a chord.
            if let [s] = a.keys {
                assert!(
                    !BINDINGS
                        .iter()
                        .any(|b| b.keys.len() == 2 && b.keys[0] == *s),
                    "{:?} shadows a chord prefix",
                    a.action
                );
            }
        }
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
            press(&mut c, now, CMD_SHIFT, Key::N),
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
        for b in BINDINGS.iter().filter(|b| !b.action.is_local()) {
            let mut c = Chord::default();
            let mut last = Feed::Ignored;
            for s in b.keys {
                last = press(&mut c, now, s.mods, s.key);
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
        let find = |a| BINDINGS.iter().find(|b| b.action == a).expect("binding");
        assert_eq!(binding_label(find(Action::NewFile), true), "\u{2318}N");
        assert_eq!(
            binding_label(find(Action::NewSession), true),
            "Shift+\u{2318}N"
        );
        assert_eq!(
            binding_label(find(Action::Shortcuts), true),
            "\u{2318}K \u{2318}S"
        );
        assert_eq!(binding_label(find(Action::NextWindow), true), "Ctrl+Tab");
        assert_eq!(
            binding_label(find(Action::ToggleView), true),
            "Shift+\u{2318}H"
        );
        assert_eq!(
            binding_label(find(Action::Shortcuts), false),
            "Ctrl+K Ctrl+S"
        );
        assert_eq!(
            binding_label(find(Action::FocusWindow(1)), true),
            "\u{2318}1 \u{2026} \u{2318}9"
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
