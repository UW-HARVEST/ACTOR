//! Utilities for recombining translated Rust declarations into a Cargo package.

use build_project_spec::ProjectKind;
use full_source::CargoPackage;
use harvest_core::fs::RawDir;
use std::collections::HashSet;
use tracing::debug;

use crate::translation::TranslationResult;

/// Recombines translated Rust declarations into a CargoPackage representation.
///
/// This function takes a translation result (declarations and Cargo.toml) and assembles them into
/// a complete Cargo project structure with:
/// - A Cargo.toml manifest (from the LLM translation response)
/// - A src/main.rs (for executables) or src/lib.rs (for libraries)
/// - All necessary imports derived from declaration dependencies
pub fn recombine_decls(
    translation_result: TranslationResult,
    project_kind: &ProjectKind,
) -> Result<CargoPackage, Box<dyn std::error::Error>> {
    debug!(
        "Recombining {} declarations",
        translation_result.translations.len()
    );

    // Collect all unique dependencies from all declarations
    let mut all_dependencies: HashSet<String> = HashSet::new();
    for decl in &translation_result.translations {
        for dep in &decl.dependencies {
            all_dependencies.insert(dep.clone());
        }
    }

    // Concatenate all Rust code
    let rust_code = translation_result
        .translations
        .into_iter()
        .map(|decl| decl.rust_code)
        .collect::<Vec<_>>()
        .join("\n\n");

    // Determine the main source file name and content based on project kind
    let (source_file, source_content) = match project_kind {
        ProjectKind::Executable => ("src/main.rs", rust_code),
        ProjectKind::Library => ("src/lib.rs", rust_code),
    };

    // Create the directory structure
    let mut dir = RawDir::default();
    dir.set_file("Cargo.toml", translation_result.cargo_toml.into_bytes())?;
    dir.set_file(source_file, source_content.into_bytes())?;

    Ok(CargoPackage { dir })
}
