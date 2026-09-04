use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{sse::Event, IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};
use tracing_appender;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// Modules
mod core;
mod login;
#[cfg(target_family = "windows")]
mod tray;

use core::account_router::AccountRouter;
use core::catalog::ModelCatalog;
use core::chat_completions;
use core::config::Config;
use core::limits::LimitsCache;
use core::models::{ChatRequest, ModelList, MAX_CHAT_REQUEST_BYTES};
use login::lib::CodexAuth;

// For CLI menu
use std::io::{self, Write};
use std::sync::Mutex;
use tokio::task::JoinHandle;

// Global registry for server handles
use once_cell::sync::Lazy;
static SERVER_HANDLES: Lazy<Mutex<Vec<JoinHandle<()>>>> = Lazy::new(|| Mutex::new(Vec::new()));

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    accounts: AccountRouter,
    limits: LimitsCache,
    // What the backend serves today, refreshed on a TTL; /v1/models and the
    // request validator both read it (core::catalog).
    catalog: ModelCatalog,
    // One pooled client for every upstream call: Client::new() per request cost a
    // fresh DNS+TCP+TLS handshake to chatgpt.com on every single LLM call.
    client: reqwest::Client,
}

#[tokio::main]
async fn main() {
    // Declare per-monitor DPI awareness before any window exists: without it
    // Windows bitmap-scales the process UI on high-DPI displays and the tray
    // context menu renders blurry.  The console window itself belongs to
    // conhost and manages its own DPI either way.
    #[cfg(target_family = "windows")]
    unsafe {
        windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    // The container runs from /app, whose logs directory is the persisted
    // compose mount. RELAY_LOG_DIR keeps non-container deployments explicit.
    let logs_dir = std::env::var_os("RELAY_LOG_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                .join("logs")
        });
    if let Err(e) = std::fs::create_dir_all(&logs_dir) {
        eprintln!("Failed to create logs directory: {}", e);
    }
    // Initialize tracing with both console and file output
    let file_appender = tracing_appender::rolling::daily(logs_dir.clone(), "relay.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "codex_proxy=info,tower_http=info".into()),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stdout)
                .with_ansi(true)
                .with_target(false)
                .with_thread_ids(false)
                .with_thread_names(false),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_target(true)
                .with_thread_ids(true)
                .with_thread_names(true),
        )
        .init();

    info!("=== Starting Codex Proxy Server ==="); // Codex Proxy Server
    info!("Log directory: {}", logs_dir.display());
    info!("Timestamp: {}", chrono::Utc::now().to_rfc3339());

    // Display CLI menu
    loop {
        display_menu();
        let choice = get_user_choice();

        match choice.as_str() {
            "1" => {
                if let Err(e) = run_server().await {
                    error!("Failed to start server: {}", e);
                }
            }
            "2" => {
                // Close all servers functionality
                if let Err(e) = close_all_servers().await {
                    error!("Failed to close servers: {}", e);
                }
            }
            "3" => {
                if let Err(e) = run_login().await {
                    error!("Login failed: {}", e);
                }
            }
            "4" => {
                if let Err(e) = refresh_token().await {
                    error!("Token refresh failed: {}", e);
                }
            }
            "5" => {
                println!("Exiting...");
                break;
            }
            "6" => {
                if let Err(e) = list_running_servers().await {
                    error!("Failed to list running servers: {}", e);
                }
            }
            "7" => {
                if let Err(e) = edit_instructions() {
                    error!("Editing instructions failed: {}", e);
                }
            }
            _ => {
                println!("Invalid choice. Please try again.");
            }
        }
    }
}

async fn run_login() -> anyhow::Result<()> {
    info!("Starting login process");
    // The relay logs in only to its own dedicated auth directory (local_auth
    // next to the executable, or RELAY_AUTH_DIR) — never ~/.codex or ~/.opencode,
    // so relay credentials cannot mix with a normal Codex install.
    let config = Config::load()?;
    let auth_root = config.codex_home;

    println!("Login into which subscription slot?");
    println!("  [Enter] primary   — the main subscription");
    println!("  2       secondary — a standby subscription for quota failover");
    print!("Slot: ");
    io::stdout().flush().unwrap();
    let secondary = get_user_choice() == "2";

    let accounts_root = auth_root.join("accounts");
    let legacy_auth = auth_root.join("auth.json");
    let auth_dir = if secondary {
        // The account router reads either the single legacy auth.json OR the
        // accounts/ layout — never both.  A second subscription therefore
        // needs the layout; migrate an existing single login into primary
        // instead of silently orphaning it.
        let primary_dir = accounts_root.join("primary");
        if legacy_auth.is_file() && !primary_dir.join("auth.json").is_file() {
            std::fs::create_dir_all(&primary_dir)?;
            std::fs::rename(&legacy_auth, primary_dir.join("auth.json"))?;
            println!(
                "Moved the existing login to {:?} (slot primary).",
                primary_dir.join("auth.json")
            );
        }
        accounts_root.join("secondary")
    } else if accounts_root.join("primary").join("auth.json").is_file()
        || accounts_root.join("secondary").join("auth.json").is_file()
    {
        // The layout already exists: a primary (re-)login belongs in its slot.
        accounts_root.join("primary")
    } else {
        // Single-account install: keep the simple single-file layout.
        auth_root.clone()
    };

    std::fs::create_dir_all(&auth_dir)?;
    let auth_path = auth_dir.join("auth.json");
    println!("Relay auth file: {:?}", auth_path);

    // Success is defined by auth.json landing on disk, NOT by the login helper
    // exiting: the helper's HTTP server only shuts itself down when the browser
    // loads /success, and it kills itself on ANY unexpected request (a page
    // refresh, even favicon.ico).  Waiting for the process therefore hung the
    // menu after perfectly successful logins, and a refresh made a successful
    // login look failed.  Watch the file, and stop the helper ourselves.
    let modified_before = std::fs::metadata(&auth_path)
        .and_then(|meta| meta.modified())
        .ok();
    let auth_written = |before: &Option<std::time::SystemTime>| {
        let now = std::fs::metadata(&auth_path)
            .and_then(|meta| meta.modified())
            .ok();
        now.is_some() && now != *before
    };

    let spawned = login::lib::spawn_login_with_chatgpt(&auth_dir)?;
    println!("A browser window should open. Sign in to the ChatGPT account for this slot.");

    let started = std::time::Instant::now();
    let mut printed_url = false;
    let outcome = loop {
        if auth_written(&modified_before) {
            break Ok(());
        }
        if !printed_url {
            // get_login_url returns the last http token from the helper's
            // stderr; early on that is its own "http://localhost:1455" banner.
            // The real sign-in URL is the https:// one — wait for it.
            if let Some(url) = spawned.get_login_url() {
                if url.starts_with("https://") {
                    println!("If the browser did not open, use this URL:\n\n{url}\n");
                    printed_url = true;
                }
            }
        }
        let exit_status = spawned
            .child
            .lock()
            .ok()
            .and_then(|mut child| child.try_wait().ok().flatten());
        if let Some(status) = exit_status {
            // The helper may exit right after writing the file (or die on a
            // stray browser request just after success) — check once more.
            if auth_written(&modified_before) {
                break Ok(());
            }
            let stderr_tail = spawned
                .stderr
                .lock()
                .ok()
                .map(|buffer| String::from_utf8_lossy(&buffer).to_string())
                .unwrap_or_default();
            let tail_start = stderr_tail.len().saturating_sub(400);
            break Err(anyhow::anyhow!(
                "Login helper exited ({status}) before {:?} was written.\n{}",
                auth_path,
                &stderr_tail[tail_start..]
            ));
        }
        if started.elapsed() > std::time::Duration::from_secs(600) {
            break Err(anyhow::anyhow!("Login timed out after 10 minutes."));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    };

    // The helper's job ends the moment auth.json lands; left alone it lingers
    // on port 1455 until the browser hits /success.  Stop it either way.
    if let Ok(mut child) = spawned.child.lock() {
        let _ = child.kill();
        let _ = child.wait();
    }
    outcome?;

    println!("Auth file created successfully at: {:?}", auth_path);
    info!("Login successful");
    println!("Login completed!");
    println!("If the relay server was running while you added a NEW slot, restart it to pick the slot up.");
    Ok(())
}

/// Menu 7: show what currently rides at the top of every request, and hand it over.
///
/// `instructions` is sent above the whole conversation on every call, and it is the
/// one part of the prompt that an agent's own system prompt cannot reach: whatever
/// stands there frames everything the agent says about itself afterwards.  Until now
/// the text was compiled into the binary, so the relay spoke for its operator.  A
/// plain file is the smallest thing that gives the words back.
fn edit_instructions() -> anyhow::Result<()> {
    let path = core::config::instructions_file_path();
    println!();
    println!("=== Instructions sent above every request ===");
    println!("File: {}", path.display());
    match core::config::custom_instructions() {
        Some(text) => {
            println!("Source: this file");
            println!();
            println!("{text}");
        }
        None => {
            println!("Source: built-in text (the file is absent or blank)");
            println!();
            println!("{}", chat_completions::MINIMAL_INSTRUCTIONS);
        }
    }
    println!();
    println!("  e      write your own (opens an editor)");
    println!("  r      reset to the built-in text (deletes the file)");
    println!("  Enter  back");
    print!("> ");
    io::stdout().flush().ok();
    let mut choice = String::new();
    if io::stdin().read_line(&mut choice)? == 0 {
        return Ok(());
    }
    match choice.trim() {
        "e" | "E" => {
            if !path.exists() {
                // Seed with the built-in text.  Editing from a working example shows
                // the shape the field expects; a blank page invites an empty file,
                // which upstream would like even less than a wrong one.
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).ok();
                }
                let mut seed = chat_completions::MINIMAL_INSTRUCTIONS.to_string();
                seed.push('\n');
                std::fs::write(&path, seed)?;
            }
            open_in_editor(&path);
            println!("Saved text applies to the next request; no restart needed.");
        }
        "r" | "R" => match std::fs::remove_file(&path) {
            Ok(()) => println!("Removed {}; the built-in text is back.", path.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                println!("Nothing to remove; the built-in text was already in use.")
            }
            Err(error) => return Err(error.into()),
        },
        _ => {}
    }
    Ok(())
}

/// RELAY_EDITOR, then the usual VISUAL/EDITOR, then whatever the platform always has.
/// A failure to launch is not an error worth aborting on: the path is printed, and
/// editing the file by hand does the same job.
fn open_in_editor(path: &std::path::Path) {
    use std::process::Command;
    let editor = ["RELAY_EDITOR", "VISUAL", "EDITOR"]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_family = "windows") { "notepad".to_string() } else { "nano".to_string() }
        });
    println!("Opening {} in {editor} ...", path.display());
    match Command::new(&editor).arg(path).status() {
        Ok(status) if status.success() => {}
        Ok(status) => println!("{editor} exited with {status}. The file is at {}", path.display()),
        Err(error) => {
            println!("Could not launch {editor} ({error}).");
            println!("Edit this file by hand: {}", path.display());
        }
    }
}

fn display_menu() {
    println!("\n=== Codex Proxy Server===");
    println!("1. Run server");
    println!("2. Close all servers");
    println!("3. Login");
    println!("4. Refresh token");
    println!("5. Exit");
    println!("6. List running servers");
    println!("7. Edit instructions");
    print!("Please select an option (1-7): ");
    io::stdout().flush().unwrap();
}

fn get_user_choice() -> String {
    let mut choice = String::new();
    match io::stdin().read_line(&mut choice) {
        // EOF (stdin closed or redirected input ran out).  Without this the
        // menu loop spins forever printing "Invalid choice" at 100% CPU.
        Ok(0) => {
            println!("stdin closed; exiting.");
            "5".to_string()
        }
        Ok(_) => choice.trim().to_string(),
        Err(error) => {
            println!("Failed to read input ({error}); exiting.");
            "5".to_string()
        }
    }
}

async fn run_server() -> anyhow::Result<()> {
    info!("Starting Codex Proxy Server");

    // Load configuration
    let config = match Config::load() {
        Ok(config) => {
            info!("Configuration loaded successfully");
            Arc::new(config)
        }
        Err(e) => {
            error!("Failed to load configuration: {}", e);
            return Err(e);
        }
    };

    // Load the active subscription and any standby before accepting traffic.
    let accounts = AccountRouter::load(&config.codex_home).await?;

    // Create app state
    let client = reqwest::Client::builder()
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
        .unwrap_or_else(|error| {
            warn!("shared client builder failed ({}), falling back to default", error);
            reqwest::Client::new()
        });
    let app_state = AppState {
        config,
        accounts,
        limits: LimitsCache::default(),
        catalog: ModelCatalog::from_env(),
        client,
    };

    // Create router.  The chat endpoint answers both with and without the /v1
    // prefix: OpenAI-compatible clients disagree about whether base_url already
    // contains "/v1", and a silent 404 from the bare router is a support trap.
    // Same for /models vs /v1/models.
    let app = Router::new()
        .route(
            "/chat/completions",
            post(chat_completions_handler).layer(DefaultBodyLimit::max(MAX_CHAT_REQUEST_BYTES)),
        )
        .route(
            "/v1/chat/completions",
            post(chat_completions_handler).layer(DefaultBodyLimit::max(MAX_CHAT_REQUEST_BYTES)),
        )
        .route("/v1/models", get(models_handler))
        .route("/models", get(models_handler))
        .route("/v1/limits", get(limits_handler))
        .route("/v1/account", get(account_handler))
        .route("/v1/account/switch", post(account_switch_handler))
        .route("/health", get(health_handler))
        .layer(CorsLayer::permissive())
        .with_state(app_state);

    // Configure server.  Loopback only, deliberately; RELAY_PORT rescues the
    // rare machine where 5011 is already taken.
    let port = std::env::var("RELAY_PORT")
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .unwrap_or(5011);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    info!("Server listening on {}", addr);

    // Start server and block until it exits
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    println!("Server is running. Press Ctrl+C to stop.");
    #[cfg(target_family = "windows")]
    {
        println!("The relay minimizes to the system tray (icon by the clock).");
        println!("Double-click the tray icon to show this window again; quit from the tray menu.");
        tray::spawn(port);
    }
    axum::serve(listener, app).await.unwrap();
    Ok(())
}

async fn refresh_token() -> anyhow::Result<()> {
    println!("Refreshing token(s)...");

    // Load configuration
    let config = Config::load()?;

    // Walk every place a login can live: the legacy single file and both
    // account slots.  Menu 3 may have migrated the login into accounts/, and
    // a refresh that only ever looked at the root would report "no auth" on a
    // perfectly logged-in relay.
    let auth_root = config.codex_home;
    let candidates = [
        ("primary (legacy)", auth_root.clone()),
        ("primary", auth_root.join("accounts").join("primary")),
        ("secondary", auth_root.join("accounts").join("secondary")),
    ];

    let mut refreshed = 0;
    for (slot, dir) in candidates {
        if !dir.join("auth.json").is_file() {
            continue;
        }
        // The relay's own auth only — never ~/.codex or the OPENAI_API_KEY env var
        let codex_auth = match CodexAuth::from_auth_dir(&dir) {
            Ok(Some(auth)) => auth,
            _ => {
                println!("Slot {slot}: auth.json is invalid. Choose menu option 3 (Login) to replace it.");
                continue;
            }
        };
        // Get token data which will automatically refresh if needed
        match codex_auth.get_token_data().await {
            Ok(data) => {
                refreshed += 1;
                match &data.account_id {
                    Some(account_id) => println!("Slot {slot}: token OK, account {account_id}"),
                    None => println!("Slot {slot}: token OK, account id missing"),
                }
            }
            Err(error) => {
                println!("Slot {slot}: refresh failed ({error}). Choose menu option 3 (Login).");
            }
        }
    }

    if refreshed == 0 {
        return Err(anyhow::anyhow!(
            "No usable relay authentication found. Choose menu option 3 (Login) first."
        ));
    }
    println!("Done: {refreshed} slot(s) refreshed.");
    Ok(())
}

async fn close_all_servers() -> anyhow::Result<()> {
    println!("Closing all servers (system-wide)...");
    let mut closed = 0;
    for port in 5011..=5020 {
        let pids = get_pids_for_port(port);
        for pid in pids {
            if kill_pid(pid) {
                println!("Killed server on port {} (PID {})", port, pid);
                closed += 1;
            }
        }
    }
    println!("Closed {} running server(s) on ports 5011-5020.", closed);
    Ok(())
}

// Get PIDs listening on a port
fn get_pids_for_port(port: u16) -> Vec<u32> {
    #[cfg(target_family = "unix")]
    {
        use std::process::Command;
        let output = Command::new("lsof")
            .arg("-ti")
            .arg(format!(":{}", port))
            .output();
        if let Ok(out) = output {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout
                .lines()
                .filter_map(|line| line.trim().parse::<u32>().ok())
                .collect()
        } else {
            vec![]
        }
    }
    #[cfg(target_family = "windows")]
    {
        use std::process::Command;
        let output = Command::new("netstat").arg("-ano").output();
        if let Ok(out) = output {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout
                .lines()
                .filter_map(|line| {
                    if line.contains(&format!(":{}", port)) {
                        line.split_whitespace().last()?.parse::<u32>().ok()
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            vec![]
        }
    }
}

// Kill a process by PID
fn kill_pid(pid: u32) -> bool {
    #[cfg(target_family = "unix")]
    {
        use std::process::Command;
        let status = Command::new("kill").arg("-9").arg(pid.to_string()).status();
        status.map(|s| s.success()).unwrap_or(false)
    }
    #[cfg(target_family = "windows")]
    {
        use std::process::Command;
        let status = Command::new("taskkill")
            .arg("/PID")
            .arg(pid.to_string())
            .arg("/F")
            .status();
        status.map(|s| s.success()).unwrap_or(false)
    }
}

// Utility: Check if a port is in use (cross-platform)
fn is_port_in_use(port: u16) -> bool {
    #[cfg(target_family = "unix")]
    {
        use std::process::Command;
        // Try lsof first
        let lsof_output = Command::new("lsof")
            .arg("-i")
            .arg(format!(":{}", port))
            .output();
        if let Ok(out) = lsof_output {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if stdout.contains(&format!(":{}", port)) {
                return true;
            }
        } else {
            eprintln!("lsof failed for port {}", port);
        }
        // Fallback to netstat
        let netstat_output = Command::new("netstat").arg("-an").output();
        if let Ok(out) = netstat_output {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if stdout.contains(&format!(":{}", port)) {
                return true;
            }
        } else {
            eprintln!("netstat failed for port {}", port);
        }
        false
    }
    #[cfg(target_family = "windows")]
    {
        use std::process::Command;
        let output = Command::new("netstat").arg("-ano").output();
        if let Ok(out) = output {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if stdout.contains(&format!(":{}", port)) {
                return true;
            }
        } else {
            eprintln!("netstat failed for port {}", port);
        }
        false
    }
}

async fn list_running_servers() -> anyhow::Result<()> {
    println!("Checking ports 5011-5020 for running servers...");
    let mut found = false;
    for port in 5011..=5020 {
        if is_port_in_use(port) {
            println!("Port {}: RUNNING", port);
            found = true;
        }
    }
    if !found {
        println!("No running servers found on ports 5011-5020.");
    }
    Ok(())
}

async fn check_authentication(config: &Config) -> anyhow::Result<()> {
    info!(
        "Checking authentication in directory: {:?}",
        &config.codex_home
    );
    let auth_file_path = config.codex_home.join("auth.json");
    info!("Looking for auth file at: {:?}", auth_file_path);

    if !auth_file_path.is_file() {
        warn!("Dedicated relay auth file not found");
        return Err(anyhow::anyhow!(
            "No relay authentication found at {:?}. Choose menu option 3 (Login) first.",
            auth_file_path
        ));
    }

    let codex_auth = match CodexAuth::from_auth_dir(&config.codex_home) {
        Ok(Some(auth)) => auth,
        _ => {
            return Err(anyhow::anyhow!("Relay auth.json is invalid. Choose menu option 3 (Login) to replace it."));
        }
    };

    let token_data = match codex_auth.get_token_data().await {
        Ok(data) => data,
        Err(_) => {
            return Err(anyhow::anyhow!("Relay token data is unavailable. Choose menu option 3 (Login)."));
        }
    };

    if token_data.access_token.is_empty() {
        return Err(anyhow::anyhow!("Relay access token is empty. Choose menu option 3 (Login)."));
    }

    if token_data.account_id.is_none() {
        return Err(anyhow::anyhow!("Relay account ID is unavailable. Choose menu option 3 (Login)."));
    }

    // Log token information for debugging
    info!("Authentication successful");
    info!(
        "Plan type: {}",
        codex_auth.get_plan_type().as_deref().unwrap_or("None")
    );

    Ok(())
}

async fn health_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    info!("💓 Health check endpoint requested");
    let response = serde_json::json!({
        "status": "healthy",
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "service": "relay",
        "version": env!("CARGO_PKG_VERSION"),
        "account_router": {
            "active_slot": state.accounts.active_slot().await,
            "configured_slots": state.accounts.account_count(),
        },
        "model_catalog": state.catalog.status().await,
        "codex_client_version": chat_completions::codex_cli_version(),
    });
    info!("✅ Health check response: {}", response);
    Json(response)
}

async fn models_handler(State(state): State<AppState>) -> Json<ModelList> {
    info!("📋 Models endpoint requested");
    // The backend's own catalog (core::catalog), the same snapshot the request
    // validator below reads, so the advertised set cannot drift from the
    // accepted set. Static FALLBACK_MODELS only when the backend cannot be asked.
    let list = state
        .catalog
        .model_list(&state.accounts, &state.client)
        .await;
    info!("✅ Returning {} available models", list.data.len());
    Json(list)
}

async fn limits_handler(State(state): State<AppState>) -> Response {
    match state.limits.get(&state.accounts).await {
        Ok(value) => {
            let mut response = Json(value.clone()).into_response();
            core::limits::apply_response_headers(response.headers_mut(), &value);
            response
        }
        Err(error) => {
            warn!("Limits request failed: {}", error);
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": {
                        "message": "OpenAI usage limits are temporarily unavailable",
                        "type": "upstream_error",
                        "code": "limits_unavailable"
                    }
                })),
            )
                .into_response()
        }
    }
}

/// Which subscriptions exist and which one is live right now.
///
/// The active slot lives in the router's memory, so editing auth files on a
/// running relay changes nothing until a restart.  Without this pair of
/// endpoints "switch me to the other subscription" had no executor at all —
/// only an instruction telling a human to do it by hand.
async fn account_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "active_slot": state.accounts.active_slot().await,
        "configured_slots": state.accounts.account_count(),
        "slots": state.accounts.describe().await,
    }))
}

#[derive(serde::Deserialize)]
struct AccountSwitchRequest {
    slot: String,
}

async fn account_switch_handler(
    State(state): State<AppState>,
    payload: Result<Json<AccountSwitchRequest>, JsonRejection>,
) -> Response {
    let requested = match payload {
        Ok(Json(request)) => request.slot,
        Err(rejection) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": {
                        "message": format!("expected {{\"slot\": \"...\"}}: {rejection}"),
                        "type": "invalid_request_error",
                        "code": "invalid_json"
                    }
                })),
            )
                .into_response();
        }
    };
    let previous = state.accounts.active_slot().await;
    match state.accounts.switch_to(&requested).await {
        Ok(lease) => {
            info!("account switch requested: {} -> {}", previous, lease.slot);
            // No cache to clear: LimitsCache is keyed by account, and `get`
            // leases the active slot first — so "how much is left?" already
            // answers about the subscription that is now live.
            Json(serde_json::json!({
                "previous_slot": previous,
                "active_slot": lease.slot,
                "slots": state.accounts.describe().await,
            }))
            .into_response()
        }
        Err(error) => {
            warn!("account switch refused: {}", error);
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": {
                        "message": error.to_string(),
                        "type": "account_switch_refused",
                        "code": "switch_refused"
                    },
                    "active_slot": state.accounts.active_slot().await,
                    "slots": state.accounts.describe().await,
                })),
            )
                .into_response()
        }
    }
}

/// Collect the upstream chunk stream into one OpenAI-shaped `chat.completion`.
///
/// The upstream path is stream-only, and until now the parsed `"stream": false`
/// was ignored — every client got SSE whether it could read it or not, so a
/// non-streaming OpenAI SDK call tried to parse an SSE body as JSON and failed.
/// Tool calls arrive as complete calls (one array element each, with its own
/// index), so aggregation appends them; content concatenates; the last
/// finish_reason, usage, and relay_terminal win.
async fn aggregate_to_single_response(
    mut chunks: tokio::sync::mpsc::Receiver<anyhow::Result<core::models::ResponseEvent>>,
    requested_model: String,
) -> Response {
    let mut id = None;
    let mut created = None;
    let mut role = String::from("assistant");
    let mut content = String::new();
    let mut tool_calls: Vec<serde_json::Value> = Vec::new();
    let mut finish_reason: Option<String> = None;
    let mut usage: Option<core::models::Usage> = None;
    let mut relay_terminal: Option<core::models::RelayTerminal> = None;

    while let Some(event) = chunks.recv().await {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                error!("Non-streaming aggregation failed mid-stream: {}", error);
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({
                        "error": {
                            "message": format!("Upstream stream failed: {}", error),
                            "type": "upstream_error",
                            "code": "stream_error"
                        }
                    })),
                )
                    .into_response();
            }
        };
        if id.is_none() {
            id = Some(event.id.clone());
            created = Some(event.created);
        }
        if event.usage.is_some() {
            usage = event.usage.clone();
        }
        if event.relay_terminal.is_some() {
            relay_terminal = event.relay_terminal.clone();
        }
        for choice in &event.choices {
            if let Some(new_role) = &choice.delta.role {
                role = new_role.clone();
            }
            if let Some(chunk_content) = &choice.delta.content {
                content.push_str(chunk_content);
            }
            if let Some(calls) = choice.delta.tool_calls.as_ref().and_then(|v| v.as_array()) {
                tool_calls.extend(calls.iter().cloned());
            }
            if let Some(reason) = &choice.finish_reason {
                finish_reason = Some(reason.clone());
            }
        }
    }

    let Some(id) = id else {
        return (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": {
                    "message": "Upstream produced no chunks at all",
                    "type": "upstream_error",
                    "code": "empty_response"
                }
            })),
        )
            .into_response();
    };

    // OpenAI returns content: null (not "") on a pure tool-call turn.
    let content_value = if content.is_empty() && !tool_calls.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(content)
    };
    let mut message = serde_json::json!({ "role": role, "content": content_value });
    if !tool_calls.is_empty() {
        message["tool_calls"] = serde_json::Value::Array(tool_calls);
    }
    let mut body = serde_json::json!({
        "id": id,
        "object": "chat.completion",
        "created": created.unwrap_or_else(|| chrono::Utc::now().timestamp()),
        "model": requested_model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason.unwrap_or_else(|| "stop".to_string()),
        }],
        "usage": usage.unwrap_or(core::models::Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            prompt_tokens_details: None,
        }),
    });
    if let Some(terminal) = relay_terminal {
        body["relay_terminal"] =
            serde_json::to_value(&terminal).unwrap_or(serde_json::Value::Null);
    }
    Json(body).into_response()
}

async fn chat_completions_handler(
    State(state): State<AppState>,
    _headers: HeaderMap,
    payload: Result<Json<ChatRequest>, JsonRejection>,
) -> Result<Response, StatusCode> {
    let mut request = match payload {
        Ok(Json(request)) => request,
        Err(rejection) => {
            let status = rejection.status();
            let code = if status == StatusCode::PAYLOAD_TOO_LARGE {
                "request_too_large"
            } else {
                "invalid_json"
            };
            return Ok((
                status,
                Json(serde_json::json!({
                    "error": {
                        "message": rejection.body_text(),
                        "type": "invalid_request_error",
                        "param": null,
                        "code": code
                    }
                })),
            )
                .into_response());
        }
    };

    // llama.cpp-style clients (Ouroboros's local lane among them) hardcode the
    // slug "local-model" because llama-cpp-python ignores the field.  Map that
    // one literal onto the relay default BEFORE validation; every other unknown
    // name stays a strict-list 404 so real typos keep failing loudly.
    if request.model == core::models::LOCAL_MODEL_ALIAS {
        info!(
            "model alias: {} -> {}",
            core::models::LOCAL_MODEL_ALIAS,
            state.config.model
        );
        request.model = state.config.model.clone();
    }

    info!("🚀 CHAT COMPLETIONS REQUEST RECEIVED!");
    info!("Request messages count: {}", request.messages.len());
    info!("Request tools count: {}", request.tools.len());
    info!("Request image parts count: {}", request.image_part_count());

    // Validate the model against the backend's live catalog (core::catalog), not
    // a constant and not a prefix: a slug the cached catalog does not know
    // triggers one early refresh, so a model released after the last fetch is
    // accepted on first ask; a typo is still refused loudly here, and the 404
    // lists what is actually on offer right now.
    let model_known = state
        .catalog
        .is_known(&request.model, &state.accounts, &state.client)
        .await;
    info!("Request model supported: {}", model_known);
    if !model_known {
        warn!("Invalid model requested (value redacted)");
        let snapshot = state
            .catalog
            .snapshot(&state.accounts, &state.client)
            .await;
        return Ok((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": {
                    "message": format!(
                        "Model not found. Available models ({}): {}. Requested: {}",
                        snapshot.source.as_str(),
                        snapshot.listed_slugs().join(", "),
                        request.model
                    ),
                    "type": "model_not_found",
                    "code": "model_not_found"
                }
            }))
        ).into_response());
    }

    // Reasoning effort: the catalog says which levels this model takes.
    // gpt-6-astra refuses "none"/"minimal" with a 400, which used to cost a
    // failed call plus a conservative retry carrying the full Codex preamble
    // (~5k prompt tokens per turn). Clamp to the nearest level the model
    // supports before anything goes upstream; models the catalog knows no
    // levels for are left exactly as before.
    let snapshot = state
        .catalog
        .snapshot(&state.accounts, &state.client)
        .await;
    if let Some(entry) = snapshot.find(&request.model) {
        let wanted = request
            .reasoning_effort
            .clone()
            .or_else(|| state.config.reasoning_effort.clone());
        let clamped = core::catalog::clamp_effort(
            wanted.as_deref(),
            &entry.reasoning_efforts,
            entry.default_reasoning_effort.as_deref(),
        );
        if clamped != wanted {
            info!(
                "reasoning effort {:?} -> {:?} for {} (model takes {:?})",
                wanted, clamped, request.model, entry.reasoning_efforts
            );
        }
        if wanted.is_some() {
            request.reasoning_effort = clamped;
        }
    }

    if let Err(validation_error) = request.validate_content() {
        warn!(
            "Invalid message content at {}: {}",
            validation_error.param, validation_error.message
        );
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": {
                    "message": validation_error.to_string(),
                    "type": "invalid_request_error",
                    "param": validation_error.param,
                    "code": "invalid_message_content"
                }
            })),
        )
            .into_response());
    }

    let wants_stream = request.stream;
    let requested_model = request.model.clone();

    // Process the chat completion
    match chat_completions::stream_chat_completions(
        &state.config,
        state.accounts.clone(),
        request,
        state.client.clone(),
    )
    .await
    {
        Ok(response_stream) => {
            if !wants_stream {
                info!("✅ Chat completion started (non-streaming aggregation)");
                return Ok(aggregate_to_single_response(response_stream, requested_model).await);
            }
            info!("✅ Chat completion stream started successfully");
            // Convert the response stream to SSE
            let sse_stream = tokio_stream::wrappers::ReceiverStream::new(response_stream)
                .map(|result| {
                    match result {
                        Ok(event) => {
                            let json = serde_json::to_string(&event).unwrap_or_else(|e| {
                                error!("Failed to serialize event: {}", e);
                                r#"{"error": "Failed to serialize event"}"#.to_string()
                            });
                            Ok::<Event, Box<dyn std::error::Error + Send + Sync>>(Event::default().data(json))
                        }
                        Err(e) => {
                            error!("Stream error: {}", e);
                            let error_json = serde_json::to_string(&serde_json::json!({
                                "error": {
                                    "message": format!("Stream error: {}", e),
                                    "type": "stream_error",
                                    "code": "stream_error"
                                }
                            })).unwrap_or_else(|_| r#"{"error":{"message":"Failed to format error","type":"format_error","code":"format_error"}}"#.to_string());
                            Ok::<Event, Box<dyn std::error::Error + Send + Sync>>(Event::default().data(error_json))
                        }
                    }
                });

            Ok(axum::response::Sse::new(sse_stream).into_response())
        }
        Err(e) => {
            error!("❌ Chat completions error: {}", e);
            error!("Error details: {:?}", e);
            Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": {
                        "message": format!("Failed to process chat completion: {}", e),
                        "type": "server_error",
                        "code": "internal_error"
                    }
                })),
            )
                .into_response())
        }
    }
}
