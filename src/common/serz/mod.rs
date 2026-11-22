//! Binary SERZ format reader, writer, XML bridge, and AST transforms.

mod ops;
mod reader;
mod writer;
mod xml_io;

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

pub use reader::SerzReader;
pub use writer::{
    SerzWriter, encode_tgpcdx_to_vec, encode_to_vec, patch_tgpcdx_blobs, validate_tgpcdx_binary,
};
pub use xml_io::{compile_tracks_xml_to_bin, tracks_bin_to_xml};

#[derive(Debug, PartialEq, Clone)]
pub struct SerzPreamble {
    pub flag: u8,
    pub val: u32,
}

#[derive(Debug, PartialEq, Clone)]
pub enum SerzNode {
    Element {
        name: String,
        id: u32,
        version: u32,
        children: Vec<SerzNode>,
    },
    Property {
        name: String,
        type_name: String,
        value: String,
        alt_encoding: Option<String>,
    },
    Array {
        name: String,
        type_name: String,
        values: Vec<String>,
    },
    Reference {
        name: String,
        target_id: u32,
    },
    Blob {
        data: Vec<u8>,
    },
    Nil,
    Directive {
        flag: u8,
        val: u32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct SerzTokenKey {
    pub opcode: u8,
    pub name: String,
    pub type_name: String,
}

pub fn run_test(input_file: &str, serz_exe: &str) {
    let input_path = PathBuf::from(input_file);
    let serz_exe_path = PathBuf::from(serz_exe);

    println!("========================================================");
    println!(" ТЕСТИРОВАНИЕ SERZ: {}", input_path.display());
    println!("========================================================");

    let temp_xml = input_path.with_extension("expected.xml");
    let t0 = Instant::now();
    let status = Command::new(&serz_exe_path)
        .arg(&input_path)
        .arg(format!("/xml:{}", temp_xml.display()))
        .status();

    if let Ok(s) = status
        && s.success()
    {
        println!("-> serz.exe отработал за: {:?}", t0.elapsed());
    }

    let expected_xml = fs::read_to_string(&temp_xml).unwrap_or_default();
    let raw_bytes = match fs::read(&input_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Ошибка чтения: {e:?}");
            return;
        }
    };

    println!("-> Запуск Rust SerzReader...");
    let t1 = Instant::now();
    let mut reader = SerzReader::new(&raw_bytes);
    match reader.parse() {
        Ok(ast) => {
            println!("   [✓] ДЕКОДИРОВАНО за: {:?}", t1.elapsed());
            println!("   Строк в пуле: {}", reader.string_pool_len());

            let actual_xml = format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n{}",
                SerzReader::to_xml(&ast, 0, true)
            );
            let actual_path = input_path.with_extension("actual.xml");
            let _ = fs::write(&actual_path, &actual_xml);

            let exp_lines: Vec<&str> = expected_xml
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            let act_lines: Vec<&str> = actual_xml
                .lines()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();

            println!("\n-> Сравнение строк XML:");
            println!("   Строк в expected.xml: {}", exp_lines.len());
            println!("   Строк в actual.xml:   {}", act_lines.len());

            let mut diffs = 0usize;
            for (i, (exp, act)) in exp_lines.iter().zip(&act_lines).enumerate() {
                if exp != act {
                    diffs += 1;
                    if diffs <= 5 {
                        println!("\n[!] Расхождение на строке {}:", i + 1);
                        println!("  serz.exe: {exp}");
                        println!("  Rust:     {act}");
                    }
                }
            }

            if diffs == 0 && exp_lines.len() == act_lines.len() {
                println!("[✓] 100% ПОЛНОЕ СОВПАДЕНИЕ XML С ОФИЦИАЛЬНЫМ SERZ.EXE!");
            } else {
                println!("Всего расхождений: {diffs}");
            }

            println!("\n-> Тестирование SerzWriter (обратная упаковка в бинарник)...");
            let t2 = Instant::now();
            match encode_to_vec(&ast, reader.preamble.as_ref()) {
                Ok(reencoded_bin) => {
                    println!("   [✓] СЕРИАЛИЗОВАНО за: {:?}", t2.elapsed());
                    println!("   Размер оригинала:    {} байт", raw_bytes.len());
                    println!("   Размер ре-экспорта:  {} байт", reencoded_bin.len());

                    let ext = input_path
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("bin");
                    let reencoded_path = input_path.with_extension(format!("reencoded.{ext}"));
                    let _ = fs::write(&reencoded_path, &reencoded_bin);

                    if raw_bytes == reencoded_bin {
                        println!(
                            "\n[★] БИТ В БИТ! 100% ПОБАЙТОВЫЙ ПАРИТЕТ С ОРИГИНАЛЬНЫМ БИНАРНИКОМ!"
                        );
                    } else {
                        let mut re_reader = SerzReader::new(&reencoded_bin);
                        match re_reader.parse() {
                            Ok(re_ast) if ast == re_ast => {
                                println!(
                                    "\n[✓] ROUND-TRIP УСПЕШЕН: AST полностью идентично исходному файлу!"
                                );
                            }
                            Ok(_) => println!("\n[!] AST после повторного чтения отличается!"),
                            Err(e) => {
                                eprintln!("\n[!] Ошибка чтения собственного ре-экспорта: {e}")
                            }
                        }
                    }
                }
                Err(err) => eprintln!("\n[!] Ошибка сериализации: {err}"),
            }
        }
        Err(err) => eprintln!("\n[!] Ошибка чтения: {err}"),
    }
}
