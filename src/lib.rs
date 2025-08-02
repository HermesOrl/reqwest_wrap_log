use reqwest::{Client, Method, RequestBuilder, Response, Url, Proxy};
use reqwest::header::HeaderMap;
use reqwest_cookie_store::CookieStoreMutex;
use cookie_store::{CookieStore, Cookie};
use serde::{Deserialize, Serialize};
use serde_json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::error::Error;
use std::time::Instant;
use chrono::{Utc, FixedOffset};
use std::io::Cursor;

// Структуры для данных (с добавлением времен)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RequestData {
    pub method: String,
    pub endpoint: String,
    pub headers: HashMap<String, String>,
    pub body: Option<String>,  // JSON/form/multipart как строка, если возможно; для stream - None
    pub cookies: HashMap<String, String>,  // Куки, отправленные в запросе
    pub request_time: String,  // Время отправки запроса в МСК (RFC3339)
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ResponseData {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: String,
    pub set_cookies: Vec<String>,  // Set-Cookie headers для обновленных куки
    pub response_time: String,  // Время получения ответа в МСК (RFC3339)
    pub duration_ms: u64,  // Длительность выполнения запроса в миллисекундах
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RequestResponseData {
    pub request_data: RequestData,
    pub response_data: Option<ResponseData>,  // Option на случай ошибки
    pub error: Option<String>,  // Если запрос упал
}

// Обертка клиента с новым CookieStore
pub struct TrackedClient {
    pub inner: Client,
    pub collector: Arc<Mutex<HashMap<String, RequestResponseData>>>,
    pub cookie_store: Arc<CookieStoreMutex>,
}

impl TrackedClient {
    pub fn new() -> Self {
        let store = Arc::new(CookieStoreMutex::new(CookieStore::new(None)));
        let client = Client::builder()
            .cookie_provider(store.clone())
            .build()
            .unwrap();

        TrackedClient {
            inner: client,
            collector: Arc::new(Mutex::new(HashMap::new())),
            cookie_store: store,
        }
    }

    pub async fn from_redis_cookies(
        _email: String,
        _password: String,
        proxy: String,
        cookie_json: &str,                   // передаём строку (из кеша!)
    ) -> Result<Self, Box<dyn Error>> {
        // 1. Десериализуем cookies
        let reader = Cursor::new(cookie_json);
        let store = CookieStore::load_json_all(reader).map_err(|e| e as Box<dyn Error>)?;
        let jar = Arc::new(CookieStoreMutex::new(store));

        // 2. Собираем клиент с нужным прокси
        let mut client_builder = Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .cookie_provider(jar.clone())
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36");

        let proxy_http  = Proxy::http(&proxy)?;
        let proxy_https = Proxy::https(&proxy)?;
        client_builder = client_builder.proxy(proxy_http).proxy(proxy_https);

        let client = client_builder.build().map_err(|e| Box::new(e) as Box<dyn Error>)?;

        Ok(TrackedClient {
            inner: client,
            collector: Arc::new(Mutex::new(HashMap::new())),
            cookie_store: jar,
        })
    }

    // Универсальный метод для tracked send: принимает готовый RequestBuilder
    pub async fn tracked_send(&self, key: &str, builder: RequestBuilder) -> Result<(), Box<dyn Error>> {
        let mut req = builder.build()?;

        // Время перед трекингом (примерно время отправки)
        let msk_offset = FixedOffset::east_opt(3 * 3600).unwrap();
        let request_time = Utc::now().with_timezone(&msk_offset).to_rfc3339();

        let method = req.method().as_str().to_string();
        let endpoint = req.url().to_string();
        let headers: HashMap<String, String> = req.headers().iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = req.body().and_then(|b| b.as_bytes()).map(|b| String::from_utf8_lossy(b).to_string());

        // Cookies из store для этого URL
        let url = req.url().clone();
        let cookies_sent: HashMap<String, String> = {
            let store = self.cookie_store.lock().unwrap();
            store.get_request_cookies(&url)
                .map(|cookie| (cookie.name().to_string(), cookie.value().to_string()))
                .collect()
        };

        let data = RequestData {
            method,
            endpoint,
            headers,
            body,
            cookies: cookies_sent,
            request_time,
        };

        let mut collector = self.collector.lock().unwrap();
        collector.insert(key.to_string(), RequestResponseData {
            request_data: data,
            response_data: None,
            error: None,
        });
        drop(collector);  // Освободить lock перед await

        // Измеряем время выполнения
        let start = Instant::now();

        // Отправляем запрос
        let response = self.inner.execute(req).await;

        let duration_ms = start.elapsed().as_millis() as u64;

        let response_time = Utc::now().with_timezone(&msk_offset).to_rfc3339();

        // Теперь обрабатываем ответ и трекаем
        let mut collector = self.collector.lock().unwrap();
        if let Some(entry) = collector.get_mut(key) {
            match response {
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    let headers: HashMap<String, String> = resp.headers().iter()
                        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                        .collect();

                    // Set-Cookie headers для обновленных куки
                    let set_cookies: Vec<String> = resp.headers().get_all("set-cookie")
                        .iter()
                        .map(|v| v.to_str().unwrap_or("").to_string())
                        .collect();

                    let body = resp.text().await.unwrap_or_default();

                    entry.response_data = Some(ResponseData {
                        status,
                        headers,
                        body,
                        set_cookies,
                        response_time,
                        duration_ms,
                    });
                }
                Err(e) => {
                    entry.error = Some(e.to_string());
                    return Err(Box::new(e));
                }
            }
        }

        Ok(())
    }

    // Получить собранные данные и сериализовать
    pub fn get_collected_data(&self) -> String {
        let collector = self.collector.lock().unwrap();
        serde_json::to_string(&*collector).unwrap()
    }

    // Очистить коллектор после отправки
    pub fn clear_collector(&self) {
        let mut collector = self.collector.lock().unwrap();
        collector.clear();
    }
}

// Пример использования в шаге (функция в агрегаторе)
pub async fn example_step(client: &TrackedClient, step_id: &str) -> Result<(), Box<dyn Error>> {
    // Пример: GET с кастом headers
    let get_builder = client.inner.get("https://google.com/api1")
        .header("Custom-Header", "Value")
        .query(&[("param", "value")]);
    client.tracked_send(&format!("http_get_1_on_{}", step_id), get_builder).await?;

    // Пример: POST с JSON
    let json_post_builder = client.inner.post("https://google.com/api2")
        .header("Authorization", "Bearer token")
        .json(&serde_json::json!({"key": "value"}));
    client.tracked_send(&format!("http_post_json_on_{}", step_id), json_post_builder).await?;

    // Пример: POST с form data
    let form_post_builder = client.inner.post("https://google.com/api3")
        .form(&[("field1", "value1"), ("field2", "value2")]);
    client.tracked_send(&format!("http_post_form_on_{}", step_id), form_post_builder).await?;

    // Пример: POST с custom body (например, multipart)
    use reqwest::multipart;
    let part = multipart::Part::text("file content").file_name("file.txt").mime_str("text/plain")?;
    let form = multipart::Form::new().part("file", part);
    let multipart_builder = client.inner.post("https://google.com/upload")
        .multipart(form);
    client.tracked_send(&format!("http_post_multipart_on_{}", step_id), multipart_builder).await?;

    // После всех запросов — сериализуем и отправляем в ваш API
    let json_data = client.get_collected_data();
    // Здесь отправьте json_data в ваш веб API, например:
    // client.inner.post("your-api-url").body(json_data).send().await?;
    println!("Collected: {}", json_data);  // Для примера

    client.clear_collector();  // Очистить для следующего шага

    Ok(())
}
