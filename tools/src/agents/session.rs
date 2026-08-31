//! How an agent CLI is invoked: one builder per backend, shared by translate and
//! verify.
//!
//! The script text is rendered from the same value [`crate::cache::Recipe`] hashes, so
//! the recipe recorded beside a cached artifact cannot describe a command that did not
//! run. It used to: the recipe named claude's 10800s / 1000-turn / bypassPermissions
//! invocation for every backend, including kiro runs that really ran `timeout 2700`
//! with no turn limit and none of [`AGENT_ENV`].
//!
//! Every session runs under `bash -c`, never `bash -lc`. A login shell re-resolves
//! PATH from the profile, while the key names what *harvest-tools itself* resolved —
//! [`crate::cache::ToolchainId`] from `rustc`, [`crate::cache::CliVersion`] from
//! `<program> --version`, both spawned without a shell. On this machine those two
//! PATHs already disagree about which `node` is found, and node is what runs claude
//! and opencode.

use crate::store::ModelId;
use anyhow::Result;
use std::path::Path;
use std::process::Command;

/// Deliberately matches kiro_plain.json (built-in tools only, no skills/plugins/
/// MCP, no extra system prompt) so the two agents are compared on equal footing.
pub const CLAUDE_PLAIN_AGENT_JSON: &str = r#"{"claude_plain":{"description":"Bare-bones agent matching kiro_plain","prompt":"You are a coding assistant. Use the available tools to complete the user's task.","tools":["Bash","Edit","Read","Write","Task"]}}"#;

/// Agent-runtime settings that materially change how a session behaves.
///
/// They belong here, not in the shell driver as bare `export`s, so
/// [`crate::cache::Recipe`] hashes them by construction: otherwise two sweeps with
/// different retry policy would share a cache entry.
pub const AGENT_ENV: &[(&str, &str)] = &[
    // A single request may legitimately run this long on a large project; the
    // per-session wall clock is bounded separately by `timeout`.
    ("API_TIMEOUT_MS", "1200000"),
    ("API_FORCE_IDLE_TIMEOUT", "0"),
    // Bedrock throttles under concurrency; without generous retries a throttle
    // becomes a dead case, which #67 then correctly refuses to score.
    ("CLAUDE_CODE_MAX_RETRIES", "20"),
    ("CLAUDE_CODE_RETRY_WATCHDOG", "1"),
];

/// Empty, measured: codex's translate set only `OPENSSL_DIR`, which [`Session::shell`] supplies. Its
/// retry/auth settings live in `~/.codex/config.toml` -- outside the key, like kiro's `kiro_plain`.
const CODEX_AGENT_ENV: &[(&str, &str)] = &[];

const CLAUDE_MAX_TURNS: u32 = 1000;
const CLAUDE_PERMISSION_MODE: &str = "bypassPermissions";

#[derive(Copy, Clone)]
pub struct Caps {
    pub fsize_blocks: u64,
    pub data_kb: u64,
}

impl Caps {
    /// The SAME caps for every backend. They used to be `Option<Caps>`, `Some` for claude and `None`
    /// for codex, kiro and opencode -- so three of four agents ran with no file-size limit at all, and
    /// kiro wrote a 9.3 GB transcript for one case. Not a `tee` inheritance problem as first supposed:
    /// `tee` is in the same pipeline and inherits fine, the limit was simply never set. A resource cap
    /// that varies by tool is the same defect as a wall-clock ceiling that varies by tool, and the
    /// same answer applies -- make it unrepresentable rather than keep four values in agreement.
    pub const UNIFORM: Self = Self {
        fsize_blocks: crate::io::workdir::AGENT_FSIZE_BLOCKS,
        data_kb: crate::io::workdir::AGENT_DATA_KB,
    };
}

/// One agentic session: the CLI, its wall-clock cap, the flags that shape how it
/// behaves, and the exact shell script that applies them.
///
/// The knobs are fields rather than literals inside the script text, and the script is
/// built from those fields, so the two cannot disagree — [`Self::assert_declares`]
/// re-checks it for the case where someone types a value back into the text.
pub struct Session {
    program: &'static str,
    timeout_secs: u64,
    max_turns: Option<u32>,
    permission_mode: Option<&'static str>,
    caps: Caps,
    /// Agent definition passed as `--agents`; empty for the CLIs that take none.
    agents_json: &'static str,
    agent_env: &'static [(&'static str, &'static str)],
    script: String,
}

impl Session {
    pub fn claude(timeout_secs: u64) -> Self {
        let s = Self {
            program: "claude",
            timeout_secs,
            max_turns: Some(CLAUDE_MAX_TURNS),
            permission_mode: Some(CLAUDE_PERMISSION_MODE),
            caps: Caps::UNIFORM,
            agents_json: CLAUDE_PLAIN_AGENT_JSON,
            agent_env: AGENT_ENV,
            script: String::new(),
        };
        let script = format!(
            "{} -p \"$1\" --strict-mcp-config --disable-slash-commands \
             --settings \"$3\" --agents \"$4\" --agent claude_plain{} \
             --model \"$5\" --verbose --output-format stream-json \
             < /dev/null 2>&1 | tee \"$2\"",
            s.prefix(),
            s.session_flags(),
        );
        Self { script, ..s }
    }

    /// Routed through `Session` for the reason every other backend is: translate built this inline
    /// with a hardcoded `timeout 10800`, so the ceiling was invisible to `Recipe::digest` and verify
    /// could not ask for the same command. `$5`/`$6` are positional so a `.`-bearing model id cannot
    /// be re-lexed; codex has no `--agents`, and `-p/--profile` ships nothing from this repo.
    pub fn codex(timeout_secs: u64) -> Self {
        let s = Self {
            program: "codex",
            timeout_secs,
            max_turns: None,
            permission_mode: None,
            caps: Caps::UNIFORM,
            agents_json: "",
            agent_env: CODEX_AGENT_ENV,
            script: String::new(),
        };
        let script = format!(
            "{} exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox \
             -C \"$3\" -c model=\"$5\" -c model_providers.amazon-bedrock.aws.region=\"$6\" \
             --json \"$1\" < /dev/null 2>&1 | tee \"$2\"",
            s.prefix(),
        );
        Self { script, ..s }
    }

    /// kiro-cli has no turn limit, no permission mode and no `--agents`: its
    /// `kiro_plain` definition lives in its own config, outside this repo, so the key
    /// cannot see it.
    pub fn kiro(timeout_secs: u64) -> Self {
        let s = Self {
            program: "kiro-cli",
            timeout_secs,
            max_turns: None,
            permission_mode: None,
            caps: Caps::UNIFORM,
            agents_json: "",
            agent_env: &[],
            script: String::new(),
        };
        // `--model` is not optional: kiro-cli's default is whatever the picker last made active, so an
        // unpinned invocation keys one model and runs another.
        let script = format!(
            "{} chat --no-interactive --trust-all-tools --agent kiro_plain --model \"$3\" \"$1\" \
             < /dev/null 2>&1 | tee \"$2\"",
            s.prefix(),
        );
        Self { script, ..s }
    }

    /// No `--pure`: it clears external plugins, disabling the compaction plugin
    /// [`crate::agents::opencode::materialize_config`] writes.
    pub fn opencode(phase: crate::agents::opencode::Phase, timeout_secs: u64) -> Self {
        let s = Self {
            program: "opencode",
            timeout_secs,
            max_turns: None,
            permission_mode: None,
            caps: Caps::UNIFORM,
            agents_json: "",
            agent_env: &[],
            script: String::new(),
        };
        let script = format!(
            "{} run --format json --thinking --dangerously-skip-permissions \
             --agent {} --model \"$1\" \"$2\" < /dev/null 2>&1 | tee \"$3\"",
            s.prefix(),
            phase.agent_name(),
        );
        Self { script, ..s }
    }

    fn prefix(&self) -> String {
        let c = self.caps;
        let ulimit = format!("ulimit -f {} -d {}; ", c.fsize_blocks, c.data_kb);
        // `set -o pipefail` on every backend: without it the pipeline's status is
        // tee's, so `timeout`'s 124 never reaches `record_agent_exit` and a killed
        // session is recorded as a clean exit.
        format!(
            "{ulimit}set -o pipefail; timeout {} {}",
            self.timeout_secs, self.program
        )
    }

    fn session_flags(&self) -> String {
        let mut f = String::new();
        if let Some(n) = self.max_turns {
            f.push_str(&format!(" --max-turns {n}"));
        }
        if let Some(m) = self.permission_mode {
            f.push_str(&format!(" --permission-mode {m}"));
        }
        f
    }

    /// Every field plus the rendered script, as one canonical line for the cache key.
    ///
    /// Hashing the script is what makes this complete: a flag nobody thought to add as
    /// a field still changes the key. Machine-independent by construction — the script
    /// carries no paths, they all arrive as positional arguments or environment.
    pub fn shape(&self) -> String {
        let mut out = format!("program={} timeout={}", self.program, self.timeout_secs);
        if let Some(n) = self.max_turns {
            out += &format!(" max_turns={n}");
        }
        if let Some(m) = self.permission_mode {
            out += &format!(" permission_mode={m}");
        }
        out += &format!(
            " fsize_blocks={} data_kb={}",
            self.caps.fsize_blocks, self.caps.data_kb
        );
        out += &format!(" agents={}", self.agents_json);
        // Sorted, so a reordering of the constant is not a different recipe.
        let mut env: Vec<_> = self.agent_env.to_vec();
        env.sort_unstable();
        for (k, v) in env {
            out += &format!(" env:{k}={v}");
        }
        out += &format!(" script={}", self.script);
        out
    }

    /// Fails if the script does not apply what the recorded fields claim.
    ///
    /// Rendering makes that impossible today; this catches a value being typed back
    /// into the script text, which is how kiro's `timeout 2700` came to sit under a
    /// recipe recording 10800.
    pub fn assert_declares(&self) -> Result<()> {
        let mut want = vec![format!("timeout {} {}", self.timeout_secs, self.program)];
        if let Some(n) = self.max_turns {
            want.push(format!("--max-turns {n}"));
        }
        if let Some(m) = self.permission_mode {
            want.push(format!("--permission-mode {m}"));
        }
        // Unconditional: `assert_declares` is what catches a script whose text drifted from the
        // fields, and while caps were optional this check simply did not run for three of four agents.
        want.push(format!(
            "ulimit -f {} -d {}",
            self.caps.fsize_blocks, self.caps.data_kb
        ));
        for w in &want {
            anyhow::ensure!(
                self.script.contains(w.as_str()),
                "the {} invocation does not apply {w:?}, so the recipe in the cache key \
                 would describe a run that did not happen:\n{}",
                self.program,
                self.script,
            );
        }
        Ok(())
    }

    fn shell(&self) -> Command {
        let mut c = Command::new("bash");
        c.arg("-c")
            .arg(&self.script)
            // `$0` for the script, so the values below start at `$1`.
            .arg("--")
            .env(
                "OPENSSL_DIR",
                std::env::var("OPENSSL_DIR").unwrap_or_else(|_| "/usr".into()),
            )
            .envs(self.agent_env.iter().copied());
        c
    }

    pub fn claude_command(&self, run: &ClaudeRun<'_>) -> Command {
        let mut c = self.shell();
        // Positional, never interpolated: the model id contains `[1m]`, which bash
        // would take for a bracket glob inside the script text. Positional rather than
        // exported also keeps the prompt out of the environment every tool the agent
        // spawns inherits.
        c.arg(run.prompt)
            .arg(run.log)
            .arg(run.settings)
            .arg(self.agents_json)
            .arg(run.model.as_str())
            .env("TMPDIR", run.agent_tmp)
            .env("CLAUDE_CODE_TMPDIR", run.agent_tmp)
            // The prompts delegate to subagents via Task; without this the pin would
            // cover only the top-level session.
            .env("CLAUDE_CODE_SUBAGENT_MODEL", run.model.as_str())
            .current_dir(run.cwd);
        c
    }

    /// `__unused__` holds `$4`'s place: claude's script uses it for the agents file, and keeping the
    /// positions aligned across backends is what lets one `shell()` serve all of them.
    pub fn codex_command(
        &self,
        cwd: &Path,
        prompt: &str,
        log: &Path,
        model: &str,
        region: &str,
    ) -> Command {
        let mut c = self.shell();
        c.arg(prompt)
            .arg(log)
            .arg(cwd)
            .arg("__unused__")
            .arg(model)
            .arg(region);
        c
    }

    pub fn kiro_command(
        &self,
        cwd: &Path,
        prompt: &str,
        log: &Path,
        model: &crate::store::ModelId,
    ) -> Command {
        let mut c = self.shell();
        c.arg(prompt).arg(log).arg(model.as_str()).current_dir(cwd);
        c
    }

    pub fn opencode_command(&self, run: OpencodeRun<'_>) -> Command {
        let mut c = self.shell();
        c.arg(run.model_arg)
            .arg(run.prompt)
            .arg(run.log)
            .env("XDG_CONFIG_HOME", run.xdg_config_home)
            .current_dir(run.cwd);
        c
    }
}

/// The per-case values an opencode session needs.
///
/// A struct rather than five positional parameters for the same reason [`ClaudeRun`]
/// is one: three of them are `&Path` with no type distinction between them, so
/// transposing `log` and `xdg_config_home` compiles and writes the agent's transcript
/// where its config belongs.
pub struct OpencodeRun<'a> {
    pub cwd: &'a Path,
    pub prompt: &'a str,
    pub log: &'a Path,
    pub model_arg: &'a str,
    /// Per-case config root, so parallel opencode agents cannot share credentials
    /// state.
    pub xdg_config_home: &'a Path,
}

/// The per-case values a claude session needs, in one struct so the positional order
/// the script reads them in is fixed here instead of at each call site.
pub struct ClaudeRun<'a> {
    pub cwd: &'a Path,
    pub prompt: &'a str,
    pub log: &'a Path,
    pub settings: &'a Path,
    /// Agent scratch, on disk inside the work root rather than the /tmp tmpfs — see
    /// [`crate::io::workdir`].
    pub agent_tmp: &'a Path,
    pub model: &'a ModelId,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<Session> {
        vec![
            Session::claude(10_800),
            Session::kiro(2_700),
            // Was absent, so every rule below silently exempted the backend it was added for.
            Session::codex(10_800),
            Session::opencode(crate::agents::opencode::Phase::Verify, 10_800),
        ]
    }

    #[test]
    fn every_backend_applies_what_its_recipe_records() {
        for s in all() {
            s.assert_declares()
                .unwrap_or_else(|e| panic!("{}: {e:#}", s.program));
        }
    }

    #[test]
    fn a_hardcoded_value_in_the_script_is_caught() {
        // The defect this exists for: the recorded timeout and the one in the command
        // drifting apart.
        let mut s = Session::claude(10_800);
        s.script = s.script.replace("timeout 10800", "timeout 2700");
        let err = format!("{:#}", s.assert_declares().expect_err("must refuse"));
        assert!(err.contains("timeout 10800 claude"), "{err}");
    }

    #[test]
    fn only_claude_limits_turns_or_bypasses_permissions() {
        // The K2 defect: kiro's recipe claimed 1000 turns and bypassPermissions.
        let claude = Session::claude(10_800);
        assert!(claude.shape().contains("max_turns=1000"));
        assert!(claude.shape().contains("permission_mode=bypassPermissions"));
        for s in [
            Session::kiro(2_700),
            Session::opencode(crate::agents::opencode::Phase::Verify, 10_800),
        ] {
            let shape = s.shape();
            assert!(!shape.contains("max_turns"), "{}: {shape}", s.program);
            assert!(!shape.contains("permission_mode"), "{}: {shape}", s.program);
            assert!(!shape.contains("agents={\""), "{}: {shape}", s.program);
        }
    }

    #[test]
    fn each_backend_has_a_shape_of_its_own() {
        let shapes: Vec<String> = all().iter().map(Session::shape).collect();
        for (i, a) in shapes.iter().enumerate() {
            for b in &shapes[i + 1..] {
                assert_ne!(a, b, "two backends must not share a recipe");
            }
        }
    }

    /// The env is a session constant now, so the two properties below moved here from
    /// `cache::Recipe`'s tests, where it used to be a settable field.
    fn shape_with_env(env: &'static [(&'static str, &'static str)]) -> String {
        Session {
            agent_env: env,
            ..Session::kiro(1)
        }
        .shape()
    }

    #[test]
    fn the_shape_covers_the_agent_runtime_env() {
        // Retry count changes how a throttled session ends, so it changes what the agent
        // produces; these once lived in a shell driver where the key could not see them.
        let twenty = shape_with_env(&[("CLAUDE_CODE_MAX_RETRIES", "20")]);
        let one = shape_with_env(&[("CLAUDE_CODE_MAX_RETRIES", "1")]);
        assert_ne!(twenty, one, "retry policy must change the key");

        let extra = shape_with_env(&[("CLAUDE_CODE_MAX_RETRIES", "20"), ("API_TIMEOUT_MS", "5")]);
        assert_ne!(twenty, extra, "adding a setting must change the key");
    }

    #[test]
    fn the_shape_is_insensitive_to_env_ordering() {
        // If reordering the constant were a different key, a cosmetic edit would
        // silently invalidate every stored entry.
        assert_eq!(
            shape_with_env(&[("A", "1"), ("B", "2")]),
            shape_with_env(&[("B", "2"), ("A", "1")]),
        );
    }

    /// A resource cap that varies by tool is the defect a per-tool wall-clock ceiling already was.
    /// `caps` was `Option<Caps>`: `Some` for claude, `None` for codex, kiro and opencode -- so three of
    /// four agents ran with no file-size limit, and kiro wrote a 9.3 GB transcript for one case of
    /// B01_synthetic before anyone looked.
    #[test]
    fn every_backend_caps_its_agent_the_same_way() {
        let sessions = [
            ("claude", Session::claude(60)),
            ("codex", Session::codex(60)),
            ("kiro", Session::kiro(60)),
            (
                "opencode",
                Session::opencode(crate::agents::opencode::Phase::Verify, 60),
            ),
        ];
        let want = format!(
            "ulimit -f {} -d {}",
            crate::io::workdir::AGENT_FSIZE_BLOCKS,
            crate::io::workdir::AGENT_DATA_KB
        );
        for (name, s) in &sessions {
            // `assert_declares` compares the script text against the fields, so it refuses exactly
            // when the cap is absent from the script -- which is what never ran for three of four.
            assert!(
                s.shape()
                    .contains(&crate::io::workdir::AGENT_FSIZE_BLOCKS.to_string()),
                "{name} declares no file-size cap in its shape, so nothing bounds what it writes"
            );
            assert!(
                s.assert_declares().is_ok(),
                "{name} script does not apply {want:?}"
            );
        }
    }

    #[test]
    fn the_shape_covers_the_caps_and_the_runtime_env() {
        // A cap or a retry policy changes what the agent can produce, so changing the
        // constant must change the key.
        let shape = Session::claude(10_800).shape();
        assert!(
            shape.contains(&crate::io::workdir::AGENT_FSIZE_BLOCKS.to_string()),
            "{shape}"
        );
        assert!(
            shape.contains(&crate::io::workdir::AGENT_DATA_KB.to_string()),
            "{shape}"
        );
        for (k, v) in AGENT_ENV {
            assert!(shape.contains(&format!("env:{k}={v}")), "{shape}");
        }
    }

    /// A shell `export` of one of these is invisible to [`crate::cache::Recipe`], so a
    /// driver that kept its own copy could change agent behaviour without changing the
    /// cache key — and `.envs()` means the copy never took effect in the first place.
    #[test]
    fn no_shell_script_names_an_agent_env_key() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("tools/ has a parent");
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["ls-files", "-z", "--", "*.sh"])
            .output()
            .expect("git ls-files");
        assert!(out.status.success(), "git ls-files: {}", out.status);
        let listing = String::from_utf8(out.stdout).expect("paths are utf-8");
        let scripts: Vec<&str> = listing.split('\0').filter(|s| !s.is_empty()).collect();
        assert!(
            !scripts.is_empty(),
            "no committed *.sh found, so this rule would pass vacuously"
        );
        for rel in scripts {
            let text = std::fs::read_to_string(root.join(rel)).expect(rel);
            for (key, value) in AGENT_ENV {
                assert!(
                    !text.contains(key),
                    "{rel} names {key}; AGENT_ENV owns it (={value}) and applies it via \
                     .envs(), so a copy in the driver is at best dead and at worst a \
                     divergent value the cache key cannot see"
                );
            }
        }
    }

    #[test]
    fn the_timeout_reaches_the_script_rather_than_a_literal() {
        assert!(Session::claude(42).script.contains("timeout 42 claude"));
        assert!(Session::kiro(43).script.contains("timeout 43 kiro-cli"));
        assert!(
            Session::opencode(crate::agents::opencode::Phase::Translate, 44)
                .script
                .contains("timeout 44 opencode")
        );
    }

    #[test]
    fn no_session_leaks_a_path_into_its_script() {
        // A machine-specific path in the script would be a nonce in the key, so no
        // entry would ever hit and caching would look enabled while never working.
        for s in all() {
            let rest = s.script.replace("< /dev/null", "");
            assert!(!rest.contains('/'), "{}: {rest}", s.program);
        }
    }

    #[test]
    fn no_session_invokes_its_cli_without_pinning_the_model() {
        // kiro's script carried no model flag while its key recorded one, so every published kiro row
        // came from whichever model the picker had last made active. A missing pin is invisible after
        // the run, so it is asserted on the command line rather than on the transcript.
        for s in all() {
            assert!(
                s.script.contains("--model \"$") || s.script.contains("-c model=\"$"),
                "{} runs unpinned: {}",
                s.program,
                s.script
            );
        }
    }

    #[test]
    fn every_pipeline_sets_pipefail_so_a_timeout_is_visible() {
        // Without it the status is tee's 0 and a killed session records a clean exit.
        for s in all() {
            assert!(
                s.script.contains("set -o pipefail"),
                "{}: {}",
                s.program,
                s.script
            );
        }
    }
}
