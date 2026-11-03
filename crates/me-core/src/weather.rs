//! 天气领域模型、端口与纯函数。
//!
//! 约束：本模块不做 IO、不依赖 tokio / rusqlite / 任何协议库。
//! 时间统一为 **UTC Unix 秒**；展示用的时区换算由界面按地点的 IANA 时区处理。
//!
//! 数据来源固定为和风天气（QWeather），但表结构保留 `provider` 列，
//! 将来接第二个数据源不必改表。
//!
//! 三个对外职责：
//! - [`WeatherKind::from_icon_code`]：和风天气图标码 → 天气种类（未知归 `Unknown`，不报错）；
//! - [`is_expired`]：快照是否过期（决定能否直接复用缓存）；
//! - [`hourly_series`]：逐时预报 → 折线图采样点（纯几何，界面直接照着画）。

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 最多可保存的地点数量（`doc/v03架构设计.md` §5.3.1）。
pub const MAX_WEATHER_LOCATIONS: usize = 8;

/// 快照默认有效期（秒）。到期后页面打开会触发一次刷新。
pub const SNAPSHOT_TTL_SECS: i64 = 30 * 60;

/// 两次刷新请求之间的最小间隔（秒），防止手滑把额度打光。
pub const MIN_REFRESH_INTERVAL_SECS: i64 = 60;

/// 逐时预报的最大条数（服务商返回 24 条，留 2 倍余量做校验）。
pub const MAX_HOURLY: usize = 48;

/// 逐日预报的最大条数。
pub const MAX_DAILY: usize = 7;

/// 城市搜索结果的最大条数。
pub const MAX_LOCATION_CANDIDATES: usize = 10;

/// 城市搜索输入的最大长度。
pub const MAX_QUERY_LEN: usize = 100;

/// API Key 的最大长度（超长直接拒绝，避免把异常输入写进密钥库）。
pub const MAX_API_KEY_LEN: usize = 4096;

/// 数据来源标识。
pub const PROVIDER_QWEATHER: &str = "qweather";

/// 默认天气 Host。
///
/// 官方现已为每个项目分配**专属域名**（形如 `abcxyz.qweatherapi.com`，
/// 在控制台「设置」页查看），公共域名逐步停服。这里保留公共域名作为
/// 兜底默认值，用户应在配置弹窗里填自己的专属 Host。
pub const DEFAULT_API_HOST: &str = "https://devapi.qweather.com";
/// 配置弹窗里给用户的提示：Host 从哪里取。
pub const HOST_HINT: &str =
    "在 console.qweather.com 的「设置」页查看你的专属 API Host（形如 abcxyz.qweatherapi.com）";

// ---------------------------------------------------------------------------
// 标识与模型
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WeatherLocationId(pub i64);

impl fmt::Display for WeatherLocationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "weather-location#{}", self.0)
    }
}

/// 一个已保存的地点。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeatherLocation {
    pub id: WeatherLocationId,
    /// 数据来源（当前只有 `qweather`）。
    pub provider: String,
    /// 服务商的地点 ID；刷新时用它，不依赖经纬度。
    pub provider_location_id: String,
    pub name: String,
    /// 省 / 州。
    pub admin1: Option<String>,
    /// 市 / 区。
    pub admin2: Option<String>,
    pub country: Option<String>,
    pub latitude: f64,
    pub longitude: f64,
    /// IANA 时区名（如 `Asia/Shanghai`）。
    pub timezone: String,
    pub is_default: bool,
    pub sort_order: i64,
    pub created_at: i64,
}

/// 城市搜索返回的候选地点（尚未保存）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeatherLocationCandidate {
    pub provider: String,
    pub provider_location_id: String,
    pub name: String,
    pub admin1: Option<String>,
    pub admin2: Option<String>,
    pub country: Option<String>,
    pub latitude: f64,
    pub longitude: f64,
    pub timezone: String,
}

impl WeatherLocationCandidate {
    /// 展示用标题：`市 · 省`；缺市时退回省，再退回名称。
    pub fn title(&self) -> String {
        let city = self.admin2.as_deref().filter(|text| !text.is_empty());
        let province = self.admin1.as_deref().filter(|text| !text.is_empty());
        match (city, province) {
            (Some(city), Some(province)) => format!("{city} · {province}"),
            (Some(city), None) => city.to_string(),
            (None, Some(province)) => province.to_string(),
            (None, None) => self.name.clone(),
        }
    }
}

/// 天气种类。界面按它绘制图标，不依赖字体是否收录符号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeatherKind {
    Clear,
    PartlyCloudy,
    Cloudy,
    Fog,
    Haze,
    Dust,
    Drizzle,
    Rain,
    Sleet,
    Snow,
    Thunderstorm,
    Hail,
    Unknown,
}

impl WeatherKind {
    /// 中文标签（字体子集含中文，可直接显示）。
    pub fn label(self) -> &'static str {
        match self {
            WeatherKind::Clear => "晴",
            WeatherKind::PartlyCloudy => "多云",
            WeatherKind::Cloudy => "阴",
            WeatherKind::Fog => "雾",
            WeatherKind::Haze => "霾",
            WeatherKind::Dust => "浮尘",
            WeatherKind::Drizzle => "小雨",
            WeatherKind::Rain => "雨",
            WeatherKind::Sleet => "雨夹雪",
            WeatherKind::Snow => "雪",
            WeatherKind::Thunderstorm => "雷阵雨",
            WeatherKind::Hail => "冰雹",
            WeatherKind::Unknown => "未知",
        }
    }

    /// 和风天气图标码 → 天气种类。
    ///
    /// 认不出的码归 [`WeatherKind::Unknown`]，**不报错**：图标码会随服务商
    /// 增加而变，界面上显示服务商给的文字描述即可。
    pub fn from_icon_code(code: &str) -> Self {
        let code = code.trim();
        match code {
            // 晴 / 多云 / 阴
            "100" | "700" | "800" => WeatherKind::Clear,
            "101" | "102" | "103" | "104" | "701" | "702" | "703" | "704" | "801" | "802"
            | "803" | "804" | "805" => WeatherKind::PartlyCloudy,
            // 雾 / 霾 / 浮尘
            "500" | "509" | "711" | "733" => WeatherKind::Fog,
            "501" | "502" | "712" => WeatherKind::Haze,
            "503" | "504" | "713" | "714" => WeatherKind::Dust,
            // 雨（含冻雨：冻雨仍按雨处理）
            "300" | "301" | "302" | "303" | "304" | "307" | "309" | "314" | "806" | "807"
            | "808" | "816" | "817" | "818" | "819" | "820" | "821" | "822" => WeatherKind::Rain,
            // 雷暴（强对流）
            "305" | "308" | "310" | "311" | "823" | "900" | "901" | "902" => {
                WeatherKind::Thunderstorm
            }
            // 冰雹
            "312" => WeatherKind::Hail,
            // 雪
            "400" | "401" | "402" | "403" | "404" | "405" | "315" | "810" | "811" | "812"
            | "813" | "814" | "815" | "826" | "827" | "828" | "829" | "830" => WeatherKind::Snow,
            // 雨夹雪
            "306" | "313" | "406" | "809" | "824" => WeatherKind::Sleet,
            _ => WeatherKind::Unknown,
        }
    }
}

/// 当前天气。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CurrentWeather {
    /// 观测时刻（UTC Unix 秒）。
    pub observed_at: i64,
    pub temp_c: f64,
    pub feels_like_c: f64,
    pub humidity_percent: u32,
    /// 风力等级。
    pub wind_scale: u32,
    /// 风向（如「东北风」）。
    pub wind_dir: String,
    /// 风速（km/h）。
    pub wind_speed_kmh: f64,
    /// 降水量（mm）。
    pub precip_mm: f64,
    /// 气压（hPa）。
    pub pressure_hpa: f64,
    /// 能见度（km）。
    pub visibility_km: f64,
    /// 云量（%）。
    pub cloud_percent: u32,
    pub kind: WeatherKind,
    /// 服务商给的中文描述（「多云」等），作为辅助说明。
    pub text: String,
}

/// 逐时预报。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HourlyForecast {
    /// 该小时的起始时刻（UTC Unix 秒）。
    pub time: i64,
    pub temp_c: f64,
    /// 降水概率（%）。
    pub pop_percent: u32,
    /// 降水量（mm）。
    pub precip_mm: f64,
    pub kind: WeatherKind,
    pub text: String,
}

/// 逐日预报。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DailyForecast {
    /// 日期（`YYYY-MM-DD`，服务商所在时区）。
    pub date: String,
    /// 日出时间（`HH:MM`，地点所在时区的当地时间）。
    pub sunrise: Option<String>,
    /// 日落时间（`HH:MM`，地点所在时区的当地时间）。
    pub sunset: Option<String>,
    pub temp_max_c: f64,
    pub temp_min_c: f64,
    pub precip_mm: f64,
    pub kind: WeatherKind,
    pub text: String,
}

/// 一次刷新的完整结果（不含缓存元信息）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeatherForecast {
    pub current: CurrentWeather,
    pub hourly: Vec<HourlyForecast>,
    pub daily: Vec<DailyForecast>,
}

/// 缓存的天气快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeatherSnapshot {
    pub provider: String,
    /// 抓取时刻（UTC Unix 秒）。
    pub fetched_at: i64,
    /// 过期时刻（UTC Unix 秒）。
    pub expires_at: i64,
    pub current: CurrentWeather,
    pub hourly: Vec<HourlyForecast>,
    pub daily: Vec<DailyForecast>,
}

impl WeatherSnapshot {
    /// 按当前默认有效期构造快照。
    pub fn new(provider: impl Into<String>, fetched_at: i64, forecast: WeatherForecast) -> Self {
        Self {
            provider: provider.into(),
            fetched_at,
            expires_at: fetched_at + SNAPSHOT_TTL_SECS,
            current: forecast.current,
            hourly: forecast.hourly,
            daily: forecast.daily,
        }
    }
}

/// 快照在数据库里的存储形式。`v` 用于将来格式演进。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredWeatherSnapshot {
    pub v: u32,
    pub snapshot: WeatherSnapshot,
}

impl StoredWeatherSnapshot {
    /// 当前快照格式版本。
    pub const VERSION: u32 = 1;

    pub fn new(snapshot: WeatherSnapshot) -> Self {
        Self {
            v: Self::VERSION,
            snapshot,
        }
    }
}

// ---------------------------------------------------------------------------
// 凭证
// ---------------------------------------------------------------------------

/// 和风天气 API Key。`Debug` 脱敏，绝不进日志或事件载荷。
#[derive(Clone, PartialEq, Eq)]
pub struct WeatherCredential {
    key: String,
}

impl WeatherCredential {
    pub fn new(key: impl Into<String>) -> Self {
        Self { key: key.into() }
    }

    /// 取明文 Key。只在构造请求 URL 的那一处使用。
    pub fn expose(&self) -> &str {
        &self.key
    }

    pub fn is_empty(&self) -> bool {
        self.key.trim().is_empty()
    }
}

impl fmt::Debug for WeatherCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WeatherCredential(<redacted>)")
    }
}

// ---------------------------------------------------------------------------
// 端口
// ---------------------------------------------------------------------------

/// 天气数据源端口。实现放在 `me-proto`（唯一出网的地方）。
///
/// 用返回 `BoxFuture` 而不是 `async fn`，是为了让 `me-sync` 能把它存成
/// `Arc<dyn WeatherProvider>`（`async fn in trait` 不是 dyn 兼容的）。
pub type WeatherFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 天气数据源。
pub trait WeatherProvider: Send + Sync {
    /// 按城市名搜索候选地点。
    fn search_locations<'a>(
        &'a self,
        query: &'a str,
        credential: &'a WeatherCredential,
    ) -> WeatherFuture<'a, Result<Vec<WeatherLocationCandidate>, CoreError>>;

    /// 取某地点的当前天气 + 逐时 + 逐日预报。
    fn forecast<'a>(
        &'a self,
        location: &'a WeatherLocation,
        credential: &'a WeatherCredential,
    ) -> WeatherFuture<'a, Result<WeatherForecast, CoreError>>;
}

/// 天气地点与快照的仓储端口。
///
/// **写入只允许由 `me-sync` 的用例调用**（见 `doc/v03架构设计.md` §2.7）：
/// `me-app` 只能通过 [`crate::port::ReadOnlyRepo`] 的只读方法读取。
pub trait WeatherRepo {
    fn list_weather_locations(&self) -> Result<Vec<WeatherLocation>, CoreError>;
    fn get_weather_location(
        &self,
        id: WeatherLocationId,
    ) -> Result<Option<WeatherLocation>, CoreError>;
    /// 默认地点；没有地点时返回 `None`。
    fn get_default_weather_location(&self) -> Result<Option<WeatherLocation>, CoreError>;
    /// 插入地点。同一 `(provider, provider_location_id)` 已存在时返回既有地点。
    fn insert_weather_location(
        &self,
        candidate: &WeatherLocationCandidate,
    ) -> Result<WeatherLocation, CoreError>;
    fn remove_weather_location(&self, id: WeatherLocationId) -> Result<(), CoreError>;
    /// 把某个地点设为默认（其余地点取消默认）。
    fn set_default_weather_location(&self, id: WeatherLocationId) -> Result<(), CoreError>;

    fn get_weather_snapshot(
        &self,
        location_id: WeatherLocationId,
    ) -> Result<Option<WeatherSnapshot>, CoreError>;
    /// 写入/覆盖快照。
    fn save_weather_snapshot(
        &self,
        location_id: WeatherLocationId,
        snapshot: &WeatherSnapshot,
    ) -> Result<(), CoreError>;
}

// ---------------------------------------------------------------------------
// 纯函数
// ---------------------------------------------------------------------------

/// 快照是否已过期。
pub fn is_expired(now: i64, expires_at: i64) -> bool {
    now >= expires_at
}

/// 温度显示：整数 + 摄氏度。
pub fn format_temp(celsius: f64) -> String {
    format!("{:.0}°", celsius)
}

/// 风速显示：`3 级 · 12 km/h`。
pub fn format_wind(scale: u32, speed_kmh: f64) -> String {
    if scale == 0 {
        return format!("{speed_kmh:.0} km/h");
    }
    format!("{scale} 级 · {speed_kmh:.0} km/h")
}

/// 降水量显示：`0.0 mm`。
pub fn format_precip(mm: f64) -> String {
    format!("{mm:.1} mm")
}

/// 湿度显示。
pub fn format_humidity(percent: u32) -> String {
    format!("{percent}%")
}

/// 校验城市搜索输入。
pub fn validate_query(query: &str) -> Result<String, CoreError> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(CoreError::validation("请输入城市名"));
    }
    if trimmed.chars().count() > MAX_QUERY_LEN {
        return Err(CoreError::validation(format!(
            "城市名过长（最多 {MAX_QUERY_LEN} 个字符）"
        )));
    }
    Ok(trimmed.to_string())
}

/// 校验候选地点是否可以入库。
pub fn validate_candidate(candidate: &WeatherLocationCandidate) -> Result<(), CoreError> {
    if candidate.provider.trim().is_empty() {
        return Err(CoreError::validation("地点缺少数据来源"));
    }
    if candidate.provider_location_id.trim().is_empty() {
        return Err(CoreError::validation("地点缺少服务商 ID"));
    }
    if candidate.name.trim().is_empty() {
        return Err(CoreError::validation("地点缺少名称"));
    }
    if !(-90.0..=90.0).contains(&candidate.latitude) {
        return Err(CoreError::validation("地点纬度超出范围"));
    }
    if !(-180.0..=180.0).contains(&candidate.longitude) {
        return Err(CoreError::validation("地点经度超出范围"));
    }
    if candidate.timezone.trim().is_empty() {
        return Err(CoreError::validation("地点缺少时区"));
    }
    Ok(())
}

/// 折线图的一个采样点。坐标已归一化到 `0..=1`，界面直接照着画。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeriesPoint {
    /// 横坐标（`0..=1`，沿时间轴）。
    pub x: f32,
    /// 纵坐标（`0..=1`，0 为最低温、1 为最高温）。
    pub y: f32,
    pub temp_c: f64,
    pub time: i64,
    pub kind: WeatherKind,
    /// 降水概率（%）。
    pub pop_percent: u32,
}

/// 逐时预报 → 折线图采样点。
///
/// 温度区间取本组数据的最小/最大值；全部相等时画成水平直线（`y = 0.5`）。
/// 点数不足两个时返回空数组。
pub fn hourly_series(hourly: &[HourlyForecast], samples: usize) -> Vec<SeriesPoint> {
    if hourly.len() < 2 || samples < 2 {
        return Vec::new();
    }
    let mut min = f64::MAX;
    let mut max = f64::MIN;
    for hour in hourly.iter().take(samples) {
        min = min.min(hour.temp_c);
        max = max.max(hour.temp_c);
    }
    let span = max - min;
    let count = hourly.len().min(samples);
    (0..count)
        .map(|index| {
            let hour = &hourly[index];
            let x = if count == 1 {
                0.0
            } else {
                index as f32 / (count - 1) as f32
            };
            let y = if span.abs() < f64::EPSILON {
                0.5
            } else {
                ((hour.temp_c - min) / span) as f32
            };
            SeriesPoint {
                x,
                y,
                temp_c: hour.temp_c,
                time: hour.time,
                kind: hour.kind,
                pop_percent: hour.pop_percent,
            }
        })
        .collect()
}

/// 逐时预报的整体温度范围（用于坐标轴标注）。
pub fn hourly_temp_range(hourly: &[HourlyForecast]) -> Option<(f64, f64)> {
    if hourly.is_empty() {
        return None;
    }
    let mut min = f64::MAX;
    let mut max = f64::MIN;
    for hour in hourly {
        min = min.min(hour.temp_c);
        max = max.max(hour.temp_c);
    }
    Some((min, max))
}

/// 校验一次刷新的结果是否可用（条数上限、时间单调、数值有限）。
pub fn validate_forecast(forecast: &WeatherForecast) -> Result<(), CoreError> {
    if forecast.hourly.len() > MAX_HOURLY {
        return Err(CoreError::parse(format!(
            "逐时预报条数异常（{} > {MAX_HOURLY}）",
            forecast.hourly.len()
        )));
    }
    if forecast.daily.len() > MAX_DAILY {
        return Err(CoreError::parse(format!(
            "逐日预报条数异常（{} > {MAX_DAILY}）",
            forecast.daily.len()
        )));
    }
    let mut previous = i64::MIN;
    for hour in &forecast.hourly {
        if hour.time < previous {
            return Err(CoreError::parse("逐时预报时间不是单调的"));
        }
        previous = hour.time;
        if !hour.temp_c.is_finite() || !hour.precip_mm.is_finite() {
            return Err(CoreError::parse("逐时预报含非法数值"));
        }
    }
    if !forecast.current.temp_c.is_finite() || !forecast.current.feels_like_c.is_finite() {
        return Err(CoreError::parse("当前天气含非法数值"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate() -> WeatherLocationCandidate {
        WeatherLocationCandidate {
            provider: PROVIDER_QWEATHER.to_string(),
            provider_location_id: "101010100".to_string(),
            name: "北京".to_string(),
            admin1: Some("北京市".to_string()),
            admin2: Some("东城区".to_string()),
            country: Some("中国".to_string()),
            latitude: 39.9,
            longitude: 116.4,
            timezone: "Asia/Shanghai".to_string(),
        }
    }

    fn hour(time: i64, temp: f64) -> HourlyForecast {
        HourlyForecast {
            time,
            temp_c: temp,
            pop_percent: 0,
            precip_mm: 0.0,
            kind: WeatherKind::Cloudy,
            text: "阴".to_string(),
        }
    }

    #[test]
    fn icon_codes_map_to_kinds() {
        assert_eq!(WeatherKind::from_icon_code("100"), WeatherKind::Clear);
        assert_eq!(
            WeatherKind::from_icon_code("104"),
            WeatherKind::PartlyCloudy
        );
        assert_eq!(WeatherKind::from_icon_code("300"), WeatherKind::Rain);
        assert_eq!(WeatherKind::from_icon_code("302"), WeatherKind::Rain);
        assert_eq!(WeatherKind::from_icon_code("306"), WeatherKind::Sleet);
        assert_eq!(
            WeatherKind::from_icon_code("305"),
            WeatherKind::Thunderstorm
        );
        assert_eq!(WeatherKind::from_icon_code("312"), WeatherKind::Hail);
        assert_eq!(WeatherKind::from_icon_code("401"), WeatherKind::Snow);
        assert_eq!(WeatherKind::from_icon_code("500"), WeatherKind::Fog);
        assert_eq!(WeatherKind::from_icon_code("502"), WeatherKind::Haze);
        assert_eq!(WeatherKind::from_icon_code("503"), WeatherKind::Dust);
    }

    #[test]
    fn unknown_icon_code_does_not_panic() {
        // 服务商新增图标码时不能报错，只是归为未知。
        assert_eq!(WeatherKind::from_icon_code("999"), WeatherKind::Unknown);
        assert_eq!(WeatherKind::from_icon_code(""), WeatherKind::Unknown);
        assert_eq!(WeatherKind::from_icon_code("abc"), WeatherKind::Unknown);
    }

    #[test]
    fn credential_debug_is_redacted() {
        let credential = WeatherCredential::new("super-secret-key");
        let shown = format!("{credential:?}");
        assert!(!shown.contains("super-secret-key"));
        assert!(shown.contains("redacted"));
        assert_eq!(credential.expose(), "super-secret-key");
    }

    #[test]
    fn expiry_is_inclusive() {
        assert!(!is_expired(100, 101));
        assert!(is_expired(100, 100));
        assert!(is_expired(101, 100));
    }

    #[test]
    fn query_validation_trims_and_rejects_empty() {
        assert_eq!(validate_query("  北京  ").unwrap(), "北京");
        assert!(validate_query("   ").is_err());
        assert!(validate_query(&"x".repeat(MAX_QUERY_LEN + 1)).is_err());
        assert!(validate_query(&"x".repeat(MAX_QUERY_LEN)).is_ok());
    }

    #[test]
    fn candidate_validation_checks_bounds() {
        assert!(validate_candidate(&candidate()).is_ok());
        let mut bad = candidate();
        bad.latitude = 120.0;
        assert!(validate_candidate(&bad).is_err());
        let mut bad = candidate();
        bad.longitude = -200.0;
        assert!(validate_candidate(&bad).is_err());
        let mut bad = candidate();
        bad.provider_location_id = "  ".to_string();
        assert!(validate_candidate(&bad).is_err());
        let mut bad = candidate();
        bad.timezone = String::new();
        assert!(validate_candidate(&bad).is_err());
    }

    #[test]
    fn hourly_series_is_normalized_and_ordered() {
        let hours: Vec<HourlyForecast> = (0..24)
            .map(|i| hour(i64::from(i), 10.0 + i as f64))
            .collect();
        let points = hourly_series(&hours, 24);
        assert_eq!(points.len(), 24);
        assert_eq!(points[0].x, 0.0);
        assert!((points[23].x - 1.0).abs() < 1e-6);
        // 温度单调上升 → y 也单调上升，最低 0 最高 1。
        assert!(points[0].y < points[23].y);
        assert!(points.iter().all(|p| (0.0..=1.0).contains(&p.y)));
        assert!(points.windows(2).all(|w| w[0].x <= w[1].x));
    }

    #[test]
    fn hourly_series_handles_flat_and_short_input() {
        let flat: Vec<HourlyForecast> = (0..5).map(|i| hour(i, 20.0)).collect();
        let points = hourly_series(&flat, 5);
        assert_eq!(points.len(), 5);
        assert!(points.iter().all(|p| (p.y - 0.5).abs() < 1e-6));

        assert!(hourly_series(&flat[..1], 5).is_empty());
        assert!(hourly_series(&[], 5).is_empty());
        assert!(hourly_series(&flat, 1).is_empty());
    }

    #[test]
    fn hourly_series_respects_sample_limit() {
        let hours: Vec<HourlyForecast> = (0..48).map(|i| hour(i, 10.0)).collect();
        assert_eq!(hourly_series(&hours, 12).len(), 12);
    }

    #[test]
    fn temp_range_reports_min_and_max() {
        let hours = vec![hour(0, 5.0), hour(1, 15.0), hour(2, 10.0)];
        assert_eq!(hourly_temp_range(&hours), Some((5.0, 15.0)));
        assert_eq!(hourly_temp_range(&[]), None);
    }

    #[test]
    fn forecast_validation_rejects_bad_shapes() {
        let good = WeatherForecast {
            current: CurrentWeather {
                observed_at: 1,
                temp_c: 20.0,
                feels_like_c: 19.0,
                humidity_percent: 50,
                wind_scale: 2,
                wind_dir: "北风".into(),
                wind_speed_kmh: 5.0,
                precip_mm: 0.0,
                pressure_hpa: 1013.0,
                visibility_km: 10.0,
                cloud_percent: 20,
                kind: WeatherKind::Clear,
                text: "晴".into(),
            },
            hourly: vec![hour(0, 20.0), hour(3600, 21.0)],
            daily: Vec::new(),
        };
        assert!(validate_forecast(&good).is_ok());

        let mut too_many = good.clone();
        too_many.hourly = (0..MAX_HOURLY + 1).map(|i| hour(i as i64, 20.0)).collect();
        assert!(validate_forecast(&too_many).is_err());

        let mut unordered = good.clone();
        unordered.hourly = vec![hour(3600, 20.0), hour(0, 21.0)];
        assert!(validate_forecast(&unordered).is_err());

        let mut nan = good.clone();
        nan.current.temp_c = f64::NAN;
        assert!(validate_forecast(&nan).is_err());
    }

    #[test]
    fn snapshot_payload_round_trips_with_version() {
        let snapshot = WeatherSnapshot::new(
            PROVIDER_QWEATHER,
            1_000,
            WeatherForecast {
                current: CurrentWeather {
                    observed_at: 999,
                    temp_c: 20.0,
                    feels_like_c: 19.0,
                    humidity_percent: 50,
                    wind_scale: 2,
                    wind_dir: "北风".into(),
                    wind_speed_kmh: 5.0,
                    precip_mm: 0.0,
                    pressure_hpa: 1013.0,
                    visibility_km: 10.0,
                    cloud_percent: 20,
                    kind: WeatherKind::Clear,
                    text: "晴".into(),
                },
                hourly: vec![hour(0, 20.0)],
                daily: Vec::new(),
            },
        );
        let stored = StoredWeatherSnapshot::new(snapshot.clone());
        let json = serde_json::to_string(&stored).expect("序列化");
        assert!(json.contains("\"v\":1"));
        let back: StoredWeatherSnapshot = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back.snapshot, snapshot);
        assert_eq!(back.v, StoredWeatherSnapshot::VERSION);
    }

    #[test]
    fn formats_are_stable() {
        assert_eq!(format_temp(23.6), "24°");
        assert_eq!(format_temp(-3.2), "-3°");
        assert_eq!(format_wind(3, 12.4), "3 级 · 12 km/h");
        assert_eq!(format_wind(0, 7.0), "7 km/h");
        assert_eq!(format_precip(0.04), "0.0 mm");
        assert_eq!(format_humidity(65), "65%");
    }
}
