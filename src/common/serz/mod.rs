//! Binary SERZ format reader, writer, XML bridge, and AST transforms.

mod ops;
mod reader;
mod writer;
mod xml_io;

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
