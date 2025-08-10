use reqwest::{Client, RequestBuilder, Proxy, Response};
use reqwest_cookie_store::CookieStoreMutex;
use cookie_store::CookieStore;
use serde::{Deserialize, Serialize};
use serde_json::{self, Value};
use std::sync::Arc;
use std::collections::HashMap;
use std::time::Instant;
use chrono::{Utc, FixedOffset};
use std::io::Cursor;
use anyhow::{anyhow, Context, Result};
use tokio::sync::Mutex;

// Функция для усечения строки до max символов
fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[0..max-3])
    } else {
        s.to_string()
    }
}

// Структуры для данных (с добавлением времен)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RequestData {
    pub method: String,
    pub endpoint: String,
    pub headers: HashMap<String, String>,
    pub body: Option<String>,
    pub cookies: HashMap<String, String>,
    pub request_time: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ResponseData {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: String,
    pub set_cookies: Vec<String>,
    pub response_time: String,
    pub duration_ms: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RequestResponseData {
    pub request_data: RequestData,
    pub response_data: Option<ResponseData>,
    pub error: Option<String>,
    pub cookies: Option<String>,
}

pub struct TrackedClient {
    pub inner: Client,
    pub collector: Arc<Mutex<HashMap<String, RequestResponseData>>>,
    pub cookie_store: Arc<CookieStoreMutex>,
}

impl TrackedClient {
    pub fn new() -> Result<Self> {
        let store = Arc::new(CookieStoreMutex::new(CookieStore::new(None)));
        let client = Client::builder()
            .cookie_provider(store.clone())
            .build()
            .context("Failed to build HTTP client")?;

        Ok(TrackedClient {
            inner: client,
            collector: Arc::new(Mutex::new(HashMap::new())),
            cookie_store: store,
        })
    }

    pub async fn from_redis_cookies(
        proxy: String,
        cookie_json: &str,
    ) -> Result<Self> {
        let reader = Cursor::new(cookie_json);
        let store_inner = CookieStore::load_json_all(reader)
            .map_err(|e| anyhow!("Failed to load cookies JSON: {}", e))?;
        let jar = Arc::new(CookieStoreMutex::new(store_inner));

        let proxy_http = Proxy::http(&proxy)
            .context("Invalid HTTP proxy URL")?;
        let proxy_https = Proxy::https(&proxy)
            .context("Invalid HTTPS proxy URL")?;
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .cookie_provider(jar.clone())
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
            .proxy(proxy_http)
            .proxy(proxy_https)
            .build()
            .context("Failed to build HTTP client with proxy")?;

        Ok(TrackedClient {
            inner: client,
            collector: Arc::new(Mutex::new(HashMap::new())),
            cookie_store: jar,
        })
    }

    pub async fn new_basic(
        proxy: String,
        jar: Arc<CookieStoreMutex>,
    ) -> Result<Self> {
        let proxy_http = Proxy::http(&proxy)
            .context("Invalid HTTP proxy URL")?;
        let proxy_https = Proxy::https(&proxy)
            .context("Invalid HTTPS proxy URL")?;
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .cookie_provider(jar.clone())
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
            .proxy(proxy_http)
            .proxy(proxy_https)
            .build()
            .context("Failed to build basic HTTP client with proxy")?;

        Ok(TrackedClient {
            inner: client,
            collector: Arc::new(Mutex::new(HashMap::new())),
            cookie_store: jar,
        })
    }

    pub fn dump_cookies(&self) -> Result<String> {
        let store = self.cookie_store
            .lock()
            .map_err(|e| anyhow!("Cookie store lock error: {}", e))?;

        let mut buf: Vec<u8> = Vec::new();
        store
            .save_incl_expired_and_nonpersistent_json(&mut buf)
            .map_err(|e| anyhow!("Failed to save cookies to JSON buffer: {}", e))?;

        let raw = String::from_utf8(buf)
            .context("Failed to convert cookie buffer to UTF-8 string")?;

        let mut arr: Vec<Value> = Vec::new();
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(line)
                .context("Invalid cookie JSON line format")?;
            arr.push(v);
        }
        serde_json::to_string(&arr)
            .context("Failed to serialize cookies array to string")
    }

    // теперь возвращает ResponseData для дальнейшего использования
    pub async fn tracked_send(&self, key: &str, builder: RequestBuilder) -> Result<Response> {
        // === NEW: пробуем заранее сделать клон билдерa, чтобы потом вернуть «свежий» Response ===
        let builder_for_return = builder.try_clone();

        // --- подготовка RequestData как раньше ---
        let mut req = builder
            .build()
            .context("Failed to build request")?;
        let msk = FixedOffset::east_opt(3 * 3600).unwrap();
        let request_time = Utc::now().with_timezone(&msk).to_rfc3339();
        let method = req.method().as_str().to_string();
        let endpoint = req.url().to_string();
        let headers: HashMap<_, _> = req
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = req
            .body()
            .and_then(|b| b.as_bytes())
            .map(|b| String::from_utf8_lossy(b).to_string());
        // куки, отправленные в запросе
        let url = req.url().clone();
        let cookies_sent = {
            let store = self.cookie_store
                .lock()
                .map_err(|e| anyhow!("Cookie store lock error: {}", e))?;
            store.get_request_cookies(&url)
                .map(|c| (c.name().to_string(), c.value().to_string()))
                .collect()
        };
        let req_data = RequestData { method, endpoint, headers, body, cookies: cookies_sent, request_time };
        {
            let mut coll = self.collector.lock().await;
            coll.insert(
                key.to_string(),
                RequestResponseData {
                    request_data: req_data,
                    response_data: None,
                    error: None,
                    cookies: None,
                },
            );
        }

        // --- отправка и измерение времени ---
        let start = Instant::now();
        let resp = match self.inner.execute(req).await {
            Ok(r) => r,
            Err(e) => {
                // залогируем ошибку
                let mut coll = self.collector.lock().await;
                if let Some(ent) = coll.get_mut(key) {
                    ent.error = Some(e.to_string());
                }
                return Err(anyhow!("Request execution failed: {}", e));
            }
        };
        let duration_ms = start.elapsed().as_millis() as u64;
        let response_time = Utc::now().with_timezone(&msk).to_rfc3339();

        // --- сбор метаданных ответа ---
        let status = resp.status().as_u16();
        let resp_headers: HashMap<_, _> = resp.headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let set_cookies: Vec<_> = resp
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_string))
            .collect();

        // === NEW: если можем заново отправить такой же запрос — читаем body сейчас и логируем ===
        if let Some(builder2) = builder_for_return {
            // Читаем body для логирования
            let body_bytes = resp.bytes().await.unwrap_or_default();
            let body_string = String::from_utf8_lossy(&body_bytes).to_string();

            {
                let mut coll = self.collector.lock().await;
                if let Some(ent) = coll.get_mut(key) {
                    ent.response_data = Some(ResponseData {
                        status,
                        headers: resp_headers,
                        body: body_string,
                        set_cookies,
                        response_time: response_time.clone(),
                        duration_ms,
                    });
                    ent.cookies = Some(self.dump_cookies()?);
                }
            }

            // Возвращаем «родной» Response вторым запросом (идемпотентные запросы — ОК)
            let req2 = builder2.build().context("Failed to build cloned request")?;
            let resp2 = self.inner.execute(req2).await
                .context("Failed to re-execute request for returned Response")?;
            return Ok(resp2);
        }

        // === Fallback (как раньше): не можем клонировать запрос — не читаем body, чтобы не сломать вызывающего ===
        {
            let mut coll = self.collector.lock().await;
            if let Some(ent) = coll.get_mut(key) {
                ent.response_data = Some(ResponseData {
                    status,
                    headers: resp_headers,
                    body: String::new(), // тут body НЕ читаем, иначе «съедим» поток
                    set_cookies,
                    response_time: response_time.clone(),
                    duration_ms,
                });
                ent.cookies = Some(self.dump_cookies()?);
            }
        }

        Ok(resp)
    }

    pub async fn get_collected_data(&self) -> Result<String> {
        let coll = self.collector.lock().await;
        serde_json::to_string(&*coll).context("Failed to serialize collected data")
    }

    pub async fn get_pretty_truncated_data(&self) -> Result<String> {
        let raw = self.get_collected_data().await?;
        let mut data: Value = serde_json::from_str(&raw).context("Failed to parse collected JSON")?;

        fn truncate_fields(value: &mut Value) {
            match value {
                Value::Object(map) => {
                    for v in map.values_mut() {
                        truncate_fields(v);
                    }
                    if let Some(Value::Object(hdrs)) = map.get_mut("headers") {
                        for inner in hdrs.values_mut() {
                            if let Value::String(s) = inner {
                                *s = truncate(s, 50);
                            }
                        }
                    }
                    if let Some(Value::String(s)) = map.get_mut("cookies") {
                        *s = truncate(s, 500);
                    }
                    if let Some(Value::Array(arr)) = map.get_mut("set_cookies") {
                        for item in arr {
                            if let Value::String(s) = item {
                                *s = truncate(s, 50);
                            }
                        }
                    }
                }
                Value::Array(arr) => {
                    for v in arr {
                        truncate_fields(v);
                    }
                }
                _ => {}
            }
        }

        truncate_fields(&mut data);
        serde_json::to_string_pretty(&data).context("Failed to serialize pretty truncated data")
    }
    pub async fn take_collected_data(&self) -> anyhow::Result<String> {
        let mut coll = self.collector.lock().await;
        let s = serde_json::to_string(&*coll)?;
        coll.clear();
        Ok(s)
    }

    pub async fn clear_collector(&self) {
        let mut coll = self.collector.lock().await;
        coll.clear();
    }
}

pub async fn example_step(client: &TrackedClient, step_id: &str) -> Result<()> {
    let mut map = HashMap::new();
    map.insert("email", "sdfsdf".clone());
    let builder = client.inner.get("https://httpbin.org/ip").form(&map);
    let resp = client.tracked_send(&format!("step_{}", step_id), builder).await?;
    println!("Response status: {}", resp.status());
    let pretty = client.get_pretty_truncated_data().await?;
    let default = client.get_collected_data().await?;
    println!("Collected: {}", pretty);
    println!("Collected2: {}", default);
    client.clear_collector().await;
    Ok(())
}

