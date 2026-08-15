//! OpenCode (<https://opencode.ai>) headless-CLI backend. Runs ACTOR's
//! `prompts/claude/*.md` unchanged against any model (notably Bedrock): the
//! only prompt difference is the appended [`prompt_suffix`], which is empty for
//! every other agent, so prompts stay byte-identical across backends.
//!
//! Each numbered WORKAROUND below guards a SILENT upstream failure — a hang, a
//! truncated turn, a dropped system prompt — that never appears in a log.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Matches the region the other Bedrock-backed agents use (`translate.rs`
/// codex/c2saferrust arms).
const DEFAULT_BEDROCK_REGION: &str = "us-west-2";

const BEDROCK_PROVIDER: &str = "amazon-bedrock";

/// `webfetch`/`websearch`/`skill` are denied so a run cannot reach outside the
/// benchmark or pull in ambient instructions.
const LOCAL_PERMISSIONS: &[(&str, &str)] = &[
    ("bash", "allow"),
    ("read", "allow"),
    ("edit", "allow"),
    ("write", "allow"),
    ("glob", "allow"),
    ("grep", "allow"),
    ("task", "allow"),
    ("todowrite", "allow"),
    ("lsp", "allow"),
    ("webfetch", "deny"),
    ("websearch", "deny"),
    ("skill", "deny"),
];

/// Auto-loaded by OpenCode from `.opencode/plugin/`; mechanism in its header.
const COMPACTION_PLUGIN: &str = include_str!("opencode_compaction_recovery.js");

/// WORKAROUND 1 (of 4) — sub-agent deadlock. `external_directory` defaults to
/// `"ask"` and task sub-agents do NOT inherit `--dangerously-skip-permissions`,
/// so a headless sub-agent touching a path outside the project blocks FOREVER
/// on a prompt nobody can answer. [`project_config`] makes "ask" unreachable;
/// this block names the boundary so a denial is a fast error instead. ACTOR's
/// prompts do use sub-agents (`translate-library.md`, `verify.md`).
const WORKDIR_BOUNDARY_TEMPLATE: &str = r#"### Filesystem boundary
- `{TEMPDIR}` is the ONLY directory you may read or write.
- Keep all scratch files inside the project directory.
- Prefer relative paths. Never retype the absolute temp-directory prefix
  by hand: a single typo in it makes the path "external" and the call is
  denied.
- When you dispatch sub-agents, copy this entire "Filesystem boundary"
  section into every sub-agent prompt verbatim."#;

/// WORKAROUND 2 (of 4) — 32k output cap (opencode#29363). OpenCode caps each
/// response at `min(limit.output, 32000)`, thinking included; a turn that burns
/// the cap on thinking ends with no tool call and counts as complete, so a
/// sub-agent returns empty having written nothing. [`invoke`] raises the cap but
/// a hard one remains, so the prompt must steer away from long thinking too.
const OUTPUT_CAP_WARNING: &str = "\
### OpenCode output-token cap bug
OpenCode caps the output tokens of each model response (upstream issue #29363). \
Thinking tokens count against the same cap. \
If thinking uses the full cap before your first tool call, the turn ends as if it were complete. \
The session then stops silently. \
A sub-agent that stops this way returns an empty result and writes no files.
Therefore: keep thinking short. Do not draft a whole file in thinking. \
Write long files in parts: create the file with one `write` call, \
then append each next part with `edit`. Keep each part under ~300 lines. \
Copy this whole warning into EVERY sub-agent prompt.";

// ── Model ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub provider: String,
    pub model_id: String,
}

impl Model {
    pub fn as_arg(&self) -> String {
        format!("{}/{}", self.provider, self.model_id)
    }

    /// The `:suffix` routing hint must go to OpenCode verbatim but is absent
    /// from registry metadata, so limit lookups must match on the bare id.
    fn metadata_id(&self) -> &str {
        self.model_id.split_once(':').map_or(&self.model_id, |(id, _)| id)
    }

    fn is_bedrock(&self) -> bool {
        self.provider == BEDROCK_PROVIDER
    }
}

/// Rejects a bare model name: OpenCode cannot resolve one and would fail deep
/// inside the run instead of at startup.
pub fn parse_model(s: &str) -> Result<Model> {
    match s.split_once('/') {
        Some((provider, model_id)) if !provider.is_empty() && !model_id.is_empty() => Ok(Model {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
        }),
        _ => anyhow::bail!(
            "--agent opencode needs --model <provider>/<model-id>, got {s:?}.\n\
             Examples:\n  \
               amazon-bedrock/us.anthropic.claude-sonnet-5\n  \
               amazon-bedrock/openai.gpt-5.5\n  \
               openrouter/deepseek/deepseek-v4-pro"
        ),
    }
}

/// The Bedrock regional prefix (`us.`/`eu.`/`global.`/`au.`/`jp.`) is routing,
/// not identity, so the same model in two regions must not produce two results
/// dirs. Vendor prefix likewise — the agent name already says which harness ran.
pub fn results_slug(m: &Model) -> String {
    // Last slash segment, matching Agent::Oneshot's convention.
    let id = m.model_id.rsplit('/').next().unwrap_or(&m.model_id);
    let id = id.split_once(':').map_or(id, |(head, _)| head);
    let bare = id
        .strip_prefix("us.")
        .or_else(|| id.strip_prefix("eu."))
        .or_else(|| id.strip_prefix("global."))
        .or_else(|| id.strip_prefix("au."))
        .or_else(|| id.strip_prefix("jp."))
        .unwrap_or(id);
    let bare = bare.split_once('.').map_or(bare, |(_, rest)| rest);
    format!("opencode-{bare}")
}

// ── Phase ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Translate,
    Verify,
}

impl Phase {
    fn agent_name(self) -> &'static str {
        match self {
            Phase::Translate => "harvest-translate",
            Phase::Verify => "harvest-verify",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Phase::Translate => "ACTOR agentic translation backend",
            Phase::Verify => "ACTOR agentic verification backend",
        }
    }

    /// `verify.md` GATES its later phases on `SYMBOLS.md`/`ERRORS.md`/
    /// `CONFIGS.md`; losing them to a compaction silently un-gates those phases
    /// — the agent keeps working and reports success against a table it can no
    /// longer see. The translate prompts keep no persistent artifacts, so
    /// `None` here means no compaction plugin is written.
    fn recovery_command(self) -> Option<&'static str> {
        match self {
            Phase::Translate => None,
            Phase::Verify => Some("cat SYMBOLS.md ERRORS.md CONFIGS.md"),
        }
    }
}

// ── Prompt additions ───────────────────────────────────────────────────

pub fn prompt_suffix(tmp_root: &Path) -> String {
    format!(
        "\n\n---\n\n{}\n\n{}\n",
        WORKDIR_BOUNDARY_TEMPLATE.replace("{TEMPDIR}", &tmp_root.display().to_string()),
        OUTPUT_CAP_WARNING,
    )
}

// ── Run-private configuration ──────────────────────────────────────────

/// Project-level `opencode.json` for one run. `profile` is OMITTED when
/// `AWS_PROFILE` is unset: an empty or guessed name breaks the default AWS
/// credential chain that otherwise works.
///
/// OpenCode resolves permission rules LAST-MATCH-WINS, so the catch-all deny
/// must be emitted BEFORE the tempdir allow — reversing them denies everything.
/// Hence ordered text rather than a `HashMap`.
fn project_config(tmp_root: &Path, m: &Model) -> String {
    let tempdir_pattern = format!("{}/**", tmp_root.display());
    let provider_block = if m.is_bedrock() {
        let region = std::env::var("AWS_REGION")
            .ok()
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| DEFAULT_BEDROCK_REGION.to_string());
        let mut opts = vec![format!(
            "        \"region\": {}",
            json_str(&region)
        )];
        if let Some(profile) = std::env::var("AWS_PROFILE").ok().filter(|p| !p.is_empty()) {
            opts.push(format!("        \"profile\": {}", json_str(&profile)));
        }
        format!(
            "  \"provider\": {{\n    \"{BEDROCK_PROVIDER}\": {{\n      \"options\": {{\n{}\n      }}\n    }}\n  }},\n",
            opts.join(",\n"),
        )
    } else {
        String::new()
    };

    format!(
        "{{\n  \"$schema\": \"https://opencode.ai/config.json\",\n{provider_block}  \
         \"permission\": {{\n    \"external_directory\": {{\n      \
         \"*\": \"deny\",\n      {}: \"allow\"\n    }}\n  }}\n}}\n",
        json_str(&tempdir_pattern),
    )
}

/// Avoids a map type, which would reorder keys (see [`project_config`]).
fn json_str(s: &str) -> String {
    serde_json::to_string(s).expect("string serializes to JSON")
}

/// The body after the frontmatter MUST stay EMPTY. OpenCode assembles requests
/// as `agent.prompt ? [agent.prompt] : SystemPrompt.provider(model)` and the
/// `.md` body becomes `agent.prompt`, so any body content silently REPLACES
/// OpenCode's default coding system prompt instead of adding to it. Recovery
/// instructions belong in the compaction plugin, not here.
fn agent_definition(phase: Phase) -> String {
    let mut permissions = String::new();
    for (tool, policy) in LOCAL_PERMISSIONS {
        permissions.push_str(&format!("  {tool}: {policy}\n"));
    }
    format!(
        "---\ndescription: {}\nmode: primary\npermission:\n{}---\n",
        phase.description(),
        permissions,
    )
}

pub fn materialize_config(tmp_root: &Path, phase: Phase, m: &Model) -> Result<()> {
    let oc_dir = tmp_root.join(".opencode");
    let agents_dir = oc_dir.join("agents");
    std::fs::create_dir_all(&agents_dir)
        .with_context(|| format!("creating {}", agents_dir.display()))?;

    std::fs::write(oc_dir.join("opencode.json"), project_config(tmp_root, m))
        .context("writing .opencode/opencode.json")?;

    std::fs::write(
        agents_dir.join(format!("{}.md", phase.agent_name())),
        agent_definition(phase),
    )
    .context("writing .opencode/agents/<phase>.md")?;

    // WORKAROUND 4 — post-compaction recovery (see Phase::recovery_command).
    if let Some(cmd) = phase.recovery_command() {
        let plugin_dir = oc_dir.join("plugin");
        std::fs::create_dir_all(&plugin_dir)
            .with_context(|| format!("creating {}", plugin_dir.display()))?;
        std::fs::write(
            plugin_dir.join("compaction-recovery.js"),
            COMPACTION_PLUGIN.replace("{RECOVERY_CMD}", cmd),
        )
        .context("writing .opencode/plugin/compaction-recovery.js")?;
    }

    // WORKAROUND 3 — XDG isolation target; empty (see xdg_config_dir).
    std::fs::create_dir_all(xdg_config_dir(tmp_root)).context("creating run-private XDG dir")?;

    Ok(())
}

/// WORKAROUND 3 (of 4) — global-config isolation. OpenCode resolves global
/// config via `XDG_CONFIG_HOME/opencode`, so aiming that at a run-private empty
/// dir makes the developer's `opencode.json`, plugins and `AGENTS.md`
/// unreachable — ambient instructions cannot silently change a benchmark
/// result. Auth (XDG *data*) and models cache (XDG *cache*) are unaffected, so
/// Bedrock credentials still resolve. Stronger than `--pure`, which only clears
/// external plugins — and would disable our compaction plugin, so [`invoke`]
/// does not pass it.
fn xdg_config_dir(tmp_root: &Path) -> PathBuf {
    tmp_root.join(".opencode-xdg")
}

// ── Model limits (WORKAROUND 2) ────────────────────────────────────────

/// A model's registry limits, from `opencode models --verbose`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelLimits {
    pub context: u64,
    pub output: Option<u64>,
}

/// Best-effort: on failure the run proceeds with the 32k cap in place.
fn load_model_limits(m: &Model) -> Option<ModelLimits> {
    // Provider-scoped listing first; some providers appear only in the global one.
    for provider_arg in [Some(m.provider.as_str()), None] {
        let mut cmd = Command::new("opencode");
        cmd.arg("models");
        if let Some(p) = provider_arg {
            cmd.arg(p);
        }
        cmd.arg("--verbose");
        let Ok(out) = cmd.output() else { continue };
        if !out.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(limits) = extract_limits(&text, &m.provider, m.metadata_id()) {
            return Some(limits);
        }
    }
    None
}

/// `opencode models --verbose` emits a stream of JSON objects (pretty-printed
/// or single-line), not one document, hence brace-depth accumulation.
fn extract_limits(output: &str, provider: &str, model_id: &str) -> Option<ModelLimits> {
    let mut buf: Vec<&str> = Vec::new();
    let mut depth: i32 = 0;

    for line in output.lines() {
        let trimmed = line.trim();
        if buf.is_empty() && !trimmed.starts_with('{') {
            continue;
        }
        buf.push(trimmed);
        depth += trimmed.matches('{').count() as i32 - trimmed.matches('}').count() as i32;
        if depth > 0 {
            continue;
        }

        let joined = buf.join("\n");
        buf.clear();
        depth = 0;

        let Ok(v) = serde_json::from_str::<serde_json::Value>(&joined) else { continue };
        let matches_model = v.get("providerID").and_then(|x| x.as_str()) == Some(provider)
            && v.get("id").and_then(|x| x.as_str()) == Some(model_id);
        if !matches_model {
            continue;
        }
        let limit = v.get("limit")?;
        let context = limit.get("context").and_then(|x| x.as_u64())?;
        return Some(ModelLimits {
            context,
            output: limit.get("output").and_then(|x| x.as_u64()),
        });
    }
    None
}

// ── Invocation ─────────────────────────────────────────────────────────

/// `tmp_root` is what [`materialize_config`] wrote `.opencode/` into, and the
/// root of the filesystem boundary. `prompt` must already include
/// [`prompt_suffix`] — callers append it so the text recorded to
/// `logs/prompt.md` is exactly what the agent received.
pub fn invoke(
    phase: Phase,
    prompt: &str,
    log_path: &Path,
    work_dir: &Path,
    tmp_root: &Path,
    m: &Model,
    timeout_secs: u64,
) -> Result<()> {
    let openssl_dir = std::env::var("OPENSSL_DIR").unwrap_or_else(|_| "/usr".into());

    // No `--pure`: it clears external plugins, disabling the compaction plugin.
    let script = format!(
        "set -o pipefail; timeout {timeout_secs} opencode run \
         --format json --thinking --dangerously-skip-permissions \
         --agent {} --model \"$MODEL\" \"$PROMPT\" \
         < /dev/null 2>&1 | tee \"$LOG\"",
        phase.agent_name(),
    );

    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(&script)
        .env("PROMPT", prompt)
        .env("LOG", log_path)
        .env("MODEL", m.as_arg())
        .env("OPENSSL_DIR", &openssl_dir)
        .env("XDG_CONFIG_HOME", xdg_config_dir(tmp_root))
        .current_dir(work_dir);

    // WORKAROUND 2 — raise the 32k per-response cap to the registry limit.
    match load_model_limits(m) {
        Some(ModelLimits { output: Some(out), .. }) => {
            println!("  opencode: raising output cap to {out} (opencode#29363 workaround)");
            cmd.env("OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX", out.to_string());
        }
        Some(_) => eprintln!(
            "  ⚠️  opencode: model {} has no registry output limit; the 32k cap stays (opencode#29363)",
            m.as_arg()
        ),
        None => eprintln!(
            "  ⚠️  opencode: could not resolve limits for {}; the 32k cap stays (opencode#29363)",
            m.as_arg()
        ),
    }

    let status = cmd
        .status()
        .with_context(|| format!("invoking opencode ({})", phase.agent_name()))?;
    crate::translate::record_agent_exit(status);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bedrock_sonnet() -> Model {
        parse_model("amazon-bedrock/us.anthropic.claude-sonnet-5").unwrap()
    }

    #[test]
    fn parse_model_accepts_provider_slash_model() {
        let m = bedrock_sonnet();
        assert_eq!(m.provider, "amazon-bedrock");
        assert_eq!(m.model_id, "us.anthropic.claude-sonnet-5");
        assert_eq!(m.as_arg(), "amazon-bedrock/us.anthropic.claude-sonnet-5");
    }

    #[test]
    fn parse_model_rejects_bare_model_name() {
        assert!(parse_model("claude-sonnet-5").is_err());
        assert!(parse_model("").is_err());
        assert!(parse_model("/model").is_err());
        assert!(parse_model("provider/").is_err());
    }

    #[test]
    fn metadata_id_strips_routing_suffix() {
        let m = parse_model("openrouter/deepseek/deepseek-v4-pro:floor").unwrap();
        assert_eq!(m.as_arg(), "openrouter/deepseek/deepseek-v4-pro:floor");
        assert_eq!(m.metadata_id(), "deepseek/deepseek-v4-pro");
    }

    #[test]
    fn results_slug_drops_region_and_vendor_prefixes() {
        // The same model in two regions must NOT produce two results dirs.
        for region in ["us.", "eu.", "global.", "au.", "jp."] {
            let m = parse_model(&format!("amazon-bedrock/{region}anthropic.claude-sonnet-5")).unwrap();
            assert_eq!(results_slug(&m), "opencode-claude-sonnet-5", "region {region}");
        }
        let gpt = parse_model("amazon-bedrock/openai.gpt-5.5").unwrap();
        assert_eq!(results_slug(&gpt), "opencode-gpt-5.5");
        let dsk = parse_model("openrouter/deepseek/deepseek-v4-pro").unwrap();
        assert_eq!(results_slug(&dsk), "opencode-deepseek-v4-pro");
    }

    #[test]
    fn project_config_denies_before_allowing_tempdir() {
        // LAST-MATCH-WINS: a deny emitted after the allow denies everything.
        let cfg = project_config(Path::new("/tmp/harvest-xyz"), &bedrock_sonnet());
        let deny = cfg.find(r#""*": "deny""#).expect("catch-all deny present");
        let allow = cfg.find("/tmp/harvest-xyz/**").expect("tempdir allow present");
        assert!(deny < allow, "deny must precede allow, got:\n{cfg}");
    }

    #[test]
    fn project_config_is_valid_json_with_bedrock_block() {
        let cfg = project_config(Path::new("/tmp/harvest-xyz"), &bedrock_sonnet());
        let v: serde_json::Value = serde_json::from_str(&cfg).expect("valid JSON");
        let opts = v.pointer("/provider/amazon-bedrock/options").expect("bedrock options");
        assert!(opts.get("region").and_then(|r| r.as_str()).is_some());
        assert_eq!(
            v.pointer("/permission/external_directory/*").and_then(|d| d.as_str()),
            Some("deny"),
        );
    }

    #[test]
    fn project_config_omits_bedrock_block_for_other_providers() {
        let m = parse_model("openrouter/deepseek/deepseek-v4-pro").unwrap();
        let cfg = project_config(Path::new("/tmp/harvest-xyz"), &m);
        let v: serde_json::Value = serde_json::from_str(&cfg).expect("valid JSON");
        assert!(v.get("provider").is_none(), "no provider block:\n{cfg}");
        assert!(v.pointer("/permission/external_directory").is_some());
    }

    #[test]
    fn agent_definition_body_is_empty() {
        // A non-empty body silently REPLACES OpenCode's default system prompt.
        let def = agent_definition(Phase::Translate);
        let body = def
            .rsplit_once("---\n")
            .map(|(_, body)| body)
            .expect("frontmatter terminator");
        assert!(body.is_empty(), "agent body must stay empty, got {body:?}");
        assert!(def.contains("mode: primary"));
        assert!(def.contains("webfetch: deny"));
    }

    #[test]
    fn only_verify_recovers_persistent_artifacts() {
        assert_eq!(
            Phase::Verify.recovery_command(),
            Some("cat SYMBOLS.md ERRORS.md CONFIGS.md"),
        );
        assert_eq!(Phase::Translate.recovery_command(), None);
    }

    #[test]
    fn materialize_config_writes_plugin_only_for_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let m = bedrock_sonnet();

        materialize_config(root, Phase::Translate, &m).unwrap();
        assert!(root.join(".opencode/opencode.json").is_file());
        assert!(root.join(".opencode/agents/harvest-translate.md").is_file());
        assert!(root.join(".opencode-xdg").is_dir(), "XDG isolation dir");
        assert!(
            !root.join(".opencode/plugin/compaction-recovery.js").exists(),
            "translate has no artifacts to recover",
        );

        materialize_config(root, Phase::Verify, &m).unwrap();
        let plugin = std::fs::read_to_string(root.join(".opencode/plugin/compaction-recovery.js"))
            .expect("verify writes the compaction plugin");
        assert!(plugin.contains("cat SYMBOLS.md ERRORS.md CONFIGS.md"));
        assert!(!plugin.contains("{RECOVERY_CMD}"), "placeholder substituted");
    }

    #[test]
    fn prompt_suffix_carries_both_workarounds() {
        let s = prompt_suffix(Path::new("/tmp/harvest-xyz"));
        assert!(s.contains("/tmp/harvest-xyz"), "boundary names the tempdir");
        assert!(s.contains("Filesystem boundary"));
        assert!(s.contains("output-token cap"));
        assert!(!s.contains("{TEMPDIR}"), "placeholder substituted");
    }

    #[test]
    fn extract_limits_reads_pretty_and_single_line_objects() {
        let pretty = r#"
{
  "providerID": "amazon-bedrock",
  "id": "us.anthropic.claude-sonnet-5",
  "limit": { "context": 200000, "output": 64000 }
}
"#;
        assert_eq!(
            extract_limits(pretty, "amazon-bedrock", "us.anthropic.claude-sonnet-5"),
            Some(ModelLimits { context: 200000, output: Some(64000) }),
        );

        let single = r#"{"providerID":"amazon-bedrock","id":"openai.gpt-5.5","limit":{"context":400000}}"#;
        assert_eq!(
            extract_limits(single, "amazon-bedrock", "openai.gpt-5.5"),
            Some(ModelLimits { context: 400000, output: None }),
        );

        assert_eq!(extract_limits(pretty, "amazon-bedrock", "openai.gpt-5.5"), None);
    }
}
