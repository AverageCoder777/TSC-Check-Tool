    use super::*;

    fn file_with_uses(stamp: FileStamp, uses: &[(&str, u32)]) -> CachedFile {
        CachedFile {
            stamp,
            uses: uses
                .iter()
                .map(|(path, count)| (path.to_string(), *count, crate::domain::AssetKind::Other))
                .collect(),
        }
    }

    #[test]
    fn saves_and_reuses_unchanged_files() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path();
        let file = route.join("Scenery").join("+000000+000000.bin");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"payload").unwrap();
        let stamp = FileStamp::of(&file).unwrap();

        let mut cache = AnalysisCache::empty();
        cache.put(
            "Scenery/+000000+000000.bin".to_owned(),
            file_with_uses(stamp, &[("Provider/Product/Scenery/asset.bin", 3)]),
        );
        cache.save(route).expect("кэш должен сохраниться");

        let loaded = AnalysisCache::load(route);
        let cached = loaded
            .get("Scenery/+000000+000000.bin", stamp)
            .expect("файл не менялся — разбор должен взять из кэша");
        assert_eq!(cached.uses.len(), 1);
        assert_eq!(cached.uses[0].0, "Provider/Product/Scenery/asset.bin");
        assert_eq!(cached.uses[0].1, 3);
    }

    #[test]
    fn stores_repeated_assets_as_single_entry_with_count() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path();
    let stamp = FileStamp {
        modified: 5,
        size: 7,
        hash: 0,
    };
        let mut cache = AnalysisCache::empty();
        cache.put(
            "Scenery/+000000+000000.bin".to_owned(),
            file_with_uses(stamp, &[("Provider/Product/Scenery/asset.bin", 12)]),
        );
        cache.save(route).expect("кэш должен сохраниться");

        let data = fs::read(cache_path(route)).unwrap();
        let text = String::from_utf8_lossy(&data);
        assert_eq!(
            text.matches("Provider/Product/Scenery/asset.bin").count(),
            1,
            "путь должен храниться один раз"
        );
        let loaded = AnalysisCache::load(route);
        assert_eq!(
            loaded
                .get("Scenery/+000000+000000.bin", stamp)
                .unwrap()
                .uses[0]
                .1,
            12
        );
    }

    #[test]
    fn ignores_cache_of_changed_file() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path();
        let file = route.join("+000000+000000.bin");
        std::fs::write(&file, b"payload").unwrap();
        let stamp = FileStamp::of(&file).unwrap();

        let mut cache = AnalysisCache::empty();
        cache.put("+000000+000000.bin".to_owned(), file_with_uses(stamp, &[]));

        // Файл изменился: прежний разбор больше не подходит.
        let changed = FileStamp {
            modified: stamp.modified + 1,
            size: stamp.size,
            hash: stamp.hash,
        };
        assert!(cache.get("+000000+000000.bin", changed).is_none());
        assert!(cache.get("+000000+000000.bin", stamp).is_some());
    }

    #[test]
    fn ignores_damaged_cache() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path();
        std::fs::write(cache_path(route), b"{ this is not json").unwrap();

        let cache = AnalysisCache::load(route);
        assert!(
            cache
                .get(
                    "+000000+000000.bin",
                    FileStamp {
                        modified: 0,
                        size: 0,
                        hash: 0,
                    }
                )
                .is_none()
        );
    }

    #[test]
    fn ignores_cache_of_other_version() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path();
        std::fs::write(
            cache_path(route),
            br#"{"version":1,"assets":[],"files":{}}"#,
        )
        .unwrap();

        let cache = AnalysisCache::load(route);
        assert!(
            cache
                .get(
                    "+000000+000000.bin",
                    FileStamp {
                        modified: 0,
                        size: 0,
                        hash: 0,
                    }
                )
                .is_none()
        );
    }

    #[test]
    fn drops_entries_of_deleted_files() {
        let temp = tempfile::tempdir().unwrap();
        let route = temp.path();
        let mut cache = AnalysisCache::empty();
        let stamp = FileStamp {
            modified: 1,
            size: 2,
            hash: 0,
        };
        cache.put("kept.bin".to_owned(), file_with_uses(stamp, &[]));
        cache.put("removed.bin".to_owned(), file_with_uses(stamp, &[]));

        let mut names = HashMap::new();
        names.insert("kept.bin".to_owned(), ());
        cache.retain(&names);
        cache.save(route).expect("кэш должен сохраниться");

        let loaded = AnalysisCache::load(route);
        assert!(loaded.get("kept.bin", stamp).is_some());
        assert!(loaded.get("removed.bin", stamp).is_none());
    }
