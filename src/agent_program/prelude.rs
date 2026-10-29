use super::Program;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

pub(super) const SOURCE: &str = include_str!("prelude.js");
pub(super) fn hash() -> String { format!("{:x}", Sha256::digest(SOURCE.as_bytes())) }
pub(super) fn program() -> &'static Program {
    static PROGRAM: OnceLock<Program> = OnceLock::new();
    PROGRAM.get_or_init(|| super::compiler::compile_prelude().expect("valid internal prelude"))
}
