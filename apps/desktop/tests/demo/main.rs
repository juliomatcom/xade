//! Records the website's and README's media from the real app: the same startup, UI, session
//! manager and store as `xade <folder>`, with only the models scripted (`story.rs`). Ignored by
//! default; run it through `scripts/record-demo.sh`, which also encodes the web assets.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod provider;
mod stage;
mod story;

#[test]
#[ignore = "renders media on a GPU or Mesa; run scripts/record-demo.sh"]
fn record() {
    let out = std::env::var_os("XADE_DEMO_OUT").expect("set XADE_DEMO_OUT to an output folder");
    story::record(std::path::Path::new(&out));
}
