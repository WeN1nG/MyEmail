//! 和风天气（QWeather）HTTPS 适配。
//!
//! 这是**唯一**允许访问天气外部服务的 crate。约束（见 `doc/v03架构设计.md` §6.3）：
//! - 只允许 HTTPS，且 Host 必须是公网地址（按字面量拒绝 loopback / 私网 /
//!   CGNAT / 链路本地；**不做 DNS 预解析**，避免解析与连接之间的时间差）；
//! - 禁止重定向、禁止 Cookie 与 Referer；
//! - 单请求 10s 超时、响应体 ≤ 256 KiB；
//! - API Key 只出现在请求 URL 的查询参数里，**任何日志都不得记录完整 URL**。
//!
//! Host 校验、URL 构造与响应解析都拆成纯函数，便于离线单测。

use std::net::IpAddr;
use std::time::Duration;

use chrono::DateTime;
use me_core::CoreError;
use me_core::weather::{
    CurrentWeather, DailyForecast, HourlyForecast, MAX_LOCATION_CANDIDATES, PROVIDER_QWEATHER,
    WeatherCredential, WeatherForecast, WeatherFuture, WeatherKind, WeatherLocation,
    WeatherLocationCandidate, WeatherProvider,
};
use std::sync::atomic::{AtomicU8, Ordering};

use reqwest::Url;
use serde_json::Value;

/// 默认 Host 与配置提示定义在领域层（UI 也要用），此处转出。
pub use me_core::weather::{DEFAULT_API_HOST, HOST_HINT};

/// 单请求超时。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 响应体大小上限。
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// 天气接口路径。
const PATH_NOW: &str = "/v7/weather/now";
const PATH_24H: &str = "/v7/weather/24h";
const PATH_7D: &str = "/v7/weather/7d";
/// 城市搜索接口路径（**统一 Host**，官方现行）。
///
/// 官方把 GeoAPI 也收进了专属 Host，路径带 `/geo` 前缀。
const PATH_CITY_LOOKUP: &str = "/geo/v2/city/lookup";
/// 城市搜索接口路径（**旧公共 Host**，如 `geoapi.qweather.com`）。
///
/// 旧路径没有 `/geo` 前缀。程序会自动回退到这里，用户不必知道这��区别。
const PATH_CITY_LOOKUP_LEGACY: &str = "/v2/city/lookup";
/// API KEY 认证的请求头名（官方规定）。
const HEADER_API_KEY: &str = "X-QW-Api-Key";

// ---------------------------------------------------------------------------
// Host 校验
// ---------------------------------------------------------------------------

/// 校验并规范化 Host（去掉末尾 `/`）。
///
/// 规则：`https` 开头、无用户名密码、无查询串、无路径（只允许 `/`）、
/// 主机名不是私有地址字面量、不是 `localhost`。
pub fn validate_host(host: &str) -> Result<String, CoreError> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return Err(CoreError::validation("Host 不能为空"));
    }
    let url = Url::parse(trimmed)
        .map_err(|_| CoreError::validation("Host 格式无效，请以 https:// 开头"))?;
    if url.scheme() != "https" {
        return Err(CoreError::validation("Host 必须使用 https"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(CoreError::validation("Host 不能包含用户名或密码"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(CoreError::validation("Host 不能包含查询串"));
    }
    let path = url.path();
    if !path.is_empty() && path != "/" {
        return Err(CoreError::validation("Host 不能包含路径"));
    }
    let host = match url.host_str() {
        Some(host) => host.to_ascii_lowercase(),
        None => return Err(CoreError::validation("Host 缺少主机名")),
    };
    // IPv6 在 URL 里带方括号（`[::1]`），比较与解析都要先剥掉。
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare == "localhost" || bare.ends_with(".localhost") || bare.ends_with(".local") {
        return Err(CoreError::validation("Host 不能指向本机"));
    }
    if let Ok(ip) = bare.parse::<IpAddr>()
        && !is_public_ip(ip)
    {
        return Err(CoreError::validation("Host 不能指向内网或保留地址"));
    }
    Ok(format!("https://{host}"))
}

/// 地址是否为公网地址。
///
/// 拒绝：回环、私网、链路本地、CGNAT（100.64.0.0/10）、未指定的 IPv6、
/// IPv6 ULA（fc00::/7）与 IPv4 保留段。
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            !(octets[0] == 127
                // 私网 10/8、172.16/12、192.168/16
                || (octets[0] == 10)
                || (octets[0] == 172 && (16..32).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 168)
                // 链路本地 169.254/16
                || (octets[0] == 169 && octets[1] == 254)
                // CGNAT 100.64/10
                || (octets[0] == 100 && (64..128).contains(&octets[1]))
                // 0.0.0.0/8
                || octets[0] == 0
                // 224/4 组播、240/4 保留
                || octets[0] >= 224)
        }
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            if v6.is_loopback() || v6.is_unspecified() {
                return false;
            }
            // fc00::/7 唯一本地地址
            if (segments[0] & 0xfe00) == 0xfc00 {
                return false;
            }
            // fe80::/10 链路本地
            if (segments[0] & 0xffc0) == 0xfe80 {
                return false;
            }
            // IPv4 映射地址按内嵌的 v4 判断
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            true
        }
    }
}

/// 构造请求 URL。`params` 里的值会被 URL 编码。
pub fn build_url(host: &str, path: &str, params: &[(&str, &str)]) -> Result<Url, CoreError> {
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
        let icon = str_field(item, "icon").unwrap_or_default();
        out.push(DailyForecast {
            date,
            sunrise: parse_clock(item, "sunrise"),
            sunset: parse_clock(item, "sunset"),
            temp_max_c: f64_field(item, "tempMax").unwrap_or(0.0),
            temp_min_c: f64_field(item, "tempMin").unwrap_or(0.0),
            precip_mm: f64_field(item, "precip").unwrap_or(0.0),
            kind: WeatherKind::from_icon_code(&icon),
            text: str_field(item, "text").unwrap_or_default(),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 客户端
// ---------------------------------------------------------------------------

/// 和风天气数据源。
pub struct QWeatherProvider {
    client: reqwest::Client,
    /// 统一 API Host：城市搜索与天气都用它。
    api_host: String,
    /// 城市搜索实际使用的路径。
    ///
    /// 官方统一 Host 用 `/geo/v2/city/lookup`，旧公共 Host 用 `/v2/city/lookup`。
    /// 第一次遇到 404 就自动换一条并记住，避免用户手工区分 Host 类型。
    city_path: AtomicU8,
}

/// 城市搜索路径的探测状态（存进 [`AtomicU8`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum CityPath {
    /// 还没试过（先用统一 Host 的路径）。
    Unknown = 0,
    /// 统一 Host 路径可用。
    Unified = 1,
    /// 统一 Host 路径 404，应改用旧路径。
    Legacy = 2,
}

impl CityPath {
    fn from_raw(value: u8) -> Self {
        match value {
            1 => CityPath::Unified,
            2 => CityPath::Legacy,
            _ => CityPath::Unknown,
        }
    }

    fn path(self) -> &'static str {
        match self {
            CityPath::Legacy => PATH_CITY_LOOKUP_LEGACY,
            _ => PATH_CITY_LOOKUP,
        }
    }
}

impl QWeatherProvider {
    /// 用默认 Host 构造。
    pub fn new() -> Result<Self, CoreError> {
        Self::with_host(DEFAULT_API_HOST)
    }

    /// 用指定 Host 构造。Host 会被校验并规范化。
    ///
    /// 只有**一个** Host：官方已把城市搜索与天气收进同一个专属域名，
    /// 用户不必再填「天气 Host + 城市搜索 Host」两个。
    pub fn with_host(api_host: &str) -> Result<Self, CoreError> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // 禁止重定向：避免被引到内网。
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("MyEmail/0.3 (weather)")
            .build()
            .map_err(|err| CoreError::network(false, format!("创建天气客户端失败: {err}")))?;
        Ok(Self {
            client,
            api_host: validate_host(api_host)?,
            city_path: AtomicU8::new(CityPath::Unknown as u8),
        })
    }

    pub fn api_host(&self) -> &str {
        &self.api_host
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
    fn search_locations<'a>(
        &'a self,
        query: &'a str,
        credential: &'a WeatherCredential,
    ) -> WeatherFuture<'a, Result<Vec<WeatherLocationCandidate>, CoreError>> {
        Box::pin(async move {
            // 先按当前探测状态选路径；若 404 且用的是统一路径，就换成旧路径重试。
            let path = CityPath::from_raw(self.city_path.load(Ordering::Relaxed));
            let url = build_url(
                &self.api_host,
                path.path(),
                &[("location", query), ("lang", "zh"), ("unit", "m")],
            )?;
            let body = match self.get(url, credential).await {
                Ok(body) => {
                    if path == CityPath::Unknown {
                        self.city_path
                            .store(CityPath::Unified as u8, Ordering::Relaxed);
                    }
                    body
                }
                Err(err) if err.is_not_found() && path != CityPath::Legacy => {
                    // 统一路径 404：多半是旧公共 Host，换旧路径再试一次。
                    self.city_path
                        .store(CityPath::Legacy as u8, Ordering::Relaxed);
                    let retry = build_url(
                        &self.api_host,
                        PATH_CITY_LOOKUP_LEGACY,
                        &[("location", query), ("lang", "zh"), ("unit", "m")],
                    )?;
                    self.get(retry, credential).await?
                }
                Err(err) => return Err(err),
            };
            parse_city_lookup(&body)
        })
    }

    fn forecast<'a>(
        &'a self,
        location: &'a WeatherLocation,
        credential: &'a WeatherCredential,
    ) -> WeatherFuture<'a, Result<WeatherForecast, CoreError>> {
        Box::pin(async move {
            let id = location.provider_location_id.as_str();
            let now_url = build_url(
                &self.api_host,
                PATH_NOW,
                &[("location", id), ("lang", "zh"), ("unit", "m")],
            )?;
            let hourly_url = build_url(
                &self.api_host,
                PATH_24H,
                &[("location", id), ("lang", "zh"), ("unit", "m")],
            )?;
            let daily_url = build_url(
                &self.api_host,
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
    fn host_must_be_public_https() {
        assert_eq!(
            validate_host("https://devapi.qweather.com").unwrap(),
            "https://devapi.qweather.com"
        );
        assert_eq!(
            validate_host("  https://devapi.qweather.com/  ").unwrap(),
            "https://devapi.qweather.com"
        );
        // 非 https
        assert!(validate_host("http://devapi.qweather.com").is_err());
        // 本机与内网
        assert!(validate_host("https://localhost").is_err());
        assert!(validate_host("https://127.0.0.1").is_err());
        assert!(validate_host("https://10.1.2.3").is_err());
        assert!(validate_host("https://192.168.0.1").is_err());
        assert!(validate_host("https://172.20.0.1").is_err());
        assert!(validate_host("https://169.254.1.1").is_err());
        // CGNAT
        assert!(validate_host("https://100.64.0.1").is_err());
        // IPv6 回环与 ULA
        assert!(validate_host("https://[::1]").is_err());
        assert!(validate_host("https://[fd00::1]").is_err());
        assert!(validate_host("https://[fe80::1]").is_err());
        // 带凭据 / 查询串 / 路径
        assert!(validate_host("https://user:pass@devapi.qweather.com").is_err());
        assert!(validate_host("https://devapi.qweather.com?a=1").is_err());
        assert!(validate_host("https://devapi.qweather.com/v7").is_err());
        // 空
        assert!(validate_host("   ").is_err());
    }

    #[test]
    fn public_ip_classification() {
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_public_ip("127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("0.0.0.0".parse().unwrap()));
        assert!(!is_public_ip("224.0.0.1".parse().unwrap()));
        assert!(is_public_ip("2606:4700::1111".parse().unwrap()));
        assert!(!is_public_ip("::1".parse().unwrap()));
    }

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
        assert_eq!(candidates[0].title(), "东城区 · 北京市");
        assert_eq!(candidates[1].title(), "上海市");
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
    }

    #[test]
    fn provider_normalizes_hosts() {
        // 只有一个 Host：用户不必再分别填天气 Host 与城市搜索 Host。
        let provider = QWeatherProvider::with_host("https://devapi.qweather.com/").unwrap();
        assert_eq!(provider.api_host(), "https://devapi.qweather.com");
        assert!(QWeatherProvider::with_host("http://x.com").is_err());
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
        // 状态与序号互相对应，原子里存的就是这个值。
        assert_eq!(CityPath::from_raw(0), CityPath::Unknown);
        assert_eq!(CityPath::from_raw(1), CityPath::Unified);
        assert_eq!(CityPath::from_raw(2), CityPath::Legacy);
        assert_eq!(CityPath::from_raw(9), CityPath::Unknown);
    }
}
