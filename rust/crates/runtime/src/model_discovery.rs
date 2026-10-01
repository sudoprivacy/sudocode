//! Model discovery scoped to the actual endpoint and authentication headers.
//! Credentials are kept in memory; disk paths contain only their SHA-256 digest.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fs_backend::{FsBackend, StdFsBackend};
use crate::model_capabilities::{parse_api_response, ModelCapabilitiesFile, ModelCapability};

const TTL: u64 = 300;
const RETRY_DELAY: u64 = 30;

#[derive(Clone)]
pub struct DiscoverySource {
    pub models_url: String,
    pub headers: BTreeMap<String, String>,
}

#[derive(Clone)]
pub struct ModelCatalog(Arc<CatalogInner>);

struct CatalogInner {
    source: DiscoverySource,
    path: PathBuf,
    snapshot: RwLock<Option<ModelCapabilitiesFile>>,
    refresh: tokio::sync::Mutex<()>,
    last_attempt: Mutex<u64>,
    background_running: AtomicBool,
}

#[derive(Serialize, Deserialize)]
struct CachedCatalog {
    version: u32,
    catalog: ModelCapabilitiesFile,
}

static CATALOGS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<CatalogInner>>>> = OnceLock::new();
thread_local! {
    static ACTIVE: RefCell<Option<ModelCatalog>> = const { RefCell::new(None) };
}

pub struct CatalogGuard(Option<ModelCatalog>, PhantomData<Rc<()>>);
impl Drop for CatalogGuard {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            active.replace(self.0.take());
        });
    }
}

impl ModelCatalog {
    #[must_use]
    pub fn open(config_home: &Path, source: DiscoverySource) -> Self {
        let mut hash = Sha256::new();
        hash.update(source.models_url.as_bytes());
        for (name, value) in &source.headers {
            hash.update([0]);
            hash.update(name.as_bytes());
            hash.update([0]);
            hash.update(value.as_bytes());
        }
        let path = config_home
            .join("cache")
            .join("model-catalogs")
            .join(format!("{:x}.json", hash.finalize()));
        let mut registry = CATALOGS
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(inner) = registry.get(&path).and_then(Weak::upgrade) {
            return Self(inner);
        }
        registry.retain(|_, value| value.strong_count() > 0);
        let snapshot = StdFsBackend
            .read_to_string(&path.to_string_lossy())
            .ok()
            .and_then(|raw| serde_json::from_str::<CachedCatalog>(&raw).ok())
            .filter(|cached| cached.version == 1)
            .map(|mut cached| {
                cached.catalog.default = ModelCapabilitiesFile::default().default;
                cached.catalog
            });
        let inner = Arc::new(CatalogInner {
            source,
            path: path.clone(),
            snapshot: RwLock::new(snapshot),
            refresh: tokio::sync::Mutex::new(()),
            last_attempt: Mutex::new(0),
            background_running: AtomicBool::new(false),
        });
        registry.insert(path, Arc::downgrade(&inner));
        Self(inner)
    }

    #[must_use]
    pub fn snapshot(&self) -> ModelCapabilitiesFile {
        self.0
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| ModelCapabilitiesFile {
                models: BTreeMap::new(),
                ..ModelCapabilitiesFile::default()
            })
    }

    #[must_use]
    pub fn model_ids(&self) -> Vec<String> {
        self.snapshot().models.into_keys().collect()
    }

    #[must_use]
    pub fn enter(&self) -> CatalogGuard {
        CatalogGuard(
            ACTIVE.with(|active| active.replace(Some(self.clone()))),
            PhantomData,
        )
    }

    /// Enter only while polling: a suspended future must never leave a
    /// thread-local route installed on a shared Tokio worker.
    pub async fn scope<F: Future>(&self, future: F) -> F::Output {
        let mut future = Box::pin(future);
        std::future::poll_fn(|cx| {
            let _scope = self.enter();
            future.as_mut().poll(cx)
        })
        .await
    }

    pub fn refresh_in_background(&self) {
        if !self.stale() || self.0.background_running.swap(true, Ordering::AcqRel) {
            return;
        }
        let catalog = self.clone();
        std::thread::spawn(move || {
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                if let Err(error) = rt.block_on(catalog.refresh(false)) {
                    tracing::debug!(%error, "model catalog refresh failed; retaining previous snapshot");
                }
            }
            catalog.0.background_running.store(false, Ordering::Release);
        });
    }

    fn stale(&self) -> bool {
        let file = self
            .0
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        file.as_ref()
            .is_none_or(|file| now().saturating_sub(file.updated_at) >= TTL)
    }

    /// A failed or partial response leaves the last complete snapshot intact.
    /// Error strings deliberately omit endpoint URLs, credentials, and bodies.
    pub async fn refresh(&self, force: bool) -> Result<(), String> {
        let _lock = self.0.refresh.lock().await;
        if !force && !self.stale() {
            return Ok(());
        }
        {
            let mut attempted = self
                .0
                .last_attempt
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !force && now().saturating_sub(*attempted) < RETRY_DELAY {
                return Ok(());
            }
            *attempted = now();
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("scode/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| "Could not initialize model discovery")?;
        let mut url = reqwest::Url::parse(&self.0.source.models_url)
            .map_err(|_| "Invalid model discovery endpoint")?;
        let mut models = BTreeMap::new();
        let bundled = ModelCapabilitiesFile::default();
        let mut cursors = BTreeSet::new();
        for page in 0..20 {
            let mut request = client.get(url.clone());
            for (name, value) in &self.0.source.headers {
                request = request.header(name, value);
            }
            let mut response = request
                .send()
                .await
                .map_err(|_| "Model discovery request failed")?;
            if !response.status().is_success() {
                return Err(format!(
                    "Model discovery returned HTTP {}",
                    response.status().as_u16()
                ));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| "Model discovery response was interrupted")?
            {
                if body.len() + chunk.len() > 4 * 1024 * 1024 {
                    return Err("Model discovery response was too large".into());
                }
                body.extend_from_slice(&chunk);
            }
            let json: serde_json::Value = serde_json::from_slice(&body)
                .map_err(|_| "Model discovery returned invalid JSON")?;
            let data = json
                .get("data")
                .and_then(serde_json::Value::as_array)
                .ok_or("Model discovery returned no data array")?;
            let entries = parse_api_response(&json);
            if entries.len() != data.len() || entries.iter().any(|entry| entry.id.trim().is_empty())
            {
                return Err("Model discovery returned invalid model IDs".into());
            }
            // A valid empty list is an authoritative empty entitlement; it must
            // not resurrect models from another key or the bundled catalog.
            for entry in entries {
                let curated = bundled.models.get(&entry.id);
                let cap = ModelCapability {
                    context_window: entry
                        .context_window
                        .or_else(|| curated.and_then(|c| c.context_window)),
                    max_output_tokens: entry
                        .max_output_tokens
                        .or_else(|| curated.and_then(|c| c.max_output_tokens)),
                    vision_supported: entry
                        .vision_supported
                        .or_else(|| curated.and_then(|c| c.vision_supported)),
                    image_max_bytes: entry
                        .image_max_bytes
                        .or_else(|| curated.and_then(|c| c.image_max_bytes)),
                    image_max_dimension: entry
                        .image_max_dimension
                        .or_else(|| curated.and_then(|c| c.image_max_dimension)),
                    endpoint_types: entry.supported_endpoint_types,
                };
                models.insert(entry.id, cap);
            }
            if json.get("has_more").and_then(serde_json::Value::as_bool) != Some(true) {
                let file = ModelCapabilitiesFile {
                    updated_at: now(),
                    default: bundled.default,
                    models,
                };
                let encoded = serde_json::to_vec(&CachedCatalog {
                    version: 1,
                    catalog: file.clone(),
                })
                .map_err(|_| "Could not encode model catalog")?;
                if let Some(parent) = self.0.path.parent() {
                    StdFsBackend
                        .create_dir_all(&parent.to_string_lossy())
                        .map_err(|_| "Could not create model catalog cache")?;
                }
                StdFsBackend
                    .write_atomic(&self.0.path.to_string_lossy(), &encoded)
                    .map_err(|_| "Could not save model catalog cache")?;
                *self
                    .0
                    .snapshot
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(file);
                return Ok(());
            }
            let cursor = json
                .get("last_id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or("Model discovery pagination omitted its cursor")?;
            if page == 19 || !cursors.insert(cursor.to_owned()) {
                return Err("Model discovery pagination did not finish".into());
            }
            let query: Vec<(String, String)> = url
                .query_pairs()
                .filter(|(name, _)| name != "after_id")
                .map(|(name, value)| (name.into_owned(), value.into_owned()))
                .collect();
            url.query_pairs_mut()
                .clear()
                .extend_pairs(query)
                .append_pair("after_id", cursor);
        }
        Err("Model discovery pagination did not finish".into())
    }
}

pub(crate) fn active_snapshot() -> Option<ModelCapabilitiesFile> {
    ACTIVE.with(|active| active.borrow().as_ref().map(ModelCatalog::snapshot))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Query, State};
    use axum::http::HeaderMap;
    use axum::{routing::get, Json, Router};
    use serde_json::{json, Value};

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
                let cap = crate::model_capabilities::lookup("claude-unseen-2040").unwrap();
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
                assert!(crate::model_capabilities::all_model_ids()
                    .contains(&"claude-unseen-2040".into()));
                assert!(!crate::model_capabilities::all_model_ids()
                    .contains(&"second-account-only".into()));
            }),
            second.scope(async {
                tokio::task::yield_now().await;
                assert_eq!(
                    crate::model_capabilities::all_model_ids(),
                    vec!["second-account-only"]
                );
            })
        );
        assert!(active_snapshot().is_none());
        *state.body.lock().unwrap() = json!({"data":[{"id":"partial"}],"has_more":true});
        assert!(first.refresh(true).await.is_err());
        assert_eq!(
            same.model_ids(),
            vec!["claude-unseen-2040", "second-page-only"]
        );
        *state.body.lock().unwrap() = json!({"data":[{"id":"claude-unseen-2040","context_window":2000000,"max_output_tokens":100000}]});
        first.refresh(true).await.unwrap();
        assert_eq!(same.model_ids(), vec!["claude-unseen-2040"]);
        same.scope(async {
            assert_eq!(
                crate::model_capabilities::context_window_or_default("claude-unseen-2040"),
                2000000
            );
        })
        .await;
        let disk = std::fs::read_to_string(&first.0.path).unwrap();
        assert!(!disk.contains("first-key"));
        assert!(!disk.contains(&address.to_string()));
        *state.body.lock().unwrap() = json!({"data":[]});
        first.refresh(true).await.unwrap();
        assert!(same.model_ids().is_empty());
        assert_eq!(second.model_ids(), vec!["second-account-only"]);
        server.abort();
        std::fs::remove_dir_all(&home).unwrap();
    }
}
