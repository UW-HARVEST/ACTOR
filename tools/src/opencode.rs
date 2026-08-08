//! OpenCode agent backend.
//!
//! OpenCode (<https://opencode.ai>) is a headless coding agent CLI. It is the
//! backend that lets ACTOR run its existing prompts against ANY model —
//! notably Amazon Bedrock — rather than only the model its vendor CLI is tied
//! to. The model is chosen with `--model <provider>/<model-id>`, e.g.
//! `--model amazon-bedrock/us.anthropic.claude-sonnet-5`.
//!
//! Everything OpenCode-specific lives here so the shared translate/verify code
//! only calls [`invoke`], exactly as it calls the one-line `claude`/`kiro-cli`
//! bash invocations. The prompts are UNCHANGED — OpenCode reads the same
//! `prompts/claude/*.md` files Claude Code does; the only prompt difference is
//! the appended [`prompt_suffix`] block, which is empty for every other agent.
//!
//! Ported from Haoran Peng's harvest-agentic fork
//! (<https://github.com/UW-HARVEST/harvest-agentic>, `agent_runner/src/lib.rs`).
//! The four upstream-bug workarounds below were each debugged there against
//! real runs; the comments explaining WHY are carried across deliberately,
//! because every one of them guards a FAILURE THAT IS SILENT — a hang, a
//! truncated turn, or a dropped system prompt. None of them announce
//! themselves in a log.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Default region when `AWS_REGION` is unset. Matches the region the existing
/// Bedrock-backed agents use (see `translate.rs` codex/c2saferrust arms).
const DEFAULT_BEDROCK_REGION: &str = "us-west-2";

/// The Bedrock provider id, as OpenCode's registry names it.
const BEDROCK_PROVIDER: &str = "amazon-bedrock";

/// Tool policy for the run-private OpenCode agent. Everything the prompts need
/// is allowed; network egress (`webfetch`/`websearch`) and `skill` are denied
/// so a run cannot reach outside the benchmark or pull in ambient instructions.
/// Order is preserved in the emitted frontmatter.
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

/// Post-compaction recovery plugin, auto-loaded by OpenCode from
/// `.opencode/plugin/`. See the file's own header for the mechanism.
const COMPACTION_PLUGIN: &str = include_str!("opencode_compaction_recovery.js");

/// WORKAROUND 1 (of 4) — sub-agent permission deadlock.
///
/// `external_directory` defaults to `"ask"`, and OpenCode's task sub-agents do
/// NOT inherit `--dangerously-skip-permissions`. In a headless run any
/// sub-agent tool call touching a path outside the project directory therefore
/// blocks FOREVER on a permission prompt nobody can answer, freezing the whole
/// session until the harness timeout kills it (hours later, with no error).
/// [`project_config`] makes "ask" unreachable by scoping external access to
/// this run's temp dir; this prompt block tells the agent about that boundary
/// so a denial is a fast, correctable error instead of a mystery.
///
/// ACTOR's prompts do use sub-agents (`prompts/claude/translate-library.md`,
/// `prompts/claude/verify.md`), so both halves are load-bearing.
const WORKDIR_BOUNDARY_TEMPLATE: &str = r#"### Filesystem boundary
- `{TEMPDIR}` is the ONLY directory you may read or write.
- Keep all scratch files inside the project directory.
- Prefer relative paths. Never retype the absolute temp-directory prefix
  by hand: a single typo in it makes the path "external" and the call is
  denied.
- When you dispatch sub-agents, copy this entire "Filesystem boundary"
  section into every sub-agent prompt verbatim."#;

/// WORKAROUND 2 (of 4) — the 32k output cap (opencode#29363).
///
/// OpenCode caps each model response at `min(limit.output, 32000)` output
/// tokens, and thinking counts against the same cap. A turn that burns the cap
/// on thinking ends with no tool call, OpenCode treats it as complete, and a
/// sub-agent that ends this way returns an empty result having written nothing.
/// [`invoke`] raises the cap via `OPENCODE_EXPERIMENTAL_OUTPUT_TOKEN_MAX`, but
/// a hard cap remains, so the prompt must also steer away from long thinking
/// and monolithic writes. Remove once the upstream cap respects `limit.output`.
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

/// A `--model` value for the opencode backend, e.g.
/// `amazon-bedrock/us.anthropic.claude-sonnet-5`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// OpenCode provider id (`amazon-bedrock`, `openrouter`, …).
    pub provider: String,
    /// Provider-specific model id, routing suffix included.
    pub model_id: String,
}

impl Model {
    /// The full `provider/model` string to hand back to `opencode --model`.
    pub fn as_arg(&self) -> String {
        format!("{}/{}", self.provider, self.model_id)
    }

    /// The model id with any `:suffix` routing hint stripped. The suffix must
    /// be passed to OpenCode verbatim but is absent from registry metadata, so
    /// limit lookups must match on the bare id.
    fn metadata_id(&self) -> &str {
        self.model_id.split_once(':').map_or(&self.model_id, |(id, _)| id)
    }

    fn is_bedrock(&self) -> bool {
        self.provider == BEDROCK_PROVIDER
    }
}

/// Parse `provider/model`. Rejects a bare model name, because OpenCode cannot
/// resolve one and would fail deep inside the run instead of at startup.
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

/// Results-directory slug for a model, e.g.
/// `amazon-bedrock/us.anthropic.claude-sonnet-5` → `opencode-claude-sonnet-5`.
///
/// Bedrock ids carry a regional prefix (`us.`/`eu.`/`global.`/`au.`/`jp.`) that
/// is a routing detail, not a model identity — `us.anthropic.claude-sonnet-5`
/// and `eu.anthropic.claude-sonnet-5` are the same model, so they must not
/// produce two different results dirs. The vendor prefix (`anthropic.`,
/// `openai.`, …) is dropped for the same reason `Agent::Oneshot` keeps only the
/// last path segment: the agent name already says which harness ran.
pub fn results_slug(m: &Model) -> String {
    // For slash-separated ids (openrouter/deepseek/deepseek-v4-pro) keep the
    // last segment, matching Agent::Oneshot's existing convention.
    let id = m.model_id.rsplit('/').next().unwrap_or(&m.model_id);
    let id = id.split_once(':').map_or(id, |(head, _)| head);
    let bare = id
        .strip_prefix("us.")
        .or_else(|| id.strip_prefix("eu."))
        .or_else(|| id.strip_prefix("global."))
        .or_else(|| id.strip_prefix("au."))
        .or_else(|| id.strip_prefix("jp."))
        .unwrap_or(id);
    // Drop a single vendor prefix (`anthropic.`, `openai.`, `amazon.`, …).
    let bare = bare.split_once('.').map_or(bare, |(_, rest)| rest);
    format!("opencode-{bare}")
}

// ── Phase ──────────────────────────────────────────────────────────────

/// Which pipeline phase is running. Selects the OpenCode agent definition and
/// the post-compaction recovery command.
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

    /// Command the agent must run first after a context compaction, to restore
    /// the persistent artifacts its prompt depends on. `None` → no compaction
    /// plugin is written for this phase.
    ///
    /// ACTOR's `prompts/claude/verify.md` mandates three on-disk artifacts and
    /// GATES its later phases on them: `SYMBOLS.md` (every public C symbol),
    /// `ERRORS.md` (the error-surface table, gating Phase C) and `CONFIGS.md`
    /// (the configuration-surface table, gating Phase B). Losing them to a
    /// compaction silently un-gates those phases — the agent keeps working and
    /// reports success against a table it can no longer see.
    ///
    /// The translate prompts keep no such artifacts (verified: no persistent
    /// `*.md` referenced in `prompts/claude/translate-*.md`), so there is
    /// nothing to recover and no plugin is written.
    fn recovery_command(self) -> Option<&'static str> {
        match self {
            Phase::Translate => None,
            Phase::Verify => Some("cat SYMBOLS.md ERRORS.md CONFIGS.md"),
        }
    }
}

// ── Prompt additions ───────────────────────────────────────────────────

/// The OpenCode-only prompt suffix: the filesystem-boundary contract plus the
/// output-cap warning. Every other backend appends nothing, so no other
/// agent's prompt changes — which keeps the published-methodology prompts
/// byte-identical across backends.
pub fn prompt_suffix(tmp_root: &Path) -> String {
    format!(
        "\n\n---\n\n{}\n\n{}\n",
        WORKDIR_BOUNDARY_TEMPLATE.replace("{TEMPDIR}", &tmp_root.display().to_string()),
        OUTPUT_CAP_WARNING,
    )
}

// ── Run-private configuration ──────────────────────────────────────────

/// Project-level `opencode.json` for one run.
///
/// Two jobs. First, the Bedrock provider config: `region` and (when set)
/// `profile`, resolved through the standard AWS credential chain. OpenCode
/// documents config-file options as taking precedence over env vars, so this
/// is authoritative for the run. `profile` is OMITTED when `AWS_PROFILE` is
/// unset — writing an empty or guessed profile name would break the default
/// chain that otherwise works.
///
/// Second, WORKAROUND 1's permission policy. OpenCode resolves permission
/// rules LAST-MATCH-WINS, so the catch-all deny must be emitted BEFORE the
/// tempdir allow; reversing them denies everything and every tool call fails.
/// The JSON is therefore built as ordered text, never from a `HashMap`.
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

/// JSON-encode a string (quotes + escapes) without pulling in a map type that
/// would reorder keys.
fn json_str(s: &str) -> String {
    serde_json::to_string(s).expect("string serializes to JSON")
}

/// The run-private OpenCode agent definition.
///
/// The body after the frontmatter MUST stay EMPTY. OpenCode assembles requests
/// as `agent.prompt ? [agent.prompt] : SystemPrompt.provider(model)`, and the
/// `.md` body becomes `agent.prompt` — so any body content silently REPLACES
/// OpenCode's default coding system prompt (~2k tokens of tool-use guidance)
/// instead of adding to it. Haoran's fork shipped exactly that bug by putting
/// the compaction hint here. Do not "helpfully" fill this in; the recovery
/// instruction belongs in the compaction plugin, which is where it now lives.
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

/// Write `<tmp_root>/.opencode/{opencode.json,agents/<name>.md,plugin/…}`.
///
/// Called from the same place the Claude backend writes its sandboxed
/// `.claude/settings.json`; this is the OpenCode analogue.
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

    // WORKAROUND 4 — post-compaction recovery. Only for phases whose prompt
    // keeps persistent artifacts (see Phase::recovery_command).
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

    // WORKAROUND 3 — XDG isolation target. Created empty here so the run
    // cannot fall back to the developer's real config dir.
    std::fs::create_dir_all(xdg_config_dir(tmp_root)).context("creating run-private XDG dir")?;

    Ok(())
}

/// WORKAROUND 3 (of 4) — global-config isolation.
///
/// OpenCode resolves its global config via xdg-basedir
/// (`XDG_CONFIG_HOME/opencode`), so pointing that at a run-private empty dir
/// makes the developer's global `opencode.json`, plugins, and `AGENTS.md`
/// unreachable — ambient instructions cannot leak in and silently change a
/// benchmark result. Auth (XDG *data* dir) and the models cache (XDG *cache*
/// dir) are unaffected, so Bedrock credentials still resolve.
///
/// This is stronger than OpenCode's own `--pure`, whose only effect is clearing
/// the external-plugin list — which would also disable our compaction plugin.
/// That is why [`invoke`] does not pass `--pure`.
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

/// Look up the model's real output limit so [`invoke`] can raise OpenCode's
/// 32k cap. Best-effort: any failure returns `None` and the run proceeds with
/// the cap in place (degraded, not broken).
fn load_model_limits(m: &Model) -> Option<ModelLimits> {
    // Try the provider-scoped listing first (smaller, faster), then the global
    // one — some providers only appear in the global listing.
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

/// Scan `opencode models --verbose` output for the matching model's limits.
/// The output is a stream of JSON objects (pretty-printed or single-line), not
/// one JSON document, so objects are accumulated by brace depth and parsed
/// individually.
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

/// Run OpenCode headlessly for one phase.
///
/// `tmp_root` is the run's private temp dir (the parent of the crate dir), the
/// same directory [`materialize_config`] wrote `.opencode/` into and the root
/// of the filesystem boundary. `prompt` must already include
/// [`prompt_suffix`] — callers append it when building the prompt so the exact
/// text handed to the agent is what gets recorded to `logs/prompt.md`.
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

    // No `--pure`: it would clear external plugins, disabling the compaction
    // plugin. XDG isolation below is strictly stronger. See xdg_config_dir.
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

    // WORKAROUND 2 — raise the 32k per-response output cap to the model's real
    // registry limit. Best-effort: a failed probe leaves the cap in place.
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
        // A bare name would fail deep inside the agent run instead of at startup.
        assert!(parse_model("claude-sonnet-5").is_err());
        assert!(parse_model("").is_err());
        assert!(parse_model("/model").is_err());
        assert!(parse_model("provider/").is_err());
    }

    #[test]
    fn metadata_id_strips_routing_suffix() {
        let m = parse_model("openrouter/deepseek/deepseek-v4-pro:floor").unwrap();
        // The suffix goes to OpenCode verbatim...
        assert_eq!(m.as_arg(), "openrouter/deepseek/deepseek-v4-pro:floor");
        // ...but registry metadata is keyed on the bare id.
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
        // Slash-separated ids keep the last segment (Agent::Oneshot convention).
        let dsk = parse_model("openrouter/deepseek/deepseek-v4-pro").unwrap();
        assert_eq!(results_slug(&dsk), "opencode-deepseek-v4-pro");
    }

    #[test]
    fn project_config_denies_before_allowing_tempdir() {
        // OpenCode resolves permissions LAST-MATCH-WINS. If the catch-all deny
        // came after the tempdir allow it would deny everything, and every
        // tool call in the run would fail. Assert on byte order.
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
        // A non-Bedrock provider must not get a Bedrock config it can't use.
        let m = parse_model("openrouter/deepseek/deepseek-v4-pro").unwrap();
        let cfg = project_config(Path::new("/tmp/harvest-xyz"), &m);
        let v: serde_json::Value = serde_json::from_str(&cfg).expect("valid JSON");
        assert!(v.get("provider").is_none(), "no provider block:\n{cfg}");
        // The permission policy still applies to every provider.
        assert!(v.pointer("/permission/external_directory").is_some());
    }

    #[test]
    fn agent_definition_body_is_empty() {
        // A non-empty body becomes `agent.prompt` and silently REPLACES
        // OpenCode's default coding system prompt. This test is the guard.
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
        // verify.md gates Phases B/C on SYMBOLS/ERRORS/CONFIGS.md; the
        // translate prompts keep no persistent artifacts, so no plugin.
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

        // A non-matching model must not be mistaken for the requested one.
        assert_eq!(extract_limits(pretty, "amazon-bedrock", "openai.gpt-5.5"), None);
    }
}
