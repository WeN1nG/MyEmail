//! 天气用例层集成测试：`Command` → 用例 → 仓储 → `Event`。
//!
//! 用假的数据源，不联网、不碰系统密钥库（凭证直接注入会话内存）。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use me_core::port::MetaRepo;
use me_core::weather::{
    CurrentWeather, DailyForecast, HourlyForecast, PROVIDER_QWEATHER, WeatherCredential,
    WeatherForecast, WeatherKind, WeatherLocationCandidate, WeatherProvider, WeatherSnapshot,
};
use me_core::{Command, CoreError, Event};
use me_store::Store;
use me_sync::SyncService;
use me_sync::events::EventSender;
use tokio::sync::mpsc;

/// 假的数据源：搜索与预报结果都可预设。
struct FakeProvider {
    candidates: Mutex<Vec<WeatherLocationCandidate>>,
    /// 用 `String` 存错误：`CoreError` 不可克隆。
    forecast: Mutex<Result<WeatherForecast, String>>,
    calls: Mutex<usize>,
}

impl FakeProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            candidates: Mutex::new(Vec::new()),
            forecast: Mutex::new(Ok(sample_forecast())),
            calls: Mutex::new(0),
        })
    }
}

impl WeatherProvider for FakeProvider {
    fn search_locations<'a>(
        &'a self,
        _query: &'a str,
        _credential: &'a WeatherCredential,
    ) -> me_core::weather::WeatherFuture<'a, Result<Vec<WeatherLocationCandidate>, CoreError>> {
        Box::pin(async move { Ok(self.candidates.lock().unwrap().clone()) })
    }

    fn forecast<'a>(
        &'a self,
        _location: &'a me_core::weather::WeatherLocation,
        _credential: &'a WeatherCredential,
    ) -> me_core::weather::WeatherFuture<'a, Result<WeatherForecast, CoreError>> {
        Box::pin(async move {
            *self.calls.lock().unwrap() += 1;
            match self.forecast.lock().unwrap().clone() {
                Ok(forecast) => Ok(forecast),
                Err(message) => Err(CoreError::network(true, message)),
            }
        })
    }
}

fn sample_forecast() -> WeatherForecast {
    WeatherForecast {
        current: CurrentWeather {
            observed_at: 1_700_000_000,
            temp_c: 18.5,
            feels_like_c: 17.0,
            humidity_percent: 55,
            wind_scale: 2,
            wind_dir: "北风".into(),
            wind_speed_kmh: 8.0,
            precip_mm: 0.0,
            pressure_hpa: 1015.0,
            visibility_km: 20.0,
            cloud_percent: 30,
            kind: WeatherKind::PartlyCloudy,
            text: "多云".into(),
        },
        hourly: (0..24)
            .map(|i| HourlyForecast {
                time: 1_700_000_000 + i * 3600,
                temp_c: 18.0 + i as f64 * 0.5,
                pop_percent: 10,
                precip_mm: 0.0,
                kind: WeatherKind::PartlyCloudy,
                text: "多云".into(),
            })
            .collect(),
        daily: vec![DailyForecast {
            date: "2026-11-04".into(),
            sunrise: Some("06:30".into()),
            sunset: Some("17:12".into()),
            temp_max_c: 22.0,
            temp_min_c: 11.0,
            precip_mm: 0.0,
            kind: WeatherKind::Clear,
            text: "晴".into(),
        }],
    }
}

fn candidate(name: &str, id: &str) -> WeatherLocationCandidate {
    WeatherLocationCandidate {
        provider: PROVIDER_QWEATHER.to_string(),
        provider_location_id: id.to_string(),
        name: name.to_string(),
        admin1: Some("北京市".into()),
        admin2: None,
        country: Some("中国".into()),
        latitude: 39.9,
        longitude: 116.4,
        timezone: "Asia/Shanghai".into(),
    }
}

fn start_service(
    store: &Store,
    provider: Option<Arc<FakeProvider>>,
) -> (mpsc::UnboundedSender<Command>, mpsc::Receiver<Event>) {
    let (command_tx, command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, event_rx) = mpsc::channel::<Event>(1024);
    let events = EventSender::new(event_tx, Arc::new(|| {}));
    let mut service = SyncService::new(
        store.writable(),
        me_proto::ProtoFactory,
        me_proto::ProtoSmtp,
        store.paths(),
        events,
        Arc::new(me_sync::NoopBrowser),
    );
    if let Some(provider) = provider {
        service = service.with_weather_provider(provider);
    }
    service = service.with_weather_credential(WeatherCredential::new("test-key"));
    // 测试里把最小间隔设为 0，避免被 60 秒节流挡住。
    service = service.with_weather_min_interval(0);
    tokio::spawn(async move { service.run(command_rx).await });
    (command_tx, event_rx)
}

async fn wait_for(
    rx: &mut mpsc::Receiver<Event>,
    mut predicate: impl FnMut(&Event) -> bool,
) -> Event {
    let deadline = Duration::from_secs(5);
    let started = std::time::Instant::now();
    loop {
        let remaining = deadline
            .checked_sub(started.elapsed())
            .expect("等待事件超时");
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(event)) => {
                if predicate(&event) {
                    return event;
                }
            }
            Ok(None) => panic!("事件通道已关闭"),
            Err(_) => panic!("等待事件超时"),
        }
    }
}

#[tokio::test]
async fn searching_cities_emits_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let provider = FakeProvider::new();
    provider
        .candidates
        .lock()
        .unwrap()
        .push(candidate("北京", "101010100"));
    let (tx, mut rx) = start_service(&store, Some(provider));

    tx.send(Command::SearchWeatherLocations {
        request_id: 7,
        query: "北京".into(),
    })
    .unwrap();

    let event = wait_for(&mut rx, |event| {
        matches!(event, Event::WeatherLocationCandidates { .. })
    })
    .await;
    match event {
        Event::WeatherLocationCandidates {
            request_id,
            candidates,
        } => {
            assert_eq!(request_id, 7);
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].provider_location_id, "101010100");
        }
        other => panic!("事件类型不对：{other:?}"),
    }
}

#[tokio::test]
async fn adding_a_location_persists_it_and_refreshes_weather() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let provider = FakeProvider::new();
    let (tx, mut rx) = start_service(&store, Some(provider.clone()));

    tx.send(Command::AddWeatherLocation {
        candidate: candidate("北京", "101010100"),
    })
    .unwrap();

    wait_for(&mut rx, |event| {
        matches!(event, Event::WeatherLocationsChanged)
    })
    .await;
    let updated = wait_for(&mut rx, |event| {
        matches!(event, Event::WeatherUpdated { .. })
    })
    .await;
    match updated {
        Event::WeatherUpdated {
            location_id,
            fetched_at,
            expires_at,
        } => {
            assert!(expires_at > fetched_at, "过期时间应晚于抓取时间");
            // 快照已落库。
            let snapshot =
                me_core::port::WeatherRepo::get_weather_snapshot(&store.writable(), location_id)
                    .unwrap()
                    .expect("应写入快照");
            assert_eq!(snapshot.current.temp_c, 18.5);
            assert_eq!(snapshot.hourly.len(), 24);
        }
        other => panic!("事件类型不对：{other:?}"),
    }
    assert_eq!(*provider.calls.lock().unwrap(), 1, "应只请求一次预报");
}

#[tokio::test]
async fn failed_refresh_keeps_the_cached_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let provider = FakeProvider::new();
    let (tx, mut rx) = start_service(&store, Some(provider.clone()));

    // 先成功一次，拿到缓存。
    tx.send(Command::AddWeatherLocation {
        candidate: candidate("北京", "101010100"),
    })
    .unwrap();
    wait_for(&mut rx, |event| {
        matches!(event, Event::WeatherUpdated { .. })
    })
    .await;

    // 再让数据源失败。
    *provider.forecast.lock().unwrap() = Err("连接超时".to_string());
    tx.send(Command::RefreshWeather { force: true }).unwrap();

    let failed = wait_for(&mut rx, |event| {
        matches!(event, Event::WeatherFailed { .. })
    })
    .await;
    match failed {
        Event::WeatherFailed { message, has_cache } => {
            assert!(has_cache, "本地仍有缓存，应提示而不是空白");
            assert!(!message.is_empty());
        }
        other => panic!("事件类型不对：{other:?}"),
    }

    // 缓存仍在。
    let locations = me_core::port::WeatherRepo::list_weather_locations(&store.writable()).unwrap();
    assert_eq!(locations.len(), 1);
    let snapshot =
        me_core::port::WeatherRepo::get_weather_snapshot(&store.writable(), locations[0].id)
            .unwrap()
            .expect("失败后应保留旧快照");
    assert_eq!(snapshot.current.temp_c, 18.5);
}

#[tokio::test]
async fn refresh_without_key_reports_configuration_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let provider = FakeProvider::new();
    // 不注入凭证：模拟「用户还没配置 Key」。
    let (command_tx, command_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, mut event_rx) = mpsc::channel::<Event>(1024);
    let events = EventSender::new(event_tx, Arc::new(|| {}));
    let service = SyncService::new(
        store.writable(),
        me_proto::ProtoFactory,
        me_proto::ProtoSmtp,
        store.paths(),
        events,
        Arc::new(me_sync::NoopBrowser),
    )
    .with_weather_provider(provider.clone())
    // 空凭证 = 明确未配置（不去读系统密钥库，避免受本机环境影响）。
    .with_weather_credential(WeatherCredential::new(""))
    .with_weather_min_interval(0);
    tokio::spawn(async move { service.run(command_rx).await });

    // 先加一个地点：没有 Key 时刷新应当提示「配置 Key」而不是「没有地点」。
    command_tx
        .send(Command::AddWeatherLocation {
            candidate: candidate("北京", "101010100"),
        })
        .unwrap();
    let failed = wait_for(&mut event_rx, |event| {
        matches!(event, Event::WeatherFailed { .. })
    })
    .await;
    match failed {
        Event::WeatherFailed { message, .. } => {
            assert!(message.contains("Key"), "应提示配置 Key，实际 {message}");
        }
        other => panic!("事件类型不对：{other:?}"),
    }

    command_tx
        .send(Command::RefreshWeather { force: true })
        .unwrap();
    let failed = wait_for(&mut event_rx, |event| {
        matches!(event, Event::WeatherFailed { .. })
    })
    .await;
    match failed {
        Event::WeatherFailed { message, has_cache } => {
            assert!(message.contains("Key"), "应提示配置 Key，实际 {message}");
            assert!(!has_cache);
        }
        other => panic!("事件类型不对：{other:?}"),
    }
    assert_eq!(*provider.calls.lock().unwrap(), 0, "没有 Key 时不应发请求");
}

#[tokio::test]
async fn stale_search_results_are_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let provider = FakeProvider::new();
    provider
        .candidates
        .lock()
        .unwrap()
        .push(candidate("北京", "101010100"));
    let (tx, mut rx) = start_service(&store, Some(provider));

    // 先发一个旧请求号，再发新的；旧结果不应再冒出来。
    tx.send(Command::SearchWeatherLocations {
        request_id: 1,
        query: "北".into(),
    })
    .unwrap();
    tx.send(Command::SearchWeatherLocations {
        request_id: 2,
        query: "北京".into(),
    })
    .unwrap();

    // 收集若干事件：最终应该只看到 request_id = 2 的结果。
    let mut seen = Vec::new();
    for _ in 0..6 {
        let event = wait_for(&mut rx, |event| {
            matches!(
                event,
                Event::WeatherLocationCandidates { .. } | Event::WeatherFailed { .. }
            )
        })
        .await;
        if let Event::WeatherLocationCandidates { request_id, .. } = event {
            seen.push(request_id);
            break;
        }
    }
    assert_eq!(seen, vec![2], "只应接受最新一次请求的结果");
}

#[tokio::test]
async fn empty_api_key_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (tx, mut rx) = start_service(&store, None);

    tx.send(Command::SetWeatherConfig {
        host: "https://devapi.qweather.com".into(),
        key: Some("   ".into()),
    })
    .unwrap();
    let failed = wait_for(&mut rx, |event| {
        matches!(event, Event::WeatherFailed { .. })
    })
    .await;
    match failed {
        Event::WeatherFailed { message, .. } => {
            assert!(message.contains("不能为空"), "实际 {message}");
        }
        other => panic!("事件类型不对：{other:?}"),
    }
}

#[tokio::test]
async fn invalid_host_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (tx, mut rx) = start_service(&store, None);

    tx.send(Command::SetWeatherConfig {
        host: "http://127.0.0.1".into(),
        key: None,
    })
    .unwrap();
    let failed = wait_for(&mut rx, |event| {
        matches!(event, Event::WeatherFailed { .. })
    })
    .await;
    match failed {
        Event::WeatherFailed { message, .. } => {
            assert!(
                message.contains("https"),
                "应说明必须用 https，实际 {message}"
            );
        }
        other => panic!("事件类型不对：{other:?}"),
    }
}

#[tokio::test]
async fn snapshot_is_reused_when_still_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let provider = FakeProvider::new();
    let (tx, mut rx) = start_service(&store, Some(provider.clone()));

    tx.send(Command::AddWeatherLocation {
        candidate: candidate("北京", "101010100"),
    })
    .unwrap();
    wait_for(&mut rx, |event| {
        matches!(event, Event::WeatherUpdated { .. })
    })
    .await;

    // 快照未过期，普通刷新应直接复用，不发请求。
    tx.send(Command::RefreshWeather { force: false }).unwrap();
    let updated = wait_for(&mut rx, |event| {
        matches!(event, Event::WeatherUpdated { .. })
    })
    .await;
    assert!(matches!(updated, Event::WeatherUpdated { .. }));
    assert_eq!(
        *provider.calls.lock().unwrap(),
        1,
        "未过期时不应再次请求数据源"
    );
}

#[tokio::test]
async fn weather_snapshot_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let provider = FakeProvider::new();
    let (tx, mut rx) = start_service(&store, Some(provider));
    tx.send(Command::AddWeatherLocation {
        candidate: candidate("北京", "101010100"),
    })
    .unwrap();
    wait_for(&mut rx, |event| {
        matches!(event, Event::WeatherUpdated { .. })
    })
    .await;

    // 重新打开同一个库，缓存应仍在。
    let reopened = Store::open(dir.path()).unwrap();
    let locations =
        me_core::port::WeatherRepo::list_weather_locations(&reopened.writable()).unwrap();
    assert_eq!(locations.len(), 1);
    let snapshot =
        me_core::port::WeatherRepo::get_weather_snapshot(&reopened.writable(), locations[0].id)
            .unwrap()
            .expect("重启后缓存应保留");
    assert_eq!(snapshot.provider, PROVIDER_QWEATHER);
    assert_eq!(snapshot.daily.len(), 1);
    let _: &WeatherSnapshot = &snapshot;
}

#[tokio::test]
async fn connection_test_reports_success() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let provider = FakeProvider::new();
    provider
        .candidates
        .lock()
        .unwrap()
        .push(candidate("北京", "101010100"));
    let (tx, mut rx) = start_service(&store, Some(provider));

    tx.send(Command::TestWeatherConnection {
        host: "https://abc.qweatherapi.com".into(),
        key: "test-key".into(),
    })
    .unwrap();
    let tested = wait_for(&mut rx, |event| {
        matches!(event, Event::WeatherConnectionTested { .. })
    })
    .await;
    match tested {
        Event::WeatherConnectionTested { ok, message } => {
            assert!(ok, "应测试成功，实际 {message}");
            assert!(message.contains("连接成功"), "实际 {message}");
        }
        other => panic!("事件类型不对：{other:?}"),
    }
}

#[tokio::test]
async fn connection_test_does_not_persist_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let provider = FakeProvider::new();
    let (tx, mut rx) = start_service(&store, Some(provider));

    tx.send(Command::TestWeatherConnection {
        host: "https://abc.qweatherapi.com".into(),
        key: "test-key".into(),
    })
    .unwrap();
    wait_for(&mut rx, |event| {
        matches!(event, Event::WeatherConnectionTested { .. })
    })
    .await;

    // 测试连接**不落库**：Host 不该被写进设置。
    let stored = store
        .writable()
        .get_setting(me_core::settings_key::WEATHER_API_HOST)
        .unwrap();
    assert!(stored.is_none(), "测试连接不应写入 Host，实际 {stored:?}");
}

#[tokio::test]
async fn connection_test_rejects_invalid_host() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (tx, mut rx) = start_service(&store, None);

    tx.send(Command::TestWeatherConnection {
        host: "http://127.0.0.1".into(),
        key: "test-key".into(),
    })
    .unwrap();
    let tested = wait_for(&mut rx, |event| {
        matches!(event, Event::WeatherConnectionTested { .. })
    })
    .await;
    match tested {
        Event::WeatherConnectionTested { ok, message } => {
            assert!(!ok, "应测试失败");
            assert!(
                message.contains("https"),
                "应说明必须用 https，实际 {message}"
            );
        }
        other => panic!("事件类型不对：{other:?}"),
    }
}

#[tokio::test]
async fn connection_test_requires_a_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (tx, mut rx) = start_service(&store, None);

    tx.send(Command::TestWeatherConnection {
        host: "https://abc.qweatherapi.com".into(),
        key: "   ".into(),
    })
    .unwrap();
    let tested = wait_for(&mut rx, |event| {
        matches!(event, Event::WeatherConnectionTested { .. })
    })
    .await;
    match tested {
        Event::WeatherConnectionTested { ok, message } => {
            assert!(!ok, "应测试失败");
            assert!(message.contains("Key"), "实际 {message}");
        }
        other => panic!("事件类型不对：{other:?}"),
    }
}

#[tokio::test]
async fn saving_config_stores_only_the_single_host() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let (tx, mut rx) = start_service(&store, None);

    tx.send(Command::SetWeatherConfig {
        host: "https://abc.qweatherapi.com".into(),
        key: Some("test-key".into()),
    })
    .unwrap();
    // 注意：启动时 `load_settings` 也会发一条 `WeatherConfigChanged`（用默认 Host），
    // 所以这里要精确匹配 Host，不能只匹配事件类型。
    let changed = wait_for(&mut rx, |event| {
        matches!(
            event,
            Event::WeatherConfigChanged { api_host, .. }
                if api_host == "https://abc.qweatherapi.com"
        )
    })
    .await;
    match changed {
        Event::WeatherConfigChanged {
            key_present,
            api_host,
        } => {
            assert!(key_present, "应记为已配置 Key");
            assert_eq!(api_host, "https://abc.qweatherapi.com");
        }
        other => panic!("事件类型不对：{other:?}"),
    }

    // 只应写一个 Host 设置项。
    let stored = store
        .writable()
        .get_setting(me_core::settings_key::WEATHER_API_HOST)
        .unwrap();
    assert_eq!(
        stored.as_deref(),
        Some("https://abc.qweatherapi.com"),
        "Host 应被持久化"
    );
}
