use lazy_static::lazy_static;
use log::{info, warn};
use std::collections::HashMap;
use std::sync::Arc;
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

/// Build an HTTP client that routes through the SOCKS5 proxy when configured.
pub fn make_http_client() -> reqwest::Client {
    let mut builder = reqwest::Client::builder();
    if let Some(conf) = Config::get_socks() {
        let proxy_url = if !conf.username.is_empty() {
            format!("socks5://{}:{}@{}", conf.username, conf.password, conf.proxy)
        } else {
            format!("socks5://{}", conf.proxy)
        };
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
}

impl OnceAPI {
    async fn call(&self) -> Result<serde_json::Value, ApiError> {
        {
            let r = self.response.lock().await;
            if let Some(cached) = &*r {
                return Ok(cached.clone());
            }
        }

        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
			let local_api_json = Config::path("api.json");
            if let Ok(mut file) = fs::File::open(&local_api_json) {
				let mut body = String::new();
				file.read_to_string(&mut body).ok();
				match serde_json::from_str::<serde_json::Value>(&body) {
					Ok(ret) => {
						let mut r = self.response.lock().await;
						*r = Some(ret.clone());
						info!("Loaded local api.json");
						return Ok(ret);
					}
					Err(_e) => {
						warn!("Found api.json but invalid format.");
					}
				}
            }
        }

        // Check persistent cache before making a network call.
        // This avoids redundant API requests from child processes (e.g. --connect)
        // that don't share the in-memory cache with the main process.
        if let Ok(cached) = self.load_from_persistent_cache().await {
            return Ok(cached);
        }

        let api_uri_trim = API_URI.trim();
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
            Err(e) => {
                warn!("API call failed: {:?}", e);
                self.load_from_persistent_cache().await
            }
        }
    }

    async fn try_api_call(&self, api_uri: &str) -> Result<serde_json::Value, ApiError> {
        let mut last_calls = self.last_calls.lock().await;
        let now = Instant::now();

        if let Some(&last_call_time) = last_calls.get(api_uri) {
            if now.duration_since(last_call_time) < Duration::from_secs(60) {
                //info!("Rate limiting, API call skipped");
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

    fn start_background_refresh(&self, api_uri_trim: &str) {
        let response = self.response.clone();
        let api_uri_trim = api_uri_trim.to_owned();

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(TokioDuration::from_secs(30000)).await;
                let api_uri = Config2::get().options.get("custom-api-url")
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| api_uri_trim.to_owned());

                info!("Refreshing API {}", api_uri);

                if let Ok(resp) = make_http_client().get(&api_uri).send().await {
                    if let Ok(txt) = resp.text().await {
                        if let Ok(ret) = serde_json::from_str::<serde_json::Value>(&txt) {
                            {
                                let mut r = response.lock().await;
                                *r = Some(ret.clone());
                            }

                            let ret_clone = ret.clone();
                            tokio::spawn(async move {
								let cache_value = base64::encode(serde_json::to_string(&ret_clone).unwrap_or_default(), base64::Variant::Original);
								if !ret_clone.is_null() && !cache_value.is_empty() && ret_clone.is_object() { Config::set_option("api-cache".to_owned(), cache_value); }
                            });

                            info!("Background API refresh successful");
                        }
                    }
                }
            }
        });
    }

    async fn erase(&self) {
		{
            let mut r = self.response.lock().await;
            *r = None;
        }

        // Also clear the rate limiting cache
        {
            let mut last_calls = self.last_calls.lock().await;
            last_calls.clear();
        }

        tokio::spawn(async move {
            Config::set_option("api-cache".to_owned(), "".to_owned());
        });
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
