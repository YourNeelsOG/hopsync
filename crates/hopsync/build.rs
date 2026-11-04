//! Compiles the built web UI into the binary, so a server needs no Node, npm or asset files.
//!
//! Walks `web/dist` (the output of `pnpm build` in `web/`) and writes a table of `include_bytes!`
//! entries for `src/api.rs`. Without a built UI, the binary serves a page saying how to build one,
//! so Rust contributors can build and test without Node. Release builds set
//! `HOPSYNC_REQUIRE_UI=1`, which makes a missing UI a build error.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs};

const PLACEHOLDER: &str = "<!doctype html>\n<html lang=\"en\">\n<meta charset=\"utf-8\">\n\
<title>Hopsync</title>\n<h1>The web UI was not built</h1>\n\
<p>Run <code>pnpm install --frozen-lockfile &amp;&amp; pnpm build</code> in \
<code>crates/hopsync/web</code>, then build Hopsync again.</p>\n</html>\n";

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        // Anything else in the output is a surprise worth stopping for: it would ship to browsers.
        _ => panic!("unexpected file in web/dist: {}", path.display()),
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read web/dist") {
        let path = entry.expect("read web/dist entry").path();
        if path.is_dir() {
            walk(&path, out);
        } else if path.extension().is_none_or(|e| e != "gz") {
            out.push(path);
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=web/dist");
    println!("cargo:rerun-if-env-changed=HOPSYNC_REQUIRE_UI");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("out dir"));
    let dist = manifest.join("web/dist");

    let mut table = String::from("const ASSETS: &[Asset] = &[\n");
    if dist.join("index.html").is_file() {
        let mut files = Vec::new();
        walk(&dist, &mut files);
        files.sort();
        for file in &files {
            let rel = file.strip_prefix(&dist).expect("file under dist");
            let rel = rel.to_str().expect("UTF-8 asset name").replace('\\', "/");
            let name = if rel == "index.html" { "" } else { &rel };
            let gz = PathBuf::from(format!("{}.gz", file.display()));
            let gz = if gz.is_file() {
                format!("Some(include_bytes!({:?}))", gz.display().to_string())
            } else {
                "None".to_owned()
            };
            writeln!(
                table,
                "    Asset {{ name: {name:?}, ctype: {:?}, body: include_bytes!({:?}), gzip: {gz} }},",
                content_type(file),
                file.display().to_string(),
            )
            .expect("write to string");
        }
    } else {
        assert!(
            env::var_os("HOPSYNC_REQUIRE_UI").is_none(),
            "HOPSYNC_REQUIRE_UI is set but {} has no index.html: run pnpm build in web/",
            dist.display()
        );
        println!("cargo:warning=web/dist is missing: this binary has no web UI (run pnpm build)");
        writeln!(
            table,
            "    Asset {{ name: \"\", ctype: \"text/html; charset=utf-8\", body: {PLACEHOLDER:?}.as_bytes(), gzip: None }},"
        )
        .expect("write to string");
    }
    table.push_str("];\n");
    fs::write(out_dir.join("ui_assets.rs"), table).expect("write ui_assets.rs");
}
