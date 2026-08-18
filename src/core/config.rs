use std::path::{Path, PathBuf};

pub const RELAY_AUTH_DIR_ENV_VAR: &str = "RELAY_AUTH_DIR";

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub codex_home: PathBuf,
    pub chatgpt_base_url: String,
    pub model: String,
    pub user_instructions: Option<String>,
    pub reasoning_effort: Option<String>,
    pub instructions_mode: Option<String>,
    pub parallel_tool_calls: bool,
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        // Relay credentials deliberately live outside ~/.codex and ~/.opencode.
        // The default sits next to the executable, matching the server layout
        // where /opt/relay/auth is mounted at /app/local_auth.
        let codex_home = find_relay_auth_dir()?;

        // Load user instructions from AGENTS.md
        let user_instructions = Self::load_instructions(Some(&codex_home));
        let reasoning_effort = Self::load_reasoning_effort();
        let instructions_mode = Self::load_instructions_mode();
        let parallel_tool_calls = Self::load_parallel_tool_calls();

        Ok(Config {
            codex_home,
            chatgpt_base_url: "https://chatgpt.com/backend-api/codex".to_string(),
            model: "gpt-5.4".to_string(), // Default, but can be changed to any gpt-5* variant
            user_instructions,
            reasoning_effort,
            instructions_mode,
            parallel_tool_calls,
        })
    }

    /// RELAY_INSTRUCTIONS: "codex" (default — full vendored prompt.md, known-accepted
    /// by the backend) or "minimal" — a ~60-word neutral stub instead of ~5k tokens of
    /// coding-agent instructions per call.  When minimal is rejected upstream (4xx) the
    /// request is retried once with the full prompt, so the worst case is the default.
    pub fn minimal_instructions(&self) -> bool {
        self.instructions_mode.as_deref() == Some("minimal")
    }

    fn load_instructions_mode() -> Option<String> {
        std::env::var("RELAY_INSTRUCTIONS").ok().map(|v| v.trim().to_lowercase())
    }

    /// RELAY_PARALLEL_TOOL_CALLS: unset/"true" lets the model batch several tool calls
    /// in one response (the only client executes batched tool_use natively); "false"
    /// restores the old one-tool-per-round-trip behavior.
    fn load_parallel_tool_calls() -> bool {
        !matches!(
            std::env::var("RELAY_PARALLEL_TOOL_CALLS").ok().as_deref().map(str::trim),
            Some("false") | Some("0") | Some("off")
        )
    }

    /// RELAY_REASONING_EFFORT: none|minimal|low|medium|high|xhigh, applied when a
    /// request carries no reasoning_effort of its own.  "passthrough"/"default"/""
    /// omit the field entirely (upstream default, currently medium).  Env unset
    /// keeps today's behavior: reasoning disabled ("none") on every call.
    fn load_reasoning_effort() -> Option<String> {
        match std::env::var("RELAY_REASONING_EFFORT") {
            Ok(value) => {
                let value = value.trim().to_lowercase();
                if value.is_empty() || value == "passthrough" || value == "default" {
                    None
                } else {
                    Some(value)
                }
            }
            Err(_) => Some("none".to_string()),
        }
    }

    fn load_instructions(codex_dir: Option<&std::path::Path>) -> Option<String> {
        let mut p = match codex_dir {
            Some(p) => p.to_path_buf(),
            None => return None,
        };

        p.push("AGENTS.md");
        std::fs::read_to_string(&p).ok().and_then(|s| {
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        })
    }
}

fn find_relay_auth_dir() -> anyhow::Result<PathBuf> {
    if let Some(value) = std::env::var_os(RELAY_AUTH_DIR_ENV_VAR) {
        if value.is_empty() {
            anyhow::bail!("{RELAY_AUTH_DIR_ENV_VAR} must not be empty");
        }

        let path = PathBuf::from(value);
        return if path.is_absolute() {
            Ok(path)
        } else {
            Ok(std::env::current_dir()?.join(path))
        };
    }

    default_relay_auth_dir(&std::env::current_exe()?)
}

fn default_relay_auth_dir(executable: &Path) -> anyhow::Result<PathBuf> {
    let executable_dir = executable
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Could not determine relay executable directory"))?;
    Ok(executable_dir.join("local_auth"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_auth_dir_is_next_to_executable() {
        let executable = PathBuf::from("bundle").join("praxis-relay.exe");
        assert_eq!(
            default_relay_auth_dir(&executable).unwrap(),
            PathBuf::from("bundle").join("local_auth")
        );
    }
}
