//! The wasm viewer, embedded into the bridge at build time (board-b0o).
//!
//! One artifact: the binary serves its own UI, so the bridge and the payload it renders can never
//! be different versions. `build.rs` writes the table; this module is only lookup.

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

/// Whether a real bundle was embedded (as opposed to the placeholder page).
pub fn embedded() -> bool {
    EMBEDDED
}

/// How many files are inside, for the startup line.
pub fn count() -> usize {
    ASSETS.len()
}

/// Look up one asset by its path under `/app/`. `""` and any `…/` mean `index.html`.
///
/// Unknown or traversing paths return `None` — the table has no `..` entries to reach.
pub fn get(path: &str) -> Option<(&'static [u8], &'static str)> {
    let path = path.trim_start_matches('/');
    let name = if path.is_empty() || path.ends_with('/') {
        format!("{path}index.html")
    } else if path.contains("..") {
        return None;
    } else {
        path.to_string()
    };
    ASSETS
        .iter()
        .find(|(file, _)| *file == name)
        .map(|(_, bytes)| (*bytes, content_type(&name)))
}

/// What the browser needs to know about the bytes, by extension.
fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_is_reachable_by_both_spellings() {
        let (root, root_type) = get("").expect("the bundle always has an index");
        let (slash, _) = get("/").expect("/ is the same page");
        assert_eq!(root, slash);
        assert_eq!(root_type, "text/html; charset=utf-8");
        assert!(!root.is_empty());
    }

    #[test]
    fn only_real_assets_are_served() {
        assert!(get("nope.js").is_none());
        assert!(get("../Cargo.toml").is_none());
        assert!(get("a/../../etc/passwd").is_none());
    }

    #[test]
    fn the_bundle_is_what_the_build_embedded() {
        // Either a real dist (trunk output) or the placeholder; both must be a page, and the
        // placeholder is one file while a real build carries at least html + js + wasm.
        assert!(count() >= 1);
        if embedded() {
            assert!(count() >= 3, "a real bundle is html + js + wasm");
            assert!(
                ASSETS.iter().any(|(name, _)| name.ends_with(".wasm")),
                "the wasm itself must be embedded"
            );
        }
    }
}
