//! 天气用例：城市搜索、地点管理、刷新编排与密钥存取。
//!
//! 分层约束（`doc/v03架构设计.md` §2.7）：这里是**唯一**能写天气地点与快照的
//! 地方；界面只发 `Command`，写操作全部收敛到本文件。
//!
//! 安全约束（§6.3）：
//! - API Key 只进系统密钥库或会话内存，**不进数据库、不进事件载荷、不写日志**；
//! - 城市名与坐标不写日志；
//! - 失败时保留旧快照，界面显示缓存而不是空白。
//!
//! 额度约束：最小间隔与退避**按地点分别计算**——某个城市连续失败不能拖累
//! 其他城市，刚刷新完一个城市也不该挡住切换到另一个城市。
//! 三次后停止自动重试（手动刷新仍可用）。

use std::collections::HashMap;
use std::sync::Arc;

use me_core::weather::{
    MAX_API_KEY_LEN, MIN_REFRESH_INTERVAL_SECS, PROVIDER_QWEATHER, WeatherCredential,
    WeatherLocation, WeatherLocationCandidate, WeatherLocationId, WeatherProvider, WeatherSnapshot,
    is_expired, validate_candidate, validate_query,
};
use me_core::{CoreError, Event, now_unix};

use crate::SyncService;

/// 密钥库里的服务名与用户名（与邮件账户的 `MyEmail` 分开）。
pub const WEATHER_KEYRING_SERVICE: &str = "MyEmail.Weather";
pub const WEATHER_KEYRING_USER: &str = "qweather";

/// 退避序列（秒）：连续失败后逐级拉长。
const BACKOFF_SECS: [i64; 3] = [30, 120, 300];
/// 连续失败多少次后不再自动重试。
const MAX_AUTO_RETRIES: u32 = 3;

/// 单个地点的运行时状态。
///
/// **按地点分别计算**：多地点时，某城失败后的退避不该让其他城市也退避，
/// 刚刷新完一个城市也不该把切换到另一个城市的请求挡在最小间隔之外。
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct LocationRuntime {
    /// 该地点是否有刷新在途（同一地点单飞）。
    pub(crate) refreshing: bool,
    /// 连续失败次数（决定退避时长）。
    pub(crate) failures: u32,
    /// 退避截止时间（UTC 秒）。
    pub(crate) backoff_until: i64,
    /// 最近一次请求时间（UTC 秒），用于最小间隔。
    pub(crate) last_request_at: i64,
}

/// 会话内的天气运行时状态。
pub(crate) struct WeatherState {
    /// keyring 不可用时的会话内存 Key。
    pub(crate) credential: Option<WeatherCredential>,
    /// 当前生效的 API Host（已规范化为 `https://host[:port]`）。
    ///
    /// Host 是**每次请求的参数**（见 `WeatherProvider`），存在这里是为了
    /// 让「当前该用哪个 Host」有唯一来源：启动时从 `meta` 读回，
    /// 用户改配置时更新。
    pub(crate) api_host: String,
    /// 最近一次搜索的 `request_id`；更早的结果一律丢弃。
    pub(crate) last_search_id: u64,
    /// 两次刷新请求之间的最小间隔（秒）。默认 60；测试可调小。
    pub(crate) min_interval_secs: i64,
    /// 每个地点各自的运行时状态（单飞 / 退避 / 最小间隔）。
    pub(crate) locations: HashMap<WeatherLocationId, LocationRuntime>,
}

impl WeatherState {
    /// 取某地点的运行时状态（不存在时按默认值读，不写入）。
    fn runtime(&self, id: WeatherLocationId) -> LocationRuntime {
        self.locations.get(&id).copied().unwrap_or_default()
    }

    /// 取某地点的运行时状态（不存在则插入默认值）。
    fn runtime_mut(&mut self, id: WeatherLocationId) -> &mut LocationRuntime {
        self.locations.entry(id).or_default()
    }
}

/// 取天气数据源（未配置时返回 `None`）。
pub(crate) fn provider<S, F, M>(service: &SyncService<S, F, M>) -> Option<Arc<dyn WeatherProvider>>
where
    S: me_core::port::StorePort + Clone,
    F: me_core::port::ImapSessionFactory + 'static,
    M: me_core::port::SmtpSender + 'static,
{
    service
        .inner
        .weather_provider
        .read()
        .ok()
        .and_then(|guard| guard.clone())
}

impl Default for WeatherState {
    fn default() -> Self {
        Self {
            credential: None,
            api_host: me_core::DEFAULT_API_HOST.to_string(),
            last_search_id: 0,
            min_interval_secs: MIN_REFRESH_INTERVAL_SECS,
            locations: HashMap::new(),
        }
    }
}

impl<S, F, M> SyncService<S, F, M>
where
    S: me_core::port::StorePort + Clone,
    F: me_core::port::ImapSessionFactory + 'static,
    M: me_core::port::SmtpSender + 'static,
{
    /// 注入天气数据源。
    pub fn with_weather_provider(&self, provider: Arc<dyn WeatherProvider>) -> Self {
        if let Ok(mut guard) = self.inner.weather_provider.write() {
            *guard = Some(provider);
        }
        self.clone()
    }

    /// 调整两次请求之间的最小间隔（秒）。默认 60。
    pub fn with_weather_min_interval(&self, secs: i64) -> Self {
        if let Ok(mut state) = self.inner.weather_state.lock() {
            state.min_interval_secs = secs.max(0);
        }
        self.clone()
    }

    /// 记录一次搜索请求号。**必须在派发时同步调用**：
    /// 否则先发的请求可能先返回，旧结果会被误当成最新结果。
    pub(crate) fn note_weather_search(&self, request_id: u64) {
        if let Ok(mut state) = self.inner.weather_state.lock() {
            state.last_search_id = request_id;
        }
    }

    /// 注入会话内的天气凭证（**不写系统密钥库**）。
    ///
    /// 用于「仅本次会话有效」的场景与测试；正常路径是 [`SyncService::set_weather_api_key`]。
    pub fn with_weather_credential(&self, credential: WeatherCredential) -> Self {
        if let Ok(mut state) = self.inner.weather_state.lock() {
            state.credential = Some(credential);
        }
        self.clone()
    }

    /// 读取天气 API Key：先会话内存，再系统密钥库。
    pub(crate) async fn weather_credential(&self) -> Option<WeatherCredential> {
        if let Some(credential) = self
            .inner
            .weather_state
            .lock()
            .ok()
            .and_then(|s| s.credential.clone())
        {
            // 空凭证表示「明确未配置」，不再去读密钥库。
            return if credential.is_empty() {
                None
            } else {
                Some(credential)
            };
        }
        let value = tokio::task::spawn_blocking(|| {
            let entry = keyring::Entry::new(WEATHER_KEYRING_SERVICE, WEATHER_KEYRING_USER).ok()?;
            match entry.get_password() {
                Ok(value) if !value.trim().is_empty() => Some(value),
                _ => None,
            }
        })
        .await
        .ok()
        .flatten()?;
        let credential = WeatherCredential::new(value);
        if let Ok(mut state) = self.inner.weather_state.lock() {
            state.credential = Some(credential.clone());
        }
        Some(credential)
    }

    /// 保存 Host。校验通过后写 `meta` 并更新会话状态。
    /// 保存天气配置：**一个 Host** + 可选的 API Key。
    ///
    /// `key` 为 `None` 表示「只改 Host，不动已保存的 Key」。
    pub(crate) async fn set_weather_config(&self, host: String, key: Option<String>) {
        // Host 规范化：用户填裸域名即可，程序自己补 https://。
        let api = match me_core::weather::normalize_host(&host) {
            Ok(host) => host,
            Err(err) => {
                self.fail_weather(None, err.user_message(), false).await;
                return;
            }
        };

        // 先写 Key（若给了），再落 Host。Key 写失败不影响 Host 生效。
        if let Some(key) = key {
            let trimmed = key.trim().to_string();
            if trimmed.is_empty() {
                self.fail_weather(None, "API Key 不能为空".to_string(), false)
                    .await;
                return;
            }
            if trimmed.len() > MAX_API_KEY_LEN {
                self.fail_weather(
                    None,
                    format!("API Key 过长（最多 {MAX_API_KEY_LEN} 个字符）"),
                    false,
                )
                .await;
                return;
            }
            let persisted = self.write_weather_key(&trimmed).await;
            if let Ok(mut state) = self.inner.weather_state.lock() {
                state.credential = Some(WeatherCredential::new(trimmed));
                // 换了 Key 之后重新计数，避免沿用旧的退避。
                for runtime in state.locations.values_mut() {
                    runtime.failures = 0;
                    runtime.backoff_until = 0;
                }
            }
            self.emit(Event::WeatherKeyStored { persisted }).await;
        }

        let api_key = api.clone();
        let stored = self
            .db(move |store| {
                store.set_setting(me_core::settings_key::WEATHER_API_HOST, &api_key)?;
                Ok(())
            })
            .await;
        if let Err(err) = stored {
            self.fail_weather(None, err.user_message(), false).await;
            return;
        }

        // 更新会话里的生效 Host：之后每次请求都用它。
        if let Ok(mut state) = self.inner.weather_state.lock() {
            state.api_host = api.clone();
        }

        let key_present = self.weather_credential().await.is_some();
        self.emit(Event::WeatherConfigChanged {
            key_present,
            api_host: api,
        })
        .await;
    }

    /// 测试连接：用给定的 Host 与 Key 发一次真实请求。
    ///
    /// **不落库**：Host 与 Key 只在这一个请求里生效，
    /// 不会覆盖已保存的配置，也不会写进密钥库。
    ///
    /// 探测端点用城市搜索（一个 GET 就够），比拉整套预报省额度。
    pub(crate) async fn test_weather_connection(&self, host: String, key: String) {
        // 用**用户当场填的 Host**（而不是会话里已生效的那个）——
        // 否则测的就不是用户想测的地址了。
        let api = match me_core::weather::normalize_host(&host) {
            Ok(host) => host,
            Err(err) => {
                self.emit(Event::WeatherConnectionTested {
                    ok: false,
                    message: err.user_message(),
                })
                .await;
                return;
            }
        };
        let trimmed = key.trim().to_string();
        if trimmed.is_empty() {
            self.emit(Event::WeatherConnectionTested {
                ok: false,
                message: "请先填写 API Key".to_string(),
            })
            .await;
            return;
        }
        if trimmed.len() > MAX_API_KEY_LEN {
            self.emit(Event::WeatherConnectionTested {
                ok: false,
                message: format!("API Key 过长（最多 {MAX_API_KEY_LEN} 个字符）"),
            })
            .await;
            return;
        }

        // 已注入自定义数据源时用它（测试与自建实现）；否则按用户填的 Host 构造真实数据源。
        let injected = self
            .inner
            .weather_provider
            .read()
            .ok()
            .and_then(|guard| guard.clone());
        let credential = WeatherCredential::new(trimmed);
        let result = match injected {
            Some(provider) => provider.search_locations(&api, "北京", &credential).await,
            None => match me_proto::qweather::QWeatherProvider::new() {
                Ok(provider) => provider.search_locations(&api, "北京", &credential).await,
                Err(err) => {
                    self.emit(Event::WeatherConnectionTested {
                        ok: false,
                        message: err.user_message(),
                    })
                    .await;
                    return;
                }
            },
        };

        let (ok, message) = match result {
            Ok(candidates) if !candidates.is_empty() => (
                true,
                format!("连接成功，已识别 {} 个城市", candidates.len()),
            ),
            Ok(_) => (true, "连接成功（服务返回了空结果，Key 有效）".to_string()),
            Err(err) => (false, err.user_message()),
        };
        self.emit(Event::WeatherConnectionTested { ok, message })
            .await;
    }

    /// 当前生效的 API Host（已规范化）。
    fn api_host(&self) -> String {
        self.inner
            .weather_state
            .lock()
            .map(|state| state.api_host.clone())
            .unwrap_or_else(|_| me_core::DEFAULT_API_HOST.to_string())
    }

    /// 写系统密钥库。返回是否持久化成功（失败时只保留在会话内存）。
    async fn write_weather_key(&self, key: &str) -> bool {
        let key = key.to_string();
        let result = tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new(WEATHER_KEYRING_SERVICE, WEATHER_KEYRING_USER)
                .map_err(|err| err.to_string())?;
            entry.set_password(&key).map_err(|err| err.to_string())
        })
        .await;
        match result {
            Ok(Ok(())) => true,
            Ok(Err(err)) => {
                // 只记错误类别，不记 Key 本身。
                tracing::warn!(%err, "keyring unavailable; weather key kept in session memory only");
                false
            }
            Err(join) => {
                tracing::warn!(%join, "keyring task failed; weather key kept in session memory only");
                false
            }
        }
    }

    /// 按城市名搜索候选地点。
    pub(crate) async fn search_weather_locations(&self, request_id: u64, query: String) {
        let query = match validate_query(&query) {
            Ok(query) => query,
            Err(err) => {
                self.fail_weather(None, err.user_message(), false).await;
                return;
            }
        };
        let Some(provider) = provider(self) else {
            self.fail_weather(None, "天气服务尚未初始化".to_string(), false)
                .await;
            return;
        };
        let Some(credential) = self.weather_credential().await else {
            self.fail_weather(None, "请先配置和风天气 API Key".to_string(), false)
                .await;
            return;
        };
        let api_host = self.api_host();

        match provider
            .search_locations(&api_host, &query, &credential)
            .await
        {
            Ok(candidates) => {
                // 结果回来时再确认一次 request_id 仍是最新。
                let stale = self
                    .inner
                    .weather_state
                    .lock()
                    .map(|state| state.last_search_id != request_id)
                    .unwrap_or(true);
                if stale {
                    return;
                }
                self.emit(Event::WeatherLocationCandidates {
                    request_id,
                    candidates,
                })
                .await;
            }
            Err(err) => {
                self.fail_weather(None, err.user_message(), false).await;
            }
        }
    }

    /// 添加地点。
    pub(crate) async fn add_weather_location(&self, candidate: WeatherLocationCandidate) {
        if let Err(err) = validate_candidate(&candidate) {
            self.fail_weather(None, err.user_message(), false).await;
            return;
        }
        let result = self
            .db(move |store| store.insert_weather_location(&candidate))
            .await;
        match result {
            Ok(location) => {
                self.emit(Event::WeatherLocationsChanged).await;
                // 新加的地点立刻拉一次：用户加它就是为了看它。
                // （早先只在 `is_default` 时刷新，导致第二个及以后的地点
                // 永远没有快照，界面读不出来。）
                self.refresh_weather(location.id, false).await;
            }
            Err(err) => {
                self.fail_weather(None, err.user_message(), false).await;
            }
        }
    }

    /// 删除地点。
    pub(crate) async fn remove_weather_location(&self, id: WeatherLocationId) {
        let result = self
            .db(move |store| store.remove_weather_location(id))
            .await;
        if let Err(err) = result {
            self.fail_weather(Some(id), err.user_message(), false).await;
            return;
        }
        self.emit(Event::WeatherLocationsChanged).await;
    }

    /// 设为默认地点。
    pub(crate) async fn set_default_weather_location(&self, id: WeatherLocationId) {
        let result = self
            .db(move |store| store.set_default_weather_location(id))
            .await;
        if let Err(err) = result {
            self.fail_weather(Some(id), err.user_message(), false).await;
            return;
        }
        self.emit(Event::WeatherLocationsChanged).await;
        self.refresh_weather(id, false).await;
    }

    /// 刷新**指定地点**的天气。
    ///
    /// `id` 指向要刷新的地点：多地点时每个地点各自持有快照，
    /// 刷新其中一个不会影响其他地点。
    ///
    /// `force` 只跳过「未过期直接复用」；最小间隔与退避仍然生效
    /// （两者都按地点分别计算）。
    pub(crate) async fn refresh_weather(&self, id: WeatherLocationId, force: bool) {
        let location = match self.db(move |store| store.get_weather_location(id)).await {
            Ok(Some(location)) => location,
            Ok(None) => {
                self.fail_weather(Some(id), "该地点已被移除".to_string(), false)
                    .await;
                return;
            }
            Err(err) => {
                self.fail_weather(Some(id), err.user_message(), false).await;
                return;
            }
        };

        let now = now_unix();
        {
            let Ok(state) = self.inner.weather_state.lock() else {
                return;
            };
            let runtime = state.runtime(id);
            // 单飞：同一地点正在刷新时直接返回。
            if runtime.refreshing {
                return;
            }
            // 退避中：自动刷新跳过，手动刷新仍然允许。
            if !force && runtime.backoff_until > now {
                return;
            }
        }

        // 快照还在有效期内：直接复用，不发请求（也不受最小间隔限制）。
        let existing = self
            .db(move |store| store.get_weather_snapshot(location.id))
            .await
            .ok()
            .flatten();
        if !force
            && let Some(snapshot) = &existing
            && !is_expired(now, snapshot.expires_at)
        {
            self.emit(Event::WeatherUpdated {
                location_id: location.id,
                fetched_at: snapshot.fetched_at,
                expires_at: snapshot.expires_at,
            })
            .await;
            return;
        }

        // 最小间隔：手动刷新也遵守，避免把额度打光。
        // 按地点分别计时，切换城市不会被上一个城市的请求挡住。
        let blocked = {
            let Ok(mut state) = self.inner.weather_state.lock() else {
                return;
            };
            let min_interval = state.min_interval_secs;
            let runtime = state.runtime_mut(id);
            if runtime.last_request_at > 0 && now - runtime.last_request_at < min_interval {
                true
            } else {
                runtime.refreshing = true;
                runtime.last_request_at = now;
                false
            }
        };
        if blocked {
            // 刚更新过：把现有缓存再发一次，界面至少不会停在「正在更新」。
            match existing {
                Some(snapshot) => {
                    self.emit(Event::WeatherUpdated {
                        location_id: location.id,
                        fetched_at: snapshot.fetched_at,
                        expires_at: snapshot.expires_at,
                    })
                    .await;
                }
                None => {
                    self.fail_weather(
                        Some(location.id),
                        "刚刚更新过，请稍后再试".to_string(),
                        false,
                    )
                    .await;
                }
            }
            return;
        }

        let result = self.do_refresh(&location).await;

        // 状态更新放在同步块里：锁守卫不能跨 `.await`（否则 future 不是 Send）。
        enum Outcome {
            Updated(WeatherSnapshot),
            Failed(String),
        }
        let outcome = {
            let Ok(mut state) = self.inner.weather_state.lock() else {
                return;
            };
            let runtime = state.runtime_mut(id);
            runtime.refreshing = false;
            match result {
                Ok(snapshot) => {
                    runtime.failures = 0;
                    runtime.backoff_until = 0;
                    Outcome::Updated(snapshot)
                }
                Err(err) => {
                    runtime.failures = runtime.failures.saturating_add(1);
                    let index = (runtime.failures as usize).saturating_sub(1);
                    let backoff = BACKOFF_SECS[index.min(BACKOFF_SECS.len() - 1)];
                    runtime.backoff_until = now + backoff;
                    let failures = runtime.failures;
                    let message = if failures >= MAX_AUTO_RETRIES {
                        format!(
                            "{}（连续失败 {} 次，请稍后手动重试）",
                            err.user_message(),
                            failures
                        )
                    } else {
                        err.user_message()
                    };
                    Outcome::Failed(message)
                }
            }
        };

        match outcome {
            Outcome::Updated(snapshot) => {
                self.emit(Event::WeatherUpdated {
                    location_id: location.id,
                    fetched_at: snapshot.fetched_at,
                    expires_at: snapshot.expires_at,
                })
                .await;
            }
            Outcome::Failed(message) => {
                let has_cache = self.has_cache(location.id).await;
                self.fail_weather(Some(location.id), message, has_cache)
                    .await;
            }
        }
    }

    /// 真正发起请求并写快照。三个 GET 由数据源内部并发完成。
    ///
    /// 快照是否可复用已在外层判断，这里只负责请求与落库。
    async fn do_refresh(&self, location: &WeatherLocation) -> Result<WeatherSnapshot, CoreError> {
        let Some(provider) = provider(self) else {
            return Err(CoreError::validation("天气服务尚未初始化"));
        };
        let Some(credential) = self.weather_credential().await else {
            return Err(CoreError::validation("请先配置和风天气 API Key"));
        };

        let location_id = location.id;
        let now = now_unix();
        let api_host = self.api_host();
        let forecast = provider.forecast(&api_host, location, &credential).await?;
        me_core::weather::validate_forecast(&forecast)?;
        let snapshot = WeatherSnapshot::new(PROVIDER_QWEATHER, now, forecast);
        let to_store = snapshot.clone();
        self.db(move |store| store.save_weather_snapshot(location_id, &to_store))
            .await?;
        Ok(snapshot)
    }

    /// 本地是否还有可用缓存。
    async fn has_cache(&self, location_id: WeatherLocationId) -> bool {
        self.db(move |store| store.get_weather_snapshot(location_id))
            .await
            .ok()
            .and_then(|snapshot| snapshot)
            .is_some()
    }

    /// 统一的失败出口。
    ///
    /// `location_id` 指出是哪个地点失败的；`None` 表示与地点无关的全局失败
    /// （如 Host 非法、没配 API Key）。
    async fn fail_weather(
        &self,
        location_id: Option<WeatherLocationId>,
        message: String,
        has_cache: bool,
    ) {
        self.emit(Event::WeatherFailed {
            location_id,
            message,
            has_cache,
        })
        .await;
    }
}
