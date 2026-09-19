//! Contract: the release profile must unwind on panic. With `abort`, a panic
//! in any task (one bad publisher hitting a parser bug) kills every stream on
//! the server; tests run in debug, which always unwinds, so no other test
//! can catch that regression.

#[test]
fn release_profile_unwinds_on_panic() {
    let manifest = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml")).unwrap();
    let doc: toml::Table = manifest.parse().unwrap();
    let panic = doc
        .get("profile")
        .and_then(|p| p.get("release"))
        .and_then(|r| r.get("panic"))
        .and_then(|v| v.as_str())
        .unwrap_or("unwind");
    assert_eq!(panic, "unwind", "[profile.release] panic must be \"unwind\"");
}
