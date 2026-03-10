//! Attempts to directly turn a C project into a Cargo project by throwing it at
//! an LLM via the `llm` crate.

use build_project_spec::{ProjectKind, ProjectSpec};
use full_source::{CargoPackage, RawSource};
use harvest_core::config::unknown_field_warning;
use harvest_core::fs::RawDir;
use harvest_core::llm::{HarvestLLM, LLMConfig, LLMUsageTotals, build_request};
use harvest_core::tools::{RunContext, Tool};
use harvest_core::{Id, Representation};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::read_to_string;
use std::path::PathBuf;
use tracing::{debug, info, trace};

/// Structured output JSON schema for Ollama.
const STRUCTURED_OUTPUT_SCHEMA: &str = include_str!("structured_schema.json");

const SYSTEM_PROMPT_EXECUTABLE: &str = include_str!("system_prompt_executable.txt");
const SYSTEM_PROMPT_LIBRARY: &str = include_str!("system_prompt_library.txt");

pub struct RawSourceToCargoLlm;

impl Tool for RawSourceToCargoLlm {
    fn name(&self) -> &'static str {
        "raw_source_to_cargo_llm"
    }

    fn run(
        self: Box<Self>,
        context: RunContext,
        inputs: Vec<Id>,
    ) -> Result<Box<dyn Representation>, Box<dyn std::error::Error>> {
        let config =
            Config::deserialize(context.config.tools.get("raw_source_to_cargo_llm").unwrap())?;
        debug!("LLM Configuration {config:?}");
        // Get both inputs: RawSource and ProjectSpec
        let in_dir = context
            .ir_snapshot
            .get::<RawSource>(inputs[0])
            .ok_or("No RawSource representation found in IR")?;
        let project_spec = context
            .ir_snapshot
            .get::<ProjectSpec>(inputs[1])
            .ok_or("No ProjectSpec representation found in IR")?;
        let project_kind = &project_spec.kind;

        // Use the llm crate to connect to the LLM.
        // Select system prompt based on project kind
        let (config_prompt, builtin_prompt) = match project_kind {
            ProjectKind::Executable => (config.prompt_executable, SYSTEM_PROMPT_EXECUTABLE),
            ProjectKind::Library => (config.prompt_library, SYSTEM_PROMPT_LIBRARY),
        };
        let system_prompt = config_prompt
            .map(read_to_string)
            .transpose()?
            .unwrap_or_else(|| builtin_prompt.to_owned());

        // Build LLM client using core/llm
        let llm = HarvestLLM::build(&config.llm, STRUCTURED_OUTPUT_SCHEMA, &system_prompt)?;

        // Assemble the LLM request.
        let files: Vec<OutputFile> = in_dir
            .dir
            .files_recursive()
            .iter()
            .map(|(path, contents)| OutputFile {
                path: path.clone(),
                contents: String::from_utf8_lossy(contents).into(),
            })
            .collect();

        #[derive(Serialize)]
        struct RequestBody {
            files: Vec<OutputFile>,
        }

        let request = build_request(
            "Please translate the following C project into a Rust project including Cargo manifest:",
            &RequestBody { files },
        )?;

        // Make the LLM call.
        trace!("Making LLM call with {:?}", request);
        let mut usage_totals = LLMUsageTotals::default();
        let (response, usage) = llm.invoke(&request)?;
        usage_totals.add_usage(usage.as_ref());

        // Parse the response, convert it into a CargoPackage representation.
        #[derive(Deserialize)]
        struct OutputFiles {
            files: Vec<OutputFile>,
        }
        trace!("LLM responded: {:?}", &response);
        let files: OutputFiles = serde_json::from_str(&response)?;
        info!("LLM response contains {} files.", files.files.len());
        let mut out_dir = RawDir::default();
        for file in files.files {
            out_dir.set_file(&file.path, file.contents.into())?;
        }

        info!(
            "Token usage [total] - prompt: {}, output: {}, total: {}",
            usage_totals.prompt_tokens, usage_totals.output_tokens, usage_totals.total_tokens
        );

        Ok(Box::new(CargoPackage { dir: out_dir }))
    }
}

#[derive(Debug, Deserialize)]
pub struct Config {
    /// LLM configuration.
    #[serde(flatten)]
    pub llm: LLMConfig,

    /// System prompt to use for executable projects. If not specified, a built-in default prompt
    /// will be used.
    pub prompt_executable: Option<PathBuf>,

    /// System prompt to use for library projects. If not specified, a built-in default prompt will
    /// be used.
    pub prompt_library: Option<PathBuf>,

    #[serde(flatten)]
    unknown: HashMap<String, Value>,
}

impl Config {
    pub fn validate(&self) {
        unknown_field_warning("tools.raw_source_to_cargo_llm", &self.unknown);
    }

    /// Returns a mock config for testing.
    pub fn mock() -> Self {
        Self {
            llm: LLMConfig {
                address: None,
                api_key: None,
                backend: "mock_llm".into(),
                model: "mock_model".into(),
                max_tokens: 1000,
            },
            prompt_executable: None,
            prompt_library: None,
            unknown: HashMap::new(),
        }
    }
}

/// Structure representing a file created by the LLM.
#[derive(Debug, Deserialize, Serialize)]
struct OutputFile {
    contents: String,
    path: PathBuf,
}
