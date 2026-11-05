//! Real HTTP producer -> relay -> client checks. No vendor login or network.
use crate::*;
use axum::body::Body;
use base64::Engine;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};

fn state(root: &std::path::Path, accounts: AccountRouter) -> AppState {
    AppState {
        api_key: None,
        config: Arc::new(Config {
            codex_home: root.to_path_buf(),
            chatgpt_base_url: String::new(),
            model: "gpt-5.6-sol".into(),
            user_instructions: None,
            reasoning_effort: None,
            instructions_mode: Some("minimal".into()),
            parallel_tool_calls: true,
        }),
        accounts,
        limits: LimitsCache::default(),
        catalog: ModelCatalog::static_only(),
        client: reqwest::Client::new(),
    }
}

fn account(root: &std::path::Path) {
    let home = root.join("accounts/primary");
    std::fs::create_dir_all(&home).unwrap();
    let claims = json!({"email":"synthetic@example.test","https://api.openai.com/auth":{"chatgpt_plan_type":"pro"}});
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&claims).unwrap());
    std::fs::write(
        home.join("auth.json"),
        json!({"OPENAI_API_KEY":null,
        "tokens":{"id_token":format!("e30.{encoded}.c2ln"),"access_token":"synthetic-primary",
        "refresh_token":"synthetic-refresh","account_id":"synthetic-account"},
        "last_refresh":chrono::Utc::now().to_rfc3339()})
        .to_string(),
    )
    .unwrap();
}

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(socket, app).await.unwrap();
    });
    (format!("http://{address}"), task)
}

fn sse(events: Vec<Value>) -> String {
    events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

fn message(text: &str) -> Value {
    json!({"type":"response.output_item.done","item":{"type":"message","role":"assistant",
        "content":[{"type":"output_text","text":text}]}})
}
fn completed(usage: bool) -> Value {
    let mut value =
        json!({"type":"response.completed","response":{"model":"gpt-5.6-sol","output":[]}});
    if usage {
        value["response"]["usage"] = json!({"input_tokens":17,"output_tokens":3,"total_tokens":20,
        "input_tokens_details":{"cached_tokens":11}});
    }
    value
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn http_chat_stream_flag_selects_json_or_sse_without_replaying() {
    let _guard = chat_completions::test_lever_guard();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let upstream = Router::new().fallback(post(move |Json(payload): Json<Value>| {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            assert_eq!(payload["stream"], true, "upstream remains streaming");
            (
                [("content-type", "text/event-stream")],
                sse(vec![message("Hello"), completed(true)]),
            )
        }
    }));
    let (upstream_url, upstream_task) = serve(upstream).await;
    chat_completions::set_test_upstream_url(Some(format!("{upstream_url}/responses")));
    let root = tempfile::tempdir().unwrap();
    account(root.path());
    let accounts = AccountRouter::load(root.path()).await.unwrap();
    let (url, relay_task) = serve(app_router(state(root.path(), accounts))).await;
    let client = reqwest::Client::new();
    for path in ["/chat/completions", "/v1/chat/completions"] {
        for stream in [None, Some(false), Some(true)] {
            let mut body =
                json!({"model":"gpt-5.6-sol","messages":[{"role":"user","content":"Hi"}]});
            if let Some(value) = stream {
                body["stream"] = json!(value);
            }
            let response = client
                .post(format!("{url}{path}"))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            let content_type = response.headers()["content-type"]
                .to_str()
                .unwrap()
                .to_string();
            let wire = response.text().await.unwrap();
            if stream == Some(true) {
                assert!(content_type.starts_with("text/event-stream"));
                assert!(wire.starts_with("data:"));
            } else {
                assert_eq!(content_type, "application/json");
                let output: Value = serde_json::from_str(&wire).unwrap();
                assert_eq!(output["object"], "chat.completion");
                assert_eq!(output["model"], "gpt-5.6-sol");
                assert_eq!(output["choices"][0]["message"]["content"], "Hello");
                assert_eq!(output["choices"][0]["finish_reason"], "stop");
                assert_eq!(
                    output["usage"]["prompt_tokens_details"]["cached_tokens"],
                    11
                );
            }
        }
    }
    assert_eq!(hits.load(Ordering::SeqCst), 6);
    relay_task.abort();
    upstream_task.abort();
    chat_completions::set_test_upstream_url(None);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn http_json_preserves_parallel_tools_unknown_usage_and_failure() {
    let _guard = chat_completions::test_lever_guard();
    let wire = Arc::new(std::sync::Mutex::new(String::new()));
    let current = wire.clone();
    let upstream = Router::new().fallback(post(move || {
        let current = current.clone();
        async move {
            (
                [("content-type", "text/event-stream")],
                current.lock().unwrap().clone(),
            )
        }
    }));
    let (upstream_url, upstream_task) = serve(upstream).await;
    chat_completions::set_test_upstream_url(Some(format!("{upstream_url}/responses")));
    let root = tempfile::tempdir().unwrap();
    account(root.path());
    let accounts = AccountRouter::load(root.path()).await.unwrap();
    let (url, relay_task) = serve(app_router(state(root.path(), accounts))).await;
    let client = reqwest::Client::new();
    let body =
        json!({"model":"gpt-5.6-sol","messages":[{"role":"user","content":"Hi"}],"stream":false});
    let tool = |id, name, args| {
        json!({"type":"response.output_item.done","item":{
        "type":"function_call","call_id":id,"name":name,"arguments":args}})
    };
    *wire.lock().unwrap() = sse(vec![
        tool("call_a", "alpha", "{\"a\":1}"),
        tool("call_b", "beta", "{}"),
        completed(false),
    ]);
    let output: Value = client
        .post(format!("{url}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(output.get("usage").is_none());
    assert_eq!(
        output["model"], "gpt-5.6-sol",
        "not placeholder model from tool chunk"
    );
    assert_eq!(output["choices"][0]["finish_reason"], "tool_calls");
    let calls = output["choices"][0]["message"]["tool_calls"]
        .as_array()
        .unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["id"], "call_a");
    assert_eq!(calls[0]["function"]["arguments"], "{\"a\":1}");
    assert_eq!(calls[1]["function"]["name"], "beta");
    assert!(calls[0].get("index").is_none());
    for events in [
        vec![message("partial")],
        vec![
            tool("call_a", "alpha", "{}"),
            json!({"type":"response.failed","response":{"error":{"message":"synthetic failure"}}}),
        ],
    ] {
        *wire.lock().unwrap() = sse(events);
        let output: Value = client
            .post(format!("{url}/chat/completions"))
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(output.get("error").is_some());
        assert_eq!(output["choices"][0]["finish_reason"], "error");
        assert!(output["choices"][0]["message"]["content"].is_null());
        assert!(output["choices"][0]["message"].get("tool_calls").is_none());
    }
    *wire.lock().unwrap() = sse(vec![completed(false)]);
    let output: Value = client
        .post(format!("{url}/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(output["choices"][0]["finish_reason"], "stop");
    assert!(output.get("usage").is_none());
    relay_task.abort();
    upstream_task.abort();
    chat_completions::set_test_upstream_url(None);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn dropping_json_client_before_first_byte_cancels_upstream() {
    let _guard = chat_completions::test_lever_guard();
    struct Dropped(Arc<tokio::sync::Semaphore>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.add_permits(1);
        }
    }
    let started = Arc::new(tokio::sync::Semaphore::new(0));
    let dropped = Arc::new(tokio::sync::Semaphore::new(0));
    let seen = started.clone();
    let ended = dropped.clone();
    let upstream = Router::new().fallback(post(move || {
        let seen = seen.clone();
        let ended = ended.clone();
        async move {
            let stream = async_stream::stream! {
                let _drop = Dropped(ended); seen.add_permits(1);
                yield Ok::<_,std::io::Error>(bytes::Bytes::from(sse(vec![message("partial")])));
                std::future::pending::<()>().await;
            };
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
    }));
    let (upstream_url, upstream_task) = serve(upstream).await;
    chat_completions::set_test_upstream_url(Some(format!("{upstream_url}/responses")));
    let root = tempfile::tempdir().unwrap();
    account(root.path());
    let accounts = AccountRouter::load(root.path()).await.unwrap();
    let (url, relay_task) = serve(app_router(state(root.path(), accounts))).await;
    let client_task = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{url}/chat/completions"))
            .json(&json!({"model":"gpt-5.6-sol","messages":[{"role":"user","content":"Hi"}]}))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), started.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    client_task.abort();
    let _ = client_task.await;
    tokio::time::timeout(std::time::Duration::from_secs(3), dropped.acquire())
        .await
        .expect("JSON body drop must cancel upstream before any result byte")
        .unwrap()
        .forget();
    relay_task.abort();
    upstream_task.abort();
    chat_completions::set_test_upstream_url(None);
}

#[tokio::test]
async fn json_error_keeps_legacy_diagnosis_and_typed_attempts_out_of_assistant_text() {
    use crate::core::models::ResponseEvent;
    for terminal in [
        None,
        Some(
            json!({"code":"mixed_account_failure", "message":"quota then auth",
        "attempts":[{"slot":"primary","code":"quota_exhausted","status":429,"resets_at":12345},
                    {"slot":"secondary","code":"auth_failed","status":401}]}),
        ),
    ] {
        let mut raw = json!({"id":"failure","object":"chat.completion.chunk","created":1,
            "model":"gpt-5.6-sol","choices":[{"index":0,"delta":{"role":"assistant",
                "content":"401: subscription credentials require login"},"finish_reason":"error"}],
            "usage":{"prompt_tokens":7,"completion_tokens":1,"total_tokens":8}});
        if let Some(value) = terminal.clone() {
            raw["relay_terminal"] = value;
        }
        let event: ResponseEvent = serde_json::from_value(raw).unwrap();
        let response = crate::chat_response::json_response(
            futures::stream::iter(vec![Ok(event)]),
            "gpt-5.6-sol".into(),
        );
        let bytes = axum::body::to_bytes(response.into_body(), 100000)
            .await
            .unwrap();
        let output: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(output["choices"][0]["message"]["content"].is_null());
        assert_eq!(output["choices"][0]["finish_reason"], "error");
        assert_eq!(output["usage"]["total_tokens"], 8);
        if terminal.is_some() {
            assert_eq!(output["relay_terminal"]["attempts"][0]["status"], 429);
            assert_eq!(output["relay_terminal"]["attempts"][1]["status"], 401);
            assert_eq!(output["error"]["message"], "quota then auth");
        } else {
            assert_eq!(
                output["error"]["message"],
                "401: subscription credentials require login"
            );
        }
    }
}

fn decoded_chat_events(wire: &str) -> Vec<Value> {
    wire.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "[DONE]")
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn fast_mode_forwards_requested_tier_and_reports_actual_json_and_sse() {
    let _guard = chat_completions::test_lever_guard();
    let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let received = seen.clone();
    let upstream = Router::new().fallback(post(move |Json(payload): Json<Value>| {
        let received = received.clone();
        async move {
            received.lock().unwrap().push(payload.clone());
            let scenario = payload["input"][0]["content"].as_str().unwrap();
            let (first, last) = match scenario {
                "downgrade" => (Some("priority"), Some("default")),
                "early_only" => (Some("priority"), None),
                "terminal_only" => (None, Some("fast")),
                "future_tier" => (Some("scale"), None),
                "terminal_null" => (Some("priority"), None),
                _ => (None, None),
            };
            let mut created = json!({"type":"response.created","response":{"model":"gpt-5.6-sol"}});
            let mut terminal = completed(true);
            if let Some(tier) = first {
                created["response"]["service_tier"] = json!(tier);
            }
            if let Some(tier) = last {
                terminal["response"]["service_tier"] = json!(tier);
            }
            if scenario == "terminal_null" {
                terminal["response"]["service_tier"] = Value::Null;
            }
            (
                [("content-type", "text/event-stream")],
                sse(vec![created, message("Hello"), terminal]),
            )
        }
    }));
    let (upstream_url, upstream_task) = serve(upstream).await;
    chat_completions::set_test_upstream_url(Some(format!("{upstream_url}/responses")));
    let root = tempfile::tempdir().unwrap();
    account(root.path());
    let accounts = AccountRouter::load(root.path()).await.unwrap();
    let (url, relay_task) = serve(app_router(state(root.path(), accounts))).await;
    let client = reqwest::Client::new();
    let requested = [
        None,
        Some(Value::Null),
        Some(json!("fast")),
        Some(json!("priority")),
        Some(json!("default")),
        Some(json!("auto")),
        Some(json!("flex")),
    ];
    for path in ["/chat/completions", "/v1/chat/completions"] {
        for streaming in [false, true] {
            for tier in &requested {
                for (scenario, first, last) in [
                    ("downgrade", Some("priority"), Some("default")),
                    ("early_only", Some("priority"), Some("priority")),
                    ("terminal_only", None, Some("fast")),
                    ("future_tier", Some("scale"), Some("scale")),
                    ("terminal_null", Some("priority"), None),
                    ("unknown", None, None),
                ] {
                    let effort = if tier.as_ref().and_then(Value::as_str) == Some("fast") { "ultra" } else { "high" };
                    let mut body = json!({"model":"gpt-5.6-sol","stream":streaming,
                        "reasoning_effort":effort,"prompt_cache_key":"tier-fixture",
                        "messages":[{"role":"user","content":scenario}]});
                    if let Some(tier) = tier {
                        body["service_tier"] = tier.clone();
                    }
                    let response = client
                        .post(format!("{url}{path}"))
                        .json(&body)
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(response.status(), reqwest::StatusCode::OK);
                    let wire = response.text().await.unwrap();
                    let captured = seen.lock().unwrap().last().cloned().unwrap();
                    let expected = tier.as_ref().and_then(Value::as_str).map(|tier| {
                        if tier == "fast" {
                            "priority"
                        } else {
                            tier
                        }
                    });
                    assert_eq!(
                        captured.get("service_tier").and_then(Value::as_str),
                        expected
                    );
                    assert_eq!(captured["reasoning"]["effort"], effort);
                    assert_eq!(captured["prompt_cache_key"], "tier-fixture");
                    assert_eq!(captured["input"][0]["content"], scenario);
                    if streaming {
                        let events = decoded_chat_events(&wire);
                        assert_eq!(
                            events
                                .first()
                                .unwrap()
                                .get("service_tier")
                                .and_then(Value::as_str),
                            first
                        );
                        assert_eq!(
                            events
                                .last()
                                .unwrap()
                                .get("service_tier")
                                .and_then(Value::as_str),
                            last
                        );
                        assert_eq!(
                            events.last().unwrap()["choices"][0]["finish_reason"],
                            "stop"
                        );
                        if scenario == "terminal_null" {
                            assert_eq!(
                                events.last().unwrap().get("service_tier"),
                                Some(&Value::Null)
                            );
                        }
                    } else {
                        let output: Value = serde_json::from_str(&wire).unwrap();
                        assert_eq!(output.get("service_tier").and_then(Value::as_str), last);
                        assert_eq!(output["choices"][0]["message"]["content"], "Hello");
                        if scenario == "terminal_null" {
                            assert_eq!(output.get("service_tier"), Some(&Value::Null));
                        }
                    }
                }
            }
        }
    }
    relay_task.abort();
    upstream_task.abort();
    chat_completions::set_test_upstream_url(None);
}

#[tokio::test]
async fn invalid_service_tier_is_http400_before_catalog_or_account_work() {
    use tower::ServiceExt;
    let root = tempfile::tempdir().unwrap();
    let accounts = AccountRouter::open(root.path()).await.unwrap();
    let app = app_router(state(root.path(), accounts));
    for path in ["/chat/completions", "/v1/chat/completions"] {
        for tier in [
            json!(""),
            json!("FAST"),
            json!("premium"),
            json!(" priority "),
            json!(true),
            json!(1),
            json!([]),
            json!({"tier":"fast"}),
        ] {
            let request = axum::http::Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model":"unknown-tier-model","messages":[],"service_tier":tier})
                        .to_string(),
                ))
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["error"]["param"], "service_tier");
            assert_eq!(value["error"]["code"], "invalid_service_tier");
        }
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn rejected_service_tier_is_not_retried_without_requested_knobs() {
    let _guard = chat_completions::test_lever_guard();
    let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let received = seen.clone();
    let upstream = Router::new().fallback(post(move |Json(payload): Json<Value>| {
        let parameter = payload["input"][0]["content"].as_str().unwrap().to_owned();
        received.lock().unwrap().push(payload);
        async move {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":{"type":"invalid_request_error",
            "message":"Controlled parameter rejection", "param":parameter}})),
            )
        }
    }));
    let (upstream_url, upstream_task) = serve(upstream).await;
    chat_completions::set_test_upstream_url(Some(format!("{upstream_url}/responses")));
    let root = tempfile::tempdir().unwrap();
    account(root.path());
    let accounts = AccountRouter::load(root.path()).await.unwrap();
    let (url, relay_task) = serve(app_router(state(root.path(), accounts))).await;
    for parameter in ["service_tier", "prompt_cache_key"] {
        let response = reqwest::Client::new()
            .post(format!("{url}/v1/chat/completions"))
            .json(
                &json!({"model":"gpt-5.6-sol","service_tier":"fast","reasoning_effort":"high",
                "messages":[{"role":"user","content":parameter}]}),
            )
            .send()
            .await
            .unwrap();
        let output: Value = response.json().await.unwrap();
        assert!(output.get("error").is_some());
        assert!(output.get("service_tier").is_none());
    }
    let captured = seen.lock().unwrap();
    assert_eq!(captured.len(), 2);
    for payload in captured.iter() {
        assert_eq!(payload["service_tier"], "priority");
        assert_eq!(payload["reasoning"]["effort"], "high");
    }
    relay_task.abort();
    upstream_task.abort();
    chat_completions::set_test_upstream_url(None);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn failed_stream_keeps_actual_tier_without_replaying_receipted_attempt() {
    let _guard = chat_completions::test_lever_guard();
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let upstream=Router::new().fallback(post(move || {
        counted.fetch_add(1,Ordering::SeqCst);
        async { ([ ("content-type","text/event-stream") ],sse(vec![
            json!({"type":"response.created","response":{"model":"gpt-5.6-sol","service_tier":"priority"}}),
            json!({"type":"response.failed","response":{"service_tier":"default","error":{"message":"Controlled failure"}}}),
        ])) }
    }));
    let (upstream_url, upstream_task) = serve(upstream).await;
    chat_completions::set_test_upstream_url(Some(format!("{upstream_url}/responses")));
    let root = tempfile::tempdir().unwrap();
    account(root.path());
    let accounts = AccountRouter::load(root.path()).await.unwrap();
    let (url, relay_task) = serve(app_router(state(root.path(), accounts))).await;
    for streaming in [false, true] {
        let response = reqwest::Client::new()
            .post(format!("{url}/v1/chat/completions"))
            .json(
                &json!({"model":"gpt-5.6-sol","stream":streaming,"service_tier":"fast",
                "messages":[{"role":"user","content":"Hi"}]}),
            )
            .send()
            .await
            .unwrap();
        let wire = response.text().await.unwrap();
        if streaming {
            let events = decoded_chat_events(&wire);
            assert_eq!(events.first().unwrap()["service_tier"], "priority");
            assert_eq!(events.last().unwrap()["service_tier"], "default");
            assert!(events.last().unwrap().get("relay_terminal").is_some());
        } else {
            let output: Value = serde_json::from_str(&wire).unwrap();
            assert_eq!(output["service_tier"], "default");
            assert!(output.get("error").is_some());
        }
    }
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    relay_task.abort();
    upstream_task.abort();
    chat_completions::set_test_upstream_url(None);
}
