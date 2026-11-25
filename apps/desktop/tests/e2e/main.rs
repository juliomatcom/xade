//! End-to-end tests: the real app, started the way `xade <folder>` starts it (the same
//! `xade::backend` and `App`, over a throwaway data folder), driven like a person: key presses,
//! typed text and clicks on what is on screen, found by its label. Run them with
//! `cargo test -p xade --test e2e`.
//!
//! Only core flows belong here (they are slow compared with the unit tests); every other kind
//! of test lives beside the code it tests. Each flow is one test in a file named for its area,
//! and a new one is proven by breaking the feature and watching it fail.

mod files;
mod support;
