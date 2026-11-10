use super::{
    TEMP_WORK_DIRECTORY_PREFIX, collect_route_files, leftover_temporary_work_directories,
    referenced_geometry_paths,
};
use std::path::Path;

/// Кэш разбора: повторная проверка берёт неизменившиеся файлы из кэша.
#[test]
fn rechecks_route_using_analysis_cache() {
    let temp = tempfile::tempdir().unwrap();
    let route = temp.path().join("route");
    let scenery = route.join("Scenery");
    std::fs::create_dir_all(&scenery).unwrap();
    let first = scenery.join("+000000+000000.xml");
    let second = scenery.join("+000000+000001.xml");
    std::fs::write(
            &first,
            "<Provider>First</Provider><Product>ProductA</Product><BlueprintID>Scenery\\one.xml</BlueprintID>\
             <Provider>First</Provider><Product>ProductA</Product><BlueprintID>Scenery\\one.xml</BlueprintID>",
        )
        .unwrap();
    std::fs::write(
            &second,
            "<Provider>Second</Provider><Product>ProductB</Product><BlueprintID>Scenery\\two.xml</BlueprintID>",
        )
        .unwrap();
    let assets = temp.path().join("Assets");
    std::fs::create_dir_all(&assets).unwrap();

    let check = super::check_route(&route, &assets, true, 2, |_| {}).unwrap();
    assert_eq!(check.asset_inventory.len(), 2);
    assert!(crate::analysis_cache::cache_path(&route).is_file());
    // Счётчик использований считает каждое вхождение, а не только файлы.
    let first_asset = check
        .asset_inventory
        .iter()
        .find(|asset| asset.path == "First/ProductA/Scenery/one.bin")
        .expect("ассет должен попасть в инвентарь");
    assert_eq!(first_asset.uses.iter().map(|u| u.count).sum::<u32>(), 2);

    // Файлы не менялись — всё берётся из кэша.
    let check = super::check_route(&route, &assets, true, 2, |_| {}).unwrap();
    assert_eq!(check.asset_inventory.len(), 2);
    let first_asset = check
        .asset_inventory
        .iter()
        .find(|asset| asset.path == "First/ProductA/Scenery/one.bin")
        .expect("ассет должен попасть в инвентарь");
    assert_eq!(first_asset.uses.iter().map(|u| u.count).sum::<u32>(), 2);

    // Один файл изменился: пересобирается только он.
    std::fs::write(
            &second,
            "<Provider>Second</Provider><Product>ProductB</Product><BlueprintID>Scenery\\two-new.xml</BlueprintID>",
        )
        .unwrap();
    let check = super::check_route(&route, &assets, true, 2, |_| {}).unwrap();
    assert_eq!(check.asset_inventory.len(), 2);
    let paths: Vec<&str> = check
        .asset_inventory
        .iter()
        .map(|asset| asset.path.as_str())
        .collect();
    assert!(
        paths.contains(&"Second/ProductB/Scenery/two-new.bin"),
        "{paths:?}"
    );
}

/// Архивы .ap распаковываются: без них список ассетов остаётся пустым.
#[test]
fn extracts_route_archives() {
    use std::io::Write;

    let temp = tempfile::tempdir().unwrap();
    let route = temp.path();
    let archive_path = route.join("MainContent.ap");
    {
        let file = std::fs::File::create(&archive_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file(
            "Networks/Loft Tiles/+000000+000000.bin",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"data").unwrap();
        zip.finish().unwrap();
    }

    let members = super::collect_archive_members(route, false).unwrap();
    assert_eq!(members.len(), 1);
    assert!(
        members[0].file_name().ends_with("+000000+000000.bin"),
        "запись архива должна сохранять имя файла"
    );
}

/// Файл рядом с архивом перекрывает одноимённый файл из архива.
#[test]
fn files_outside_archive_override_archive_members() {
    use std::io::Write;

    let temp = tempfile::tempdir().unwrap();
    let route = temp.path();
    {
        let file = std::fs::File::create(route.join("MainContent.ap")).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file(
            "Scenery/route.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"<root/>").unwrap();
        zip.finish().unwrap();
    }
    let members = super::collect_archive_members(route, false).unwrap();
    let inside = members[0].route_name(route).unwrap();
    assert_eq!(inside, "Scenery/route.xml");

    // Тот же файл рядом с архивом: игра читает его вместо файла из архива.
    let outside = route.join("Scenery/route.xml");
    std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
    std::fs::write(&outside, b"<root/>").unwrap();
    let name = crate::domain::RouteFile::Path(outside)
        .route_name(route)
        .unwrap();
    assert!(name.eq_ignore_ascii_case(&inside));
}

/// Изменённый файл из архива кладётся рядом с архивом: так он перекроет
/// оригинал, а сам архив останется нетронутым.
#[test]
fn changed_archive_file_is_installed_next_to_the_archive() {
    let temp = tempfile::tempdir().unwrap();
    let route = temp.path();
    let inside = crate::domain::RouteFile::Archive {
        archive: route.join("MainContent.ap"),
        name: "Scenery/route.xml".to_owned(),
        stamp: None,
    };
    let target = inside.install_target(route);
    assert_eq!(target, route.join("Scenery/route.xml"));
}

/// Ссылки на составы лежат внутри iRailVehicleBlueprint: такие ассеты —
/// файлы поездов, а не объекты маршрута.
#[test]
fn detects_vehicle_references_by_their_context() {
    let xml = r#"
            <root>
                <SceneryRenderBlueprint>
                    <iBlueprintLibrary-cAbsoluteBlueprintID>
                        <BlueprintSetID><iBlueprintLibrary-cBlueprintSetID>
                            <Provider d:type="cDeltaString">Prov</Provider>
                            <Product d:type="cDeltaString">Prod</Product>
                        </iBlueprintLibrary-cBlueprintSetID></BlueprintSetID>
                        <BlueprintID d:type="cDeltaString">Scenery/House.bin</BlueprintID>
                    </iBlueprintLibrary-cAbsoluteBlueprintID>
                </SceneryRenderBlueprint>
                <iRailVehicleBlueprint>
                    <iBlueprintLibrary-cAbsoluteBlueprintID>
                        <BlueprintSetID><iBlueprintLibrary-cBlueprintSetID>
                            <Provider d:type="cDeltaString">Loco</Provider>
                            <Product d:type="cDeltaString">Wagons</Product>
                        </iBlueprintLibrary-cBlueprintSetID></BlueprintSetID>
                        <BlueprintID d:type="cDeltaString">Trains/Loco.bin</BlueprintID>
                    </iBlueprintLibrary-cAbsoluteBlueprintID>
                </iRailVehicleBlueprint>
            </root>
        "#;
    let references = crate::reference::extract_asset_references_with_locations(xml);
    assert_eq!(references.len(), 2);
    assert_eq!(references[0].kind, crate::domain::AssetKind::Scenery);
    assert_eq!(references[1].kind, crate::domain::AssetKind::Vehicle);
}

/// Замена ассета в маршруте, упакованном в .ap: файл из архива читается
/// прямо из zip, а изменённая копия кладётся рядом с архивом.
#[test]
fn replaces_asset_in_route_packed_into_archive() {
    use std::io::Write;

    let temp = tempfile::tempdir().unwrap();
    let route = temp.path();
    let archive = route.join("MainContent.ap");
    {
        let file = std::fs::File::create(&archive).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file(
            "Scenery/Grass.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(
                br#"<root xmlns:d="http://schemas.microsoft.com/ado/2007/08/dataservices"><SceneryRenderBlueprint><iBlueprintLibrary-cAbsoluteBlueprintID><BlueprintSetID><iBlueprintLibrary-cBlueprintSetID><Provider d:type="cDeltaString">Prov</Provider><Product d:type="cDeltaString">Prod</Product></iBlueprintLibrary-cBlueprintSetID></BlueprintSetID><BlueprintID d:type="cDeltaString">Scenery/Old.bin</BlueprintID></iBlueprintLibrary-cAbsoluteBlueprintID></SceneryRenderBlueprint></root>"#,
            )
            .unwrap();
        zip.finish().unwrap();
    }
    let assets = temp.path().join("Assets");
    std::fs::create_dir_all(assets.join("Prov2/Prod2/Scenery")).unwrap();
    std::fs::write(assets.join("Prov2/Prod2/Scenery/New.bin"), b"new").unwrap();

    let uses = [crate::domain::AssetUse {
        source_file: crate::domain::RouteFile::Archive {
            archive: archive.clone(),
            name: "Scenery/Grass.xml".to_owned(),
            stamp: None,
        },
        tile: "тайл X=+000000 Z=+000000".to_owned(),
        tile_kind: crate::domain::RouteFile::Archive {
            archive: archive.clone(),
            name: "Scenery/Grass.xml".to_owned(),
            stamp: None,
        }
        .tile_kind(),
        kind: crate::domain::AssetKind::Scenery,
        count: 1,
        tile_search: "тайл X=+000000 Z=+000000".to_lowercase(),
        source_file_search: "Scenery/Grass.xml".to_lowercase(),
    }];

    let xml = std::fs::read_to_string(&archive).ok();
    let _ = xml;
    let extracted = super::extract_asset_references_with_locations(
        "<root><SceneryRenderBlueprint><iBlueprintLibrary-cAbsoluteBlueprintID><BlueprintSetID><iBlueprintLibrary-cBlueprintSetID><Provider d:type=\"cDeltaString\">Prov</Provider><Product d:type=\"cDeltaString\">Prod</Product></iBlueprintLibrary-cBlueprintSetID></BlueprintSetID><BlueprintID d:type=\"cDeltaString\">Scenery/Old.bin</BlueprintID></iBlueprintLibrary-cAbsoluteBlueprintID></SceneryRenderBlueprint></root>",
    );
    eprintln!(
        "DBG references: {:?}",
        extracted
            .iter()
            .map(|reference| reference.path.clone())
            .collect::<Vec<_>>()
    );

    let report = super::replace_missing_asset(
        route,
        &assets,
        "Prov/Prod/Scenery/Old.bin",
        &assets.join("Prov2/Prod2/Scenery/New.bin"),
        &uses,
    )
    .expect("замена ассета в архиве должна проходить");

    assert_eq!(report.replaced_files, 1);
    // Изменённый файл лежит рядом с архивом: игра прочитает его вместо
    // записи внутри архива.
    let installed = route.join("Scenery/Grass.xml");
    assert!(
        installed.is_file(),
        "изменённый файл должен лежать рядом с архивом"
    );
    let contents = std::fs::read_to_string(&installed).unwrap();
    assert!(
        contents.contains(r"Scenery\New.xml"),
        "ссылка в файле должна вести на новый ассет: {contents}"
    );
}

/// Пути, отличающиеся только регистром, — это один и тот же ассет: игра
/// регистр не различает.
#[test]
fn assets_with_different_case_are_the_same_asset() {
    assert_eq!(
        super::asset_key("RSSLO_Routes/Wien/Scenery/Grass/Grass_Mix_06.bin"),
        super::asset_key("RSSLO_Routes/wien/scenery/grass/grass_mix_06.bin")
    );
}

/// Ссылки, отличающиеся регистром, попадают в один ассет: игра читает
/// пути без учёта регистра.
#[test]
fn merges_assets_referenced_with_different_case() {
    let temp = tempfile::tempdir().unwrap();
    let route = temp.path();
    std::fs::create_dir_all(route.join("Scenery")).unwrap();
    for (name, folder) in [("a.xml", "Scenery"), ("b.xml", "scenery")] {
        let xml = format!(
            "<root xmlns:d=\"http://schemas.microsoft.com/ado/2007/08/dataservices\"><SceneryRenderBlueprint><iBlueprintLibrary-cAbsoluteBlueprintID><BlueprintSetID><iBlueprintLibrary-cBlueprintSetID><Provider d:type=\"cDeltaString\">Prov</Provider><Product d:type=\"cDeltaString\">Prod</Product></iBlueprintLibrary-cBlueprintSetID></BlueprintSetID><BlueprintID d:type=\"cDeltaString\">{folder}/Grass.bin</BlueprintID></iBlueprintLibrary-cAbsoluteBlueprintID></SceneryRenderBlueprint></root>"
        );
        std::fs::write(route.join(name), xml).unwrap();
    }
    let assets = temp.path().join("Assets");
    std::fs::create_dir_all(&assets).unwrap();

    let result = super::check_route(route, &assets, true, 1, |_| {})
        .expect("проверка маршрута должна пройти");

    let scenery: Vec<_> = result
        .asset_inventory
        .iter()
        .filter(|asset| asset.path.to_lowercase().contains("grass.bin"))
        .collect();
    assert_eq!(
        scenery.len(),
        1,
        "ссылки с разным регистром должны попасть в один ассет"
    );
    assert_eq!(
        scenery[0].uses.len(),
        2,
        "оба использования должны попасть в ассет"
    );
}

#[test]
fn excludes_leftover_work_directories_from_route_files() {
    let temp = tempfile::tempdir().unwrap();
    let route = temp.path().join("route");
    std::fs::create_dir_all(route.join("Scenery")).unwrap();
    std::fs::write(route.join("Scenery").join("tile.bin"), b"data").unwrap();
    let leftover = route
        .join("Scenery")
        .join(format!("{TEMP_WORK_DIRECTORY_PREFIX}abcd1234"));
    std::fs::create_dir_all(&leftover).unwrap();
    std::fs::write(leftover.join("source.xml"), b"<route />").unwrap();
    std::fs::write(leftover.join("replacement.bin"), b"data").unwrap();

    let files = collect_route_files(&route, false).unwrap();
    assert_eq!(files.len(), 1);
    assert!(files[0].ends_with("Scenery\\tile.bin") || files[0].ends_with("Scenery/tile.bin"));
    assert_eq!(
        leftover_temporary_work_directories(&route).unwrap(),
        [leftover.display().to_string()]
    );
}

#[test]
fn extracts_locations_for_multiple_asset_references() {
    let xml = r#"
            <Provider>First</Provider><Product>ProductA</Product>
            <BlueprintID>Scenery\first.xml</BlueprintID>
            <Provider>Second</Provider><Product>ProductB</Product>
            <BlueprintID>Networks\second.xml</BlueprintID>
        "#;
    let references = crate::reference::extract_asset_references_with_locations(xml);

    assert_eq!(references.len(), 2);
    assert_eq!(references[0].path, "First/ProductA/Scenery/first.bin");
    assert_eq!(references[1].path, "Second/ProductB/Networks/second.bin");
    assert_eq!(&xml[references[0].provider_range.clone()], "First");
    assert_eq!(&xml[references[1].product_range.clone()], "ProductB");
}

#[test]
fn extracts_references_from_wrapped_absolute_blueprint_blocks() {
    let xml = r#"
            <BlueprintID>
                <iBlueprintLibrary-cAbsoluteBlueprintID>
                    <BlueprintSetID>
                        <iBlueprintLibrary-cBlueprintSetID>
                            <Provider d:type="cDeltaString">JustTrains</Provider>
                            <Product d:type="cDeltaString">CommonLibrary</Product>
                        </iBlueprintLibrary-cBlueprintSetID>
                    </BlueprintSetID>
                    <BlueprintID d:type="cDeltaString">Scenery\Clutter\jt_clutter_pipe_low.xml</BlueprintID>
                </iBlueprintLibrary-cAbsoluteBlueprintID>
            </BlueprintID>
        "#;
    let references = crate::reference::extract_asset_references_with_locations(xml);

    assert_eq!(references.len(), 1);
    assert_eq!(
        references[0].path,
        "JustTrains/CommonLibrary/Scenery/Clutter/jt_clutter_pipe_low.bin"
    );
    assert_eq!(&xml[references[0].provider_range.clone()], "JustTrains");
    assert_eq!(&xml[references[0].product_range.clone()], "CommonLibrary");
    assert_eq!(
        &xml[references[0].blueprint_range.clone()],
        r"Scenery\Clutter\jt_clutter_pipe_low.xml"
    );
}

#[test]
fn empty_reskin_block_does_not_hide_the_next_object_reference() {
    let xml = r#"
            <cDynamicEntity>
                <BlueprintID>
                    <iBlueprintLibrary-cAbsoluteBlueprintID>
                        <BlueprintSetID>
                            <iBlueprintLibrary-cBlueprintSetID>
                                <Provider d:type="cDeltaString">First</Provider>
                                <Product d:type="cDeltaString">ProductA</Product>
                            </iBlueprintLibrary-cBlueprintSetID>
                        </BlueprintSetID>
                        <BlueprintID d:type="cDeltaString">Scenery\first.xml</BlueprintID>
                    </iBlueprintLibrary-cAbsoluteBlueprintID>
                </BlueprintID>
                <ReskinBlueprintID>
                    <iBlueprintLibrary-cAbsoluteBlueprintID>
                        <BlueprintSetID>
                            <iBlueprintLibrary-cBlueprintSetID>
                                <Provider d:type="cDeltaString"></Provider>
                                <Product d:type="cDeltaString"></Product>
                            </iBlueprintLibrary-cBlueprintSetID>
                        </BlueprintSetID>
                        <BlueprintID d:type="cDeltaString"></BlueprintID>
                    </iBlueprintLibrary-cAbsoluteBlueprintID>
                </ReskinBlueprintID>
            </cDynamicEntity>
            <cDynamicEntity>
                <BlueprintID>
                    <iBlueprintLibrary-cAbsoluteBlueprintID>
                        <BlueprintSetID>
                            <iBlueprintLibrary-cBlueprintSetID>
                                <Provider d:type="cDeltaString">Second</Provider>
                                <Product d:type="cDeltaString">ProductB</Product>
                            </iBlueprintLibrary-cBlueprintSetID>
                        </BlueprintSetID>
                        <BlueprintID d:type="cDeltaString">Networks\second.xml</BlueprintID>
                    </iBlueprintLibrary-cAbsoluteBlueprintID>
                </BlueprintID>
                <ReskinBlueprintID>
                    <iBlueprintLibrary-cAbsoluteBlueprintID>
                        <BlueprintSetID>
                            <iBlueprintLibrary-cBlueprintSetID>
                                <Provider d:type="cDeltaString"></Provider>
                                <Product d:type="cDeltaString"></Product>
                            </iBlueprintLibrary-cBlueprintSetID>
                        </BlueprintSetID>
                        <BlueprintID d:type="cDeltaString"></BlueprintID>
                    </iBlueprintLibrary-cAbsoluteBlueprintID>
                </ReskinBlueprintID>
            </cDynamicEntity>
        "#;
    let references = crate::reference::extract_asset_references_with_locations(xml);

    assert_eq!(references.len(), 2);
    assert_eq!(references[0].path, "First/ProductA/Scenery/first.bin");
    assert_eq!(references[1].path, "Second/ProductB/Networks/second.bin");
}

#[test]
fn detects_asset_paths_encoded_as_numeric_xml_entities() {
    let xml = r#"
            <Provider>RWAustria_3</Provider>
            <Product>Marchegger_Ostbahn</Product>
            <BlueprintID>Marchegg\Bruecke Fu&#223;ganger 5.xml</BlueprintID>
        "#;
    let references = crate::reference::extract_asset_references_with_locations(xml);

    assert_eq!(references.len(), 1);
    assert_eq!(
        references[0].path,
        "RWAustria_3/Marchegger_Ostbahn/Marchegg/Bruecke Fußganger 5.bin"
    );
    assert!(crate::translit::has_umlauts(&references[0].path));
}

/// Файл без `BlueprintID` не конвертируется: ссылок в нём быть не может.
#[test]
fn skips_bin_files_without_blueprint_marker() {
    let temp = tempfile::tempdir().unwrap();
    let without_marker = temp.path().join("plain.bin");
    std::fs::write(&without_marker, b"SERZ\x00\x00\x01\x00some payload").unwrap();
    let with_marker = temp.path().join("with.bin");
    std::fs::write(&with_marker, b"SERZ\x00\x00\x01\x00BlueprintID payload").unwrap();

    assert!(!super::may_contain_asset_reference(&without_marker));
    assert!(super::may_contain_asset_reference(&with_marker));
}

#[test]
fn extracts_tile_coordinates_from_route_file_names() {
    let route = Path::new(r"C:\Routes\RouteA");
    let source = route.join("Scenery").join("+000012-000034.bin");
    assert_eq!(
        crate::reference::route_tile_label(route, &crate::domain::RouteFile::Path(source.clone())),
        "+000012-000034"
    );
}

#[test]
fn detects_geometry_and_texture_references_in_asset_xml() {
    let xml = r#"
            <GeometryID d:type="cDeltaString">RailVehicles\Brücke.GeoPcDx</GeometryID>
            <Texture d:type="cDeltaString">Textures\Bäume.TgPcDx</Texture>
        "#;
    let references = referenced_geometry_paths(xml);

    assert_eq!(references.len(), 2);
    assert_eq!(references[0].0.to_text(), r"RailVehicles\Brücke.GeoPcDx");
    assert_eq!(references[1].0.to_text(), r"Textures\Bäume.TgPcDx");
}
