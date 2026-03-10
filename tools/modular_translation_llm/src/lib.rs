//! Modular translation for C->Rust. Decomposes a C project AST into its top-level modules
//! and translates them one-by-one using an LLM.
//!
//! Translation flow:
//! - Type declarations (TypedefDecl, RecordDecl, EnumDecl) establish data layout
//! - Interface (FunctionDecl and VarDecl signatures) use type context
//! - Functions and globals (FunctionDecl, VarDecl) use type/interface context

use build_project_spec::{ProjectKind, ProjectSpec};
use c_ast::ClangAst;
use full_source::RawSource;
use harvest_core::config::unknown_field_warning;
use harvest_core::llm::LLMConfig;
use harvest_core::tools::{RunContext, Tool};
use harvest_core::{Id, Representation};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use tracing::info;

mod clang;
mod recombine;
mod translation;
mod translation_llm;
mod utils;
pub use clang::{ClangDeclarations, ClangNode, extract_top_level_decls};
pub use translation::{
    InterfaceTranslationResult, RustDeclaration, TranslationResult, TypeTranslationResult,
    translate_decls, translate_functions, translate_interface, translate_types,
};
pub use translation_llm::ModularTranslationLLM;

/// Configuration for the modular translation tool.
#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(flatten)]
    pub llm: LLMConfig,

    #[serde(flatten)]
    unknown: HashMap<String, Value>,
}

impl Config {
    pub fn validate(&self) {
        unknown_field_warning("tools.modular_translation_llm", &self.unknown);
    }

    /// Returns a mock config for testing.
    pub fn mock() -> Self {
        Self {
            llm: LLMConfig {
                address: None,
                api_key: None,
                backend: "mock_llm".into(),
                model: "mock_model".into(),
                max_tokens: 4000,
            },
            unknown: HashMap::new(),
        }
    }
}

/// The main tool struct for modular translation.
pub struct ModularTranslationLlm;

/// Extracts and validates the tool's input arguments from the context.
fn extract_args<'a>(
    context: &'a RunContext,
    inputs: &[Id],
) -> Result<(&'a RawSource, &'a ClangAst, &'a ProjectKind), Box<dyn std::error::Error>> {
    let raw_source = context
        .ir_snapshot
        .get::<RawSource>(inputs[0])
        .ok_or("No RawSource representation found in IR")?;
    let clang_ast = context
        .ir_snapshot
        .get::<ClangAst>(inputs[1])
        .ok_or("No ClangAst representation found in IR")?;
    let project_spec = context
        .ir_snapshot
        .get::<ProjectSpec>(inputs[2])
        .ok_or("No ProjectSpec representation found in IR")?;
    Ok((raw_source, clang_ast, &project_spec.kind))
}

impl Tool for ModularTranslationLlm {
    fn name(&self) -> &'static str {
        "modular_translation_llm"
    }

    fn run(
        self: Box<Self>,
        context: RunContext,
        inputs: Vec<Id>,
    ) -> Result<Box<dyn Representation>, Box<dyn std::error::Error>> {
        let config = Config::deserialize(
            context
                .config
                .tools
                .get("modular_translation_llm")
                .ok_or("No modular_translation_llm config found")?,
        )?;
        config.validate();

        let (raw_source, clang_ast, project_kind) = extract_args(&context, &inputs)?;

        // Extract and categorize top-level declarations into types, globals, and functions
        let declarations = extract_top_level_decls(clang_ast, raw_source);

        info!(
            "Extracted {} type declarations, {} global declarations, {} function declarations",
            declarations.app_types.len(),
            declarations.app_globals.len(),
            declarations.app_functions.len()
        );

        // Translation flow:
        // Types (TypedefDecl, RecordDecl, EnumDecl) - establish data layout
        // Interface (FunctionDecl and VarDecl signatures) - with type context
        // Functions and Globals (FunctionDecl, VarDecl) - with type/interface context
        let translation_result =
            translation::translate_decls(&declarations, raw_source, project_kind, &config)
                .map_err(|e| format!("Translation failed: {}", e))?;

        info!(
            "Translation complete: {} total declarations translated",
            translation_result.translations.len()
        );

        // Assemble translations into a CargoPackage representation
        let cargo_package = recombine::recombine_decls(translation_result, project_kind)?;

        Ok(Box::new(cargo_package))
    }
}
