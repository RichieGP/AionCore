pub mod constants;
pub mod error;
pub mod manifest;
pub mod permission;
pub mod template;
pub mod types;

pub use constants::*;
pub use error::ExtensionError;
pub use manifest::{parse_manifest, validate_manifest};
pub use permission::{build_permission_summary, calculate_risk_level};
pub use template::{resolve_env_map, resolve_env_templates, resolve_file_reference};
pub use types::*;
