//! Talking to the model: a prompt goes out and the reply comes back into the window.

use egui::{Key, Modifiers};
use egui_kittest::kittest::Queryable;

use crate::support::Xade;

/// Open a workspace, type a prompt, send it with the shortcut: the message shows in the chat and
/// the (mocked) model's reply streams back below it.
#[test]
fn a_prompt_is_sent_and_the_reply_comes_back() {
    let mut x = Xade::start(&[("README.md", "hi")], [1280.0, 800.0]);
    x.write("hello there");
    x.press(Modifiers::COMMAND, Key::Enter);

    x.wait_for("the model's reply", |ui| {
        ui.query_by_label("Echo: hello there").is_some()
    });
    assert!(
        x.ui.query_by_label("hello there").is_some(),
        "the prompt is in the chat"
    );
}
