//! How an agent CLI is invoked: one builder per backend, shared by translate and
//! verify.
//!
//! The script text is rendered from the same value [`crate::cache::Recipe`] hashes, so
//! the recipe recorded beside a cached artifact cannot describe a command that did not
//! run. It used to: the recipe named claude's 10800s / 1000-turn / bypassPermissions
//! invocation for every backend, including kiro runs that really ran `timeout 2700`
//! with no turn limit and none of [`crate::translate::AGENT_ENV`].
//!
//! Every session runs under `bash -c`, never `bash -lc`. A login shell re-resolves
//! PATH from the profile, while the key names what *harvest-tools itself* resolved —
//! [`crate::cache::ToolchainId`] from `rustc`, [`crate::cache::CliVersion`] from
//! `<program> --version`, both spawned without a shell. On this machine those two
//! PATHs already disagree about which `node` is found, and node is what runs claude
//! and opencode.

use crate::cache::ModelId;
use anyhow::Result;
use std::path::Path;
use std::process::Command;

const CLAUDE_MAX_TURNS: u32 = 1000;
const CLAUDE_PERMISSION_MODE: &str = "bypassPermissions";

#[derive(Copy, Clone)]
pub struct Caps {
    pub fsize_blocks: u64,
    pub data_kb: u64,
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
    caps: Option<Caps>,
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
            caps: Some(Caps {
                fsize_blocks: crate::workdir::AGENT_FSIZE_BLOCKS,
                data_kb: crate::workdir::AGENT_DATA_KB,
            }),
            agents_json: crate::translate::CLAUDE_PLAIN_AGENT_JSON,
            agent_env: crate::translate::AGENT_ENV,
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

    /// kiro-cli has no turn limit, no permission mode and no `--agents`: its
    /// `kiro_plain` definition lives in its own config, outside this repo, so the key
    /// cannot see it.
    pub fn kiro(timeout_secs: u64) -> Self {
        let s = Self {
            program: "kiro-cli",
            timeout_secs,
            max_turns: None,
            permission_mode: None,
            caps: None,
            agents_json: "",
            agent_env: &[],
            script: String::new(),
        };
        let script = format!(
            "{} chat --no-interactive --trust-all-tools --agent kiro_plain \"$1\" \
             < /dev/null 2>&1 | tee \"$2\"",
            s.prefix(),
        );
        Self { script, ..s }
    }

    /// No `--pure`: it clears external plugins, disabling the compaction plugin
    /// [`crate::opencode::materialize_config`] writes.
    pub fn opencode(phase: crate::opencode::Phase, timeout_secs: u64) -> Self {
        let s = Self {
            program: "opencode",
            timeout_secs,
            max_turns: None,
            permission_mode: None,
            caps: None,
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
        let ulimit = match self.caps {
            Some(c) => format!("ulimit -f {} -d {}; ", c.fsize_blocks, c.data_kb),
            None => String::new(),
        };
        // `set -o pipefail` on every backend: without it the pipeline's status is
        // tee's, so `timeout`'s 124 never reaches `record_agent_exit` and a killed
        // session is recorded as a clean exit.
        format!("{ulimit}set -o pipefail; timeout {} {}", self.timeout_secs, self.program)
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
        if let Some(c) = self.caps {
            out += &format!(" fsize_blocks={} data_kb={}", c.fsize_blocks, c.data_kb);
        }
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
        if let Some(c) = self.caps {
            want.push(format!("ulimit -f {} -d {}", c.fsize_blocks, c.data_kb));
        }
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
            .env("OPENSSL_DIR", std::env::var("OPENSSL_DIR").unwrap_or_else(|_| "/usr".into()))
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

    pub fn kiro_command(&self, cwd: &Path, prompt: &str, log: &Path) -> Command {
        let mut c = self.shell();
        c.arg(prompt).arg(log).current_dir(cwd);
        c
    }

    pub fn opencode_command(
        &self,
        cwd: &Path,
        prompt: &str,
        log: &Path,
        model_arg: &str,
        xdg_config_home: &Path,
    ) -> Command {
        let mut c = self.shell();
        c.arg(model_arg)
            .arg(prompt)
            .arg(log)
            .env("XDG_CONFIG_HOME", xdg_config_home)
            .current_dir(cwd);
        c
    }
}

/// The per-case values a claude session needs, in one struct so the positional order
/// the script reads them in is fixed here instead of at each call site.
pub struct ClaudeRun<'a> {
    pub cwd: &'a Path,
    pub prompt: &'a str,
    pub log: &'a Path,
    pub settings: &'a Path,
    /// Agent scratch, on disk inside the work root rather than the /tmp tmpfs — see
    /// [`crate::workdir`].
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
            Session::opencode(crate::opencode::Phase::Verify, 10_800),
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
        for s in [Session::kiro(2_700), Session::opencode(crate::opencode::Phase::Verify, 10_800)] {
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
        Session { agent_env: env, ..Session::kiro(1) }.shape()
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

    #[test]
    fn the_shape_covers_the_caps_and_the_runtime_env() {
        // A cap or a retry policy changes what the agent can produce, so changing the
        // constant must change the key.
        let shape = Session::claude(10_800).shape();
        assert!(shape.contains(&crate::workdir::AGENT_FSIZE_BLOCKS.to_string()), "{shape}");
        assert!(shape.contains(&crate::workdir::AGENT_DATA_KB.to_string()), "{shape}");
        for (k, v) in crate::translate::AGENT_ENV {
            assert!(shape.contains(&format!("env:{k}={v}")), "{shape}");
        }
    }

    #[test]
    fn the_timeout_reaches_the_script_rather_than_a_literal() {
        assert!(Session::claude(42).script.contains("timeout 42 claude"));
        assert!(Session::kiro(43).script.contains("timeout 43 kiro-cli"));
        assert!(Session::opencode(crate::opencode::Phase::Translate, 44)
            .script
            .contains("timeout 44 opencode"));
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
    fn every_pipeline_sets_pipefail_so_a_timeout_is_visible() {
        // Without it the status is tee's 0 and a killed session records a clean exit.
        for s in all() {
            assert!(s.script.contains("set -o pipefail"), "{}: {}", s.program, s.script);
        }
    }
}
