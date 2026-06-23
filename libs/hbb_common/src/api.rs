use lazy_static::lazy_static;
use log::{info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio::time::{Duration as TokioDuration};
use sodiumoxide::base64;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use std::fs;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use std::io::Read;
use crate::config::{Config, Config2};

const API_URI: &'static str = "https://api.hoptodesk.com/                                                                                                                                                                              ";

#[derive(Debug, Clone)]
pub struct ApiError(String);

impl<E: std::error::Error> From<E> for ApiError {
    fn from(e: E) -> Self {
        Self(e.to_string())
    }
}

/// Build an HTTP client that routes through the proxy when configured.
pub fn make_http_client() -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30));
    if let Some(conf) = Config::get_socks() {
        let proxy_url = crate::proxy::proxy_url(&conf);
        if let Ok(proxy) = reqwest::Proxy::all(&proxy_url) {
            builder = builder.proxy(proxy);
        }
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Default)]
struct OnceAPI {
    response: Arc<Mutex<Option<serde_json::Value>>>,
    last_calls: Arc<Mutex<HashMap<String, Instant>>>,
    refresh_started: AtomicBool,
    force_refresh: AtomicBool,
    from_local: AtomicBool,
    last_forced_refresh: Arc<Mutex<Option<Instant>>>,
}

impl OnceAPI {
    async fn call(&self) -> Result<serde_json::Value, ApiError> {
        let force = self.force_refresh.swap(false, Ordering::SeqCst);

        if self.from_local.load(Ordering::SeqCst) {
            let r = self.response.lock().await;
            if let Some(cached) = &*r {
                return Ok(cached.clone());
            }
        }

        if !force {
            let r = self.response.lock().await;
            if let Some(cached) = &*r {
                return Ok(cached.clone());
            }
        }

        let api_uri_trim = API_URI.trim();

        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            let local_api_json = Config::path("api.json");
            if let Ok(mut file) = fs::File::open(&local_api_json) {
                let mut body = String::new();
                file.read_to_string(&mut body).ok();
                match serde_json::from_str::<serde_json::Value>(&body) {
                    Ok(ret) => {
                        {
                            let mut r = self.response.lock().await;
                            *r = Some(ret.clone());
                        }
                        self.from_local.store(true, Ordering::SeqCst);
                        info!("Loaded local api.json");
                        return Ok(ret);
                    }
                    Err(_e) => {
                        warn!("Found api.json but invalid format.");
                    }
                }
            }
        }

        if !force {
            if let Ok(cached) = self.load_from_persistent_cache().await {
                self.start_background_refresh(api_uri_trim);
                return Ok(cached);
            }
        }

        let api_uri = Config2::get().options.get("custom-api-url")
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| api_uri_trim.to_owned());

        match self.try_api_call(&api_uri).await {
            Ok(json_data) => {
                {
                    let mut r = self.response.lock().await;
                    *r = Some(json_data.clone());
                }
                let json_clone = json_data.clone();
                tokio::spawn(async move {
					let cache_value = base64::encode(serde_json::to_string(&json_clone).unwrap_or_default(), base64::Variant::Original);
					if !json_clone.is_null() && !cache_value.is_empty() && json_clone.is_object() { Config::set_option("api-cache".to_owned(), cache_value); }
                });
                self.start_background_refresh(api_uri_trim);

                Ok(json_data)
            }
            Err(_e) => {
                self.load_from_persistent_cache().await
            }
        }
    }

    async fn force_refresh_on_connect_failure(&self) -> bool {
        if self.from_local.load(Ordering::SeqCst) {
            return false;
        }

        {
            let mut last = self.last_forced_refresh.lock().await;
            if let Some(at) = *last {
                if at.elapsed() < Duration::from_secs(3600) {
                    return false;
                }
            }
            *last = Some(Instant::now());
        }

        let api_uri = Config2::get().options.get("custom-api-url")
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| API_URI.trim().to_owned());

        let json_data = match self.fetch_no_ratelimit(&api_uri).await {
            Ok(json) => json,
            Err(e) => {
                warn!("Forced API refresh on connect failure failed: {:?}", e);
                return false;
            }
        };

        {
            let mut r = self.response.lock().await;
            *r = Some(json_data.clone());
        }
        {
            let mut last_calls = self.last_calls.lock().await;
            last_calls.insert(api_uri, Instant::now());
        }

        let json_clone = json_data.clone();
        tokio::spawn(async move {
            let cache_value = base64::encode(serde_json::to_string(&json_clone).unwrap_or_default(), base64::Variant::Original);
            if !json_clone.is_null() && !cache_value.is_empty() && json_clone.is_object() { Config::set_option("api-cache".to_owned(), cache_value); }
        });

        true
    }

    async fn fetch_no_ratelimit(&self, api_uri: &str) -> Result<serde_json::Value, ApiError> {
        info!("Loading API (forced refresh) {}", api_uri);
        let response = make_http_client().get(api_uri).send().await?;
        let body = response.text().await?;
        let json: serde_json::Value = serde_json::from_str(&body)?;
        Ok(json)
    }

    async fn try_api_call(&self, api_uri: &str) -> Result<serde_json::Value, ApiError> {
        let mut last_calls = self.last_calls.lock().await;
        let now = Instant::now();

        if let Some(&last_call_time) = last_calls.get(api_uri) {
            if now.duration_since(last_call_time) < Duration::from_secs(39600) {
				return Err(ApiError("Rate limited - API called too recently".to_string()));
            }
        }

        last_calls.insert(api_uri.to_string(), now);
        drop(last_calls);

        info!("Loading API {}", api_uri);
		let response = make_http_client().get(api_uri).send().await?;
        let body = response.text().await?;
        let json: serde_json::Value = serde_json::from_str(&body)?;
		//info!("API Response {}", &body);
        Ok(json)
    }

    async fn load_from_persistent_cache(&self) -> Result<serde_json::Value, ApiError> {
        let cache_str = Config2::get().options.get("api-cache").cloned();
        if let Some(cache_value) = cache_str {
            if !cache_value.is_empty() {
                if let Ok(decoded) = base64::decode(&cache_value, base64::Variant::Original) {
                    if let Ok(json_str) = String::from_utf8(decoded) {
                        if let Ok(cached_json) = serde_json::from_str::<serde_json::Value>(&json_str) {
                            {
                                let mut r = self.response.lock().await;
                                *r = Some(cached_json.clone());
								//info!("Loaded API from cache");
                            }

                            return Ok(cached_json);
                        }
                    }
                }
            }
        }

        Err(ApiError("No valid cache available".to_string()))
    }

    fn start_background_refresh(&self, _api_uri_trim: &str) {
        if self.refresh_started.swap(true, Ordering::SeqCst) {
            return;
        }

        tokio::spawn(async move {
            loop {
                ONCE.erase().await;
                let _ = ONCE.call().await;
                tokio::time::sleep(TokioDuration::from_secs(43200)).await;
            }
        });
    }

    async fn erase(&self) {
		{
            let mut r = self.response.lock().await;
            *r = None;
        }
        self.force_refresh.store(true, Ordering::SeqCst);
    }
}

lazy_static! {
    static ref ONCE: OnceAPI = OnceAPI::default();
}

pub async fn call_api() -> Result<serde_json::Value, ApiError> {
    (*ONCE).call().await
}

pub async fn erase_api() {
    (*ONCE).erase().await
}

pub async fn force_refresh_on_connect_failure() -> bool {
    (*ONCE).force_refresh_on_connect_failure().await
}
