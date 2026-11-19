use super::*;

#[test]
fn build_html_contains_cdn_and_colors() {
    let html = build_html(
        Path::new("Assets\\Kuju\\Product\\signal_metal.TgPcDx"),
        &[],
        &[],
        &[],
    );
    assert!(html.contains(VIS_NETWORK_CDN));
    assert!(html.contains("#3b82f6"));
    assert!(html.contains("#10b981"));
    assert!(html.contains("#f59e0b"));
    assert!(html.contains("#ef4444"));
    assert!(html.contains("forceAtlas2Based"));
    assert!(html.contains("damping"));
}

#[test]
fn missing_node_gets_prefix_and_dash() {
    let html = build_html(
        Path::new("Assets\\Kuju\\Product\\center.bin"),
        &[],
        &[],
        &[PathBuf::from("Assets\\Kuju\\Product\\missing.TgPcDx")],
    );
    assert!(html.contains("[Missing] missing.TgPcDx"));
    assert!(html.contains("missing.TgPcDx"));
}
