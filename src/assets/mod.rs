//! Serz conversion and asset-reference checks.

use super::*;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::OnceLock;
use walkdir::WalkDir;

mod fuzzy;
mod types;
mod serz;
mod scanner;
mod checker;
mod resolver;
mod xml_reference;
mod replacement;
mod umlaut;
mod transaction;

pub use fuzzy::*;
pub use types::*;
pub use serz::*;
pub use scanner::*;
pub use checker::*;
pub use resolver::*;
pub use xml_reference::*;
pub use replacement::*;
pub use umlaut::*;
pub use transaction::*;

#[cfg(test)]
#[path = "../tests/assets.rs"]
mod tests;
