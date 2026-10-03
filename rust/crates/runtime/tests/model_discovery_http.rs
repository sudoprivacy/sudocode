//! Discovery over real HTTP, including persistence and concurrent route scopes.

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::{routing::get, Json, Router};
use runtime::model_discovery::{DiscoverySource, ModelCatalog};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Fixture {
    body: Arc<Mutex<Value>>,
    calls: Arc<Mutex<Vec<(String, BTreeMap<String, String>)>>>,
}

async fn models(
    State(state): State<Fixture>,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Json<Value> {
    let key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    state
        .calls
        .lock()
        .unwrap()
        .push((key.to_owned(), query.clone()));
    if key == "second-key" {
        return Json(json!({"data":[{"id":"second-account-only"}]}));
    }
    if query.contains_key("after_id") {
        return Json(json!({"data":[{"id":"second-page-only"}],"has_more":false}));
    }
    Json(state.body.lock().unwrap().clone())
}

#[tokio::test]
async fn discovery_refreshes_live_limits_and_isolates_endpoint_and_key() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let state = Fixture {
        body: Arc::new(Mutex::new(json!({"data":[{
            "id":"claude-unseen-2040","max_input_tokens":1234567,"max_tokens":98765,
            "capabilities":{"image_input":{"supported":false}}
        }],"has_more":true,"last_id":"claude-unseen-2040"}))),
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route("/v1/models", get(models))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let home = std::env::temp_dir().join(format!(
        "scode-model-discovery-{}-{}",
        std::process::id(),
        address.port()
    ));
    let source = DiscoverySource {
        models_url: format!("http://{address}/v1/models?limit=2"),
        headers: BTreeMap::from([("x-api-key".into(), "first-key".into())]),
    };
    let first = ModelCatalog::open(&home, source.clone());
    let same = ModelCatalog::open(&home, source.clone());
    let second = ModelCatalog::open(
        &home,
        DiscoverySource {
            headers: BTreeMap::from([("x-api-key".into(), "second-key".into())]),
            ..source.clone()
        },
    );
    let other_endpoint = ModelCatalog::open(
        &home,
        DiscoverySource {
            models_url: format!("http://{address}/other/models"),
            ..source
        },
    );
    assert!(first.model_ids().is_empty());
    first.refresh(true).await.unwrap();
    assert_eq!(
        same.model_ids(),
        vec!["claude-unseen-2040", "second-page-only"]
    );
    assert!(other_endpoint.model_ids().is_empty());
    assert!(second.model_ids().is_empty());
    second.refresh(true).await.unwrap();
    assert_eq!(second.model_ids(), vec!["second-account-only"]);
    let calls = state.calls.lock().unwrap().clone();
    assert_eq!(calls[1].0, "first-key");
    assert_eq!(calls[1].1.get("limit").unwrap(), "2");
    assert_eq!(calls[1].1.get("after_id").unwrap(), "claude-unseen-2040");
    first
        .scope(async {
            let cap = runtime::model_capabilities::lookup("claude-unseen-2040").unwrap();
            assert_eq!(cap.context_window, Some(1234567));
            assert_eq!(cap.max_output_tokens, Some(98765));
            assert_eq!(cap.vision_supported, Some(false));
        })
        .await;
    // Yield two different routes on the same worker. Neither may leave its
    // catalog active while suspended, or borrow the other account's IDs.
    tokio::join!(
        first.scope(async {
            tokio::task::yield_now().await;
            assert!(
                runtime::model_capabilities::all_model_ids().contains(&"claude-unseen-2040".into())
            );
            assert!(!runtime::model_capabilities::all_model_ids()
                .contains(&"second-account-only".into()));
        }),
        second.scope(async {
            tokio::task::yield_now().await;
            assert_eq!(
                runtime::model_capabilities::all_model_ids(),
                vec!["second-account-only"]
            );
        })
    );
    *state.body.lock().unwrap() = json!({"data":[{"id":"partial"}],"has_more":true});
    assert!(first.refresh(true).await.is_err());
    assert_eq!(
        same.model_ids(),
        vec!["claude-unseen-2040", "second-page-only"]
    );
    // A model the gateway lists *without* token metadata. The limits must come
    // from the bundled table when it curates the model, and from `default` when
    // it does not -- never from a per-model copy of `default`, which is what
    // made a fabricated window indistinguishable from a documented one.
    // `claude-opus-5-5` is the live shape: sudorouter returns only its id and
    // `supported_endpoint_types`.
    *state.body.lock().unwrap() = json!({"data":[
        {"id":"claude-opus-5-5","supported_endpoint_types":["anthropic"]},
        {"id":"gateway-only-model","supported_endpoint_types":["openai"]}
    ]});
    first.refresh(true).await.unwrap();
    same.scope(async {
        let curated = runtime::model_capabilities::lookup("claude-opus-5-5")
            .expect("a listed model must resolve");
        assert_eq!(
            curated.context_window,
            Some(1_000_000),
            "an undocumented but curated model takes the binary's number"
        );
        assert_eq!(curated.max_output_tokens, Some(128_000));
        let unknown = runtime::model_capabilities::lookup("gateway-only-model")
            .expect("a listed model must resolve");
        assert!(
            unknown.context_window.is_none(),
            "a model nobody curates must carry no window of its own"
        );
        assert_eq!(
            unknown.endpoint_types,
            Some(vec!["openai".to_string()]),
            "what the gateway did say must survive -- it picks the wire format"
        );
    })
    .await;

    *state.body.lock().unwrap() = json!({"data":[{"id":"claude-unseen-2040","context_window":2000000,"max_output_tokens":100000}]});
    first.refresh(true).await.unwrap();
    assert_eq!(same.model_ids(), vec!["claude-unseen-2040"]);
    same.scope(async {
        assert_eq!(
            runtime::model_capabilities::context_window_or_default("claude-unseen-2040"),
            2000000
        );
    })
    .await;
    let disk = std::fs::read_dir(home.join("cache/model-catalogs"))
        .unwrap()
        .map(|file| std::fs::read_to_string(file.unwrap().path()).unwrap())
        .collect::<String>();
    assert!(!disk.contains("first-key"));
    assert!(!disk.contains(&address.to_string()));
    *state.body.lock().unwrap() = json!({"data":[]});
    first.refresh(true).await.unwrap();
    assert!(same.model_ids().is_empty());
    assert_eq!(second.model_ids(), vec!["second-account-only"]);
    server.abort();
    std::fs::remove_dir_all(&home).unwrap();
}
