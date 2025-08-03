use reqwest::{Client, Method, RequestBuilder, Response, Url, Proxy};
use reqwest::header::HeaderMap;
use reqwest_cookie_store::CookieStoreMutex;
use cookie_store::{CookieStore, Cookie};
use serde::{Deserialize, Serialize};
use serde_json::{self, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use chrono::{Utc, FixedOffset};
use std::io::Cursor;
use anyhow::{anyhow, Result};

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
struct RequestData {
    method: String,
    endpoint: String,
    headers: HashMap<String, String>,
    body: Option<String>,  // JSON/form/multipart как строка, если возможно; для stream - None
    cookies: HashMap<String, String>,  // Куки, отправленные в запросе
    request_time: String,  // Время отправки запроса в МСК (RFC3339)
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ResponseData {
    status: u16,
    headers: HashMap<String, String>,
    body: String,
    set_cookies: Vec<String>,  // Set-Cookie headers для обновленных куки
    response_time: String,  // Время получения ответа в МСК (RFC3339)
    duration_ms: u64,  // Длительность выполнения запроса в миллисекундах
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct RequestResponseData {
    request_data: RequestData,
    response_data: Option<ResponseData>,
    error: Option<String>,
    cookies: Option<String>,  // JSON-массив куки после запроса
}

// Обертка клиента с новым CookieStore
pub struct TrackedClient {
    inner: Client,
    collector: Arc<Mutex<HashMap<String, RequestResponseData>>>,
    cookie_store: Arc<CookieStoreMutex>,
}

impl TrackedClient {
    pub fn new() -> Result<Self> {
        let store = Arc::new(CookieStoreMutex::new(CookieStore::new(None)));
        let client = Client::builder()
            .cookie_provider(store.clone())
            .build()?;

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
        // Десериализуем cookies
        let reader = Cursor::new(cookie_json);
        let store_inner = CookieStore::load_json_all(reader)
            .map_err(|e| anyhow!("Failed to load cookies JSON: {}", e))?;
        let jar = Arc::new(CookieStoreMutex::new(store_inner));

        // Собираем клиент с прокси
        let proxy_http = Proxy::http(&proxy)?;
        let proxy_https = Proxy::https(&proxy)?;
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .cookie_provider(jar.clone())
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
            .proxy(proxy_http)
            .proxy(proxy_https)
            .build()?;

        Ok(TrackedClient {
            inner: client,
            collector: Arc::new(Mutex::new(HashMap::new())),
            cookie_store: jar,
        })
    }

    pub async fn new_basic(
        proxy: String,
        jar:            Arc<CookieStoreMutex>,
    ) -> Result<Self> {
        // Десериализуем cookies
        let proxy_http = Proxy::http(&proxy)?;
        let proxy_https = Proxy::https(&proxy)?;
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .cookie_provider(jar.clone())
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
            .proxy(proxy_http)
            .proxy(proxy_https)
            .build()?;

        Ok(TrackedClient {
            inner: client,
            collector: Arc::new(Mutex::new(HashMap::new())),
            cookie_store: jar,
        })
    }
    
    pub fn dump_cookies(&self) -> Result<String> {
        let store = self.cookie_store.lock().map_err(|e| anyhow!("Lock error: {}", e))?;

        // Сохраняем все куки в JSON-буфер
        let mut buf: Vec<u8> = Vec::new();
        store.save_incl_expired_and_nonpersistent_json(&mut buf)
            .map_err(|e| anyhow!("save_json failed: {}", e))?;

        let raw = String::from_utf8(buf).map_err(|e| anyhow!("UTF-8 error: {}", e))?;
        // Разбиваем по строкам и собираем единый JSON-массив
        let mut arr: Vec<Value> = Vec::new();
        for line in raw.lines() {
            if line.trim().is_empty() { continue; }
            let v: Value = serde_json::from_str(line)
                .map_err(|e| anyhow!("Invalid cookie JSON line: {}", e))?;
            arr.push(v);
        }
        Ok(serde_json::to_string(&arr)?)
    }

    pub async fn tracked_send(&self, key: &str, builder: RequestBuilder) -> Result<()> {
        let mut req = builder.build()?;
        let msk = FixedOffset::east_opt(3 * 3600).unwrap();
        let request_time = Utc::now().with_timezone(&msk).to_rfc3339();

        let method = req.method().as_str().to_string();
        let endpoint = req.url().to_string();
        let headers = req.headers().iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = req.body().and_then(|b| b.as_bytes()).map(|b| String::from_utf8_lossy(b).to_string());

        let url = req.url().clone();
        let cookies_sent = {
            let store = self.cookie_store.lock().unwrap();
            store.get_request_cookies(&url)
                .map(|c| (c.name().to_string(), c.value().to_string()))
                .collect()
        };

        let data = RequestData { method, endpoint, headers, body, cookies: cookies_sent, request_time };
        {
            let mut coll = self.collector.lock().unwrap();
            coll.insert(key.to_string(), RequestResponseData {
                request_data: data,
                response_data: None,
                error: None,
                cookies: None,
            });
        }

        let start = Instant::now();
        let response = self.inner.execute(req).await;
        let duration_ms = start.elapsed().as_millis() as u64;
        let response_time = Utc::now().with_timezone(&msk).to_rfc3339();

        let mut coll = self.collector.lock().unwrap();
        if let Some(entry) = coll.get_mut(key) {
            match response {
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    let headers = resp.headers().iter()
                        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                        .collect();
                    let set_cookies = resp.headers().get_all("set-cookie")
                        .iter().map(|v| v.to_str().unwrap_or("").to_string()).collect();
                    let body = resp.text().await.unwrap_or_default();

                    entry.response_data = Some(ResponseData { status, headers, body, set_cookies, response_time, duration_ms });
                }
                Err(e) => {
                    entry.error = Some(e.to_string());
                    return Err(anyhow!(e));
                }
            }
            entry.cookies = Some(self.dump_cookies()?);
        }
        Ok(())
    }

    pub fn get_collected_data(&self) -> String {
        let coll = self.collector.lock().unwrap();
        serde_json::to_string(&*coll).unwrap()
    }

    pub fn get_pretty_truncated_data(&self) -> String {
        let json_data = self.get_collected_data();
        let mut data: Value = serde_json::from_str(&json_data).unwrap();

        fn truncate_fields(value: &mut Value) {
            match value {
                Value::Object(map) => {
                    for v in map.values_mut() { truncate_fields(v); }
                    if let Some(Value::Object(hdrs)) = map.get_mut("headers") {
                        for inner in hdrs.values_mut() {
                            if let Value::String(s) = inner { *s = truncate(s, 50); }
                        }
                    }
                    if let Some(Value::String(s)) = map.get_mut("cookies") {
                        *s = truncate(s, 500);
                    }
                    if let Some(Value::Array(arr)) = map.get_mut("set_cookies") {
                        for item in arr { if let Value::String(s) = item { *s = truncate(s, 50); } }
                    }
                }
                Value::Array(arr) => { for v in arr { truncate_fields(v); } }
                _ => {}
            }
        }

        truncate_fields(&mut data);
        serde_json::to_string_pretty(&data).unwrap()
    }

    pub fn clear_collector(&self) {
        let mut coll = self.collector.lock().unwrap();
        coll.clear();
    }
}

// Пример использования
pub async fn example_step(client: &TrackedClient, step_id: &str) -> Result<()> {
    let builder = client.inner.get("https://httpbin.org/cookies/set?test=1");
    client.tracked_send(&format!("step_{}", step_id), builder).await?;

    let json_data = client.get_collected_data();
    println!("Collected: {}", client.get_pretty_truncated_data());
    client.clear_collector();
    Ok(())
}
