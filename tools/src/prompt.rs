//! Which prompt an invocation is handed, and how long a chain the tool runs.
//!
//! Two axes, deliberately separate. [`Role`] is which step of the chain this is; [`Shape`] is what
//! the case is. The old `PromptKind` mixed them -- `Library`, `Executable`, `Shared` and `Verify`
//! were one enum -- so a verify prompt could not depend on the shape and every executable case was
//! told to produce a cdylib. Splitting them makes that unrepresentable rather than a known bug.
//!
//! The chain is declared HERE, by tool and variant, and nowhere else. It used to be stated twice --
//! `has_verify_phase` and a `Verify => None` arm in the prompt table -- with two tests existing only
//! to hold the two in step.

use crate::cli::{Tool, Variant};
use anyhow::{Context, Result};
use std::path::Path;

/// Which step of a chain a prompt drives.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Role {
    Translate,
    Verify,
}

impl Role {
    /// The results sub-directory this step publishes into, and the name a table reports. Data, not
    /// a type: adding a step must not require a new type parameter threaded through the crate.
    pub fn dir(self) -> &'static str {
        match self {
            Role::Translate => "translated",
            Role::Verify => "verified",
        }
    }

    pub fn log(self) -> &'static str {
        match self {
            Role::Translate => "translation.log",
            Role::Verify => "verify.log",
        }
    }
}

/// What the case is. Chosen per case, not per step, so both roles see the same answer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Shape {
    Library,
    Executable,
    /// One source, many CMake feature configurations: one invocation, N derived followers.
    Shared,
}

impl Shape {
    pub fn of(is_lib: bool, shared: bool) -> Self {
        match (shared, is_lib) {
            (true, _) => Shape::Shared,
            (false, true) => Shape::Library,
            (false, false) => Shape::Executable,
        }
    }
}

/// Whether this tool has the prompts a variant needs.
///
/// The ablations are claude experiments and only claude's prompt directory holds them. Refused rather
/// than fallen back on: silently reading the base prompt under an ablation's name would file a
/// result as an experiment that never ran.
pub fn supports(tool: Tool, variant: Variant) -> Result<()> {
    anyhow::ensure!(
        variant == Variant::Default || tool == Tool::Claude,
        "--prompt {} is a claude ablation; --tool {} has no such prompt set, and reading its base \
         prompt instead would file the result as an experiment that never ran",
        variant.dir(),
        crate::cli::tool_dir(tool)
    );
    Ok(())
}

/// The steps this tool and variant runs, in order.
///
/// A one-step chain is not a tool "without a verify phase" -- it is a prompt that folds
/// verification into translation (`Variant::Combined`), or a backend that has only ever had one
/// step. Either way the chain length follows the prompt, which is the only place that knows.
pub fn chain(tool: Tool, variant: Variant) -> &'static [Role] {
    const BOTH: &[Role] = &[Role::Translate, Role::Verify];
    const ONE: &[Role] = &[Role::Translate];
    match variant {
        // Every ablation is a single-step experiment: `Combined` folds verification into its own
        // prompt, and the rest are translate-only by construction.
        Variant::Combined
        | Variant::Minimal
        | Variant::NoIter
        | Variant::NoFeatures
        | Variant::NoSubtask
        | Variant::CrossPrompt => ONE,
        Variant::Default => match tool {
            Tool::Claude | Tool::Codex | Tool::Kiro | Tool::OpenCode => BOTH,
            // A single-shot API call and the transpilers have one step and no prompt to give a
            // second one to.
            Tool::Oneshot | Tool::Kimi => ONE,
            Tool::C2rust | Tool::Laertes | Tool::C2SaferRust | Tool::SmartC2Rust => ONE,
        },
    }
}

/// The one place a prompt file is chosen, relative to the tool's prompt directory.
///
/// `None` means this tool reads no prompt for that role at all -- the transpilers are given none.
/// Returning the NAME rather than the text is what lets a test check the choice against the files on
/// disk, so a renamed prompt fails in CI instead of reaching a paid run as an empty one.
pub fn file_for(tool: Tool, variant: Variant, role: Role, shape: Shape) -> Option<&'static str> {
    if let Some(f) = ablation_file(variant, role, shape) {
        return Some(f);
    }
    match tool {
        // One arm on purpose: the backend varies, the methodology does not.
        Tool::Kiro | Tool::Claude | Tool::OpenCode | Tool::Codex => Some(match (role, shape) {
            (Role::Translate, Shape::Library) => "translate-library.md",
            (Role::Translate, Shape::Executable) => "translate-executable.md",
            (Role::Translate, Shape::Shared) => "translate-shared.md",
            // Shape-dispatched, unlike the single `verify.md` this replaces: an executable case
            // told to produce a cdylib is being asked for the wrong artifact.
            (Role::Verify, Shape::Library) => "verify-library.md",
            (Role::Verify, Shape::Executable) => "verify-executable.md",
            (Role::Verify, Shape::Shared) => "verify-shared.md",
        }),
        Tool::Oneshot | Tool::Kimi => match (role, shape) {
            (Role::Translate, Shape::Library | Shape::Shared) => Some("translate-library.md"),
            (Role::Translate, Shape::Executable) => Some("translate-executable.md"),
            (Role::Verify, _) => None,
        },
        // Transpilers and docker baselines read nothing.
        Tool::C2rust | Tool::Laertes | Tool::C2SaferRust | Tool::SmartC2Rust => None,
    }
}

/// The ablation forks, all under `<tool>/ablations/`. Exhaustive over [`Variant`] so a new one has
/// to state its files rather than silently inherit the base prompts.
fn ablation_file(variant: Variant, role: Role, shape: Shape) -> Option<&'static str> {
    use Shape::{Executable, Library, Shared};
    if role == Role::Verify {
        // Every ablation is one step; `chain` already says so, and this is the same fact seen from
        // the prompt table. Both must agree, which is why only one of them decides.
        return None;
    }
    match variant {
        Variant::Default => None,
        Variant::Combined => Some(match shape {
            Library => "ablations/translate-and-verify-library.md",
            Executable => "ablations/translate-and-verify-executable.md",
            Shared => "ablations/translate-and-verify-shared.md",
        }),
        // One prompt for every shape -- that IS the ablation.
        Variant::Minimal => Some("ablations/translate-minimal.md"),
        Variant::NoIter => Some(match shape {
            Library => "ablations/translate-no-iter-library.md",
            Executable => "ablations/translate-no-iter-executable.md",
            Shared => "ablations/translate-no-iter-shared.md",
        }),
        // E2 and E6 differ from the base on shared-source cases only, so their independent cases
        // deliberately read the unmodified prompts.
        Variant::NoFeatures => match shape {
            Shared => Some("ablations/translate-no-features-shared.md"),
            _ => None,
        },
        Variant::NoSubtask => match shape {
            Shared => Some("ablations/translate-no-subtask-shared.md"),
            _ => None,
        },
        // E4: the mismatch IS the experiment -- a library gets the executable prompt and vice
        // versa. A shared-source case has no counterpart to swap with.
        Variant::CrossPrompt => Some(match shape {
            Library => "translate-executable.md",
            Executable => "translate-library.md",
            Shared => "translate-shared.md",
        }),
    }
}

/// The tool-specific tail of a composed prompt.
pub const PROTOCOL_PART: &str = "protocol.md";

/// Where the file [`file_for`] names lives, and whether the tool's protocol part follows it.
///
/// `Shared` holds the methodology ONCE in `prompts/shared/` + `<tool>/protocol.md`. kiro's own copy
/// was HALF the length on 3 of 6 files, missing SCOPE and COMPLETION GATE; its zstd lost 79 symbols.
enum Body {
    Shared,
    OwnDir,
}

fn body(tool: Tool, file: &str) -> Body {
    // An ablation forks the whole document, protocol included.
    if file.starts_with("ablations/") {
        return Body::OwnDir;
    }
    match tool {
        Tool::Claude | Tool::OpenCode | Tool::Codex | Tool::Kiro => Body::Shared,
        // No agentic loop, so no protocol tail to differ in.
        Tool::Kimi
        | Tool::Oneshot
        | Tool::C2rust
        | Tool::Laertes
        | Tool::C2SaferRust
        | Tool::SmartC2Rust => Body::OwnDir,
    }
}

fn read_part(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the prompt {}", path.display()))?;
    anyhow::ensure!(
        !text.trim().is_empty(),
        "the prompt {} is empty",
        path.display()
    );
    Ok(text)
}

/// Where a tool's own prompts live.
pub fn dir_for(repo_root: &Path, tool: Tool) -> std::path::PathBuf {
    let prompts = repo_root.join("prompts");
    match tool {
        // The one-shot calls share a prompt set: neither runs an agentic loop, so neither has a
        // protocol part to differ in.
        Tool::Oneshot | Tool::Kimi => prompts.join("oneshot"),
        // OpenCode drives Claude models through a different CLI, and has always been given claude's
        // set -- including its sub-agent protocol. Stated here rather than left to a fallback: if
        // OpenCode ever needs its own protocol, this is the line that has to change.
        Tool::OpenCode => prompts.join("claude"),
        _ => prompts.join(crate::cli::tool_dir(tool)),
    }
}

const FLAGS: &str = "CMAKE_BUILD_FLAGS";

/// The `-D` flags the corpus builds this case's C with: the second `configurePresets` entry, the first
/// being a hidden base. Deterministic from the corpus, so it belongs in the prompt hash -- two feature
/// configurations of one source are two questions, and a prompt naming neither keys both to one entry.
fn cmake_flags(case_inputs: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(case_inputs.join("CMakePresets.json")) else {
        return String::new();
    };
    let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) else {
        return String::new();
    };
    let Some(vars) = data
        .pointer("/configurePresets/1/cacheVariables")
        .and_then(|v| v.as_object())
    else {
        return String::new();
    };
    vars.iter()
        .filter(|(k, _)| *k != "CMAKE_C_STANDARD" && *k != "CMAKE_BUILD_TYPE")
        .map(|(k, v)| format!("-D{k}={}", v.as_str().unwrap_or_default()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The text of the prompt for this step, ready to hand an agent, or `None` where the tool reads none.
///
/// A named prompt missing on disk is an error, never an empty string: an empty prompt invokes the agent
/// with nothing to do and the result is recorded as a measurement. Substitution happens HERE so a call
/// site cannot ship a placeholder -- the driver that replaced `verify.rs`'s `Rendering` step read the
/// file directly and sent `CMAKE_BUILD_FLAGS` verbatim to a paid run. `CASE_DIR_PLACEHOLDER` is gone
/// rather than substituted: naming the agent's scratch path would make every prompt hash a nonce.
pub fn read(
    repo_root: &Path,
    tool: Tool,
    variant: Variant,
    role: Role,
    shape: Shape,
    case_inputs: &Path,
) -> Result<Option<String>> {
    let Some(file) = file_for(tool, variant, role, shape) else {
        return Ok(None);
    };
    let own = dir_for(repo_root, tool);
    let text = match body(tool, file) {
        Body::OwnDir => read_part(&own.join(file))?,
        // Concatenated with no separator: the body keeps its own trailing newline, so the
        // composition is byte-for-byte what the two files were as one.
        Body::Shared => {
            read_part(&repo_root.join("prompts/shared").join(file))?
                + &read_part(&own.join(PROTOCOL_PART))?
        }
    };
    Ok(Some(text.replace(FLAGS, &cmake_flags(case_inputs))))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOOLS: &[Tool] = &[
        Tool::Claude,
        Tool::Codex,
        Tool::Kiro,
        Tool::OpenCode,
        Tool::Oneshot,
        Tool::Kimi,
        Tool::C2rust,
        Tool::Laertes,
        Tool::C2SaferRust,
        Tool::SmartC2Rust,
    ];
    const VARIANTS: &[Variant] = &[
        Variant::Default,
        Variant::Combined,
        Variant::Minimal,
        Variant::NoIter,
        Variant::NoFeatures,
        Variant::NoSubtask,
        Variant::CrossPrompt,
    ];
    const SHAPES: &[Shape] = &[Shape::Library, Shape::Executable, Shape::Shared];

    #[test]
    fn a_chain_step_always_has_a_prompt_to_run() {
        // The two used to be separate tables -- `has_verify_phase` and a `Verify => None` arm --
        // held together by two tests. One decides now, so the failure this pins is a chain that
        // schedules a step no prompt exists for: the agent would be invoked with an empty prompt
        // and the result recorded as a measurement.
        let mut checked = 0;
        for &tool in TOOLS {
            for &variant in VARIANTS {
                if supports(tool, variant).is_err() {
                    continue;
                }
                for &shape in SHAPES {
                    for &role in chain(tool, variant) {
                        let named = file_for(tool, variant, role, shape).is_some();
                        // A transpiler runs one step and reads no prompt at all; that is the one
                        // legitimate absence, and it is a property of the tool, not of the role.
                        let reads_prompts = !matches!(
                            tool,
                            Tool::C2rust | Tool::Laertes | Tool::C2SaferRust | Tool::SmartC2Rust
                        );
                        if reads_prompts {
                            assert!(
                                named,
                                "{tool:?}/{variant:?} schedules {role:?} for {shape:?} with no prompt"
                            );
                            checked += 1;
                        }
                    }
                }
            }
        }
        assert!(checked > 0, "this rule inspected nothing");
    }

    #[test]
    fn every_named_prompt_exists_on_disk() {
        // A renamed file must fail here rather than reaching a paid run as an empty prompt.
        // The real prompts/ tree, found from this file rather than the cwd: `cargo test` runs from
        // the workspace and a relative path would silently inspect nothing.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("tools/ has a parent")
            .to_path_buf();
        let mut checked = 0;
        for &tool in TOOLS {
            for &variant in VARIANTS {
                if supports(tool, variant).is_err() {
                    continue;
                }
                for &shape in SHAPES {
                    for &role in &[Role::Translate, Role::Verify] {
                        let Some(file) = file_for(tool, variant, role, shape) else {
                            continue;
                        };
                        let at = match body(tool, file) {
                            Body::OwnDir => dir_for(&root, tool).join(file),
                            Body::Shared => root.join("prompts/shared").join(file),
                        };
                        assert!(
                            at.is_file(),
                            "{tool:?}/{variant:?}/{role:?}/{shape:?} names {} which does not exist",
                            at.display()
                        );
                        if matches!(body(tool, file), Body::Shared) {
                            let protocol = dir_for(&root, tool).join(PROTOCOL_PART);
                            assert!(
                                protocol.is_file(),
                                "{tool:?} composes a shared body but has no {PROTOCOL_PART}"
                            );
                        }
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 0, "this rule inspected nothing");
    }

    #[test]
    fn no_prompt_reaches_an_agent_with_a_placeholder_left_in_it() {
        // A paid run was handed the literal `CMAKE_BUILD_FLAGS` as a cmake argument.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("tools/ has a parent")
            .to_path_buf();
        let inputs = root.join("test-corpus/Public-Tests/B02_synthetic/macrodepth_add_5");
        let mut checked = 0;
        let mut saw_a_substitution = false;
        for &tool in TOOLS {
            for &variant in VARIANTS {
                if supports(tool, variant).is_err() {
                    continue;
                }
                for &shape in SHAPES {
                    for &role in chain(tool, variant) {
                        let Some(text) = read(&root, tool, variant, role, shape, &inputs).unwrap()
                        else {
                            continue;
                        };
                        let raw = file_for(tool, variant, role, shape)
                            .map(|f| match body(tool, f) {
                                Body::OwnDir => dir_for(&root, tool).join(f),
                                Body::Shared => root.join("prompts/shared").join(f),
                            })
                            .map(|p| std::fs::read_to_string(p).unwrap_or_default())
                            .unwrap_or_default();
                        if raw.contains(FLAGS) {
                            saw_a_substitution = true;
                        }
                        for bad in [FLAGS, "PLACEHOLDER"] {
                            assert!(
                                !text.contains(bad),
                                "{tool:?}/{variant:?}/{role:?}/{shape:?} still contains {bad}"
                            );
                        }
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 0, "this rule inspected nothing");
        assert!(
            saw_a_substitution,
            "no prompt on disk carries {FLAGS}, so this proves nothing about substitution"
        );
    }

    #[test]
    fn a_verify_prompt_depends_on_the_shape_of_the_case() {
        // The whole reason Role and Shape are separate axes: one `verify.md` told an executable
        // case to produce a cdylib, which is the wrong artifact for it.
        let lib = file_for(Tool::Claude, Variant::Default, Role::Verify, Shape::Library);
        let exe = file_for(
            Tool::Claude,
            Variant::Default,
            Role::Verify,
            Shape::Executable,
        );
        assert!(lib.is_some() && exe.is_some());
        assert_ne!(lib, exe, "verify must not be shape-blind");
    }
}
