use super::*;
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use std::sync::{Arc, Mutex};

struct MockProvider {
    code: (StatusCode, Value),
    polls: Mutex<Vec<StatusCode>>,
    tokens: (StatusCode, Value),
    requests: Mutex<Vec<(String, Value)>>,
}

struct Fixture {
    issuer: String,
    provider: Arc<MockProvider>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn jwt(claims: Value) -> String {
    format!(
        "e30.{}.c2ln",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string())
    )
}
fn tokens() -> Value {
    json!({"id_token":jwt(json!({"email":"device@example.invalid", "https://api.openai.com/auth":{
        "chatgpt_account_id":"device-account", "chatgpt_plan_type":"pro"}})),
        "access_token":"synthetic-access", "refresh_token":"synthetic-refresh"})
}

impl Fixture {
    async fn new(code: Value, polls: Vec<StatusCode>, tokens: (StatusCode, Value)) -> Self {
        let provider = Arc::new(MockProvider {
            code: (StatusCode::OK, code),
            polls: Mutex::new(polls),
            tokens,
            requests: Mutex::new(vec![]),
        });
        let app = Router::new()
            .route("/api/accounts/deviceauth/usercode", post(|State(p): State<Arc<MockProvider>>, Json(body): Json<Value>| async move {
                p.requests.lock().unwrap().push(("usercode".into(), body));
                (p.code.0, Json(p.code.1.clone())).into_response()
            }))
            .route("/api/accounts/deviceauth/token", post(|State(p): State<Arc<MockProvider>>, Json(body): Json<Value>| async move {
                p.requests.lock().unwrap().push(("poll".into(), body));
                let mut polls = p.polls.lock().unwrap();
                let status = if polls.len() > 1 { polls.remove(0) } else { polls[0] };
                (status, Json(json!({"authorization_code":"synthetic-authorization", "code_verifier":"synthetic-verifier"}))).into_response()
            }))
            .route("/oauth/token", post(|State(p): State<Arc<MockProvider>>, axum::Form(body): axum::Form<std::collections::HashMap<String,String>>| async move {
                p.requests.lock().unwrap().push(("exchange".into(), serde_json::to_value(body).unwrap()));
                (p.tokens.0, Json(p.tokens.1.clone())).into_response()
            })).with_state(provider.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            issuer,
            provider,
            task,
        }
    }
    async fn normal(polls: Vec<StatusCode>) -> Self {
        Self::new(
            json!({"device_auth_id":"private-device-id", "user_code":"ABCD-12345", "interval":"5"}),
            polls,
            (StatusCode::OK, tokens()),
        )
        .await
    }
    async fn login(
        &self,
        destination: &Path,
        publish: impl FnOnce(DeviceCodePrompt),
    ) -> io::Result<tempfile::TempDir> {
        let client = Client::builder()
            .timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        staged_login_with(
            &client,
            &self.issuer,
            destination,
            Duration::from_millis(5),
            publish,
        )
        .await
    }
}

#[tokio::test]
async fn device_flow_polls_both_pending_statuses_and_installs_only_after_confirmation() {
    let fixture = Fixture::normal(vec![
        StatusCode::FORBIDDEN,
        StatusCode::NOT_FOUND,
        StatusCode::OK,
    ])
    .await;
    let root = tempfile::tempdir().unwrap();
    let previous = root.path().join("auth.json");
    std::fs::write(&previous, b"previous-account").unwrap();
    let mut public_prompt = None;
    let staged = fixture
        .login(root.path(), |prompt| public_prompt = Some(prompt))
        .await
        .unwrap();
    assert_eq!(std::fs::read(&previous).unwrap(), b"previous-account");
    let prompt = serde_json::to_value(public_prompt.unwrap()).unwrap();
    assert_eq!(prompt["user_code"], "ABCD-12345");
    assert!(!prompt.to_string().contains("private-device-id"));
    assert!(!prompt.to_string().contains("synthetic"));
    let requests = fixture.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[0].1["client_id"], super::super::lib::CLIENT_ID);
    assert_eq!(requests[1].1["device_auth_id"], "private-device-id");
    let exchange = &requests.last().unwrap().1;
    assert_eq!(exchange["grant_type"], "authorization_code");
    assert_eq!(exchange["code_verifier"], "synthetic-verifier");
    assert_eq!(
        exchange["redirect_uri"],
        format!("{}/deviceauth/callback", fixture.issuer)
    );
    super::super::lib::install_staged_login(staged.path(), root.path()).unwrap();
    let saved: Value = serde_json::from_slice(&std::fs::read(previous).unwrap()).unwrap();
    assert_eq!(saved["tokens"]["id_token"], tokens()["id_token"]);
    assert_eq!(saved["tokens"]["account_id"], "device-account");
    assert!(saved["OPENAI_API_KEY"].is_null());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(root.path().join("auth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn cancellation_drops_staging_and_never_replaces_the_active_account() {
    let fixture = Fixture::normal(vec![StatusCode::FORBIDDEN]).await;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("auth.json"), b"keep-owner-account").unwrap();
    let issuer = fixture.issuer.clone();
    let destination = root.path().to_owned();
    let (sent, received) = tokio::sync::oneshot::channel();
    let login = tokio::spawn(async move {
        let client = Client::new();
        staged_login_with(
            &client,
            &issuer,
            &destination,
            Duration::from_millis(5),
            |prompt| {
                let _ = sent.send(prompt);
            },
        )
        .await
    });
    assert!(received.await.is_ok());
    login.abort();
    assert!(login.await.unwrap_err().is_cancelled());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    assert_eq!(
        std::fs::read(root.path().join("auth.json")).unwrap(),
        b"keep-owner-account"
    );
}

#[tokio::test]
async fn expiry_and_provider_denial_preserve_existing_credentials_and_hide_response_secrets() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("auth.json"), b"old").unwrap();
    let fixture = Fixture::new(
        json!({"device_auth_id":"private", "usercode":"CODE-123", "interval":1,"expires_in":1}),
        vec![StatusCode::FORBIDDEN],
        (StatusCode::OK, tokens()),
    )
    .await;
    let error = fixture
        .login(root.path(), |_| {})
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("expired"), "{error}");
    let fixture = Fixture::new(
        json!({"device_auth_id":"private", "user_code":"CODE-123"}),
        vec![StatusCode::OK],
        (
            StatusCode::BAD_REQUEST,
            json!({"secret":"never-print-this-token"}),
        ),
    )
    .await;
    let error = fixture
        .login(root.path(), |_| {})
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("400"));
    assert!(!error.contains("never-print"));
    assert_eq!(
        std::fs::read(root.path().join("auth.json")).unwrap(),
        b"old"
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn malformed_codes_timing_and_incomplete_tokens_are_rejected_without_installation() {
    let root = tempfile::tempdir().unwrap();
    for code in [
        json!({"device_auth_id":"id","user_code":"bad\nterminal-code"}),
        json!({"device_auth_id":"id","user_code":"CODE","interval":"not-an-interval"}),
        json!({"device_auth_id":"","user_code":"CODE"}),
    ] {
        let fixture = Fixture::new(code, vec![StatusCode::OK], (StatusCode::OK, tokens())).await;
        assert!(fixture.login(root.path(), |_| {}).await.is_err());
        assert_eq!(fixture.provider.requests.lock().unwrap().len(), 1);
    }
    let mut incomplete = tokens();
    incomplete["refresh_token"] = json!("");
    let fixture = Fixture::new(
        json!({"device_auth_id":"id","user_code":"CODE"}),
        vec![StatusCode::OK],
        (StatusCode::OK, incomplete),
    )
    .await;
    assert!(fixture
        .login(root.path(), |_| {})
        .await
        .unwrap_err()
        .to_string()
        .contains("incomplete"));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn numeric_and_string_timing_and_account_claim_fallback_are_supported() {
    assert_eq!(seconds(&json!(5), 1).unwrap(), 5);
    assert_eq!(seconds(&json!("5"), 1).unwrap(), 5);
    assert_eq!(seconds(&Value::Null, 5).unwrap(), 5);
    assert!(seconds(&json!(0), 1).is_err());
    assert!(seconds(&json!(901), 1).is_err());
    let root = tempfile::tempdir().unwrap();
    persist_tokens(
        root.path(),
        Tokens {
            id_token: jwt(json!({"email":"fallback@example.invalid"})),
            access_token: jwt(
                json!({"https://api.openai.com/auth":{"chatgpt_account_id":"fallback"}}),
            ),
            refresh_token: "synthetic-refresh".into(),
        },
    )
    .unwrap();
    let auth = super::super::lib::try_read_auth_json(&get_auth_file(root.path())).unwrap();
    assert_eq!(auth.tokens.unwrap().account_id.as_deref(), Some("fallback"));
}
