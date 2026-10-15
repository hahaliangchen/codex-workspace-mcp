//! Project-owned plugin roster. Filesystem location never determines origin.
pub mod code_flow;
mod services;
pub use services::*;

pub fn packages() -> anyhow::Result<Vec<crate::skill_packages::Package>> {
    Ok(vec![code_flow::package()?])
}
