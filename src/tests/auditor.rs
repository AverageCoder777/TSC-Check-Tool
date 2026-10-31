    use super::*;

    #[test]
    fn strips_shader_prefix() {
        assert_eq!(strip_shader_prefix(r"textures\[00]body"), r"textures\body");
    }

    #[test]
    fn normalizes_texture_reference() {
        assert_eq!(
            normalize_texture_reference(r"textures\[00]body"),
            r"textures\body.TgPcDx"
        );
        assert_eq!(
            normalize_texture_reference(r"textures\body.TgPcDx"),
            r"textures\body.TgPcDx"
        );
    }

    #[test]
    fn extracts_bin_refs() {
        let xml = r#"
            <GeometryID>x\[00]mesh</GeometryID>
            <ShadowGeometryID>y\[01]shadow</ShadowGeometryID>
            <AnimationID>do01_l</AnimationID>
            <AnimationName>3DZUG\3dz_ICE4\RailVehicles\Electric\mg_bremse</AnimationName>
            <ScriptComponent>scripts\foo.lua</ScriptComponent>
            <iBlueprintLibrary-cAbsoluteBlueprintID>
                <BlueprintSetID><iBlueprintLibrary-cBlueprintSetID>
                    <Provider>Kuju</Provider>
                    <Product>RailSimulator</Product>
                </iBlueprintLibrary-cBlueprintSetID></BlueprintSetID>
                <BlueprintID>child\bar.xml</BlueprintID>
            </iBlueprintLibrary-cAbsoluteBlueprintID>
        "#;
        let refs = extract_bin_references(xml, Path::new("Assets/Kuju/RailSimulator"));
        assert!(
            refs.iter()
                .any(|(k, r)| *k == AssetResourceKind::Geometry && r.contains("mesh"))
        );
        assert!(
            refs.iter()
                .any(|(k, r)| *k == AssetResourceKind::Animation && r.contains("mg_bremse"))
        );
        // Убеждаемся, что псевдо-путь AnimationID "do01_l" НЕ попал в ссылки:
        assert!(!refs.iter().any(|(_, r)| r == "do01_l"));
        assert!(refs.iter().any(|(k, _)| *k == AssetResourceKind::Script));
        assert!(
            refs.iter()
                .any(|(k, r)| *k == AssetResourceKind::ChildBlueprint
                    && r.contains("Kuju")
                    && r.contains("bar.bin"))
        );
    }

    #[test]
    fn sound_classification() {
        let xml = r#"<SoundName>audio\horn.wav</SoundName>"#;
        let refs = extract_bin_references(xml, Path::new("Assets/Kuju/RailSimulator"));
        assert!(refs.iter().any(|(k, _)| *k == AssetResourceKind::Sound));
    }

    #[test]
    fn extracts_geo_textures_strict() {
        let xml = r#"
            <cHcExplicitStateMaterial>
                0.000000 0.000000
                <EmissiveAsVec>1 2 3</EmissiveAsVec>
                textures\[0d]farn.TgPcDx
            </cHcExplicitStateMaterial>
            <TextureByName>
                <e d:type="cDeltaString">textures\[0d]farn.TgPcDx</e>
            </TextureByName>
            <TextureName>..\textures\[0d]vr_eiche.TgPcDx</TextureName>
            <Texture>foo</Texture>
        "#;
        let refs = extract_geo_texture_references(xml);
        assert_eq!(
            refs,
            vec![
                r"textures\farn.TgPcDx".to_string(),
                r"..\textures\vr_eiche.TgPcDx".to_string(),
                r"foo.TgPcDx".to_string(),
            ]
        );
    }

    #[test]
    fn resolves_provider_geometry_without_duplication() {
        let temp = std::env::temp_dir().join("tsc_audit_test");
        let assets = temp.join("Assets");
        let provider = assets.join("RWAustria_2");
        let source_dir = assets
            .join("RWAustria_2")
            .join("Austria_RailVehicles")
            .join("Electric")
            .join("GYSEV_470")
            .join("GYSEV_470_501");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::create_dir_all(&provider).unwrap();
        let raw =
            r"RWAustria_2\Austria_RailVehicles\Electric\GYSEV_470\GYSEV_470_501\[00]1116_Bleich";
        let cands = candidate_paths(
            raw,
            &source_dir,
            &assets.join("RWAustria_2").join("Austria_RailVehicles"),
            &assets,
            AssetResourceKind::Geometry,
        );
        let expected = assets.join(r"RWAustria_2\Austria_RailVehicles\Electric\GYSEV_470\GYSEV_470_501\1116_Bleich.GeoPcDx");
        assert_eq!(cands.first().unwrap(), &normalize_path(&expected));
        // ensure no duplicated source_dir prefix
        for c in &cands {
            assert!(!c.to_string_lossy().contains("GYSEV_470_501\\RWAustria_2"));
        }
    }

    #[test]
    fn inherits_empty_provider_product_in_child_blueprint() {
        let xml = r#"
            <iBlueprintLibrary-cAbsoluteBlueprintID>
                <BlueprintSetID><iBlueprintLibrary-cBlueprintSetID>
                    <Provider d:type="cDeltaString"></Provider>
                    <Product d:type="cDeltaString"></Product>
                </iBlueprintLibrary-cBlueprintSetID></BlueprintSetID>
                <BlueprintID d:type="cDeltaString">child\bar.xml</BlueprintID>
            </iBlueprintLibrary-cAbsoluteBlueprintID>
        "#;
        let refs = extract_bin_references(xml, Path::new("Assets/Kuju/RailSimulator"));
        assert!(refs.iter().any(|(k, r)| {
            *k == AssetResourceKind::ChildBlueprint && r == r"Kuju\RailSimulator\child\bar.bin"
        }));
    }

    #[test]
    fn detects_collision_geometry() {
        let xml = r#"<CollisionGeometryID>x\[00]mesh</CollisionGeometryID>"#;
        let refs = extract_bin_references(xml, Path::new("Assets/Kuju/RailSimulator"));
        assert!(
            refs.iter()
                .any(|(k, r)| *k == AssetResourceKind::Geometry && r.contains("mesh"))
        );
    }

    #[test]
    fn script_candidates_prefer_out_then_lua() {
        let temp = std::env::temp_dir().join("tsc_script_candidate_test");
        let assets = temp.join("Assets");
        let dir = assets.join("Kuju").join("RailSimulator");
        std::fs::create_dir_all(&dir).unwrap();
        let cands = candidate_paths(
            r"scripts\foo",
            &dir,
            &dir,
            &assets,
            AssetResourceKind::Script,
        );
        assert_eq!(
            cands.first().unwrap(),
            &normalize_path(&dir.join(r"scripts\foo.out"))
        );
        assert!(
            cands
                .iter()
                .any(|p| p == &normalize_path(&dir.join(r"scripts\foo.lua")))
        );
    }

    #[test]
    fn ap_cache_lookup_is_case_insensitive() {
        let cache = ApArchiveCache::new();
        let product_dir = PathBuf::from("Assets/Kuju/RailSimulator");
        cache.inner.lock().unwrap().insert(
            product_dir.clone(),
            ["Textures\\Foo.TgPcDx".to_string()].into(),
        );
        assert!(cache.lookup(&product_dir, "textures\\foo.tgpcdx"));
    }

    #[test]
    fn filters_invisible_and_bare_slots() {
        let xml = r#"
            <cHcExplicitStateMaterial>
                <ShaderName d:type="cDeltaString">Invisible</ShaderName>
                <TextureByName>
                    <e d:type="cDeltaString">[00]</e>
                </TextureByName>
                <Visible d:type="bool">0</Visible>
            </cHcExplicitStateMaterial>
            <TextureByName>
                <e d:type="cDeltaString">textures\[00]wall</e>
                <e d:type="cDeltaString">[00]</e>
            </TextureByName>
        "#;
        let refs = extract_geo_texture_references(xml);
        assert_eq!(refs, vec![r"textures\wall.TgPcDx".to_string()]);
    }
