use rspolib::{pofile, prelude::*};
use std::env;
use std::path::PathBuf;

const LANGUAGES: &[&str] = &["ru", "de"];
const DOMAIN: &str = env!("CARGO_PKG_NAME");

fn main() {
    println!("cargo:rerun-if-changed=locales");

    // 1. Slint вшивает .po напрямую
    let config = slint_build::CompilerConfiguration::new().with_bundled_translations("locales");
    slint_build::compile_with_config("ui/app.slint", config).unwrap();

    // 2. Компилируем те же .po -> .mo в OUT_DIR для Rust-части
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    for lang in LANGUAGES {
        let po_path = PathBuf::from(format!("locales/{lang}/LC_MESSAGES/{DOMAIN}.po"));
        let mo_path = out_dir.join(format!("{lang}.mo"));
        let po = pofile(po_path.as_path())
            .unwrap_or_else(|e| panic!("Failed to parse {}: {e}", po_path.display()));
        po.save_as_mofile(mo_path.to_str().unwrap());
    }
}
