//! AI 用例层：密钥、请求组装、流式、工具循环、取消与落库。
//!
//! 这是 AI 功能里唯一“有状态”的地方：
//! - **密钥**：系统密钥库（`MyEmail.AI`）或进程内存，绝不落库/落日志；
//! - **流**：一个会话同一时刻只允许一条，全局并发 2；
//! - **工具循环**：最多 4 轮模型 / 8 次工具调用，工具本身在 `ai_tools.rs`。
//!
//! 三条必须守住的不变量：
//! 1. 流式期间只写内存与事件，**终止时一次性落库**（避免每条 delta 抢写连接）；
//! 2. 终止事件不携带全文，UI 收到后从数据库重载（`Event::AiStreamDelta` 可丢）；
//! 3. 工具调用与最终回复**不是同一事务**：日历/记忆写入立即提交，回复失败也不回滚。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use me_core::ai::{
    AiChatRequest, AiContextRef, AiContextRequest, AiConversationId, AiCredential, AiDelta,
    AiDeltaSink, AiEntity, AiMessageId, AiMessageStatus, AiModel, AiProvider, AiProviderConfig,
    AiProviderKind, AiRole, AiToolResult, AiToolTrace, ChatBuildInput, DEFAULT_MAX_INPUT_CHARS,
    MAX_TOOL_CALLS, MAX_TOOL_ROUNDS, MAX_USER_INPUT_CHARS, NewAiEntity, NewAiMessage,
    build_chat_request, find_model, next_model_id, resolve_model, tool_specs, validate_model,
};
use me_core::model::{MessageId, now_unix};
use me_core::port::StorePort;
use me_core::settings_key;
use me_core::{CoreError, Event, MIN_FREE_BYTES};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::SyncService;

/// 单会话流超时上限。
const STREAM_BUDGET: Duration = Duration::from_secs(300);
/// 清单里还有未完成的步、但模型连续空转时，最多催它几次。
///
/// 不设上限的后果很具体：6 轮预算会被「催一句 → 模型说好的 → 再催」耗光，
/// 用户看到的是一条什么都没干成的回复。两次之后就把控制权交回用户。
const MAX_PLAN_NUDGES: u32 = 2;
/// delta 节流：≥50 ms 或 ≥2 KB 发一次。
const DELTA_INTERVAL: Duration = Duration::from_millis(50);
const DELTA_BATCH_BYTES: usize = 2048;
/// 全局并发流上限。
const MAX_CONCURRENT_STREAMS: usize = 2;
/// 历史取多少条参与构建（再按字符预算裁剪）。
const HISTORY_FETCH: u32 = 100;

/// 会话内 AI 状态（进程级，不落库）。
///
/// 模型档案是这里唯一的配置来源：请求参数、工具开关、输入预算都从档案取，
/// 不再有「全局 provider 配置 + 会话 override」两层，避免两处打架。
pub(crate) struct AiState {
    /// 全部模型档案，顺序即界面顺序。
    pub models: Vec<AiModel>,
    /// 当前模型 id（新对话与未绑定档案的旧对话回退到它）。
    pub active: Option<String>,
    /// 隐私确认按**协议**记：换一个同协议的模型不必再确认一遍。
    pub privacy_ack_anthropic: bool,
    pub privacy_ack_openai: bool,
    /// 进行中的流：`conversation -> token`。
    pub streams: HashMap<AiConversationId, StreamEntry>,
    pub slots: Arc<Semaphore>,
}

pub(crate) struct StreamEntry {
    pub token: CancellationToken,
}

impl Default for AiState {
    fn default() -> Self {
        Self {
            models: Vec::new(),
            active: None,
            privacy_ack_anthropic: false,
            privacy_ack_openai: false,
            streams: HashMap::new(),
            slots: Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS)),
        }
    }
}

impl AiState {
    fn privacy_ack(&self, provider: AiProviderKind) -> bool {
        match provider {
            AiProviderKind::Anthropic => self.privacy_ack_anthropic,
            AiProviderKind::OpenAiCompatible => self.privacy_ack_openai,
        }
    }

    /// 一次对话用哪套参数：会话绑定 → 当前模型 → 列表第一个。
    fn model_for(&self, conversation_model: Option<&str>) -> Option<AiModel> {
        resolve_model(&self.models, conversation_model, self.active.as_deref()).cloned()
    }
}

/// 流式文本的节流器：`pending` 是待发送增量，`total` 是完整回复。
struct DeltaThrottle {
    pending: String,
    total: String,
    seq: u64,
    last: Instant,
}

impl DeltaThrottle {
    fn new() -> Self {
        Self {
            pending: String::new(),
            total: String::new(),
            seq: 0,
            // 初始就允许立即发送，避免首字延迟 50ms。
            last: Instant::now() - DELTA_INTERVAL,
        }
    }

    /// 返回应当发出的事件载荷（首次/满 50ms/满 2KB）。
    fn push(&mut self, text: &str) -> Option<(u64, String)> {
        self.total.push_str(text);
        self.pending.push_str(text);
        if self.last.elapsed() >= DELTA_INTERVAL || self.pending.len() >= DELTA_BATCH_BYTES {
            return Some(self.take());
        }
        None
    }

    fn take(&mut self) -> (u64, String) {
        self.seq = self.seq.saturating_add(1);
        self.last = Instant::now();
        (self.seq, std::mem::take(&mut self.pending))
    }
}

impl<S, F, M> SyncService<S, F, M>
where
    S: StorePort + Clone,
    F: me_core::port::ImapSessionFactory + 'static,
    M: me_core::port::SmtpSender + 'static,
{
    /// 注入 AI 传输层（测试用 mock；生产由 `start_native` 注入）。
    pub fn with_ai_provider(&self, provider: Arc<dyn AiProvider>) -> Self {
        if let Ok(mut guard) = self.inner.ai_provider.write() {
            *guard = Some(provider);
        }
        self.clone()
    }

    /// 注入模型档案（测试用；生产由 `load_ai_settings` 从 meta 读回）。
    pub fn with_ai_models(&self, models: Vec<AiModel>, active: Option<String>) -> Self {
        if let Ok(mut state) = self.inner.ai_state.lock() {
            state.models = models;
            state.active = active;
        }
        self.clone()
    }

    /// 跳过隐私确认（只允许测试使用；正常路径必须由用户显式确认）。
    pub fn with_ai_privacy_ack(&self) -> Self {
        if let Ok(mut state) = self.inner.ai_state.lock() {
            state.privacy_ack_anthropic = true;
            state.privacy_ack_openai = true;
        }
        self.clone()
    }

    fn ai_provider(&self) -> Option<Arc<dyn AiProvider>> {
        self.inner
            .ai_provider
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// 模型档案快照（调用方要跨 await 使用时用）。
    fn models_snapshot(&self) -> (Vec<AiModel>, Option<String>) {
        match self.inner.ai_state.lock() {
            Ok(state) => (state.models.clone(), state.active.clone()),
            Err(_) => (Vec::new(), None),
        }
    }

    /// 取这次请求要用的凭证。
    ///
    /// key 就在模型档案里，和其他参数一起读——没有缓存、没有密钥库、
    /// 也没有「档案里改了 key 但缓存还是旧的」这类状态。
    fn ai_credential(model: &AiModel) -> Option<AiCredential> {
        if model.has_key() {
            Some(AiCredential::new(model.api_key.trim().to_string()))
        } else {
            None
        }
    }

    // -----------------------------------------------------------------------
    // 设置
    // -----------------------------------------------------------------------

    /// 启动时读回模型档案、当前模型、隐私确认。
    ///
    /// 首次启动（或从 v0.3 早期的单配置版本升级）时，会把旧配置折成一个档案；
    /// 旧版按协议存的 key 不在这里读（见 [`Self::ai_credential`] 的惰性迁移）。
    pub(crate) async fn load_ai_settings(&self) {
        // 正常启动时内存里没有档案；非空说明已经初始化过（测试注入、或重复调用）。
        // 这时**不能**读 meta 覆盖：meta 里的旧值会把刚注入/刚保存的档案抹掉，
        // 表现为「刚加的模型一刷新就没了」。
        let already_loaded = self
            .inner
            .ai_state
            .lock()
            .map(|state| !state.models.is_empty())
            .unwrap_or(false);
        if already_loaded {
            self.emit_ai_config().await;
            self.repair_orphan_streams().await;
            return;
        }

        struct Settings {
            models: Option<String>,
            active: Option<String>,
            ack: Option<String>,
            legacy_config: Option<String>,
            legacy_tools: Option<String>,
        }

        let loaded = self
            .db(|db| {
                Ok(Settings {
                    models: db.get_setting(settings_key::AI_MODELS)?,
                    active: db.get_setting(settings_key::AI_ACTIVE_MODEL)?,
                    ack: db.get_setting(settings_key::AI_PRIVACY_ACK)?,
                    legacy_config: db.get_setting(settings_key::AI_CONFIG)?,
                    legacy_tools: db.get_setting(settings_key::AI_TOOL_PERMISSIONS)?,
                })
            })
            .await
            .unwrap_or(Settings {
                models: None,
                active: None,
                ack: None,
                legacy_config: None,
                legacy_tools: None,
            });

        // 解析失败**不能**落进下面「建默认档案」的分支：那会把用户存的模型配置
        // （端点、模型名、明文 key）整份抹掉，而且没有任何提示。宁可空着 + 报错。
        let (mut models, models_corrupt): (Vec<AiModel>, bool) = match loaded.models.as_deref() {
            None => (Vec::new(), false),
            Some(raw) => match serde_json::from_str::<Vec<AiModel>>(raw) {
                Ok(models) => (models, false),
                Err(err) => {
                    tracing::error!(error = %err, "AI 模型档案解析失败，保留原值、不回落默认档案");
                    (Vec::new(), true)
                }
            },
        };
        let mut active = loaded.active.clone().filter(|id| !id.trim().is_empty());
        let mut ack_anthropic = false;
        let mut ack_openai = false;

        if let Some(raw) = loaded.ack.as_deref()
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(raw)
        {
            ack_anthropic = value
                .get(AiProviderKind::Anthropic.ack_key())
                .and_then(|v| v.as_i64())
                .is_some_and(|ts| ts > 0);
            ack_openai = value
                .get(AiProviderKind::OpenAiCompatible.ack_key())
                .and_then(|v| v.as_i64())
                .is_some_and(|ts| ts > 0);
        }

        if !models_corrupt && models.is_empty() {
            let legacy: Option<AiProviderConfig> = loaded
                .legacy_config
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok());
            let migration = match legacy {
                Some(config) => {
                    AiModel {
                        id: "m1".to_string(),
                        name: config.kind.label().to_string(),
                        kind: config.kind,
                        base_url: config.base_url,
                        model: config.model,
                        max_input_chars: DEFAULT_MAX_INPUT_CHARS,
                        max_output_tokens: config.max_output_tokens,
                        // 旧版的全局工具开关并进第一份档案：老用户关掉的工具
                        // 不该因为升级就被静默打开。
                        tools: loaded
                            .legacy_tools
                            .as_deref()
                            .and_then(|raw| serde_json::from_str(raw).ok())
                            .unwrap_or_default(),
                        // 旧版 key 只在系统密钥库里，而 key 现在是普通参数，
                        // 读不回来也不编造：留空，界面提示用户重新填一次。
                        api_key: String::new(),
                    }
                }
                None => AiModel::preset(
                    AiProviderKind::Anthropic,
                    "m1".to_string(),
                    "Anthropic".to_string(),
                ),
            };
            active = Some(migration.id.clone());
            models.push(migration);
            self.persist_models(&models, active.as_deref()).await;
        }

        // `active` 指向已删除的档案时归位，否则界面会出现「选中项不存在」。
        if active
            .as_deref()
            .is_some_and(|id| find_model(&models, id).is_none())
        {
            active = models.first().map(|model| model.id.clone());
        }

        if let Ok(mut state) = self.inner.ai_state.lock() {
            state.models = models;
            state.active = active;
            // 用 `|=` 而不是覆盖：测试与「本次会话已确认」的注入不能被
            // 启动读回（meta 里还没有记录）抹掉。
            state.privacy_ack_anthropic |= ack_anthropic;
            state.privacy_ack_openai |= ack_openai;
        }

        // 不在启动路径里读密钥库：Windows 凭据管理器首次访问可能耗时数百毫秒，
        // 而 `run()` 在读完设置前不会消费命令——那会让整个应用「启动卡住」。
        self.emit_ai_config().await;
        if models_corrupt {
            // 先发配置（界面据空列表显示「没有模型」），再发错误说明原因，
            // 否则用户只看到「模型没了」，不知道是解析失败、数据还在。
            self.emit_error_message(
                "AI 模型档案读取失败，原数据未改动。请检查 meta.ai_models，或以新档案覆盖后再保存。",
            )
            .await;
        }
        self.repair_orphan_streams().await;
    }

    /// 把模型档案与当前模型写回 meta。失败只记日志：内存里已经是新的，
    /// 下次启动会重读旧值，但界面不会因为一次写盘失败而卡住。
    async fn persist_models(&self, models: &[AiModel], active: Option<&str>) {
        let raw = serde_json::to_string(models).unwrap_or_else(|_| "[]".to_string());
        let active = active.unwrap_or_default().to_string();
        let result = self
            .db(move |db| {
                db.set_setting(settings_key::AI_MODELS, &raw)?;
                db.set_setting(settings_key::AI_ACTIVE_MODEL, &active)
            })
            .await;
        if let Err(err) = result {
            tracing::warn!(error = %err.user_message(), "保存 AI 模型档案失败");
        }
    }

    /// 启动时修复崩溃留下的 `streaming` 行。
    async fn repair_orphan_streams(&self) {
        if let Ok(fixed) = self.db(|s| s.fail_orphan_streaming_messages()).await
            && fixed > 0
        {
            tracing::info!(fixed, "修复了上次异常退出留下的 AI 流式占位行");
            self.emit(Event::AiConversationsChanged).await;
        }
    }

    async fn emit_error_message(&self, message: impl Into<String>) {
        self.emit(Event::Error {
            scope: me_core::ErrorScope::Global,
            message: message.into(),
        })
        .await;
    }

    pub(crate) async fn emit_ai_config(&self) {
        let (models, active, ack_anthropic, ack_openai) = match self.inner.ai_state.lock() {
            Ok(state) => (
                state.models.clone(),
                state.active.clone(),
                state.privacy_ack_anthropic,
                state.privacy_ack_openai,
            ),
            Err(_) => (Vec::new(), None, false, false),
        };
        let entity_count = self.db(|s| s.count_entities()).await.unwrap_or_default();
        self.emit(Event::AiConfigChanged {
            models,
            active,
            privacy_ack_anthropic: ack_anthropic,
            privacy_ack_openai: ack_openai,
            entity_count,
        })
        .await;
    }

    /// 保存模型档案：`id` 已存在则更新，否则新建。
    ///
    /// 名称唯一性、URL、输入/输出上限都在 `me-core` 里校验，这里只负责
    /// 「补齐 id / 保留 key 标记 / 落库 / 广播」。
    pub(crate) async fn handle_save_ai_model(&self, mut model: AiModel) {
        model.base_url = me_core::ai::normalize_base_url(&model.base_url);
        model.model = model.model.trim().to_string();
        model.name = model.name.trim().to_string();

        let (mut models, active) = self.models_snapshot();
        if model.id.trim().is_empty() {
            model.id = next_model_id(&models);
        }

        let others: Vec<AiModel> = models
            .iter()
            .filter(|item| item.id != model.id)
            .cloned()
            .collect();
        if let Err(err) = validate_model(&model, &others) {
            self.emit_error_message(err.user_message()).await;
            return;
        }

        let is_new = !models.iter().any(|item| item.id == model.id);
        match models.iter_mut().find(|item| item.id == model.id) {
            Some(slot) => *slot = model.clone(),
            None => models.push(model.clone()),
        }

        // 当前模型可能被删过、也可能是首次添加：保证它指向一个存在的档案。
        let mut active = active.filter(|id| find_model(&models, id).is_some());
        if active.is_none() || is_new {
            active = Some(model.id.clone());
        }

        self.persist_models(&models, active.as_deref()).await;
        if let Ok(mut state) = self.inner.ai_state.lock() {
            state.models = models;
            state.active = active;
        }
        self.emit_ai_config().await;
    }

    pub(crate) async fn handle_delete_ai_model(&self, id: String) {
        let (mut models, mut active) = self.models_snapshot();
        let Some(index) = models.iter().position(|item| item.id == id) else {
            return;
        };
        models.remove(index);
        if active.as_deref() == Some(id.as_str()) {
            active = models.first().map(|item| item.id.clone());
        }
        self.persist_models(&models, active.as_deref()).await;
        if let Ok(mut state) = self.inner.ai_state.lock() {
            state.models = models;
            state.active = active;
        }
        // 会话里留下的 `model_id` 不清：它指向已删除的档案时按「当前模型」回退，
        // 而 provider/model 快照仍能告诉用户那条回复当初是谁答的。
        self.emit_ai_config().await;
        self.emit(Event::AiConversationsChanged).await;
    }

    pub(crate) async fn handle_set_active_ai_model(&self, id: String) {
        let (models, _) = self.models_snapshot();
        if find_model(&models, &id).is_none() {
            self.emit_error_message("模型不存在").await;
            return;
        }
        self.persist_models(&models, Some(&id)).await;
        if let Ok(mut state) = self.inner.ai_state.lock() {
            state.active = Some(id);
        }
        self.emit_ai_config().await;
    }

    /// 对话栏切换模型：改的是这个会话绑定的档案，不动「当前模型」。
    pub(crate) async fn handle_set_conversation_ai_model(
        &self,
        conversation: AiConversationId,
        model_id: String,
    ) {
        let (models, _) = self.models_snapshot();
        let Some(model) = find_model(&models, &model_id).cloned() else {
            self.emit_error_message("模型不存在").await;
            return;
        };
        let config = model.to_config();
        let result = self
            .db(move |s| s.set_conversation_model(conversation, &model_id, &config))
            .await;
        match result {
            Ok(()) => self.emit(Event::AiConversationsChanged).await,
            Err(err) => self.emit_ai_error(err).await,
        }
    }

    pub(crate) async fn handle_set_ai_privacy_ack(&self, provider: AiProviderKind, accepted: bool) {
        if let Ok(mut state) = self.inner.ai_state.lock() {
            match provider {
                AiProviderKind::Anthropic => state.privacy_ack_anthropic = accepted,
                AiProviderKind::OpenAiCompatible => state.privacy_ack_openai = accepted,
            }
        }
        let stamp = if accepted { now_unix() } else { 0 };
        let key = provider.ack_key();
        let result = self
            .db(move |s| {
                let raw = s
                    .get_setting(settings_key::AI_PRIVACY_ACK)?
                    .unwrap_or_else(|| "{}".to_string());
                let mut value: serde_json::Value =
                    serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}));
                value[key] = serde_json::json!(stamp);
                s.set_setting(settings_key::AI_PRIVACY_ACK, &value.to_string())
            })
            .await;
        if result.is_ok() {
            self.emit_ai_config().await;
        }
    }

    /// 发一条最小请求验证网络、认证与模型名；**不带工具、不发用户内容**。
    pub(crate) async fn handle_test_ai_model(&self, model_id: String) {
        let Some(ai) = self.ai_provider() else {
            self.emit(Event::AiConnectionTested {
                model: model_id,
                ok: false,
                detail: Some("AI 传输层尚未初始化".to_string()),
            })
            .await;
            return;
        };
        let (models, _) = self.models_snapshot();
        let Some(model) = find_model(&models, &model_id).cloned() else {
            self.emit(Event::AiConnectionTested {
                model: model_id,
                ok: false,
                detail: Some("模型不存在".to_string()),
            })
            .await;
            return;
        };
        let config = model.to_config();
        let credential = Self::ai_credential(&model);
        if credential.is_none()
            && !(model.kind == AiProviderKind::OpenAiCompatible && config.is_loopback())
        {
            self.emit(Event::AiConnectionTested {
                model: model_id,
                ok: false,
                detail: Some("尚未配置 API Key".to_string()),
            })
            .await;
            return;
        }
        let request = AiChatRequest {
            config,
            credential,
            system: "Reply with OK.".to_string(),
            messages: vec![me_core::ai::AiWireMessage::User {
                content: "ping".to_string(),
            }],
            tools: Vec::new(),
            max_output_tokens: 1,
        };
        let sink: AiDeltaSink = Arc::new(|_| {});
        let result = ai
            .stream_chat(request, sink, me_core::ai::CancelFlag::new())
            .await;
        let (ok, detail) = match result {
            Ok(_) => (true, None),
            Err(err) => (false, Some(err.user_message())),
        };
        self.emit(Event::AiConnectionTested {
            model: model_id,
            ok,
            detail,
        })
        .await;
    }

    // -----------------------------------------------------------------------
    // 会话
    // -----------------------------------------------------------------------

    pub(crate) async fn handle_create_ai_conversation(&self, model_id: Option<String>) {
        let (models, active) = self.models_snapshot();
        // 指定了档案就用它；没指定用当前模型；列表为空则提示去添加。
        let Some(model) = resolve_model(&models, model_id.as_deref(), active.as_deref()).cloned()
        else {
            self.emit_error_message("请先在设置中添加一个模型").await;
            return;
        };
        let config = model.to_config();
        let model_id = model.id.clone();
        match self
            .db(move |s| s.create_conversation("新对话", Some(&model_id), &config))
            .await
        {
            Ok(_) => self.emit(Event::AiConversationsChanged).await,
            Err(err) => self.emit_ai_error(err).await,
        }
    }

    pub(crate) async fn handle_rename_ai_conversation(&self, id: AiConversationId, title: String) {
        match self.db(move |s| s.rename_conversation(id, &title)).await {
            Ok(()) => self.emit(Event::AiConversationsChanged).await,
            Err(err) => self.emit_ai_error(err).await,
        }
    }

    pub(crate) async fn handle_delete_ai_conversation(&self, id: AiConversationId) {
        // 有进行中的流：先取消，落库为 cancelled，再删会话。
        if let Ok(mut state) = self.inner.ai_state.lock()
            && let Some(entry) = state.streams.remove(&id)
        {
            entry.token.cancel();
        }
        match self.db(move |s| s.delete_conversation(id)).await {
            Ok(()) => self.emit(Event::AiConversationsChanged).await,
            Err(err) => self.emit_ai_error(err).await,
        }
    }

    pub(crate) async fn handle_clear_ai_conversations(&self) {
        if let Ok(mut state) = self.inner.ai_state.lock() {
            for (_, entry) in state.streams.drain() {
                entry.token.cancel();
            }
        }
        match self.db(|s| s.clear_conversations()).await {
            Ok(_) => self.emit(Event::AiConversationsChanged).await,
            Err(err) => self.emit_ai_error(err).await,
        }
    }

    pub(crate) async fn handle_delete_ai_entity(&self, entity: i64) {
        match self.db(move |s| s.delete_entity(entity)).await {
            Ok(()) => {
                self.emit(Event::AiEntitiesChanged).await;
                self.emit_ai_config().await;
            }
            Err(err) => self.emit_ai_error(err).await,
        }
    }

    pub(crate) async fn handle_clear_ai_entities(&self) {
        match self.db(|s| s.clear_entities()).await {
            Ok(_) => {
                self.emit(Event::AiEntitiesChanged).await;
                self.emit_ai_config().await;
            }
            Err(err) => self.emit_ai_error(err).await,
        }
    }

    /// 撤销一条 AI 创建的日程。**只允许 `source = ai`**。
    pub(crate) async fn handle_undo_ai_calendar_create(&self, event: me_core::CalendarEventId) {
        let source = self
            .db(move |s| Ok(s.get_event(event)?.map(|e| e.source)))
            .await;
        match source {
            Ok(Some(me_core::CalendarSource::Ai)) => {
                self.delete_event(event, crate::calendar::Actor::User)
                    .await
                    .ok();
            }
            Ok(Some(_)) => {
                self.emit(Event::Error {
                    scope: me_core::ErrorScope::Global,
                    message: "只能撤销 AI 创建的日程".to_string(),
                })
                .await;
            }
            Ok(None) => {}
            Err(err) => self.emit_ai_error(err).await,
        }
    }

    pub(crate) async fn emit_ai_error(&self, err: CoreError) {
        self.emit(Event::Error {
            scope: me_core::ErrorScope::Global,
            message: err.user_message(),
        })
        .await;
    }

    // -----------------------------------------------------------------------
    // 发送
    // -----------------------------------------------------------------------

    pub(crate) async fn handle_send_ai_message(
        &self,
        conversation: AiConversationId,
        content: String,
        context: Vec<AiContextRequest>,
    ) {
        let content = content.trim().to_string();
        if content.is_empty() {
            return;
        }
        if content.chars().count() > MAX_USER_INPUT_CHARS {
            self.emit_ai_error(CoreError::validation("内容过长（上限 32 KiB）"))
                .await;
            return;
        }
        let busy = self
            .inner
            .ai_state
            .lock()
            .map(|state| state.streams.contains_key(&conversation))
            .unwrap_or(false);
        if busy {
            self.emit_ai_error(CoreError::validation("请先停止当前回复"))
                .await;
            return;
        }

        let conv = match self.db(move |s| s.get_conversation(conversation)).await {
            Ok(Some(conv)) => conv,
            Ok(None) => {
                self.emit_ai_error(CoreError::validation("会话不存在"))
                    .await;
                return;
            }
            Err(err) => {
                self.emit_ai_error(err).await;
                return;
            }
        };

        let acked = match self.inner.ai_state.lock() {
            Ok(state) => state.privacy_ack(conv.provider),
            Err(_) => false,
        };
        if !acked {
            self.emit_ai_error(CoreError::validation(
                "请先在设置中确认隐私说明（AI 会把必要内容发送给服务商）",
            ))
            .await;
            return;
        }
        let disk_ok = self
            .db(|s| s.available_bytes())
            .await
            .ok()
            .flatten()
            .is_none_or(|bytes| bytes >= MIN_FREE_BYTES);
        if !disk_ok {
            self.emit_ai_error(CoreError::validation(
                "磁盘空间不足，请清理缓存或 AI 历史后再试",
            ))
            .await;
            return;
        }

        let refs = match self.ai_context_refs(context).await {
            Ok(refs) => refs,
            Err(err) => {
                self.emit_ai_error(err).await;
                return;
            }
        };

        self.start_ai_turn(conversation, content, refs, true, false)
            .await;
    }

    /// 真正开始一轮对话。
    ///
    /// - `insert_user = false`：上一条用户消息已经在库里，再插一条会让界面出现
    ///   两个一模一样的问题（「重新生成」走这条）。
    /// - `replay_only = true`：只下发**可重放**的工具（`AiToolName::is_replay_safe`）。
    ///   重新生成会重放同一轮工具调用，`calendar_create` 有真实副作用（去重窗口
    ///   之外会造出重复日程），这一轮直接不给模型。
    async fn start_ai_turn(
        &self,
        conversation: AiConversationId,
        content: String,
        refs: Vec<AiContextRef>,
        insert_user: bool,
        replay_only: bool,
    ) {
        let conv = match self.db(move |s| s.get_conversation(conversation)).await {
            Ok(Some(conv)) => conv,
            Ok(None) => {
                self.emit_ai_error(CoreError::validation("会话不存在"))
                    .await;
                return;
            }
            Err(err) => {
                self.emit_ai_error(err).await;
                return;
            }
        };
        // 用会话绑定的档案；档案被删了就回退到当前模型。
        // 先把结果取出来再 await：`MutexGuard` 跨 await 会让整个 future 失去 `Send`。
        let resolved = match self.inner.ai_state.lock() {
            Ok(state) => state.model_for(conv.model_id.as_deref()).map(|model| {
                let acked = state.privacy_ack(model.kind);
                (model, acked)
            }),
            Err(_) => return,
        };
        let Some((model, acked)) = resolved else {
            self.emit_error_message("请先在设置中添加一个模型").await;
            return;
        };
        let config = model.to_config();
        let permissions = model.tools;
        if !acked {
            self.emit_ai_error(CoreError::validation(
                "请先在设置中确认隐私说明（AI 会把必要内容发送给服务商）",
            ))
            .await;
            return;
        }
        let credential = Self::ai_credential(&model);
        if credential.is_none()
            && !(model.kind == AiProviderKind::OpenAiCompatible && config.is_loopback())
        {
            self.emit_ai_error(CoreError::validation(format!(
                "请先为模型「{}」配置 API Key",
                model.name
            )))
            .await;
            return;
        }

        let history = self
            .db(move |s| me_core::ai::AiRepo::list_messages(&s, conversation, None, HISTORY_FETCH))
            .await
            .unwrap_or_default();
        // 用户引用的邮件 id：`mail_read` 用它短路「正文已在上下文」。
        // 只留 id，不留正文快照——后者每轮克隆几千字符没有任何收益。
        let context_refs: Vec<MessageId> = refs.iter().map(|item| item.message_id).collect();
        let entities = self.ai_entities_for(conversation, &content).await;
        let mut tools = tool_specs(&permissions);
        if replay_only {
            tools.retain(|spec| spec.name.is_replay_safe());
        }
        let (request, trimmed) = match build_chat_request(ChatBuildInput {
            config: &config,
            credential,
            history: &history,
            user_input: &content,
            context_refs: &refs,
            entities: &entities,
            tools,
            max_input_chars: model.max_input_chars as usize,
        }) {
            Ok(built) => built,
            Err(err) => {
                self.emit_ai_error(err).await;
                return;
            }
        };

        if insert_user {
            let new_message = NewAiMessage {
                conversation,
                role: AiRole::User,
                content: content.clone(),
                context_refs: refs,
                status: AiMessageStatus::Complete,
                created_at: now_unix(),
            };
            if let Err(err) = self.db(move |s| s.append_message(&new_message)).await {
                self.emit_ai_error(err).await;
                return;
            }
            let title = if conv.title.trim().is_empty() || conv.title == "新对话" {
                Some(me_store::auto_title(&content))
            } else {
                None
            };
            let _ = self
                .db(move |s| s.touch_conversation(conversation, title.as_deref()))
                .await;
        }

        let message = match self
            .db(move |s| s.begin_assistant_message(conversation))
            .await
        {
            Ok(id) => id,
            Err(err) => {
                self.emit_ai_error(err).await;
                return;
            }
        };

        let token = CancellationToken::new();
        if let Ok(mut state) = self.inner.ai_state.lock() {
            state.streams.insert(
                conversation,
                StreamEntry {
                    token: token.clone(),
                },
            );
        }

        self.emit(Event::AiConversationsChanged).await;
        self.emit(Event::AiStreamStarted {
            conversation,
            message,
            trimmed_turns: trimmed,
        })
        .await;

        let service = self.clone();
        tokio::spawn(async move {
            service
                .run_ai_stream(
                    conversation,
                    message,
                    request,
                    token,
                    permissions,
                    context_refs,
                )
                .await;
            if let Ok(mut state) = service.inner.ai_state.lock() {
                state.streams.remove(&conversation);
            }
        });
    }

    pub(crate) async fn handle_cancel_ai_stream(&self, conversation: AiConversationId) {
        if let Ok(state) = self.inner.ai_state.lock()
            && let Some(entry) = state.streams.get(&conversation)
        {
            entry.token.cancel();
        }
    }

    /// 重新生成：**替换**最后一条助手回复（v0.3 不保留旧分支）。
    ///
    /// 复用原用户消息的内容与引用快照，不重新插入用户消息；因此界面不会
    /// 出现两个相同的问题。本轮只提供可重放的工具（见 `AiToolName::is_replay_safe`），
    /// `calendar_create` 不在其中。
    pub(crate) async fn handle_regenerate_ai_message(&self, conversation: AiConversationId) {
        let busy = self
            .inner
            .ai_state
            .lock()
            .map(|state| state.streams.contains_key(&conversation))
            .unwrap_or(false);
        if busy {
            self.emit_ai_error(CoreError::validation("请先停止当前回复"))
                .await;
            return;
        }
        let messages = self
            .db(move |s| me_core::ai::AiRepo::list_messages(&s, conversation, None, 4))
            .await
            .unwrap_or_default();
        if let Some(last) = messages.last()
            && last.role == AiRole::Assistant
        {
            let id = last.id;
            if let Err(err) = self.db(move |s| s.delete_message(id)).await {
                self.emit_ai_error(err).await;
                return;
            }
        }
        let Some(user) = messages.iter().rev().find(|m| m.role == AiRole::User) else {
            return;
        };
        let content = user.content.clone();
        let refs = user.context_refs.clone();
        self.start_ai_turn(conversation, content, refs, false, true)
            .await;
    }

    // -----------------------------------------------------------------------
    // 上下文
    // -----------------------------------------------------------------------

    /// 把引用请求转成快照：摘要字段 + 缓存的正文文本。
    async fn ai_context_refs(
        &self,
        requests: Vec<AiContextRequest>,
    ) -> Result<Vec<AiContextRef>, CoreError> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let paths = self.inner.paths.clone();
        let mut refs = Vec::new();
        let mut total = 0usize;
        for request in requests {
            let summary = self
                .db(move |s| s.get_message(request.message))
                .await?
                .ok_or_else(|| CoreError::validation("引用的邮件已不存在"))?;
            let mode = request.mode;
            let subject = summary.subject.clone().unwrap_or_default();
            let from = summary
                .from
                .as_ref()
                .map(|address| address.display())
                .unwrap_or_default();
            let text = match mode {
                me_core::ai::AiContextMode::Selection => request
                    .selection
                    .clone()
                    .ok_or_else(|| CoreError::validation("缺少选中文本"))?,
                me_core::ai::AiContextMode::FullBody => {
                    let path = paths.body_path(request.message);
                    tokio::fs::read_to_string(&path).await.map_err(|_| {
                        CoreError::validation("该邮件正文尚未缓存，请先打开这封邮件")
                    })?
                }
            };
            let excerpt =
                me_core::ai::escape_context(&me_core::ai::truncate_chars(&text, mode.char_limit()));
            total += excerpt.chars().count() + subject.chars().count() + from.chars().count();
            if total > me_core::ai::MAX_CONTEXT_TOTAL_CHARS {
                return Err(CoreError::validation("引用内容过长，请减少引用"));
            }
            if refs.len() >= me_core::ai::MAX_CONTEXT_REFS {
                return Err(CoreError::validation("一次最多引用 8 封邮件"));
            }
            refs.push(AiContextRef {
                message_id: request.message,
                subject,
                from,
                date: summary.date,
                mode,
                excerpt,
            });
        }
        Ok(refs)
    }

    /// 实体上下文：本会话最近使用的 + 当前消息中命中的别名。
    async fn ai_entities_for(&self, conversation: AiConversationId, query: &str) -> Vec<AiEntity> {
        let probe = query.to_string();
        let recent = self
            .db(move |s| {
                let mut entities = s.list_conversation_entities(conversation, 12)?;
                // 别名命中可能来自全库（新会话第一次提到老联系人）。
                let by_alias = s.find_entities_by_alias(&probe, 8).unwrap_or_default();
                for entity in by_alias {
                    if !entities.iter().any(|e| e.id == entity.id) {
                        entities.push(entity);
                    }
                }
                Ok(entities)
            })
            .await
            .unwrap_or_default();
        me_core::ai::select_entities(&recent, query, me_core::ai::MAX_ENTITY_CONTEXT)
    }

    // -----------------------------------------------------------------------
    // 工具循环
    // -----------------------------------------------------------------------

    async fn run_ai_stream(
        &self,
        conversation: AiConversationId,
        message: AiMessageId,
        mut request: AiChatRequest,
        token: CancellationToken,
        permissions: me_core::ai::AiToolPermissions,
        context_refs: Vec<MessageId>,
    ) {
        let started = Instant::now();
        let permit = self
            .inner
            .ai_state
            .lock()
            .ok()
            .map(|state| state.slots.clone());
        let _permit = match permit {
            Some(slots) => slots.acquire_owned().await.ok(),
            None => None,
        };

        let throttle = Arc::new(Mutex::new(DeltaThrottle::new()));
        let usage: Arc<Mutex<(Option<u32>, Option<u32>)>> = Arc::new(Mutex::new((None, None)));
        let cancel = me_core::ai::CancelFlag::new();

        let sink: AiDeltaSink = {
            let events = self.inner.events.clone();
            let throttle = Arc::clone(&throttle);
            let usage = Arc::clone(&usage);
            Arc::new(move |delta: AiDelta| match delta {
                AiDelta::Text(text) => {
                    let Ok(mut state) = throttle.lock() else {
                        return;
                    };
                    if let Some((seq, chunk)) = state.push(&text) {
                        events.send_progress(Event::AiStreamDelta {
                            conversation,
                            message,
                            seq,
                            delta: chunk,
                        });
                    }
                }
                AiDelta::Usage { input, output } => {
                    if let Ok(mut slot) = usage.lock() {
                        if input.is_some() {
                            slot.0 = input;
                        }
                        if output.is_some() {
                            slot.1 = output;
                        }
                    }
                }
            })
        };

        let provider = match self.ai_provider() {
            Some(provider) => provider,
            None => {
                self.fail_stream(conversation, message, "AI 传输层尚未初始化", false)
                    .await;
                return;
            }
        };

        let mut traces: Vec<AiToolTrace> = Vec::new();
        let mut executed_calls = 0u32;
        // 本轮的清单是**产物**而不是配额，所以放局部变量（不放 `ToolBudget`）。
        let mut plan: Option<me_core::ai_plan::Plan> = None;
        let mut nudges = 0u32;
        let mut budget = crate::ai_tools::ToolBudget {
            permissions,
            context_refs,
            ..crate::ai_tools::ToolBudget::default()
        };
        let mut status = AiMessageStatus::Complete;
        let mut error_text: Option<String> = None;
        let mut retryable = false;

        'rounds: for _round in 0..MAX_TOOL_ROUNDS {
            if token.is_cancelled() {
                status = AiMessageStatus::Cancelled;
                break;
            }
            if started.elapsed() > STREAM_BUDGET {
                status = AiMessageStatus::Failed;
                error_text = Some("回复超时（工具循环总时长超过 240 秒）".to_string());
                retryable = true;
                break;
            }

            let turn = tokio::select! {
                biased;
                _ = token.cancelled() => {
                    cancel.cancel();
                    status = AiMessageStatus::Cancelled;
                    break 'rounds;
                }
                result = provider.stream_chat(request.clone(), sink.clone(), cancel.clone()) => result,
            };

            let turn = match turn {
                Ok(turn) => turn,
                Err(err) if err.is_cancelled() => {
                    status = AiMessageStatus::Cancelled;
                    break;
                }
                Err(err) => {
                    status = AiMessageStatus::Failed;
                    retryable = err.is_retryable();
                    error_text = Some(err.user_message());
                    break;
                }
            };

            {
                let mut usage_slot = usage.lock().expect("usage 锁");
                if turn.input_tokens.is_some() {
                    usage_slot.0 = turn.input_tokens;
                }
                if turn.output_tokens.is_some() {
                    usage_slot.1 = turn.output_tokens;
                }
            }

            if turn.tool_calls.is_empty() {
                // 清单还开着却停了：模型常常以为「说完计划就算做完了」。
                // 这里补一条提醒再给它一轮，而不是直接收尾把清单丢在半路。
                if nudges < MAX_PLAN_NUDGES
                    && plan
                        .as_ref()
                        .is_some_and(me_core::ai_plan::Plan::has_open_steps)
                {
                    nudges += 1;
                    let current = plan.as_ref().expect("刚判过非空");
                    let open = current
                        .steps
                        .iter()
                        .filter(|step| !step.status.is_terminal())
                        .map(|step| format!("第 {} 步「{}」", step.id, step.title))
                        .collect::<Vec<_>>()
                        .join("、");
                    request.messages.push(me_core::ai::AiWireMessage::User {
                        content: format!(
                            "清单还没做完：{open}。请继续执行下一步；如果确实做不了，\
把该步标成 skipped 并说明原因，然后用 [结果] 收尾。"
                        ),
                    });
                    continue;
                }
                break;
            }

            let mut results: Vec<AiToolResult> = Vec::new();
            for call in &turn.tool_calls {
                // 清单维护不算「干活」：否则一份 6 步清单光更新状态就要吃掉一半预算。
                // 轮次仍然受限，所以它不能被用来无限刷。
                if !call.name.is_bookkeeping() {
                    if executed_calls >= MAX_TOOL_CALLS {
                        status = AiMessageStatus::Failed;
                        error_text = Some("工具调用次数过多，请缩小问题范围".to_string());
                        break 'rounds;
                    }
                    executed_calls += 1;
                }
                // 动机在**执行前**就要发出去：界面靠它解释「为什么现在调这个工具」。
                let (motive, motive_source) = me_core::ai::tool_motive(&call.arguments);
                self.emit(Event::AiToolStarted {
                    conversation,
                    call_id: call.id.clone(),
                    tool: call.name,
                    motive: motive.clone(),
                    motive_source,
                })
                .await;
                let plan_before = plan.clone();
                let result = tokio::select! {
                    biased;
                    _ = token.cancelled() => {
                        status = AiMessageStatus::Cancelled;
                        break 'rounds;
                    }
                    result = self.execute_ai_tool(conversation, call, &mut budget, &mut plan) => result,
                };
                if plan != plan_before
                    && let Some(current) = plan.clone()
                {
                    self.emit(Event::AiPlanUpdated {
                        conversation,
                        message,
                        plan: current,
                    })
                    .await;
                }
                self.emit(Event::AiToolCompleted {
                    conversation,
                    call_id: result.call_id.clone(),
                    tool: result.tool,
                    status: result.trace.status,
                    summary: result.trace.summary.clone(),
                    duration_ms: result.trace.duration_ms,
                    // 用 trace 里的同一份，保证同一条活动条前后不跳变。
                    motive: result.trace.motive.clone(),
                    motive_source: result.trace.motive_source,
                })
                .await;
                traces.push(result.trace.clone());
                results.push(result);
            }

            request
                .messages
                .push(me_core::ai::AiWireMessage::Assistant {
                    content: turn.text,
                    tool_calls: turn.tool_calls.clone(),
                });
            for result in results {
                request
                    .messages
                    .push(me_core::ai::AiWireMessage::ToolResult {
                        call_id: result.call_id,
                        content: result.content,
                        is_error: result.is_error,
                    });
            }
        }

        // 收尾：把最后一段未满 50ms 的增量发出去。
        let (content, seq_tail) = {
            let Ok(mut state) = throttle.lock() else {
                return;
            };
            let tail = state.take();
            (state.total.clone(), tail)
        };
        if !seq_tail.1.is_empty() {
            self.emit(Event::AiStreamDelta {
                conversation,
                message,
                seq: seq_tail.0,
                delta: seq_tail.1,
            })
            .await;
        }

        let usage = *usage.lock().unwrap_or_else(|e| e.into_inner());
        let error_for_db = error_text.clone();
        // 清单必须是**最终态**再落库：取消时只把「进行中」的步标跳过，
        // 还没开始的步保持 pending（用户可能接着说「继续」）。
        let plan_for_db = finalize_plan(plan, status);
        let result = self
            .db(move |s| {
                s.finish_assistant_message(
                    message,
                    &content,
                    status,
                    Some(usage),
                    &traces,
                    plan_for_db.as_ref(),
                    error_for_db.as_deref(),
                )
            })
            .await;

        match result {
            Ok(()) if status == AiMessageStatus::Complete => {
                self.emit(Event::AiMessageCompleted {
                    conversation,
                    message,
                    status,
                })
                .await;
            }
            Ok(()) if status == AiMessageStatus::Cancelled => {
                self.emit(Event::AiMessageCompleted {
                    conversation,
                    message,
                    status,
                })
                .await;
            }
            Ok(()) => {
                self.emit(Event::AiStreamFailed {
                    conversation,
                    message,
                    error_text: error_text.unwrap_or_else(|| "回复失败".to_string()),
                    retryable,
                })
                .await;
            }
            Err(err) => {
                self.emit(Event::Error {
                    scope: me_core::ErrorScope::Global,
                    message: format!("保存回复失败：{}", err.user_message()),
                })
                .await;
                let _ = self.emit_ai_error(err).await;
            }
        }
    }

    async fn fail_stream(
        &self,
        conversation: AiConversationId,
        message: AiMessageId,
        detail: &str,
        retryable: bool,
    ) {
        let detail = detail.to_string();
        let error = detail.clone();
        let _ = self
            .db(move |s| {
                s.finish_assistant_message(
                    message,
                    "",
                    AiMessageStatus::Failed,
                    None,
                    &[],
                    None,
                    Some(&error),
                )
            })
            .await;
        self.emit(Event::AiStreamFailed {
            conversation,
            message,
            error_text: detail,
            retryable,
        })
        .await;
    }

    // -----------------------------------------------------------------------
    // 共享读用例（UI 与工具都走这里）
    // -----------------------------------------------------------------------

    /// 邮件检索的**唯一实现**：UI 搜索框与 `email_search` 工具共用。
    ///
    /// 分开写两套的后果是「AI 说没找到、用户自己一搜就有」，而且几乎查不出来。
    pub(crate) async fn search_mail(
        &self,
        mut query: me_core::SearchQuery,
        cap: u32,
    ) -> Result<me_core::port::SearchOutcome, CoreError> {
        query.limit = query.limit.min(cap).max(1);
        query.cursor = None;
        let mut outcome = self.db(move |s| s.search(&query)).await?;
        outcome.query_id = self
            .inner
            .query_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        Ok(outcome)
    }

    // -----------------------------------------------------------------------
    // 实体写入
    // -----------------------------------------------------------------------

    pub(crate) async fn upsert_ai_entity(
        &self,
        conversation: AiConversationId,
        entity: NewAiEntity,
    ) -> Result<AiEntity, CoreError> {
        let saved = self.db(move |s| s.upsert_entity(&entity)).await?;
        let now = now_unix();
        let id = saved.id;
        let _ = self
            .db(move |s| s.link_conversation_entity(conversation, id, now))
            .await;
        Ok(saved)
    }
}

/// 收尾时把清单改成最终态。
///
/// 规则按「这条消息还有没有可能继续」分：
/// - `Complete`：不动（模型自己会把最后一步标好；没标就是它的问题，不替它圆）。
/// - `Cancelled`：只把 `running` 标成跳过。用户按了停止，通常还会接着说「继续」，
///   把没开始的步一起标 `skipped` 会让清单看起来像做废了。
/// - `Failed`：所有未结束的步标跳过——这一轮已经结束，留着 `running` 会永远转圈。
fn finalize_plan(
    mut plan: Option<me_core::ai_plan::Plan>,
    status: AiMessageStatus,
) -> Option<me_core::ai_plan::Plan> {
    let current = plan.as_mut()?;
    match status {
        AiMessageStatus::Complete => {}
        AiMessageStatus::Cancelled => {
            current.skip_running("用户停止了生成");
        }
        AiMessageStatus::Failed | AiMessageStatus::Streaming => {
            current.skip_open("回复中断，这一步没有执行");
        }
    }
    plan
}
