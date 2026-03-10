//! Translation approach:
//! - TypedefDecl, RecordDecl, EnumDecl: translate data layout
//! - FunctionDecl and VarDecl signatures: translate interface (callable functions and global variables)
//! - FunctionDecl, VarDecl: translate code semantics
//!
//! Design decisions to come back to:
//! - Type results included as context for function/global translation
//! - No ordering constraints of function translations
//! - Cargo.toml generated after function/global translation using aggregated dependencies

use build_project_spec::ProjectKind;
use full_source::RawSource;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use tracing::{debug, error, info};

use crate::Config;
use crate::clang::{ClangDeclarations, ClangNode};
use crate::translation_llm::ModularTranslationLLM;

/// Represents a translated Rust declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustDeclaration {
    /// The translated Rust code output
    pub rust_code: String,
    /// List of dependency module paths to import
    pub dependencies: Vec<String>,
}

/// Result of the type translation containing only type declarations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeTranslationResult {
    pub translations: Vec<RustDeclaration>,
}

/// Result of the interface translation containing function and global variable signature lines
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterfaceTranslationResult {
    pub signatures: Vec<String>,
}

/// Result of the translation containing both declarations and Cargo.toml
#[derive(Debug, Deserialize)]
pub struct TranslationResult {
    pub translations: Vec<RustDeclaration>,
    pub cargo_toml: String,
}

/// Translates type declarations to Rust using an LLM.
///
/// This function translates TypedefDecl, RecordDecl, and EnumDecl to establish
/// data layout for all types in the project. The results are then used as context
/// for function and global variable translation.
///
/// Returns only the translated type declarations (no Cargo.toml).
pub fn translate_types(
    type_decls: &[ClangNode<'_>],
    raw_source: &RawSource,
    project_kind: &ProjectKind,
    modular_llm: &ModularTranslationLLM,
) -> Result<TypeTranslationResult, Box<dyn std::error::Error>> {
    debug!(
        "Starting type translation for {} declarations",
        type_decls.len()
    );

    if type_decls.is_empty() {
        // No types to translate, return empty result
        return Ok(TypeTranslationResult {
            translations: Vec::new(),
        });
    }

    let type_nodes: Vec<_> = type_decls.iter().map(|decl| decl.as_node()).collect();
    let translation_result = modular_llm.translate_types(&type_nodes, raw_source, project_kind)?;

    if translation_result.translations.len() != type_decls.len() {
        error!(
            "Type translation: LLM returned {} translations but expected {}",
            translation_result.translations.len(),
            type_decls.len()
        );
    }

    info!(
        "Type translation complete: successfully translated {} declarations",
        translation_result.translations.len()
    );

    Ok(translation_result)
}

/// Translates function and global variable declarations to Rust using an LLM.
///
/// This function translates FunctionDecl and VarDecl, with the type translations
/// and interface translations provided as context. Each declaration is translated in its own request.
///
/// Returns the translated declarations.
pub fn translate_functions(
    function_and_global_decls: &[ClangNode<'_>],
    raw_source: &RawSource,
    project_kind: &ProjectKind,
    type_translations: &TypeTranslationResult,
    interface_translations: &InterfaceTranslationResult,
    modular_llm: &ModularTranslationLLM,
) -> Result<Vec<RustDeclaration>, Box<dyn std::error::Error>> {
    debug!(
        "Starting function/global translation for {} declarations",
        function_and_global_decls.len()
    );

    if function_and_global_decls.is_empty() {
        // No functions/globals to translate, return empty result
        return Ok(Vec::new());
    }

    let mut translations = Vec::new();

    for decl in function_and_global_decls {
        let translation = modular_llm.translate_function_global(
            decl.as_node(),
            raw_source,
            project_kind,
            type_translations,
            interface_translations,
        )?;

        translations.push(translation);
    }

    info!(
        "Function/global translation complete: successfully translated {} declarations",
        translations.len()
    );

    Ok(translations)
}

/// Translates function and global variable declarations to Rust signature lines using an LLM.
///
/// This function translates FunctionDecl and VarDecl, with the type translations
/// provided as context. All declarations are translated in a single batch.
///
/// Returns the translated signature lines.
pub fn translate_interface(
    function_decls: &[ClangNode<'_>],
    global_decls: &[ClangNode<'_>],
    raw_source: &RawSource,
    project_kind: &ProjectKind,
    type_translations: &TypeTranslationResult,
    modular_llm: &ModularTranslationLLM,
) -> Result<InterfaceTranslationResult, Box<dyn std::error::Error>> {
    debug!(
        "Starting interface translation for {} function and {} global declarations",
        function_decls.len(),
        global_decls.len()
    );

    if function_decls.is_empty() && global_decls.is_empty() {
        return Ok(InterfaceTranslationResult {
            signatures: Vec::new(),
        });
    }

    let signatures = modular_llm.translate_interface(
        function_decls,
        global_decls,
        raw_source,
        project_kind,
        type_translations,
    )?;

    info!(
        "Interface translation complete: successfully translated {} declarations",
        signatures.len()
    );

    Ok(InterfaceTranslationResult { signatures })
}

fn collect_dependencies(translations: &[RustDeclaration]) -> Vec<String> {
    let deps = translations.iter().flat_map(|t| t.dependencies.iter());
    deps.collect::<BTreeSet<_>>().into_iter().cloned().collect()
}

/// Orchestrates the translation of Clang declarations to Rust using an LLM.
///
/// First, translates type declarations (TypedefDecl, RecordDecl, EnumDecl)
/// Then, translates interface (FunctionDecl and VarDecl signatures) with type context
/// Then, translates functions and globals (FunctionDecl, VarDecl) with type and interface context
/// Finally, generates a Cargo.toml manifest based on collected dependencies from all translations.
///
/// Returns the combined translated declarations and a generated Cargo.toml manifest.
pub fn translate_decls(
    declarations: &ClangDeclarations<'_>,
    raw_source: &RawSource,
    project_kind: &ProjectKind,
    config: &Config,
) -> Result<TranslationResult, Box<dyn std::error::Error>> {
    let total_decls = declarations.app_types.len()
        + declarations.app_globals.len()
        + declarations.app_functions.len();

    info!(
        "Starting translation of {} declarations ({} types, {} globals, {} functions)",
        total_decls,
        declarations.app_types.len(),
        declarations.app_globals.len(),
        declarations.app_functions.len()
    );

    if total_decls == 0 {
        return Err("No declarations to translate".into());
    }

    let modular_llm = ModularTranslationLLM::build(config)?;

    // Translate types
    let type_result = translate_types(
        &declarations.app_types,
        raw_source,
        project_kind,
        &modular_llm,
    )?;

    // Translate interface (function and global signatures) with type context
    let interface_result = translate_interface(
        &declarations.app_functions,
        &declarations.app_globals,
        raw_source,
        project_kind,
        &type_result,
        &modular_llm,
    )?;

    // Combine globals and functions for function/global translation
    let function_and_global_decls: Vec<_> = declarations.app_functions_and_globals().collect();

    // Translate functions and globals with type context
    let function_result = if function_and_global_decls.is_empty() {
        info!("No function or global declarations to translate");
        Vec::new()
    } else {
        translate_functions(
            &function_and_global_decls,
            raw_source,
            project_kind,
            &type_result,
            &interface_result,
            &modular_llm,
        )?
    };

    // Combine results: types first, then functions/globals
    let mut combined_translations = type_result.translations;
    combined_translations.extend(function_result);

    info!(
        "Translation complete: {} total declarations translated",
        combined_translations.len()
    );

    let dependencies = collect_dependencies(&combined_translations);
    let cargo_toml = modular_llm.generate_cargo_toml(dependencies, project_kind)?;
    let usage_by_call = modular_llm.usage_by_call();
    let usage_totals = modular_llm.usage_totals();

    info!(
        "token usage [types] - prompt: {}, output: {}, total: {}",
        usage_by_call.types.prompt_tokens,
        usage_by_call.types.output_tokens,
        usage_by_call.types.total_tokens
    );
    info!(
        "token usage [interface] - prompt: {}, output: {}, total: {}",
        usage_by_call.interface.prompt_tokens,
        usage_by_call.interface.output_tokens,
        usage_by_call.interface.total_tokens
    );
    info!(
        "token usage [functions] - prompt: {}, output: {}, total: {}",
        usage_by_call.functions.prompt_tokens,
        usage_by_call.functions.output_tokens,
        usage_by_call.functions.total_tokens
    );
    info!(
        "token usage [cargo_toml] - prompt: {}, output: {}, total: {}",
        usage_by_call.cargo_toml.prompt_tokens,
        usage_by_call.cargo_toml.output_tokens,
        usage_by_call.cargo_toml.total_tokens
    );

    info!(
        "token usage [total] - prompt: {}, output: {}, total: {}",
        usage_totals.prompt_tokens, usage_totals.output_tokens, usage_totals.total_tokens
    );

    Ok(TranslationResult {
        translations: combined_translations,
        cargo_toml,
    })
}
