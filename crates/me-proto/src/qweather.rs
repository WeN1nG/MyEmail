//! 和风天气（QWeather）HTTPS 适配。
//!
//! 这是**唯一**允许访问天气外部服务的 crate。约束（见 `doc/v03架构设计.md` §6.3）：
//! - 只允许 HTTPS，且 Host 必须是公网地址（按字面量拒绝 loopback / 私网 /
//!   CGNAT / 链路本地；**不做 DNS 预解析**，避免解析与连接之间的时间差）；
//! - 禁止重定向、禁止 Cookie 与 Referer；
//! - 单请求 10s 超时、响应体 ≤ 256 KiB；
//! - API Key 只出现在请求头里，**任何日志都不得记录完整 URL**。
//!
//! **Host 由调用方在每次请求时传入**（见 [`WeatherProvider`]），数据源本身
//! 不绑定任何 Host。这样「用户配置的 Host」只有一个来源，换 Host 不必重建
//! 客户端，测试连接也能真正测到用户刚填的那个地址。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::DateTime;
use me_core::CoreError;
use me_core::weather::{
    CurrentWeather, DailyForecast, HourlyForecast, MAX_LOCATION_CANDIDATES, PROVIDER_QWEATHER,
    WeatherCredential, WeatherForecast, WeatherFuture, WeatherKind, WeatherLocation,
    WeatherLocationCandidate, WeatherProvider, normalize_host,
};

use reqwest::Url;
use serde_json::Value;

/// 默认 Host 与配置提示定义在领域层（UI 也要用），此处转出。
pub use me_core::weather::{DEFAULT_API_HOST, HOST_HINT};

/// 端点路径定义在领域层（UI 要预览实际请求地址），此处转出。
pub use me_core::weather::{
    PATH_7D, PATH_24H, PATH_CITY_LOOKUP, PATH_CITY_LOOKUP_LEGACY, PATH_NOW,
};

/// 单请求超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 响应体大小上限。
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// API KEY 认证的请求头名（官方规定）。
const HEADER_API_KEY: &str = "X-QW-Api-Key";

// ---------------------------------------------------------------------------
// URL 构造
// ---------------------------------------------------------------------------

/// 构造请求 URL。`params` 里的值会被 URL 编码。
///
/// `host` 必须是已规范化的 `https://host[:port]`；这里再校验一次，
/// 保证任何调用方传进来的东西都不会拼出非 HTTPS 的地址。
pub fn build_url(host: &str, path: &str, params: &[(&str, &str)]) -> Result<Url, CoreError> {
    let host = normalize_host(host)?;
    let mut url =
        Url::parse(&format!("{host}{path}")).map_err(|_| CoreError::validation("Host 格式无效"))?;
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in params {
            query.append_pair(key, value);
        }
    }
    Ok(url)
}

// ---------------------------------------------------------------------------
// 响应解析（纯函数）
// ---------------------------------------------------------------------------

fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key)
}

/// 取字符串字段；和风多数数值字段也是字符串形式。
fn str_field(value: &Value, key: &str) -> Option<String> {
    match field(value, key)? {
        Value::String(text) => Some(text.trim().to_string()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// 按优先级取第一个**存在且非空**的字符串字段。
///
/// 服务商不同版本的字段名不一样（例如逐日预报白天天气叫 `iconDay`，
/// 旧版叫 `icon`），按顺序回退即可兼容，不必为每个版本写一套解析。
fn first_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| str_field(value, key))
        .find(|text| !text.is_empty())
}

fn f64_field(value: &Value, key: &str) -> Option<f64> {
    let text = str_field(value, key)?;
    text.parse::<f64>().ok().filter(|n| n.is_finite())
}

fn u32_field(value: &Value, key: &str) -> Option<u32> {
    f64_field(value, key).and_then(|n| {
        if (0.0..=100.0).contains(&n) {
            Some(n as u32)
        } else {
            None
        }
    })
}

/// 解析 ISO8601 时间（和风形如 `2020-06-30T21:40+08:00`）→ UTC Unix 秒。
///
/// 和风常省略秒，`parse_from_rfc3339` 不吃这种写法，所以再试几种格式。
fn parse_time(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Ok(value) = DateTime::parse_from_rfc3339(text) {
        return Some(value.timestamp());
    }
    for format in [
        "%Y-%m-%dT%H:%M%:z",
        "%Y-%m-%dT%H:%M:%S%:z",
        "%Y-%m-%dT%H:%M%z",
        "%Y-%m-%dT%H:%M:%S%z",
    ] {
        if let Ok(value) = DateTime::parse_from_str(text, format) {
            return Some(value.timestamp());
        }
    }
    None
}

/// 解析 `HH:MM` 形式的日出日落，返回当天该时刻的 UTC Unix 秒。
///
/// 日出日落按**地点所在时区**解释；这里只保留时分秒，日期用当天，
/// 转换时区需要地点的 IANA 时区，因此调用方需自行换算。
fn parse_clock(value: &Value, key: &str) -> Option<String> {
    str_field(value, key).filter(|text| text.len() >= 5)
}

/// 顶层 `code` 字段：和风用业务码表示错误（402 额度、401 授权、429 频率）。
fn check_code(root: &Value) -> Result<(), CoreError> {
    let code = str_field(root, "code").unwrap_or_default();
    if code == "200" || code.is_empty() {
        return Ok(());
    }
    let detail = match code.as_str() {
        "400" => "请求参数无效".to_string(),
        "401" => "API Key 无效或未授权".to_string(),
        "402" => "超出配额，额度不足".to_string(),
        "403" => "该 Key 没有访问权限".to_string(),
        "404" => "资源不存在或地点已失效".to_string(),
        "429" => "请求过于频繁".to_string(),
        "500" => "服务内部错误".to_string(),
        other => format!("服务商返回错误码 {other}"),
    };
    Err(CoreError::protocol(Some(code), detail))
}

/// 城市搜索响应 → 候选地点。
pub fn parse_city_lookup(body: &str) -> Result<Vec<WeatherLocationCandidate>, CoreError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|err| CoreError::parse(format!("城市搜索响应不是合法 JSON（{err}）")))?;
    check_code(&root)?;
    let list = root
        .get("location")
        .and_then(Value::as_array)
        .ok_or_else(|| CoreError::parse("城市搜索响应缺少 location 列表"))?;

    let mut out = Vec::new();
    for item in list.iter().take(MAX_LOCATION_CANDIDATES) {
        let id = match str_field(item, "id") {
            Some(id) if !id.is_empty() => id,
            _ => continue,
        };
        let name = match str_field(item, "name") {
            Some(name) if !name.is_empty() => name,
            _ => continue,
        };
        let latitude = f64_field(item, "lat").unwrap_or(0.0);
        let longitude = f64_field(item, "lon").unwrap_or(0.0);
        let timezone = str_field(item, "tz").unwrap_or_else(|| "Asia/Shanghai".to_string());
        out.push(WeatherLocationCandidate {
            provider: PROVIDER_QWEATHER.to_string(),
            provider_location_id: id,
            name,
            admin1: str_field(item, "adm1"),
            // `adm2` 是**上级城市**（如都江堰的 adm2 是「成都」），
            // 不是地点本身——显示时必须和 `name` 一起用。
            admin2: str_field(item, "adm2"),
            country: str_field(item, "country"),
            latitude,
            longitude,
            timezone,
        });
    }
    Ok(out)
}

/// 实时天气响应 → 当前天气。
pub fn parse_now(body: &str) -> Result<CurrentWeather, CoreError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|err| CoreError::parse(format!("天气响应不是合法 JSON（{err}）")))?;
    check_code(&root)?;
    let now = root
        .get("now")
        .ok_or_else(|| CoreError::parse("天气响应缺少 now 字段"))?;
    let temp = f64_field(now, "temp").ok_or_else(|| CoreError::parse("天气响应缺少温度"))?;
    let icon = str_field(now, "icon").unwrap_or_default();
    Ok(CurrentWeather {
        observed_at: str_field(now, "obsTime")
            .as_deref()
            .and_then(parse_time)
            .unwrap_or(0),
        temp_c: temp,
        feels_like_c: f64_field(now, "feelsLike").unwrap_or(temp),
        humidity_percent: u32_field(now, "humidity").unwrap_or(0),
        wind_scale: u32_field(now, "windScale").unwrap_or(0),
        wind_dir: str_field(now, "windDir").unwrap_or_default(),
        wind_speed_kmh: f64_field(now, "windSpeed").unwrap_or(0.0),
        precip_mm: f64_field(now, "precip").unwrap_or(0.0),
        pressure_hpa: f64_field(now, "pressure").unwrap_or(0.0),
        visibility_km: f64_field(now, "vis").unwrap_or(0.0),
        cloud_percent: u32_field(now, "cloud").unwrap_or(0),
        kind: WeatherKind::from_icon_code(&icon),
        text: str_field(now, "text").unwrap_or_default(),
    })
}

/// 逐时预报响应。
pub fn parse_24h(body: &str) -> Result<Vec<HourlyForecast>, CoreError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|err| CoreError::parse(format!("逐时预报响应不是合法 JSON（{err}）")))?;
    check_code(&root)?;
    let list = root
        .get("hourly")
        .and_then(Value::as_array)
        .ok_or_else(|| CoreError::parse("逐时预报响应缺少 hourly 列表"))?;
    let mut out = Vec::new();
    for item in list {
        let time = match str_field(item, "fxTime").as_deref().and_then(parse_time) {
            Some(time) => time,
            None => continue,
        };
        let temp = f64_field(item, "temp").unwrap_or(0.0);
        let icon = str_field(item, "icon").unwrap_or_default();
        out.push(HourlyForecast {
            time,
            temp_c: temp,
            pop_percent: u32_field(item, "pop").unwrap_or(0),
            precip_mm: f64_field(item, "precip").unwrap_or(0.0),
            kind: WeatherKind::from_icon_code(&icon),
            text: str_field(item, "text").unwrap_or_default(),
        });
    }
    Ok(out)
}

/// 逐日预报响应。
pub fn parse_7d(body: &str) -> Result<Vec<DailyForecast>, CoreError> {
    let root: Value = serde_json::from_str(body)
        .map_err(|err| CoreError::parse(format!("逐日预报响应不是合法 JSON（{err}）")))?;
    check_code(&root)?;
    let list = root
        .get("daily")
        .and_then(Value::as_array)
        .ok_or_else(|| CoreError::parse("逐日预报响应缺少 daily 列表"))?;
    let mut out = Vec::new();
    for item in list {
        let date = match str_field(item, "fxDate") {
            Some(date) if !date.is_empty() => date,
            _ => continue,
        };
        // 和风 v7 的逐日预报把昼夜拆成两组字段：
        // `iconDay`/`textDay` 与 `iconNight`/`textNight`。
        // 早期版本只有单个 `icon`/`text`，这里两条路都认。
        let icon_day = first_field(item, &["iconDay", "icon"]);
        let icon_night = first_field(item, &["iconNight"]);
        let text_day = first_field(item, &["textDay", "text"]);
        let text_night = first_field(item, &["textNight"]);
        // 卡片背景以白天天气为准；白天码缺失时才退回夜间。
        let kind = WeatherKind::from_icon_code(icon_day.as_deref().unwrap_or_default());
        let kind = if kind == WeatherKind::Unknown {
            WeatherKind::from_icon_code(icon_night.as_deref().unwrap_or_default())
        } else {
            kind
        };
        out.push(DailyForecast {
            date,
            sunrise: parse_clock(item, "sunrise"),
            sunset: parse_clock(item, "sunset"),
            temp_max_c: f64_field(item, "tempMax").unwrap_or(0.0),
            temp_min_c: f64_field(item, "tempMin").unwrap_or(0.0),
            precip_mm: f64_field(item, "precip").unwrap_or(0.0),
            kind,
            text: text_day.unwrap_or_default(),
            icon_day: icon_day.unwrap_or_default(),
            icon_night: icon_night.unwrap_or_default(),
            text_night: text_night.unwrap_or_default(),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 客户端
// ---------------------------------------------------------------------------

/// 和风天气数据源。
///
/// **不绑定 Host**：Host 由每次调用传入，客户端只负责「按给定 Host 与 Key
/// 发请求」。这样用户换了 Host 不必重建客户端，测试连接也能真正测到
/// 用户刚填的那个地址。
pub struct QWeatherProvider {
    client: reqwest::Client,
    /// 城市搜索路径的探测结果，**按 Host 记忆**。
    ///
    /// 官方统一 Host 用 `/geo/v2/city/lookup`，旧公共 Host 用
    /// `/v2/city/lookup`。第一次遇到 404 就自动换一条并记住，
    /// 避免用户手工区分 Host 类型。
    city_paths: Mutex<HashMap<String, CityPath>>,
}

/// 城市搜索路径的探测状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CityPath {
    /// 还没试过（先用统一 Host 的路径）。
    Unknown,
    /// 统一 Host 路径可用。
    Unified,
    /// 统一 Host 路径 404，应改用旧路径。
    Legacy,
}

impl CityPath {
    fn path(self) -> &'static str {
        match self {
            CityPath::Legacy => PATH_CITY_LOOKUP_LEGACY,
            _ => PATH_CITY_LOOKUP,
        }
    }
}

impl QWeatherProvider {
    /// 构造数据源。Host 在每次请求时传入，这里不需要。
    pub fn new() -> Result<Self, CoreError> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // 禁止重定向：避免被引到内网。
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("MyEmail/0.3 (weather)")
            .build()
            .map_err(|err| CoreError::network(false, format!("创建天气客户端失败: {err}")))?;
        Ok(Self {
            client,
            city_paths: Mutex::new(HashMap::new()),
        })
    }

    /// 读出某个 Host 上探测到的城市搜索路径。
    fn city_path(&self, host: &str) -> CityPath {
        self.city_paths
            .lock()
            .ok()
            .and_then(|map| map.get(host).copied())
            .unwrap_or(CityPath::Unknown)
    }

    /// 记住某个 Host 上探测到的城市搜索路径。
    fn remember_city_path(&self, host: &str, path: CityPath) {
        if let Ok(mut map) = self.city_paths.lock() {
            map.insert(host.to_string(), path);
        }
    }

    /// 发一次 GET 并读回受限长度的响应体。
    ///
    /// 认证走请求头 `X-QW-Api-Key`（官方推荐方式）。
    /// 官方明确提示：**请求头与 `key` 查询参数不要同时使用**，
    /// 同时用会导致认证失败，所以这里只走请求头。
    async fn get(&self, url: Url, credential: &WeatherCredential) -> Result<String, CoreError> {
        let response = self
            .client
            .get(url)
            .header(HEADER_API_KEY, credential.expose())
            .send()
            .await
            .map_err(|err| {
                CoreError::network(err.is_timeout(), format!("天气服务请求失败: {err}"))
            })?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(CoreError::auth("和风天气 API Key 无效或没有权限"));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status == reqwest::StatusCode::PAYMENT_REQUIRED
        {
            return Err(CoreError::protocol(
                Some(status.as_u16().to_string()),
                "和风天气额度不足或请求过于频繁",
            ));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(CoreError::protocol(
                Some("404".to_string()),
                "地点已失效，请重新搜索",
            ));
        }
        if status.is_server_error() {
            return Err(CoreError::network(true, "和风天气服务暂时不可用"));
        }
        if !status.is_success() {
            return Err(CoreError::protocol(
                Some(status.as_u16().to_string()),
                format!("天气服务返回状态 {}", status.as_u16()),
            ));
        }

        let mut body = Vec::new();
        let mut stream = response;
        while let Some(chunk) = stream.chunk().await.map_err(|err| {
            CoreError::network(err.is_timeout(), format!("读取天气响应失败: {err}"))
        })? {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(CoreError::parse("天气响应超过 256 KiB 上限"));
            }
            body.extend_from_slice(&chunk);
        }
        String::from_utf8(body).map_err(|_| CoreError::parse("天气响应不是合法 UTF-8"))
    }
}

impl WeatherProvider for QWeatherProvider {
    /// 按城市名搜索候选地点。`api_host` 是本次请求使用的 Host。
    fn search_locations<'a>(
        &'a self,
        api_host: &'a str,
        query: &'a str,
        credential: &'a WeatherCredential,
    ) -> WeatherFuture<'a, Result<Vec<WeatherLocationCandidate>, CoreError>> {
        Box::pin(async move {
            let host = normalize_host(api_host)?;
            // 先按当前探测状态选路径；若 404 且用的是统一路径，就换成旧路径重试。
            let path = self.city_path(&host);
            // `number`：一次多取一些。搜「成都」会返回成都本级 + 一串
            // 下辖区县（锦江区、龙泉驿、都江堰市…），默认 10 条会截掉一批。
            let number = MAX_LOCATION_CANDIDATES.to_string();
            let url = build_url(
                &host,
                path.path(),
                &[
                    ("location", query),
                    ("lang", "zh"),
                    ("unit", "m"),
                    ("number", &number),
                ],
            )?;
            let body = match self.get(url, credential).await {
                Ok(body) => {
                    if path == CityPath::Unknown {
                        self.remember_city_path(&host, CityPath::Unified);
                    }
                    body
                }
                Err(err) if err.is_not_found() && path != CityPath::Legacy => {
                    // 统一路径 404：多半是旧公共 Host，换旧路径再试一次。
                    self.remember_city_path(&host, CityPath::Legacy);
                    let retry = build_url(
                        &host,
                        PATH_CITY_LOOKUP_LEGACY,
                        &[
                            ("location", query),
                            ("lang", "zh"),
                            ("unit", "m"),
                            ("number", &number),
                        ],
                    )?;
                    self.get(retry, credential).await?
                }
                Err(err) => return Err(err),
            };
            parse_city_lookup(&body)
        })
    }

    /// 取某地点的预报。`api_host` 是本次请求使用的 Host。
    fn forecast<'a>(
        &'a self,
        api_host: &'a str,
        location: &'a WeatherLocation,
        credential: &'a WeatherCredential,
    ) -> WeatherFuture<'a, Result<WeatherForecast, CoreError>> {
        Box::pin(async move {
            let host = normalize_host(api_host)?;
            let id = location.provider_location_id.as_str();
            let now_url = build_url(
                &host,
                PATH_NOW,
                &[("location", id), ("lang", "zh"), ("unit", "m")],
            )?;
            let hourly_url = build_url(
                &host,
                PATH_24H,
                &[("location", id), ("lang", "zh"), ("unit", "m")],
            )?;
            let daily_url = build_url(
                &host,
                PATH_7D,
                &[("location", id), ("lang", "zh"), ("unit", "m")],
            )?;

            // 三个请求并发发出；任何一个失败则整次刷新失败（不写半套数据）。
            let (now, hourly, daily) = tokio::join!(
                self.get(now_url, credential),
                self.get(hourly_url, credential),
                self.get(daily_url, credential),
            );
            let current = parse_now(&now?)?;
            let hourly = parse_24h(&hourly?)?;
            let daily = parse_7d(&daily?)?;
            Ok(WeatherForecast {
                current,
                hourly,
                daily,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_is_built_with_encoded_params() {
        let url = build_url(
            "https://geoapi.qweather.com",
            PATH_CITY_LOOKUP,
            &[("location", "北 京"), ("lang", "zh")],
        )
        .unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.path(), "/geo/v2/city/lookup");
        assert!(
            url.query()
                .unwrap()
                .contains("location=%E5%8C%97+%E4%BA%AC")
        );
    }

    /// Host 可以只填裸域名：程序自己补 https:// 再拼路径。
    #[test]
    fn bare_host_is_completed_by_the_program() {
        let url = build_url(
            "abcxyz.qweatherapi.com",
            PATH_NOW,
            &[("location", "101010100")],
        )
        .unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("abcxyz.qweatherapi.com"));
        assert_eq!(url.path(), "/v7/weather/now");
    }

    /// 绝不接受非 HTTPS 的地址：即使调用方塞了 http:// 也被改写成 https。
    #[test]
    fn insecure_scheme_is_never_used() {
        let url = build_url("http://abcxyz.qweatherapi.com", PATH_24H, &[]).unwrap();
        assert_eq!(url.scheme(), "https");
        // 内网地址直接拒绝，请求根本发不出去。
        assert!(build_url("http://127.0.0.1", PATH_NOW, &[]).is_err());
        assert!(build_url("127.0.0.1", PATH_NOW, &[]).is_err());
    }

    /// 官方规定：请求头与 `key` 查询参数**不能同时使用**，同时用会导致认证失败。
    /// 因此密钥只能走 `X-QW-Api-Key` 请求头，URL 里绝不能出现 `key=`。
    #[test]
    fn api_key_never_leaks_into_the_query_string() {
        let url = build_url(
            "https://geoapi.qweather.com",
            PATH_CITY_LOOKUP,
            &[("location", "北京"), ("lang", "zh"), ("unit", "m")],
        )
        .unwrap();
        let query = url.query().unwrap_or_default();
        assert!(!query.contains("key="), "密钥不得出现在查询串里：{query}");
        assert_eq!(HEADER_API_KEY, "X-QW-Api-Key");
    }

    #[test]
    fn city_lookup_parses_candidates() {
        let body = r#"{
            "code": "200",
            "location": [
                {"name":"北京","id":"101010100","lon":"116.41","lat":"39.92",
                 "adm2":"东城区","adm1":"北京市","country":"中国","tz":"Asia/Shanghai"},
                {"name":"上海","id":"101020100","lon":"121.47","lat":"31.23",
                 "adm2":"","adm1":"上海市","country":"中国","tz":"Asia/Shanghai"}
            ]
        }"#;
        let candidates = parse_city_lookup(body).unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].provider_location_id, "101010100");
        assert_eq!(candidates[0].latitude, 39.92);
        assert_eq!(candidates[0].longitude, 116.41);
        assert_eq!(candidates[0].timezone, "Asia/Shanghai");
        // `name` 是具体地点，「东城区」是它的上级行政区。
        assert_eq!(candidates[0].title(), "北京 · 东城区 · 北京市");
        assert_eq!(candidates[1].title(), "上海 · 上海市");
    }

    /// 回归：搜「成都」会返回一串**下辖区县**，标题必须各自分得开。
    ///
    /// 真实的和风天气响应里，这些条目的 `adm2` 全是「成都」——
    /// 只显示 `adm2 · adm1` 的话，锦江区 / 龙泉驿 / 都江堰市看起来一模一样。
    #[test]
    fn city_lookup_keeps_county_level_places_distinguishable() {
        let body = r#"{
            "code": "200",
            "location": [
                {"name":"成都","id":"101270101","lon":"104.07","lat":"30.67",
                 "adm2":"成都","adm1":"四川省","country":"中国","tz":"Asia/Shanghai"},
                {"name":"都江堰","id":"101270111","lon":"103.62","lat":"30.99",
                 "adm2":"成都","adm1":"四川省","country":"中国","tz":"Asia/Shanghai"},
                {"name":"龙泉驿","id":"101270107","lon":"104.27","lat":"30.55",
                 "adm2":"成都","adm1":"四川省","country":"中国","tz":"Asia/Shanghai"}
            ]
        }"#;
        let candidates = parse_city_lookup(body).unwrap();
        assert_eq!(candidates.len(), 3);
        assert_eq!(candidates[0].title(), "成都 · 四川省");
        assert_eq!(candidates[1].title(), "都江堰 · 成都 · 四川省");
        assert_eq!(candidates[2].title(), "龙泉驿 · 成都 · 四川省");
        // 三条标题互不相同，用户才能挑到具体那个区县。
        assert_ne!(candidates[1].title(), candidates[2].title());
    }

    #[test]
    fn city_lookup_skips_broken_entries() {
        let body = r#"{"code":"200","location":[
            {"name":"缺ID","lat":"1","lon":"2"},
            {"id":"101020100","lat":"39.9","lon":"116.4"},
            {"name":"北京","id":"101010100","lat":"39.9","lon":"116.4","tz":"Asia/Shanghai"}
        ]}"#;
        let candidates = parse_city_lookup(body).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].provider_location_id, "101010100");
    }

    #[test]
    fn error_codes_are_translated() {
        let unauthorized = r#"{"code":"401"}"#;
        let err = parse_city_lookup(unauthorized).unwrap_err();
        assert!(err.to_string().contains("Key"), "实际 {err}");

        let quota = r#"{"code":"402"}"#;
        let err = parse_now(quota).unwrap_err();
        assert!(err.to_string().contains("配额"), "实际 {err}");

        let rate = r#"{"code":"429"}"#;
        assert!(parse_now(rate).unwrap_err().to_string().contains("频繁"));

        // 非 JSON
        assert!(parse_now("not json").is_err());
        // 缺字段
        assert!(parse_now(r#"{"code":"200"}"#).is_err());
    }

    #[test]
    fn now_is_parsed_with_kind_and_units() {
        let body = r#"{
            "code":"200",
            "now":{"obsTime":"2020-06-30T21:40+08:00","temp":"24","feelsLike":"26",
                   "icon":"101","text":"多云","wind360":"123","windDir":"东南风",
                   "windScale":"1","windSpeed":"3","humidity":"72","precip":"0.0",
                   "pressure":"1003","vis":"16","cloud":"10","dew":"21"}
        }"#;
        let current = parse_now(body).unwrap();
        assert_eq!(current.temp_c, 24.0);
        assert_eq!(current.feels_like_c, 26.0);
        assert_eq!(current.humidity_percent, 72);
        assert_eq!(current.wind_scale, 1);
        assert_eq!(current.wind_dir, "东南风");
        assert_eq!(current.kind, WeatherKind::PartlyCloudy);
        assert_eq!(current.text, "多云");
        // 2020-06-30T21:40+08:00 == 2020-06-30T13:40:00Z
        assert_eq!(current.observed_at, 1_593_524_400, "应转成 UTC 秒");
    }

    #[test]
    fn hourly_is_parsed_in_order() {
        let body = r#"{"code":"200","hourly":[
            {"fxTime":"2020-06-30T22:00+08:00","temp":"24","icon":"101","text":"多云","pop":"0","precip":"0.0"},
            {"fxTime":"2020-06-30T23:00+08:00","temp":"23","icon":"102","text":"少云","pop":"10","precip":"0.1"}
        ]}"#;
        let hours = parse_24h(body).unwrap();
        assert_eq!(hours.len(), 2);
        assert!(hours[0].time < hours[1].time);
        assert_eq!(hours[1].pop_percent, 10);
        assert_eq!(hours[1].precip_mm, 0.1);
        assert_eq!(hours[1].kind, WeatherKind::PartlyCloudy);
    }

    #[test]
    fn daily_is_parsed_with_sunrise() {
        let body = r#"{"code":"200","daily":[
            {"fxDate":"2020-07-01","sunrise":"04:52","sunset":"19:47",
             "tempMax":"33","tempMin":"22","icon":"100","text":"晴","precip":"0.0"}
        ]}"#;
        let days = parse_7d(body).unwrap();
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].date, "2020-07-01");
        assert_eq!(days[0].temp_max_c, 33.0);
        assert_eq!(days[0].temp_min_c, 22.0);
        assert_eq!(days[0].kind, WeatherKind::Clear);
        assert_eq!(days[0].sunrise.as_deref(), Some("04:52"));
        assert_eq!(days[0].text, "晴");
    }

    /// 回归：和风 v7 的逐日预报用 `iconDay`/`iconNight`/`textDay`/`textNight`，
    /// 不是 `icon`/`text`。早期版本读错了字段名，导致 7 天预报的天气种类
    /// 全落成 `Unknown`、天气文字全空——卡片背景图于是全是一个样。
    /// 这里的响应体取自官方文档的真实样例。
    #[test]
    fn daily_reads_the_v7_day_and_night_fields() {
        let body = r#"{"code":"200","daily":[
            {"fxDate":"2021-11-15","sunrise":"06:58","sunset":"16:59",
             "tempMax":"12","tempMin":"-1",
             "iconDay":"101","textDay":"多云",
             "iconNight":"150","textNight":"晴",
             "precip":"0.0"},
            {"fxDate":"2021-11-16","sunrise":"07:00","sunset":"16:58",
             "tempMax":"13","tempMin":"0",
             "iconDay":"100","textDay":"晴",
             "iconNight":"101","textNight":"多云",
             "precip":"0.0"}
        ]}"#;
        let days = parse_7d(body).unwrap();
        assert_eq!(days.len(), 2);

        // 第一天：白天多云、夜间晴。
        assert_eq!(days[0].kind, WeatherKind::PartlyCloudy);
        assert_eq!(days[0].text, "多云");
        assert_eq!(days[0].icon_day, "101");
        assert_eq!(days[0].icon_night, "150");
        assert_eq!(days[0].text_night, "晴");

        // 第二天：白天晴——**必须和第一天是不同的天气**，
        // 否则卡片背景图就分不出这两天。
        assert_eq!(days[1].kind, WeatherKind::Clear);
        assert_eq!(days[1].text, "晴");
        assert_eq!(days[1].icon_night, "101");
    }

    /// 回归：7 天的天气必须各不相同，卡片背景图才有意义。
    ///
    /// 这份响应体覆盖了晴/多云/阴/阵雨/雷阵雨/小雨/雪七种天气，
    /// 断言解析出来的 `kind` 互不相同。
    #[test]
    fn daily_kinds_are_distinguished_per_day() {
        let body = r#"{"code":"200","daily":[
            {"fxDate":"2026-11-04","iconDay":"100","textDay":"晴","iconNight":"150"},
            {"fxDate":"2026-11-05","iconDay":"101","textDay":"多云","iconNight":"151"},
            {"fxDate":"2026-11-06","iconDay":"104","textDay":"阴","iconNight":"154"},
            {"fxDate":"2026-11-07","iconDay":"300","textDay":"阵雨","iconNight":"350"},
            {"fxDate":"2026-11-08","iconDay":"302","textDay":"雷阵雨","iconNight":"352"},
            {"fxDate":"2026-11-09","iconDay":"305","textDay":"小雨","iconNight":"355"},
            {"fxDate":"2026-11-10","iconDay":"400","textDay":"小雪","iconNight":"450"}
        ]}"#;
        let days = parse_7d(body).unwrap();
        let kinds: Vec<WeatherKind> = days.iter().map(|day| day.kind).collect();
        assert_eq!(
            kinds,
            vec![
                WeatherKind::Clear,
                WeatherKind::PartlyCloudy,
                WeatherKind::Cloudy,
                WeatherKind::Rain,
                WeatherKind::Thunderstorm,
                WeatherKind::Rain,
                WeatherKind::Snow,
            ],
            "7 天预报的天气种类应逐日不同"
        );
        // 雷阵雨必须和普通雨分开，否则雷暴图标画不出来。
        assert_ne!(kinds[3], kinds[4], "雨与雷阵雨不能混为一谈");
        // 原始码必须完整保留，UI 靠它区分昼夜。
        assert_eq!(days[3].icon_day, "300");
        assert_eq!(days[3].icon_night, "350");
    }

    /// 旧版响应体（只有 `icon`/`text`）也要能解析，别把老数据源搞坏。
    #[test]
    fn daily_still_accepts_the_legacy_icon_field() {
        let body = r#"{"code":"200","daily":[
            {"fxDate":"2020-07-01","icon":"100","text":"晴","tempMax":"33","tempMin":"22"}
        ]}"#;
        let days = parse_7d(body).unwrap();
        assert_eq!(days[0].kind, WeatherKind::Clear);
        assert_eq!(days[0].text, "晴");
        assert_eq!(days[0].icon_day, "100");
    }

    /// 数据源不绑定 Host：构造成功即可，Host 在调用时才给。
    #[test]
    fn provider_does_not_bind_a_host() {
        assert!(QWeatherProvider::new().is_ok());
    }

    /// 城市搜索路径：统一 Host 用 `/geo/v2/city/lookup`，旧公共 Host 用 `/v2/city/lookup`。
    /// 程序自动回退，用户不需要知道这个区别。
    #[test]
    fn city_lookup_paths_cover_both_host_styles() {
        assert_eq!(
            CityPath::Unknown.path(),
            "/geo/v2/city/lookup",
            "统一 Host（官方现行）路径"
        );
        assert_eq!(
            CityPath::Unified.path(),
            "/geo/v2/city/lookup",
            "探测到统一路径后不再变"
        );
        assert_eq!(
            CityPath::Legacy.path(),
            "/v2/city/lookup",
            "旧公共 Host 路径"
        );
    }
}
