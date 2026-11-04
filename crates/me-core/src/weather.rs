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
///
/// 和风天气的 `number` 参数上限是 20：搜「成都」会返回成都本级加上
/// 锦江区、龙泉驿、都江堰市等一串下辖区县，只给 10 条会截掉一部分。
pub const MAX_LOCATION_CANDIDATES: usize = 20;

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
pub const HOST_HINT: &str = "在 console.qweather.com 的「设置」页查看你的专属 API Host（形如 abcxyz.qweatherapi.com，https:// 可省略）";

// ---------------------------------------------------------------------------
// 端点路径
// ---------------------------------------------------------------------------
//
// 路径由程序决定，用户只填 Host。这些常量放在领域层是为了让界面能预览
// 「程序实际会请求哪个地址」，实现层（me-proto）直接引用，不另写一份。

/// 实时天气端点。
pub const PATH_NOW: &str = "/v7/weather/now";
/// 逐小时预报端点。
pub const PATH_24H: &str = "/v7/weather/24h";
/// 逐日预报端点。
pub const PATH_7D: &str = "/v7/weather/7d";
/// 城市搜索端点（**统一 Host**，官方现行）。
pub const PATH_CITY_LOOKUP: &str = "/geo/v2/city/lookup";
/// 城市搜索端点（**旧公共 Host**）。程序会自动回退到这里。
pub const PATH_CITY_LOOKUP_LEGACY: &str = "/v2/city/lookup";

// ---------------------------------------------------------------------------
// Host 规范化
// ---------------------------------------------------------------------------

/// 把用户填的 Host 规范化成 `https://host[:port]`。
///
/// 控制台复制到的是**裸域名**（如 `abcxyz.qweatherapi.com`），用户也可能
/// 带上 `https://` 前缀、结尾斜杠或路径。**协议由程序决定**：一律走
/// https，因此填 `http://` 也会被升级，用户不必自己改正。
///
/// 规范化内容：
/// - 去掉 `http://` / `https://` 前缀、结尾斜杠与路径部分；
/// - 主机名转小写；
/// - 拒绝用户名/密码、查询串与片段；
/// - 拒绝本机与内网地址（按**字面量**判断，不做 DNS 预解析）。
pub fn normalize_host(raw: &str) -> Result<String, CoreError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(CoreError::validation("Host 不能为空"));
    }

    // 1. 拆协议：只允许 http/https，其余（ftp:// 等）直接拒绝。
    let rest = match trimmed.find("://") {
        Some(index) => {
            let scheme = &trimmed[..index];
            if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
                return Err(CoreError::validation("Host 只支持 http 或 https"));
            }
            &trimmed[index + 3..]
        }
        None => trimmed,
    };

    // 2. 查询串与片段在 Host 里没有意义（也常被用来夹带凭据），直接拒绝。
    if rest.contains('?') || rest.contains('#') {
        return Err(CoreError::validation("Host 不能包含查询串或片段"));
    }

    // 3. 端点路径由程序决定，Host 只保留 authority 部分。
    let authority = rest.split('/').next().unwrap_or("").trim();
    if authority.is_empty() {
        return Err(CoreError::validation("Host 不能为空"));
    }

    // 4. 用户名/密码：拒绝。
    if authority.contains('@') {
        return Err(CoreError::validation("Host 不能包含用户名或密码"));
    }

    // 5. 拆主机名与端口。
    let (host, port) = split_authority(authority)?;

    // 6. 本机与内网：按字面量拒绝。
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare == "localhost" || bare.ends_with(".localhost") || bare.ends_with(".local") {
        return Err(CoreError::validation("Host 不能指向本机"));
    }
    if let Ok(ip) = bare.parse::<std::net::IpAddr>()
        && !is_public_ip(ip)
    {
        return Err(CoreError::validation("Host 不能指向内网或保留地址"));
    }

    // 7. 主机名合法性：只允许字母、数字、'-'、'.'（IPv6 另计）。
    if !host
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' || ch == '[' || ch == ']')
    {
        return Err(CoreError::validation("Host 含有非法字符"));
    }

    let host = host.to_ascii_lowercase();
    Ok(match port {
        Some(port) => format!("https://{host}:{port}"),
        None => format!("https://{host}"),
    })
}

/// 拆 `host[:port]`。IPv6 用方括号（`[::1]`、`[::1]:8443`）。
fn split_authority(authority: &str) -> Result<(String, Option<u16>), CoreError> {
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest
            .find(']')
            .ok_or_else(|| CoreError::validation("Host 的 IPv6 地址格式不正确"))?;
        let host = format!("[{}]", &rest[..end]);
        let tail = &rest[end + 1..];
        let port = match tail.strip_prefix(':') {
            Some(port) => Some(parse_port(port)?),
            None if tail.is_empty() => None,
            None => return Err(CoreError::validation("Host 格式不正确")),
        };
        return Ok((host, port));
    }
    // 未加方括号的 IPv6 无法与端口区分，直接拒绝这种写法。
    if authority.matches(':').count() > 1 {
        return Err(CoreError::validation("IPv6 地址需要用方括号，例如 [::1]"));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Ok((host.to_string(), Some(parse_port(port)?))),
        None => Ok((authority.to_string(), None)),
    }
}

/// 解析端口号。
fn parse_port(text: &str) -> Result<u16, CoreError> {
    let port: u16 = text
        .parse()
        .map_err(|_| CoreError::validation("Host 的端口号不正确"))?;
    if port == 0 {
        return Err(CoreError::validation("Host 的端口号不正确"));
    }
    Ok(port)
}

/// 预览程序实际会请求的地址（不含 Key）。
///
/// 用户只填 Host，路径与协议都由程序决定。这里把最终地址显式展示出来，
/// 免得用户猜「我填的 Host 到底被拼成了什么」。
pub fn preview_request_url(raw_host: &str, path: &str) -> Option<String> {
    let host = normalize_host(raw_host).ok()?;
    Some(format!("{host}{path}"))
}

/// 地址是否为公网地址。
///
/// 拒绝：回环、私网、链路本地、CGNAT（100.64.0.0/10）、未指定的 IPv6、
/// IPv6 ULA（fc00::/7）与 IPv4 保留段。
pub fn is_public_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
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
    /// 具体地点名（可能是区/县/县级市，如「龙泉驿」）。
    pub name: String,
    /// 省 / 州。
    pub admin1: Option<String>,
    /// 上级行政区（`adm2`）：该地点的**上级城市**。
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

impl WeatherLocation {
    /// 展示用标题：`具体地点 · 上级城市 · 省`。
    pub fn title(&self) -> String {
        location_title(&self.name, self.admin2.as_deref(), self.admin1.as_deref())
    }
}

/// 城市搜索返回的候选地点（尚未保存）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WeatherLocationCandidate {
    pub provider: String,
    pub provider_location_id: String,
    /// 具体地点名（可能是区/县/县级市，如「龙泉驿」）。
    pub name: String,
    /// 一级行政区（省 / 州 / 直辖市）。
    pub admin1: Option<String>,
    /// 上级行政区（`adm2`）：具体地点的**上级城市**。
    ///
    /// 和风天气搜索「成都」会返回成都本级与锦江区、龙泉驿、都江堰市等
    /// 下辖区县，它们的 `adm2` 都是「成都」——所以只显示 `adm2` 会让
    /// 十几个候选看起来一模一样。
    pub admin2: Option<String>,
    pub country: Option<String>,
    pub latitude: f64,
    pub longitude: f64,
    pub timezone: String,
}

impl WeatherLocationCandidate {
    /// 展示用标题：`具体地点 · 上级城市 · 省`。
    pub fn title(&self) -> String {
        location_title(&self.name, self.admin2.as_deref(), self.admin1.as_deref())
    }
}

/// 地点展示标题：`具体地点 · 上级城市 · 省`。
///
/// 逐级回退且**去重**：直辖市的 `adm1` 与 `adm2` 同为「北京市」，
/// 重复显示没有意义；市本级的 `name` 与 `adm2` 同为「成都」时也只显示一次。
///
/// 和风天气的最小行政单位是**区/县/县级市**（搜「成都」会返回成都本级
/// 加上锦江区、龙泉驿、都江堰市等一串下辖区县），所以标题必须带上
/// `name`——只显示 `adm2 · adm1` 会让这些候选看起来一模一样。
pub fn location_title(name: &str, admin2: Option<&str>, admin1: Option<&str>) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in [Some(name), admin2, admin1] {
        let text = part.map(str::trim).filter(|text| !text.is_empty());
        if let Some(text) = text
            && !parts.contains(&text)
        {
            parts.push(text);
        }
    }
    if parts.is_empty() {
        return name.to_string();
    }
    parts.join(" · ")
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
    /// 图标码有两种形态：
    /// - 三位纯数字（`"100"`、`"150"`）；
    /// - 带昼夜后缀（`"100d"`、`"101n"`）。
    ///
    /// 两者都要认，否则夜间时段会整片落进 [`WeatherKind::Unknown`]。
    ///
    /// 码表对齐官方「天气现象」文档（`condition.code`）与其图标对照表：
    /// `100` 晴、`104` 阴、`300` 阵雨、`305` 小雨、`400` 小雪、`500` 薄雾、
    /// `502` 霾、`900` 热……夜间码是同一套现象的夜版（`150` 晴、`153` 晴间多云、
    /// `154` 阴、`350` 阵雨、`456`/`457` 雨雪……）。
    ///
    /// 认不出的码归 [`WeatherKind::Unknown`]，**不报错**：图标码会随服务商
    /// 增加而变，界面上显示服务商给的文字描述即可。
    pub fn from_icon_code(code: &str) -> Self {
        // 先剥掉昼夜后缀（`"100d"` → `"100"`），再按纯数字码表匹配。
        let trimmed = code.trim();
        let digits = trimmed
            .strip_suffix(['d', 'D', 'n', 'N'])
            .filter(|rest| !rest.is_empty())
            .unwrap_or(trimmed);

        match digits {
            // 晴（含夜间晴 `150`、夜间晴间多云 `153`）。
            "100" | "150" => WeatherKind::Clear,
            // 多云 / 少云 / 晴间多云。
            "101" | "102" | "103" | "153" => WeatherKind::PartlyCloudy,
            // 阴（`154` 是夜间阴）。
            "104" | "154" => WeatherKind::Cloudy,
            // 雾 / 薄雾 / 浓雾 / 大雾。
            "500" | "501" | "509" | "510" | "514" | "515" => WeatherKind::Fog,
            // 霾。
            "502" | "511" | "512" | "513" => WeatherKind::Haze,
            // 扬沙 / 浮尘 / 沙尘暴。
            "503" | "504" | "507" | "508" => WeatherKind::Dust,
            // 毛毛雨 / 细雨（量级最小，单独归类）。
            "309" => WeatherKind::Drizzle,
            // 雨（阵雨、小中大雨、暴雨…）。
            "300" | "301" | "305" | "306" | "307" | "308" | "310" | "311" | "312" | "314"
            | "315" | "316" | "317" | "318" | "350" | "351" | "399" => WeatherKind::Rain,
            // 雷暴（强对流）。
            "302" | "303" | "900" | "901" | "902" => WeatherKind::Thunderstorm,
            // 雷阵雨伴有冰雹。
            "304" => WeatherKind::Hail,
            // 雪（小雪、中雪、大雪、暴雪）。
            "400" | "401" | "402" | "403" | "407" | "408" | "409" | "410" | "457" | "499" => {
                WeatherKind::Snow
            }
            // 雨夹雪 / 冻雨 / 雨雪天气。
            "313" | "404" | "405" | "406" | "456" => WeatherKind::Sleet,
            // 热 / 冷：不画特殊图形，按未知处理（文字描述里会写明「热」/「冷」）。
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
    /// 白天的原始图标码（如 `100d`）。
    ///
    /// 保留原始码是为了两件事：一是**夜间图标与白天不同**
    /// （`100` 晴 / `150` 夜间晴），二是将来换成服务商的官方图标资源时
    /// 不必再改数据层。
    #[serde(default)]
    pub icon_day: String,
    /// 夜间的原始图标码（如 `150n`）。
    #[serde(default)]
    pub icon_night: String,
    /// 夜间天气文字描述（`textNight`）。
    #[serde(default)]
    pub text_night: String,
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
    ///
    /// **版本 2**：逐日预报新增原始图标码（`icon_day`/`icon_night`）与
    /// 夜间文字（`text_night`）。版本 1 的逐日预报读错了服务商的字段名
    /// （把 `iconDay` 当成 `icon`），导致 7 天天气全部退化成「未知」——
    /// 这批缓存继续用只会显示错的天气，所以版本号一升就整体作废。
    pub const VERSION: u32 = 2;

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
///
/// **Host 是每次调用的参数**，不是数据源的构造参数：用户配置的 Host
/// 只有这一个来源，换 Host 不必重建客户端，测试连接也能真正测到用户
/// 刚填的那个地址。`api_host` 传裸域名或完整 URL 均可，实现层会自行
/// 规范化（补 `https://`）并拒绝内网地址。
pub trait WeatherProvider: Send + Sync {
    /// 按城市名搜索候选地点。
    fn search_locations<'a>(
        &'a self,
        api_host: &'a str,
        query: &'a str,
        credential: &'a WeatherCredential,
    ) -> WeatherFuture<'a, Result<Vec<WeatherLocationCandidate>, CoreError>>;

    /// 取某地点的当前天气 + 逐时 + 逐日预报。
    fn forecast<'a>(
        &'a self,
        api_host: &'a str,
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

    /// 造一个候选（字段按和风天气的实际返回填）。
    fn named_candidate(name: &str, adm1: &str, adm2: &str) -> WeatherLocationCandidate {
        WeatherLocationCandidate {
            provider: PROVIDER_QWEATHER.to_string(),
            provider_location_id: "101270101".to_string(),
            name: name.to_string(),
            admin1: Some(adm1.to_string()),
            admin2: Some(adm2.to_string()),
            country: Some("中国".to_string()),
            latitude: 30.6,
            longitude: 104.0,
            timezone: "Asia/Shanghai".to_string(),
        }
    }

    /// 回归：和风天气的 `adm2` 是**上级城市**，不是地点本身。
    ///
    /// 搜「成都」会返回成都本级 + 锦江区/龙泉驿/都江堰市等下辖区县，
    /// 它们的 `adm2` 全是「成都」。早先标题只显示 `adm2 · adm1`，
    /// 于是十几个候选全都显示成「成都 · 四川省」，用户没法挑。
    /// 标题必须带上**具体地点名**。
    #[test]
    fn candidate_title_shows_the_specific_place_not_just_the_parent_city() {
        let district = named_candidate("都江堰", "四川省", "成都");
        assert_eq!(
            district.title(),
            "都江堰 · 成都 · 四川省",
            "区/县级市必须显示自己的名字"
        );

        let city = named_candidate("成都", "四川省", "成都");
        assert_eq!(
            city.title(),
            "成都 · 四川省",
            "市本级与上级城市同名时只显示一次"
        );
    }

    /// 直辖市的 `adm1` 与 `adm2` 同名，不该重复显示。
    #[test]
    fn candidate_title_drops_duplicated_admin_levels() {
        let beijing = named_candidate("东城", "北京市", "北京市");
        assert_eq!(beijing.title(), "东城 · 北京市");
    }

    /// 缺上级行政区时逐级回退，不能显示成空串。
    #[test]
    fn candidate_title_falls_back_when_admin_levels_are_missing() {
        let mut candidate = named_candidate("拉萨", "西藏自治区", "拉萨");
        candidate.admin2 = None;
        assert_eq!(candidate.title(), "拉萨 · 西藏自治区");

        let mut candidate = named_candidate("拉萨", "西藏自治区", "拉萨");
        candidate.admin1 = None;
        candidate.admin2 = None;
        assert_eq!(candidate.title(), "拉萨");
    }

    #[test]
    fn icon_codes_map_to_kinds() {
        // 映射严格对齐官方「天气现象」码表：
        // 104 是**阴**（不是多云），302 是**雷阵雨**（不是雨），
        // 305 是**小雨**（不是雷暴），312 是**特大暴雨**（不是冰雹）。
        assert_eq!(WeatherKind::from_icon_code("100"), WeatherKind::Clear);
        assert_eq!(WeatherKind::from_icon_code("104"), WeatherKind::Cloudy);
        assert_eq!(WeatherKind::from_icon_code("300"), WeatherKind::Rain);
        assert_eq!(
            WeatherKind::from_icon_code("302"),
            WeatherKind::Thunderstorm
        );
        assert_eq!(WeatherKind::from_icon_code("306"), WeatherKind::Rain);
        assert_eq!(
            WeatherKind::from_icon_code("305"),
            WeatherKind::Rain,
            "305 是小雨，不是雷暴"
        );
        assert_eq!(WeatherKind::from_icon_code("312"), WeatherKind::Rain);
        assert_eq!(WeatherKind::from_icon_code("304"), WeatherKind::Hail);
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
        // 版本号跟随常量，避免以后升版本时漏改这里。
        assert!(
            json.contains(&format!("\"v\":{}", StoredWeatherSnapshot::VERSION)),
            "序列化结果应带上当前版本号：{json}"
        );
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

    #[test]
    fn host_normalization_accepts_bare_domain() {
        // 控制台复制到的就是裸域名：必须能直接用。
        assert_eq!(
            normalize_host("abcxyz.qweatherapi.com").unwrap(),
            "https://abcxyz.qweatherapi.com"
        );
        // 带 https 前缀、结尾斜杠、路径都要归一到同一个结果。
        for input in [
            "https://abcxyz.qweatherapi.com",
            "https://abcxyz.qweatherapi.com/",
            "  https://abcxyz.qweatherapi.com/  ",
            "https://abcxyz.qweatherapi.com/v7/weather/now",
            "abcxyz.qweatherapi.com/v7",
        ] {
            assert_eq!(
                normalize_host(input).unwrap(),
                "https://abcxyz.qweatherapi.com",
                "输入 {input} 应归一到同一 Host"
            );
        }
        // 协议由程序决定：http 会被升级成 https。
        assert_eq!(
            normalize_host("http://abcxyz.qweatherapi.com").unwrap(),
            "https://abcxyz.qweatherapi.com"
        );
        // 主机名统一小写。
        assert_eq!(
            normalize_host("ABCXYZ.QWeatherAPI.com").unwrap(),
            "https://abcxyz.qweatherapi.com"
        );
        // 端口保留。
        assert_eq!(
            normalize_host("abcxyz.qweatherapi.com:8443").unwrap(),
            "https://abcxyz.qweatherapi.com:8443"
        );
    }

    #[test]
    fn host_normalization_rejects_dangerous_input() {
        // 空
        assert!(normalize_host("   ").is_err());
        // 非 http(s) 协议
        assert!(normalize_host("ftp://abc.com").is_err());
        // 查询串 / 片段 / 凭据
        assert!(normalize_host("https://abc.com?key=secret").is_err());
        assert!(normalize_host("https://abc.com#frag").is_err());
        assert!(normalize_host("https://user:pass@abc.com").is_err());
        // 本机与内网（按字面量）
        assert!(normalize_host("localhost").is_err());
        assert!(normalize_host("foo.localhost").is_err());
        assert!(normalize_host("127.0.0.1").is_err());
        assert!(normalize_host("10.1.2.3").is_err());
        assert!(normalize_host("192.168.0.1").is_err());
        assert!(normalize_host("172.20.0.1").is_err());
        assert!(normalize_host("169.254.1.1").is_err());
        assert!(normalize_host("100.64.0.1").is_err());
        assert!(normalize_host("[::1]").is_err());
        assert!(normalize_host("[fd00::1]").is_err());
        // 非法字符与端口
        assert!(normalize_host("abc com").is_err());
        assert!(normalize_host("abc.com:0").is_err());
        assert!(normalize_host("abc.com:99999").is_err());
        assert!(normalize_host("abc.com:notaport").is_err());
    }

    /// 图标码 → 天气种类，必须严格对齐官方「天气现象」码表。
    ///
    /// 这张表是卡片背景图能反映天气的根本依据：
    /// 任何一条映射错了，那一天的背景图就会画成别的天气。
    #[test]
    fn icon_codes_follow_the_official_weather_table() {
        // 晴（含夜间晴 150）。
        assert_eq!(WeatherKind::from_icon_code("100"), WeatherKind::Clear);
        assert_eq!(WeatherKind::from_icon_code("150"), WeatherKind::Clear);
        // 多云 / 少云 / 晴间多云（含夜间 153）。
        assert_eq!(
            WeatherKind::from_icon_code("101"),
            WeatherKind::PartlyCloudy
        );
        assert_eq!(
            WeatherKind::from_icon_code("102"),
            WeatherKind::PartlyCloudy
        );
        assert_eq!(
            WeatherKind::from_icon_code("103"),
            WeatherKind::PartlyCloudy
        );
        assert_eq!(
            WeatherKind::from_icon_code("153"),
            WeatherKind::PartlyCloudy
        );
        // 阴（含夜间 154）。
        assert_eq!(WeatherKind::from_icon_code("104"), WeatherKind::Cloudy);
        assert_eq!(WeatherKind::from_icon_code("154"), WeatherKind::Cloudy);
        // 雨：阵雨、小雨、中雨、大雨、暴雨…
        for code in [
            "300", "301", "305", "306", "307", "310", "311", "312", "399",
        ] {
            assert_eq!(
                WeatherKind::from_icon_code(code),
                WeatherKind::Rain,
                "码 {code} 应判为雨"
            );
        }
        // 雷阵雨必须与普通雨分开，否则雷暴图标画不出来。
        assert_eq!(
            WeatherKind::from_icon_code("302"),
            WeatherKind::Thunderstorm
        );
        assert_eq!(
            WeatherKind::from_icon_code("303"),
            WeatherKind::Thunderstorm
        );
        // 冰雹。
        assert_eq!(WeatherKind::from_icon_code("304"), WeatherKind::Hail);
        // 雪。
        for code in ["400", "401", "402", "403", "499"] {
            assert_eq!(
                WeatherKind::from_icon_code(code),
                WeatherKind::Snow,
                "码 {code} 应判为雪"
            );
        }
        // 雨夹雪 / 冻雨。
        assert_eq!(WeatherKind::from_icon_code("404"), WeatherKind::Sleet);
        assert_eq!(WeatherKind::from_icon_code("313"), WeatherKind::Sleet);
        // 雾 / 霾 / 浮尘。
        assert_eq!(WeatherKind::from_icon_code("501"), WeatherKind::Fog);
        assert_eq!(WeatherKind::from_icon_code("502"), WeatherKind::Haze);
        assert_eq!(WeatherKind::from_icon_code("503"), WeatherKind::Dust);
        assert_eq!(WeatherKind::from_icon_code("504"), WeatherKind::Dust);
    }

    /// 图标码可能带昼夜后缀（`"100d"` / `"101n"`），必须一样能认出来。
    ///
    /// 不剥后缀的话，夜间时段的天气会整片落进 `Unknown`。
    #[test]
    fn icon_codes_with_day_night_suffix_are_recognized() {
        assert_eq!(WeatherKind::from_icon_code("100d"), WeatherKind::Clear);
        assert_eq!(WeatherKind::from_icon_code("100n"), WeatherKind::Clear);
        assert_eq!(
            WeatherKind::from_icon_code("101d"),
            WeatherKind::PartlyCloudy
        );
        assert_eq!(WeatherKind::from_icon_code("104n"), WeatherKind::Cloudy);
        assert_eq!(
            WeatherKind::from_icon_code("302n"),
            WeatherKind::Thunderstorm
        );
        assert_eq!(WeatherKind::from_icon_code("400d"), WeatherKind::Snow);
        // 前后空白也要容忍。
        assert_eq!(WeatherKind::from_icon_code("  100d  "), WeatherKind::Clear);
    }

    /// 认不出的码归 `Unknown`，不报错也不误判。
    #[test]
    fn unknown_icon_codes_fall_back_to_unknown() {
        for code in ["", "   ", "999", "abc", "d", "n"] {
            assert_eq!(
                WeatherKind::from_icon_code(code),
                WeatherKind::Unknown,
                "码 {code:?} 应判为未知"
            );
        }
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
}
