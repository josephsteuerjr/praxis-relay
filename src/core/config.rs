use std::path::{Path, PathBuf};

pub const RELAY_INSTRUCTIONS_FILE_ENV_VAR: &str = "RELAY_INSTRUCTIONS_FILE";
pub const DEFAULT_INSTRUCTIONS_FILE: &str = "instructions.txt";

/// Where the operator's own instructions text lives.
///
/// `RELAY_INSTRUCTIONS_FILE` wins when set; otherwise `instructions.txt` sits next
/// to the executable, beside the auth directory the relay already keeps there.
pub fn instructions_file_path() -> PathBuf {
    if let Some(value) = std::env::var_os(RELAY_INSTRUCTIONS_FILE_ENV_VAR) {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return path;
        }
    }
    #[cfg(unix)]
    if let Ok(root) = auth_root() {
        return root.join(DEFAULT_INSTRUCTIONS_FILE);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(DEFAULT_INSTRUCTIONS_FILE)))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_INSTRUCTIONS_FILE))
}

/// The operator's instructions, if they wrote any.
///
/// Read per request rather than at startup, so an edit lands on the next call with
/// no restart.  Missing, unreadable, or blank is not an error: it means "no
/// override", and the built-in text answers instead -- a wrong path degrades to
/// today's behavior rather than to an empty instructions field upstream.
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
        let codex_home = auth_root()?;
        let user_instructions = Self::load_instructions(Some(&codex_home));
        let reasoning_effort = Self::load_reasoning_effort();
        let instructions_mode = Self::load_instructions_mode();
        let parallel_tool_calls = Self::load_parallel_tool_calls();

        Ok(Config {
            codex_home,
            chatgpt_base_url: "https://chatgpt.com/backend-api/codex".to_string(),
            model: std::env::var("RELAY_DEFAULT_MODEL").unwrap_or_default(),
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
        std::env::var("RELAY_INSTRUCTIONS")
            .ok()
            .map(|v| v.trim().to_lowercase())
    }

    /// RELAY_PARALLEL_TOOL_CALLS: unset/"true" lets the model batch several tool calls
    /// in one response (the only client executes batched tool_use natively); "false"
    /// restores the old one-tool-per-round-trip behavior.
    fn load_parallel_tool_calls() -> bool {
        !matches!(
            std::env::var("RELAY_PARALLEL_TOOL_CALLS")
                .ok()
                .as_deref()
                .map(str::trim),
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
        let mut p = codex_dir?.to_path_buf();

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

/// Installed Unix applications use per-user storage. Explicit embedding roots
/// and pre-existing portable installations retain their existing contract.
pub fn auth_root() -> std::io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("RELAY_AUTH_DIR").filter(|p| !p.is_empty()) {
        return std::path::absolute(path);
    }
    if crate::login::lib::relay_local() {
        return Ok(std::env::current_dir()?.join("local_auth"));
    }
    default_auth_root(
        &std::env::current_exe()?,
        dirs::home_dir().as_deref(),
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .as_deref(),
    )
}

fn default_auth_root(
    exe: &Path,
    home: Option<&Path>,
    xdg: Option<&Path>,
) -> std::io::Result<PathBuf> {
    let legacy = exe
        .parent()
        .ok_or_else(|| std::io::Error::other("executable has no directory"))?
        .join("local_auth");
    if cfg!(windows) || legacy.is_dir() {
        return Ok(legacy);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = xdg;
        home.map(|p| p.join("Library/Application Support/praxis-relay"))
            .ok_or_else(|| std::io::Error::other("Home directory unavailable; set RELAY_AUTH_DIR"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(xdg) = xdg.filter(|p| p.is_absolute()) {
            return Ok(xdg.join("praxis-relay"));
        }
        home.map(|p| p.join(".local/share/praxis-relay"))
            .ok_or_else(|| std::io::Error::other("Home directory unavailable; set RELAY_AUTH_DIR"))
    }
}

#[cfg(test)]
mod storage_tests {
    use super::*;
    #[test]
    fn an_existing_portable_root_keeps_its_accounts() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("local_auth");
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("auth.json"), "existing-account").unwrap();
        assert_eq!(
            default_auth_root(&root.path().join("relay"), None, None).unwrap(),
            legacy
        );
        assert_eq!(
            std::fs::read_to_string(legacy.join("auth.json")).unwrap(),
            "existing-account"
        );
    }
    #[cfg(unix)]
    #[test]
    fn installed_application_uses_the_users_writable_directory() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let actual =
            default_auth_root(&root.path().join("system/bin/relay"), Some(&home), None).unwrap();
        #[cfg(target_os = "macos")]
        assert_eq!(
            actual,
            home.join("Library/Application Support/praxis-relay")
        );
        #[cfg(not(target_os = "macos"))]
        assert_eq!(actual, home.join(".local/share/praxis-relay"));
        assert!(!actual.starts_with(root.path().join("system")));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn xdg_must_be_absolute_and_has_priority_over_home() {
        let root = tempfile::tempdir().unwrap();
        let exe = root.path().join("bin/relay");
        let home = root.path().join("home");
        let xdg = root.path().join("data");
        assert_eq!(
            default_auth_root(&exe, Some(&home), Some(&xdg)).unwrap(),
            xdg.join("praxis-relay")
        );
        assert_eq!(
            default_auth_root(&exe, Some(&home), Some(Path::new("relative"))).unwrap(),
            home.join(".local/share/praxis-relay")
        );
        assert!(default_auth_root(&exe, None, Some(Path::new("relative"))).is_err());
    }
}
