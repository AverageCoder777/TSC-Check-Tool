use super::append_asset_replacement_log;
use std::fs;

#[test]
fn appends_replacement_records_as_json_lines() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("logs").join("replacement_log.jsonl");
    let mut entry = crate::domain::AssetReplacementLogEntry {
        timestamp_unix_seconds: 0,
        operation: "Замена отсутствующего ассета".to_owned(),
        route_id: Some("route-id".to_owned()),
        route_name: Some("crate::domain::Route".to_owned()),
        route_path: Some(temp.path().join("route")),
        original_files: vec![
            temp.path()
                .join("route/Scenery/tile.bin")
                .display()
                .to_string(),
        ],
        missing_asset: Some("A/B/Scenery/old.bin".to_owned()),
        replacement_asset: Some("A/B/Scenery/new.bin".to_owned()),
        replaced_files: 1,
        replaced_instances: 2,
        uses: vec![crate::domain::AssetReplacementLogUse {
            tile: "тайл X=+000001 Z=-000002".to_owned(),
            source_file: temp
                .path()
                .join("route/Scenery/tile.bin")
                .display()
                .to_string(),
        }],
        backups: vec![temp.path().join("route/Scenery/tile.bin.bak1")],
    };

    append_asset_replacement_log(&path, &mut entry).unwrap();
    append_asset_replacement_log(&path, &mut entry).unwrap();

    let contents = fs::read_to_string(path).unwrap();
    let records: Vec<_> = contents
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["missing_asset"], "A/B/Scenery/old.bin");
    assert_eq!(records[0]["replacement_asset"], "A/B/Scenery/new.bin");
    assert_eq!(records[0]["uses"][0]["tile"], "тайл X=+000001 Z=-000002");
    assert!(
        records[0]["backups"][0]
            .as_str()
            .unwrap()
            .ends_with(".bak1")
    );
    assert!(records[0]["timestamp_unix_seconds"].as_u64().unwrap() > 0);
}
