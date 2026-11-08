use super::{
    ScenarioClueCollection, check_route, decode_xml_text, extract_asset_references,
    extract_scenario_clues, inspect_route_folder, is_skipped_route_scan_directory,
    parse_route_name, scan_routes, scan_routes_with_ignored,
};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

#[test]
fn only_skips_scenarios_when_requested_during_asset_check() {
    assert!(!is_skipped_route_scan_directory("Scenarios", false));
    assert!(is_skipped_route_scan_directory("scenarios", true));
    assert!(!is_skipped_route_scan_directory("Networks", true));
}

#[test]
fn route_check_only_collects_files_from_allowed_directories() {
    let temp = tempfile::tempdir().unwrap();
    let route = temp.path().join("route");
    for directory in [
        "Terrain",
        "Scenery",
        "Scenarios",
        "RouteInfo",
        "Networks",
        "MixMap",
        "ThirdParty",
    ] {
        let folder = route.join(directory);
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("asset.bin"), []).unwrap();
    }
    fs::write(route.join("RouteProperties.xml"), "<route />").unwrap();

    let included = super::assets::collect_route_files(&route, false).unwrap();
    let included_names: Vec<_> = included
        .iter()
        .map(|path| {
            path.strip_prefix(&route)
                .unwrap()
                .components()
                .next()
                .unwrap()
                .as_os_str()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    // Три папки маршрута и RouteProperties.xml в корне маршрута.
    assert_eq!(included.len(), 4);
    assert!(!included_names.iter().any(|name| name == "ThirdParty"));
    assert!(included.contains(&route.join("RouteProperties.xml")));

    let without_scenarios = super::assets::collect_route_files(&route, true).unwrap();
    assert_eq!(without_scenarios.len(), 3);
    assert_eq!(
        super::assets::unexpected_route_directories(&route).unwrap(),
        ["ThirdParty"]
    );
}

#[test]
fn parses_english_route_name_and_xml_entities() {
    let xml = r#"<DisplayName><Localisation-cUserLocalisedString><English d:type="cDeltaString">London &amp; Brighton</English></Localisation-cUserLocalisedString></DisplayName>"#;
    assert_eq!(parse_route_name(xml).as_deref(), Some("London & Brighton"));
    assert_eq!(decode_xml_text("Fu&#223;ganger"), "Fußganger");
    assert_eq!(decode_xml_text("Fu&#xDF;ganger"), "Fußganger");
}

#[test]
fn falls_back_to_another_localized_name() {
    let xml = r#"<DisplayName><Localisation-cUserLocalisedString><German>Hamburg</German></Localisation-cUserLocalisedString></DisplayName>"#;
    assert_eq!(parse_route_name(xml).as_deref(), Some("Hamburg"));
}

#[test]
fn falls_back_when_english_route_name_is_empty() {
    let xml = r#"<DisplayName><Localisation-cUserLocalisedString><English d:type="cDeltaString"></English><German d:type="cDeltaString">[EZY] Bad Altburger Streckennetz/German</German></Localisation-cUserLocalisedString></DisplayName>"#;
    assert_eq!(
        parse_route_name(xml).as_deref(),
        Some("[EZY] Bad Altburger Streckennetz/German")
    );
}

#[test]
fn extracts_blueprint_references_across_xml_lines() {
    let xml = r#"
            <Provider d:type="cDeltaString">Kuju</Provider>
            <Product d:type="cDeltaString">RailSimulator</Product>
            <BlueprintID d:type="cDeltaString">
                Scenery\Buildings\Station.XML
            </BlueprintID>
        "#;

    assert_eq!(
        extract_asset_references(xml),
        ["Kuju/RailSimulator/Scenery/Buildings/Station.bin".to_owned()]
            .into_iter()
            .collect()
    );
}

#[test]
fn ignores_blueprint_paths_that_escape_the_assets_directory() {
    let xml = r#"
            <Provider>Kuju</Provider>
            <Product>RailSimulator</Product>
            <BlueprintID>..\Outside\asset.xml</BlueprintID>
        "#;

    assert!(extract_asset_references(xml).is_empty());
}

#[test]
fn separates_complete_routes_from_incomplete_folder_reports() {
    let temp = tempfile::tempdir().unwrap();
    let routes_root = temp.path().join("Content/Routes");
    let valid_route = routes_root.join("valid-id");
    let incomplete = routes_root.join("leftovers");
    let emptier = routes_root.join("empty-folder");
    fs::create_dir_all(&valid_route).unwrap();
    fs::create_dir_all(incomplete.join("Scenery")).unwrap();
    fs::create_dir_all(&emptier).unwrap();
    fs::write(
        valid_route.join("RouteProperties.xml"),
        "<DisplayName><English>Valid route</English></DisplayName>",
    )
    .unwrap();
    for directory in ["Networks", "RouteInformation", "Scenery", "Terrain"] {
        let path = valid_route.join(directory);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("data.bin"), []).unwrap();
    }
    fs::write(incomplete.join("Scenery/orphan.dat"), [1, 2, 3]).unwrap();

    let result = scan_routes(temp.path()).unwrap();

    assert_eq!(result.routes.len(), 1);
    assert_eq!(result.routes[0].name, "Valid route");
    assert_eq!(result.incomplete_folders.len(), 2);
    assert_eq!(result.incomplete_folders[0].id, "empty-folder");
    assert_eq!(result.incomplete_folders[0].file_count, 0);
    assert_eq!(result.incomplete_folders[1].id, "leftovers");
    assert_eq!(result.incomplete_folders[1].file_count, 1);
    assert!(
        result.incomplete_folders[1]
            .inventory
            .iter()
            .any(|entry| entry.contains("Scenery") && entry.contains("orphan.dat"))
    );
}

#[test]
fn does_not_mark_route_incomplete_when_optional_directory_is_missing() {
    let temp = tempfile::tempdir().unwrap();
    let route_path = temp.path().join("Content/Routes/route");
    fs::create_dir_all(&route_path).unwrap();
    fs::write(
        route_path.join("RouteProperties.xml"),
        "<DisplayName><English>Complete route</English></DisplayName>",
    )
    .unwrap();
    for directory in ["Networks", "RouteInformation", "Scenery", "Terrain"] {
        let directory_path = route_path.join(directory);
        fs::create_dir_all(&directory_path).unwrap();
        fs::write(directory_path.join("data.bin"), []).unwrap();
    }
    let ignored_incomplete_path = temp.path().join("Content/Routes/ignored-incomplete");
    fs::create_dir_all(&ignored_incomplete_path).unwrap();

    let scan = scan_routes(temp.path()).unwrap();
    assert_eq!(scan.incomplete_folders.len(), 1);
    assert_eq!(scan.incomplete_folders[0].id, "ignored-incomplete");
    assert_eq!(scan.routes.len(), 1);
    assert_eq!(scan.routes[0].name, "Complete route");

    let ignored_ids = ["route".to_owned(), "ignored-incomplete".to_owned()]
        .into_iter()
        .collect();
    let ignored_scan = scan_routes_with_ignored(temp.path(), &ignored_ids, 4).unwrap();
    assert!(ignored_scan.incomplete_folders.is_empty());
    assert_eq!(ignored_scan.routes.len(), 2);
}

#[test]
fn extracts_scenario_names_descriptions_and_place_names() {
    let xml = r#"
            <ScenarioProperties>
                <ScenarioName>
                    <Localisation-cUserLocalisedString>
                        <English>Express to Brighton</English>
                    </Localisation-cUserLocalisedString>
                </ScenarioName>
                <ScenarioDescription>Stopping at Lewes and Falmer</ScenarioDescription>
                <StartLocationName>London Victoria</StartLocationName>
                <StartLocation>74151eaf-456f-41e7-8b38-6f48ee8ef031</StartLocation>
            </ScenarioProperties>
        "#;

    let clues = extract_scenario_clues(xml);

    assert!(
        clues
            .iter()
            .any(|clue| clue == "ScenarioName: Express to Brighton")
    );
    assert!(
        clues
            .iter()
            .any(|clue| clue == "ScenarioDescription: Stopping at Lewes and Falmer")
    );
    assert!(
        clues
            .iter()
            .any(|clue| clue == "StartLocationName: London Victoria")
    );
    assert!(
        !clues
            .iter()
            .any(|clue| clue.contains("74151eaf-456f-41e7-8b38-6f48ee8ef031"))
    );
}

#[test]
fn combines_repeated_start_locations_across_scenarios() {
    let mut clues = ScenarioClueCollection::default();
    clues.add(
        "<ScenarioProperties><StartLocation>King's Cross</StartLocation></ScenarioProperties>",
        "scenario-1/ScenarioProperties.xml",
    );
    clues.add(
        "<ScenarioProperties><StartLocation> King's   Cross </StartLocation></ScenarioProperties>",
        "scenario-2/ScenarioProperties.xml",
    );
    clues.add(
        "<ScenarioProperties><StartLocation>Victoria</StartLocation></ScenarioProperties>",
        "scenario-3/ScenarioProperties.xml",
    );

    let clues = clues.into_clues();
    assert!(
        clues
            .iter()
            .any(|clue| clue == "StartLocation: King's Cross (упоминается в 2 сценариях)")
    );
    assert!(clues.iter().any(|clue| clue == "StartLocation: Victoria"));
    assert_eq!(
        clues
            .iter()
            .filter(|clue| clue.starts_with("StartLocation: King's Cross"))
            .count(),
        1
    );
}

#[test]
fn includes_scenario_properties_clues_in_incomplete_route_report() {
    let temp = tempfile::tempdir().unwrap();
    let route_path = temp.path().join("Content/Routes/broken-route");
    let scenario_path = route_path.join("Scenarios/scenario-1");
    fs::create_dir_all(&scenario_path).unwrap();
    fs::write(
            scenario_path.join("ScenarioProperties.xml"),
            "<ScenarioProperties><ScenarioName><English>Coastal run</English></ScenarioName><ScenarioDescription>Through Hastings</ScenarioDescription></ScenarioProperties>",
        )
        .unwrap();

    let report = inspect_route_folder(&route_path, "broken-route")
        .unwrap()
        .unwrap();

    assert!(
        report
            .scenario_clues
            .iter()
            .any(|clue| clue.contains("ScenarioName: Coastal run"))
    );
    assert!(
        report
            .scenario_clues
            .iter()
            .any(|clue| clue.contains("ScenarioDescription: Through Hastings"))
    );
}

#[test]
fn reports_empty_required_route_directories() {
    let temp = tempfile::tempdir().unwrap();
    let route = temp.path().join("empty");
    fs::create_dir_all(&route).unwrap();
    for directory in ["Networks", "RouteInformation", "Scenery", "Terrain"] {
        fs::create_dir(route.join(directory)).unwrap();
    }

    let report = inspect_route_folder(&route, "empty").unwrap().unwrap();

    assert_eq!(report.file_count, 0);
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem.contains("RouteProperties"))
    );
    for directory in ["Networks", "Scenery", "Terrain"] {
        assert!(
            report
                .problems
                .iter()
                .any(|problem| problem == directory)
        );
    }
}

#[test]
fn treats_only_extreme_compression_ratios_as_suspicious() {
    // Настоящие файлы в .ap сжимаются в десятки раз, а не в тысячи.
    assert!(!super::is_suspicious_archive_member(
        50 * 1024 * 1024,
        2 * 1024 * 1024
    ));
    assert!(!super::is_suspicious_archive_member(1024, 10));
    assert!(super::is_suspicious_archive_member(
        1024 * 1024 * 1024,
        1024 * 1024
    ));
    assert!(super::is_suspicious_archive_member(1024 * 1024 * 1024, 0));
}

#[test]
fn finds_route_icon_in_route_information() {
    let temp = tempfile::tempdir().unwrap();
    let route = temp.path().join("incomplete-route");
    let route_information = route.join("RouteInformation");
    fs::create_dir_all(&route_information).unwrap();
    let dated_image = route_information.join("20220721195917_1.jpg");
    let standard_icon = route_information.join("Image.png");
    fs::write(&dated_image, []).unwrap();
    fs::write(&standard_icon, []).unwrap();

    assert_eq!(super::find_route_icon(&route).unwrap(), Some(standard_icon));
}

#[test]
fn extracts_route_icon_from_ap_archive() {
    let temp = tempfile::tempdir().unwrap();
    let route = temp.path().join("route");
    fs::create_dir_all(&route).unwrap();
    let archive_path = route.join("route-data.ap");
    let archive_file = fs::File::create(&archive_path).unwrap();
    let mut archive = zip::ZipWriter::new(archive_file);
    archive
        .start_file(
            "RouteInformation/Image.png",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
    archive.write_all(b"archived image").unwrap();
    archive.finish().unwrap();

    let icon_path = super::find_route_icon_with_cache(&route, &temp.path().join("icon-cache"))
        .unwrap()
        .unwrap();

    assert_eq!(fs::read(icon_path).unwrap(), b"archived image");
}

#[test]
fn incomplete_route_report_contains_route_icon_path() {
    let temp = tempfile::tempdir().unwrap();
    let route = temp.path().join("incomplete-route");
    let icon = route.join("RouteInformation/Image.png");
    fs::create_dir_all(icon.parent().unwrap()).unwrap();
    fs::write(&icon, []).unwrap();

    let report = inspect_route_folder(&route, "incomplete-route")
        .unwrap()
        .unwrap();

    assert_eq!(report.icon_path, Some(icon));
}

#[test]
fn complete_route_scan_includes_route_icon_path() {
    let temp = tempfile::tempdir().unwrap();
    let route = temp.path().join("route");
    fs::create_dir_all(route.join("RouteInformation")).unwrap();
    fs::write(
            route.join("RouteProperties.xml"),
            "<DisplayName><Localisation-cUserLocalisedString><English>Test route</English></Localisation-cUserLocalisedString></DisplayName>",
        )
        .unwrap();
    let icon = route.join("RouteInformation/Image.png");
    fs::write(&icon, []).unwrap();
    for directory in ["Networks", "RouteInformation", "Scenery", "Terrain"] {
        let path = route.join(directory);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("content.bin"), []).unwrap();
    }

    let scan_item = super::scan_route_folder(&route, "route", &HashSet::new()).unwrap();

    match scan_item {
        super::RouteScanItem::Route(route) => assert_eq!(route.icon_path, Some(icon)),
        _ => panic!("Ожидался полный маршрут."),
    }
}

#[test]
fn persists_route_thumbnail_setting() {
    let temp = tempfile::tempdir().unwrap();
    let setting_path = temp.path().join("settings/route_thumbnails_enabled.txt");

    assert!(!super::read_route_thumbnail_setting(&setting_path).unwrap());
    super::write_route_thumbnail_setting(&setting_path, true).unwrap();
    assert!(super::read_route_thumbnail_setting(&setting_path).unwrap());
    super::write_route_thumbnail_setting(&setting_path, false).unwrap();
    assert!(!super::read_route_thumbnail_setting(&setting_path).unwrap());
}

#[test]
fn persists_serz_process_limit_with_validation() {
    let temp = tempfile::tempdir().unwrap();
    let setting_path = temp.path().join("settings/serz_process_limit.txt");

    super::write_serz_process_limit(&setting_path, 1).unwrap();
    assert_eq!(fs::read_to_string(&setting_path).unwrap(), "1");
    assert!(super::write_serz_process_limit(&setting_path, 0).is_err());
    assert_eq!(super::read_serz_process_limit(&setting_path).unwrap(), 1);
}

#[test]
fn recognizes_route_structure_and_name_inside_ap_archive() {
    let temp = tempfile::tempdir().unwrap();
    let route_path = temp.path().join("Content/Routes/archive-route");
    fs::create_dir_all(&route_path).unwrap();
    let archive_path = route_path.join("route-data.ap");
    let archive_file = fs::File::create(&archive_path).unwrap();
    let mut archive = zip::ZipWriter::new(archive_file);
    let options = zip::write::SimpleFileOptions::default();
    archive.start_file("RouteProperties.xml", options).unwrap();
    archive
        .write_all(b"<DisplayName><English>Archived Route</English></DisplayName>")
        .unwrap();
    for directory in ["Networks", "RouteInformation", "Scenery", "Terrain"] {
        archive
            .start_file(format!("{directory}/route-data.bin"), options)
            .unwrap();
        archive.write_all(b"data").unwrap();
    }
    archive.finish().unwrap();

    let result = scan_routes(temp.path()).unwrap();

    assert_eq!(result.incomplete_folders.len(), 0);
    assert_eq!(result.routes.len(), 1);
    assert_eq!(result.routes[0].name, "Archived Route");
}

#[test]
fn suggests_ap_filename_when_route_name_files_are_missing() {
    let temp = tempfile::tempdir().unwrap();
    let route_path = temp.path().join("Content/Routes/unknown-route");
    fs::create_dir_all(&route_path).unwrap();
    let archive_file = fs::File::create(route_path.join("Nordbahn.ap")).unwrap();
    zip::ZipWriter::new(archive_file).finish().unwrap();

    for directory in ["Networks", "Scenery", "Terrain"] {
        let directory_path = route_path.join(directory);
        fs::create_dir_all(&directory_path).unwrap();
        fs::write(directory_path.join("content.bin"), b"data").unwrap();
    }

    let result = scan_routes(temp.path()).unwrap();

    assert_eq!(result.incomplete_folders.len(), 1);
    assert_eq!(
        result.incomplete_folders[0].suggested_name.as_deref(),
        Some("Nordbahn")
    );
}
#[test]
fn repro_route_check_hang() {
    let route = PathBuf::from(
        r"X:\SteamLibrary\steamapps\common\RailWorks\Content\Routes\8378d7b6-30ad-4c29-a5ac-16d3c2b26ff7",
    );
    let rw = PathBuf::from(r"X:\SteamLibrary\steamapps\common\RailWorks");
    let assets = rw.join("Assets");
    let start = std::time::Instant::now();
    let res = check_route(&route, &assets, true, 8, |msg| {
        println!("[{:.1}s] {}", start.elapsed().as_secs_f32(), msg);
    });
    println!(
        "RESULT after {:.1}s: {:?}",
        start.elapsed().as_secs_f32(),
        res.map(|r| (
            r.asset_inventory.len(),
            r.missing_assets.len(),
            r.conversion_errors.len(),
            r.structure_issues.len()
        ))
    );
}
