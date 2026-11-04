//! The web UI ships without a build step, so nothing compiles it: check its syntax with node when
//! node is installed (Decision log 2026-11-04, web UI stack).

use std::process::Command;

#[test]
fn app_js_parses() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/web/app.js");
    let out = match Command::new("node").arg("--check").arg(path).output() {
        Ok(out) => out,
        Err(e) => {
            eprintln!("skipping: node not available ({e})");
            return;
        }
    };
    assert!(
        out.status.success(),
        "node --check app.js failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
