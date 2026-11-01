//! Exercise compatibility at the actual HTTP boundary and compare upstream bytes.
use crate::*;
use axum::body::Body;
use serde_json::{json, Value};
use tower::ServiceExt;

fn request(hints: bool, stream: bool) -> Value {
    let mut value = json!({
        "model":"gpt-5.6-sol", "stream":stream,
        "prompt_cache_key":"fixture-affinity", "reasoning_effort":"none",
        "messages":[
            {"role":"system","content":[{"type":"text","text":"stable\r\ntext"}]},
            {"role":"user","content":"use fixture"},
            {"role":"assistant","content":null,"tool_calls":[
                {"id":"call_fixture","type":"function","function":{"name":"fixture","arguments":"{\"cache_control\":\"keep\"}"}}
            ]},
            {"role":"tool","tool_call_id":"call_fixture","content":[{"type":"text","text":"result"}]}
        ],
        "tools":[{"type":"function","function":{"name":"fixture","parameters":{
            "type":"object","properties":{"cache_control":{"type":"string"}}
        }}}]
    });
    if hints {
        value["messages"][0]["content"][0]["cache_control"] =
            json!({"type":"ephemeral","ttl":"1h"});
        value["messages"][3]["content"][0]["cache_control"] = json!({"type":"ephemeral"});
        value["tools"][0]["cache_control"] = json!({"type":"ephemeral"});
    }
    value
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn cache_hints_preserve_http_json_sse_and_upstream_wire() {
    use base64::Engine;
    let _guard = chat_completions::test_lever_guard();
    let records = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let seen = records.clone();
    let upstream = Router::new().fallback(post(move |Json(payload): Json<Value>| {
        let seen = seen.clone();
        async move {
            seen.lock().unwrap().push(payload);
            ([("content-type","text/event-stream")], concat!(
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\"}]}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5.6-sol\",\"output\":[],\"usage\":{\"input_tokens\":17,\"output_tokens\":3,\"total_tokens\":20}}}\n\n"
            ))
        }
    }));
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(socket, upstream).await.unwrap();
    });
    chat_completions::set_test_upstream_url(Some(format!("http://{address}/responses")));
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("accounts/primary");
    std::fs::create_dir_all(&home).unwrap();
    let claims = json!({"email":"fixture@example.invalid","https://api.openai.com/auth":{"chatgpt_plan_type":"pro"}});
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&claims).unwrap());
    std::fs::write(
        home.join("auth.json"),
        json!({"OPENAI_API_KEY":null,"tokens":{
        "id_token":format!("e30.{encoded}.c2ln"),"access_token":"synthetic-primary",
        "refresh_token":"synthetic-refresh","account_id":"synthetic-account"
    },"last_refresh":chrono::Utc::now().to_rfc3339()})
        .to_string(),
    )
    .unwrap();
    let app = fixture(root.path(), Some("fixture-client-key")).await;
    for path in ["/chat/completions", "/v1/chat/completions"] {
        for stream in [false, true] {
            for hints in [false, true] {
                let response = send(app.clone(), path, request(hints, stream), true).await;
                assert_eq!(
                    response.status(),
                    StatusCode::OK,
                    "hints={hints}, stream={stream}"
                );
                let content_type = response.headers()["content-type"]
                    .to_str()
                    .unwrap()
                    .to_owned();
                let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                    .await
                    .unwrap();
                if stream {
                    assert!(content_type.starts_with("text/event-stream"));
                    let wire = String::from_utf8(bytes.to_vec()).unwrap();
                    assert!(wire.contains("Hello"));
                    assert!(wire.contains("stop"));
                } else {
                    assert_eq!(content_type, "application/json");
                    let response: Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(response["choices"][0]["message"]["content"], "Hello");
                    assert_eq!(response["usage"]["prompt_tokens"], 17);
                }
            }
        }
    }
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 8);
    for payload in records.iter() {
        assert_eq!(
            payload, &records[0],
            "hints must not change the upstream payload"
        );
        assert_eq!(payload["prompt_cache_key"], "fixture-affinity");
        assert_eq!(
            payload["tools"][0]["parameters"]["properties"]["cache_control"]["type"],
            "string"
        );
        assert!(payload["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call"
                && item["arguments"] == "{\"cache_control\":\"keep\"}"));
        assert!(payload["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["output"] == "result"));
    }
    chat_completions::set_test_upstream_url(None);
    task.abort();
}

async fn fixture(root: &std::path::Path, key: Option<&str>) -> Router {
    app_router(AppState {
        api_key: key.map(Arc::from),
        config: Arc::new(Config {
            codex_home: root.into(),
            chatgpt_base_url: String::new(),
            model: "gpt-5.6-sol".into(),
            user_instructions: None,
            reasoning_effort: None,
            instructions_mode: Some("minimal".into()),
            parallel_tool_calls: true,
        }),
        accounts: AccountRouter::open(root).await.unwrap(),
        limits: LimitsCache::default(),
        catalog: ModelCatalog::static_only(),
        client: reqwest::Client::new(),
    })
}

async fn send(app: Router, path: &str, body: Value, key: bool) -> Response {
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if key {
        request = request.header("authorization", "Bearer fixture-client-key");
    }
    app.oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn cache_hints_keep_auth_and_unrelated_validation_strict() {
    let root = tempfile::tempdir().unwrap();
    let app = fixture(root.path(), Some("fixture-client-key")).await;
    let duplicate = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer fixture-client-key")
        .body(Body::from(
            r#"{"model":"gpt-5.6-sol","model":"gpt-5.6-sol","messages":[]}"#,
        ))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(duplicate).await.unwrap().status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        send(
            app.clone(),
            "/v1/chat/completions",
            request(true, false),
            false
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let mut extra_part = request(true, false);
    extra_part["messages"][0]["content"][0]["unknown_fixture"] = json!(true);
    assert_eq!(
        send(app.clone(), "/v1/chat/completions", extra_part, true)
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let mut extra_tool = request(true, false);
    extra_tool["tools"][0]["unknown_fixture"] = json!(true);
    assert_eq!(
        send(app.clone(), "/v1/chat/completions", extra_tool, true)
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let invalid_image = json!({"model":"gpt-5.6-sol","messages":[{"role":"user","content":[{
        "type":"image_url","image_url":{"url":"file:///private.png"},"cache_control":{"type":"ephemeral"}
    }]}]});
    assert_eq!(
        send(app, "/v1/chat/completions", invalid_image, true)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
}
