use super::*;
#[test]
fn checks_real_route_x() {
    let rw_path = PathBuf::from(r"X:\SteamLibrary\steamapps\common\RailWorks");
    let route = rw_path.join(r"Content\Routes\1f8f9c40-10bb-4310-ab5c-3a6a74893eb1");
    let serz = rw_path.join(r"serz.exe");
    if !route.is_dir() || !serz.is_file() {
        return;
    }
    let result =
        crate::assets::check_route(&route, &rw_path.join("Assets"), &serz, false, 1, |_| {});
    match result {
        Ok(r) => {
            println!(
                "missing: {}, errors: {}, structure: {}",
                r.missing_assets.len(),
                r.conversion_errors.len(),
                r.structure_issues.len()
            );
            if let Some(last) = r.conversion_errors.last() {
                println!("последняя ошибка: {last}");
            }
        }
        Err(e) => panic!("{e}"),
    }
}
