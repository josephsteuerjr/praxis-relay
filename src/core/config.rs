use std::path::{Path, PathBuf};

pub const RELAY_AUTH_DIR_ENV_VAR: &str = "RELAY_AUTH_DIR";
pub const RELAY_INSTRUCTIONS_FILE_ENV_VAR: &str = "RELAY_INSTRUCTIONS_FILE";
pub const DEFAULT_INSTRUCTIONS_FILE: &str = "instructions.txt";

/// Where the operator's own instructions text lives.
///
/// `RELAY_INSTRUCTIONS_FILE` wins when set; otherwise `instructions.txt` sits next
/// to the executable, the same place `local_auth/` already occupies, so a relay
/// stays one directory rather than a binary plus scattered state.  The bare
/// filename is the last resort so `cargo run` still behaves.
pub fn instructions_file_path() -> PathBuf {
    if let Some(value) = std::env::var_os(RELAY_INSTRUCTIONS_FILE_ENV_VAR) {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(DEFAULT_INSTRUCTIONS_FILE)))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_INSTRUCTIONS_FILE))
}

/// The operator's instructions, if they wrote any.
///
/// Read per request rather than once at startup.  Editing the text is the whole
/// point of the file, and an edit that needed a restart to land would be made far
/// less often; one small local read beside an HTTPS round trip costs nothing
/// measurable.
///
/// A missing, unreadable, or blank file is not an error.  It means "no override",
/// and the built-in stub answers instead — so a typo in a path degrades to today's
/// behavior rather than to an empty instructions field upstream.
pub fn custom_instructions() -> Option<String> {
    read_instructions(&instructions_file_path())
}

fn read_instructions(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

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

        // RELAY_DEFAULT_MODEL: what the "local-model" alias resolves to (clients
        // built against llama-cpp-python hardcode that slug).  Kept at the safest
        // slug by default; an unsupported override fails loudly at request time
        // with the 404 that names the real list.
        let model = std::env::var("RELAY_DEFAULT_MODEL")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "gpt-5.4".to_string());

        Ok(Config {
            codex_home,
            chatgpt_base_url: "https://chatgpt.com/backend-api/codex".to_string(),
            model,
            user_instructions,
            reasoning_effort,
            instructions_mode,
            parallel_tool_calls,
        })
    }

    /// RELAY_INSTRUCTIONS: "minimal" (default — a ~60-word neutral stub; the client's
    /// real system prompt travels inside the input as a <system> message anyway) or
    /// "codex" — the full vendored prompt.md, ~5k tokens of coding-agent instructions
    /// burned on EVERY call and steering the model toward Codex-CLI behavior.  When
    /// minimal is rejected upstream (4xx) the request is retried once with the full
    /// prompt, so the worst case equals the codex mode.
    pub fn minimal_instructions(&self) -> bool {
        self.instructions_mode.as_deref() == Some("minimal")
    }

    fn load_instructions_mode() -> Option<String> {
        match std::env::var("RELAY_INSTRUCTIONS") {
            Ok(value) => Some(value.trim().to_lowercase()),
            // Unset defaults to minimal: agents bring their own system prompt,
            // and the Codex coding-agent preamble only costs quota and skews
            // behavior.  RELAY_INSTRUCTIONS=codex restores the old default.
            Err(_) => Some("minimal".to_string()),
        }
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
    fn a_written_file_replaces_the_built_in_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instructions.txt");
        std::fs::write(&path, "  I am someone else entirely.\n\n").unwrap();
        assert_eq!(
            read_instructions(&path).as_deref(),
            Some("I am someone else entirely."),
            "surrounding whitespace would shift the cached prefix for no reason"
        );
    }

    #[test]
    fn a_blank_or_missing_file_means_no_override() {
        let dir = tempfile::tempdir().unwrap();
        let blank = dir.path().join("instructions.txt");
        std::fs::write(&blank, "   \n\t\n").unwrap();
        assert_eq!(read_instructions(&blank), None,
                   "a blank file must fall back, not send empty instructions upstream");
        assert_eq!(read_instructions(&dir.path().join("absent.txt")), None,
                   "a wrong path must degrade to the built-in stub, not fail the request");
    }

    #[test]
    fn default_auth_dir_is_next_to_executable() {
        let executable = PathBuf::from("bundle").join("praxis-relay.exe");
        assert_eq!(
            default_relay_auth_dir(&executable).unwrap(),
            PathBuf::from("bundle").join("local_auth")
        );
    }
}
