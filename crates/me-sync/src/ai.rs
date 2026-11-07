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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use me_core::ai::{
    AiChatRequest, AiContextRef, AiContextRequest, AiConversationId, AiCredential, AiDelta,
    AiDeltaSink, AiEntity, AiMessageId, AiMessageStatus, AiModel, AiProvider, AiProviderConfig,
    AiProviderKind, AiRole, AiToolName, AiToolResult, AiToolStatus, AiToolTrace, ChatBuildInput,
    DEFAULT_MAX_INPUT_CHARS, MAX_TOOL_CALLS, MAX_TOOL_ROUNDS, MAX_USER_INPUT_CHARS, NewAiEntity,
    NewAiMessage, build_chat_request, find_model, next_model_id, resolve_model, tool_specs,
    validate_model,
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
/// PLAN 阶段最多催几次「提交计划」。
///
/// 只给两次（首轮一次 + 空回复一次），第三轮由引擎合成单步计划收场：
/// 「等模型学会」的设计在生产里一定会变成死循环。
const MAX_PLAN_ROUNDS: u32 = 2;

/// 自检输出不是 JSON 时最多重问几次。
///
/// 只给一次：模型连 JSON 都写不对时，再问也是同一句话，不如按「部分完成」收场。
const MAX_SELFCHECK_RETRIES: u32 = 1;

/// 同一轮重列里，新计划被拒收（缺诊断/全是重复指纹）几次后放弃。
///
/// 注意这与「重列次数上限」是两件事：那个数已经在 `Plan.replans` 里，
/// 这里管的是「模型交不出合格的第二份计划」。
const MAX_REPLAN_REJECTIONS: u32 = 2;

/// 每个阶段的时间上限（秒）。
///
/// 阶段预算是**上界**：正常路径远小于它，超了也只是提前收尾，
/// 不会让结果变错。它的作用是「不让某个阶段把整轮的时间吃光」——
/// 比如模型在 REFLECT 阶段反复自说自话，EXECUTE 的时间就没了。
const PHASE_BUDGET: [(Phase, u64); 5] = [
    (Phase::Plan, 60),
    (Phase::Execute, 300),
    (Phase::Reflect, 45),
    (Phase::Replan, 120),
    (Phase::Final, 45),
];

/// 一个阶段的时间上限。
fn phase_budget(phase: Phase) -> Duration {
    let seconds = PHASE_BUDGET
        .iter()
        .find(|(candidate, _)| *candidate == phase)
        .map(|(_, seconds)| *seconds)
        .unwrap_or(120);
    Duration::from_secs(seconds)
}

/// 引擎自动重试的退避间隔。
///
/// 第一次重试等 300ms，第二次等 900ms：网络抖动通常一秒内就恢复，
/// 等太久用户会以为卡死了。
const RETRY_BACKOFF: [Duration; 2] = [Duration::from_millis(300), Duration::from_millis(900)];

/// 调用过工具却一个字没说时，最多催它写正文几次。
///
/// 只给一次：再催就是逼它编内容。第二次仍然空白就走引擎兜底摘要。
const MAX_BLANK_RETRIES: u32 = 1;
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

/// 一次流式回复的入参里「不属于请求本身」的那几项。
///
/// 打成一个结构体而不是继续加参数：`run_ai_stream` 已经有五个位置参数，
/// 再加三个就要靠 `#[allow(clippy::too_many_arguments)]` 压警告了。
struct StreamSetup {
    permissions: me_core::ai::AiToolPermissions,
    context_refs: Vec<MessageId>,
    replay_only: bool,
    /// 用户这一轮的原始输入。REFLECT 要把「用户到底要什么」再摆到模型面前，
    /// 否则它只能对着一份清单自评，容易自说自话。
    user_input: String,
    /// 上一轮**没做完**的清单（同一会话里最近一份带未完成步的）。
    ///
    /// 跨轮延续靠它：用户说「继续」时，模型看到的是「还剩哪几步」，
    /// 而不是从零开始重新规划——那会让用户以为自己前面做的白费了。
    resumable: Option<me_core::ai_plan::Plan>,
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

/// 流式文本的节流器：`pending` 是待发送增量，`total` 是完整回复（落库用）。
///
/// **不持有 seq**：一个回复可能有两条通道（思考 / 回答），每条一个实例，
/// 各自从 0 计数会撞号，UI 的「有没有整段丢掉」判断就失效了。
/// 序号由 sink 外层的共享计数器统一分配。
struct DeltaThrottle {
    pending: String,
    total: String,
    last: Instant,
}

impl DeltaThrottle {
    fn new() -> Self {
        Self {
            pending: String::new(),
            total: String::new(),
            // 初始就允许立即发送，避免首字延迟 50ms。
            last: Instant::now() - DELTA_INTERVAL,
        }
    }

    /// 返回应当发出的文本（首次 / 满 50ms / 满 2KB）。
    fn push(&mut self, text: &str) -> Option<String> {
        self.total.push_str(text);
        self.pending.push_str(text);
        if self.last.elapsed() >= DELTA_INTERVAL || self.pending.len() >= DELTA_BATCH_BYTES {
            return Some(self.take());
        }
        None
    }

    /// 无条件取走待发送内容（收尾时用；空也返回空串）。
    fn take(&mut self) -> String {
        self.last = Instant::now();
        std::mem::take(&mut self.pending)
    }

    /// 有残余才取走，**没有就不动时钟**。
    ///
    /// 切思考段时必须先调它：否则上一段攒在节流窗口里的尾巴会顶着新段号
    /// 到达 UI，被塞进错误的段里。这个问题只在节流窗口恰好跨越段边界时
    /// 出现，极难复现，所以由测试固定住顺序。
    fn flush(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            None
        } else {
            Some(self.take())
        }
    }
}

/// 一轮回复里思考内容的累积与分段。
///
/// 模型是「想一段 → 调工具 → 再看结果想一段」交错的，所以段边界就是
/// **工具调用轮次**。段号从 1 开始，与 `AiStreamDelta.segment` 同源。
struct ThinkingAccumulator {
    segments: Vec<me_core::ai::ThinkingSegment>,
    current: String,
    segment_no: u8,
    total_chars: usize,
    truncated: bool,
}

impl ThinkingAccumulator {
    fn new() -> Self {
        Self {
            segments: Vec::new(),
            current: String::new(),
            segment_no: 1,
            total_chars: 0,
            truncated: false,
        }
    }

    /// 返回**实际保留**的文本（超限时会被截短，后续增量直接丢弃）。
    fn push(&mut self, text: &str) -> String {
        if self.truncated || text.is_empty() {
            return String::new();
        }
        if self.segments.len() >= me_core::ai::MAX_THINKING_SEGMENTS {
            self.truncated = true;
            return String::new();
        }
        let room = me_core::ai::MAX_THINKING_CHARS.saturating_sub(self.total_chars);
        let kept: String = text.chars().take(room).collect();
        let kept_len = kept.chars().count();
        if kept_len < text.chars().count() {
            self.truncated = true;
        }
        self.total_chars += kept_len;
        self.current.push_str(&kept);
        kept
    }

    /// 收当前段。**必须在 `next_segment` 之前调用**，否则段号对不上。
    fn cut(&mut self, followed_by_tool: bool) {
        if self.current.is_empty() {
            return;
        }
        self.segments.push(me_core::ai::ThinkingSegment {
            index: self.segment_no,
            text: std::mem::take(&mut self.current),
            followed_by_tool,
        });
    }

    fn next_segment(&mut self) {
        self.segment_no = self.segment_no.saturating_add(1);
    }

    /// 不消耗累积器地取一份当前快照（检查点落库用）。
    /// 未收尾的 `current` 也算一段，否则崩溃恢复后最后一段思考会丢。
    fn snapshot(&self) -> Option<me_core::ai::ThinkingBlock> {
        let mut segments = self.segments.clone();
        if !self.current.is_empty() {
            segments.push(me_core::ai::ThinkingSegment {
                index: self.segment_no,
                text: self.current.clone(),
                followed_by_tool: false,
            });
        }
        if segments.is_empty() && !self.truncated {
            return None;
        }
        Some(me_core::ai::ThinkingBlock {
            segments,
            truncated: self.truncated,
        })
    }

    fn finish(mut self) -> Option<me_core::ai::ThinkingBlock> {
        self.cut(false);
        if self.segments.is_empty() && !self.truncated {
            return None;
        }
        Some(me_core::ai::ThinkingBlock {
            segments: self.segments,
            truncated: self.truncated,
        })
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
        // 上一轮没做完的清单：找最近一份「还有未终结步」的。
        // 从后往前找——最近的那份才是用户记得的那份。
        let resumable = history.iter().rev().find_map(|message| {
            message
                .plan
                .as_ref()
                .filter(|plan| plan.is_resumable())
                .cloned()
        });
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
                    StreamSetup {
                        permissions,
                        context_refs,
                        replay_only,
                        user_input: content.clone(),
                        resumable,
                    },
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
        setup: StreamSetup,
    ) {
        let StreamSetup {
            permissions,
            context_refs,
            replay_only,
            user_input,
            mut resumable,
        } = setup;
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

        let answer_throttle = Arc::new(Mutex::new(DeltaThrottle::new()));
        let thinking_throttle = Arc::new(Mutex::new(DeltaThrottle::new()));
        let thinking = Arc::new(Mutex::new(ThinkingAccumulator::new()));
        // 思考与回答共用一条全序：两个事件通道会让「先思考后回答」的顺序在
        // 通道层面丢失，两套 seq 也会让 UI 的缺口检测失效。
        let seq_counter = Arc::new(AtomicU64::new(0));
        let usage: Arc<Mutex<(Option<u32>, Option<u32>)>> = Arc::new(Mutex::new((None, None)));
        let cancel = me_core::ai::CancelFlag::new();

        let sink: AiDeltaSink = {
            let events = self.inner.events.clone();
            let answer_throttle = Arc::clone(&answer_throttle);
            let thinking_throttle = Arc::clone(&thinking_throttle);
            let thinking = Arc::clone(&thinking);
            let usage = Arc::clone(&usage);
            let next_seq = {
                let counter = Arc::clone(&seq_counter);
                move || counter.fetch_add(1, Ordering::Relaxed) + 1
            };
            Arc::new(move |delta: AiDelta| match delta {
                AiDelta::Text(text) => {
                    let Ok(mut state) = answer_throttle.lock() else {
                        return;
                    };
                    if let Some(chunk) = state.push(&text) {
                        events.send_progress(Event::AiStreamDelta {
                            conversation,
                            message,
                            seq: next_seq(),
                            delta: chunk,
                            kind: me_core::command::AiDeltaKind::Answer,
                            segment: 0,
                        });
                    }
                }
                AiDelta::Thinking(text) => {
                    // 先累积（可能按上限截断），再节流。思考不走 answer_throttle，
                    // 否则回答的首字会被思考攒着的那批一起拖到下一节流窗口。
                    let kept = match thinking.lock() {
                        Ok(mut acc) => acc.push(&text),
                        Err(_) => return,
                    };
                    if kept.is_empty() {
                        return;
                    }
                    let segment = thinking.lock().map(|acc| acc.segment_no).unwrap_or(1);
                    let Ok(mut state) = thinking_throttle.lock() else {
                        return;
                    };
                    if let Some(chunk) = state.push(&kept) {
                        events.send_progress(Event::AiStreamDelta {
                            conversation,
                            message,
                            seq: next_seq(),
                            delta: chunk,
                            kind: me_core::command::AiDeltaKind::Thinking,
                            segment,
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
        let mut blank_retries = 0u32;
        let mut phase = Phase::Plan;
        let mut plan_rounds = 0u32;
        let mut answered_directly = false;
        let mut reflect_retries = 0u32;
        // 阶段计时：切换阶段时重置，超预算就提前收尾。
        let mut phase_started = Instant::now();
        let mut phase_at_start = phase;
        // 这一步声明了「需要用户补充」：本轮停下来等用户回答。
        let mut awaiting_user = false;
        let mut replan_rejections = 0u32;
        // 重列需要的三样东西：旧清单、诊断、改法。都来自 REFLECT 的输出，
        // 而 `plan_submit` 的契约里没有这些字段（不新加参数，改由引擎带过来）。
        let mut prev_plan: Option<me_core::ai_plan::Plan> = None;
        let mut replan_meta: Option<ReplanMeta> = None;

        // 两套工具集：PLAN 阶段只有 `plan_submit` / `answer_directly`，
        // 物理上取消了模型「边想边查」的可能——想查就必须先说要查什么。
        let mut plan_tools = me_core::ai::plan_phase_tool_specs();
        let mut execute_tools = me_core::ai::execute_phase_tool_specs(&permissions);
        if replay_only {
            plan_tools.retain(|spec| spec.name.is_replay_safe());
            execute_tools.retain(|spec| spec.name.is_replay_safe());
        }
        // 重列阶段只给 `plan_submit`：给 `answer_directly` 等于允许它「放弃重列」，
        // 那会让「重列」变成「随时可以退出」，上限也就没有意义了。
        let replan_tools: Vec<me_core::ai::AiToolSpec> = plan_tools
            .iter()
            .filter(|spec| spec.name == AiToolName::PlanSubmit)
            .cloned()
            .collect();
        request.tools = plan_tools.clone();
        // 上一轮有没做完的清单 → **第一次请求就**告诉模型可以接着做。
        //
        // 必须在请求发出前注入，而不是等模型回答之后再补：模型可能第一轮就
        // 直接提交计划（那是正常路径），那时再补就晚了，它已经规划完了。
        if let Some(previous) = resumable.take() {
            request.messages.push(me_core::ai::AiWireMessage::User {
                content: format!(
                    "上一轮有一份还没做完的清单：目标「{}」，还剩 {}。\n如果用户是在说「继续」，就照着这份清单接着做：plan_submit 提交一份**只包含剩余步骤**的计划（已完成的步不要重做），然后执行。\n{}",
                    previous.goal,
                    previous.unfinished_summary(),
                    previous.facts_block(),
                ),
            });
        }
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
                error_text = Some("回复超时（工具循环总时长超过 600 秒）".to_string());
                retryable = true;
                break;
            }

            // 阶段变了就重置计时：每个阶段各有一份预算。
            if phase != phase_at_start {
                phase_at_start = phase;
                phase_started = Instant::now();
            }
            // 阶段预算：超了就提前收尾，不让某个阶段把整轮时间吃光。
            if phase_started.elapsed() > phase_budget(phase) {
                match phase {
                    Phase::Plan => {
                        // 计划期超时：按「模型不配合」处理，合成单步清单继续。
                        if plan.is_none() && !answered_directly {
                            plan = Some(synthetic_plan());
                            error_text =
                                Some("模型未在计划阶段内提交计划，已按单步处理".to_string());
                        }
                        phase = Phase::Execute;
                        phase_started = Instant::now();
                    }
                    Phase::Final => {
                        // 已经在收尾了：不能再催一次，否则会反复催同一句话。
                        // 直接结束这一轮，让兜底摘要接手。
                        break;
                    }
                    _ => {
                        phase = Phase::Final;
                        phase_started = Instant::now();
                        request.messages.push(me_core::ai::AiWireMessage::User {
                            content: "时间预算已用完。请按 [结果] 格式给出当前能给出的最好结果，\
把没做到的写进「未完成」。"
                                .to_string(),
                        });
                        continue;
                    }
                }
            }

            request.tools = match phase {
                Phase::Plan => plan_tools.clone(),
                Phase::Execute => execute_tools.clone(),
                Phase::Replan => replan_tools.clone(),
                // REFLECT / FINAL 不给工具：前者只要一段 JSON，后者只要一段答复。
                Phase::Reflect | Phase::Final => Vec::new(),
            };

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

            // 每轮模型返回后写一次检查点。轮数上限就是 MAX_TOOL_ROUNDS（6），
            // 所以最多 6 次写，比 1s 定时器的写放大还小；而最容易崩的
            // 「长时间工具循环 + 中途断电」正好被逐轮覆盖。
            // 崩溃后 fail_orphan_streaming_messages 只改状态、不清内容，
            // 这里写下的正文 / 思考 / 清单 / trace 就都留住了。
            let checkpoint_content = answer_throttle
                .lock()
                .map(|state| state.total.clone())
                .unwrap_or_default();
            let checkpoint_thinking = thinking.lock().ok().and_then(|acc| acc.snapshot());
            let checkpoint_traces = traces.clone();
            let checkpoint_plan = plan.clone();
            let _ = self
                .db(move |s| {
                    s.checkpoint_assistant_message(
                        message,
                        &checkpoint_content,
                        &checkpoint_traces,
                        checkpoint_plan.as_ref(),
                        checkpoint_thinking.as_ref(),
                    )
                })
                .await;

            if turn.tool_calls.is_empty() {
                // 自检：模型给一段 JSON，引擎拿事实裁剪后决定「重列」还是「收尾」。
                if phase == Phase::Reflect {
                    match me_core::ai_plan::SelfCheck::parse(&turn.text) {
                        Some(check) => {
                            let verdict = match plan.as_ref() {
                                Some(current) => me_core::ai_plan::reconcile(&check, current),
                                None => check.verdict,
                            };
                            let rounds_left =
                                plan.as_ref().map_or(0, |current| current.replans.len())
                                    < me_core::ai_plan::MAX_REPLANS as usize;
                            if verdict == me_core::ai_plan::Verdict::Failed
                                && rounds_left
                                && check.can_replan()
                            {
                                let meta = ReplanMeta {
                                    diagnosis: check
                                        .diagnosis
                                        .unwrap_or(me_core::ai_plan::Diagnosis::WrongArgs),
                                    reason: check.reason.clone(),
                                    changes: check.replan_hint.clone(),
                                    failed_step: check.failed_step,
                                };
                                prev_plan = plan.clone();
                                request.messages.push(me_core::ai::AiWireMessage::User {
                                    content: replan_prompt(&meta),
                                });
                                replan_meta = Some(meta);
                                phase = Phase::Replan;
                                continue;
                            }
                            // 自检结论落库前先发事件：界面能立刻看到
                            // 「AI 自己认为这轮做完了吗」。
                            self.emit(Event::AiPlanSelfCheck {
                                conversation,
                                message,
                                verdict,
                                failed_step: check.failed_step,
                                diagnosis: check.diagnosis,
                                reason: check.reason.clone(),
                            })
                            .await;
                            let note = if rounds_left {
                                "请按 [结果] 格式给出最终答复，把没做到的写进「未完成」。"
                            } else {
                                "重列次数已用完。请如实说明哪些没做到、为什么，并给出下一步建议。"
                            };
                            request.messages.push(me_core::ai::AiWireMessage::User {
                                content: format!("核对结论：{}。{note}", verdict.label()),
                            });
                            phase = Phase::Final;
                            continue;
                        }
                        None => {
                            // 模型没按格式回。重问一次，再不行就按「部分完成」收场——
                            // 自检不该成为新的死循环。
                            if reflect_retries < MAX_SELFCHECK_RETRIES {
                                reflect_retries += 1;
                                request.messages.push(me_core::ai::AiWireMessage::User {
                                    content: "只输出那段 JSON，不要解释、不要 Markdown 代码块：{\"verdict\":\"done|partial|failed\",\"failed_step\":4,\"diagnosis\":\"wrong_plan|wrong_args|\
missing_data|tool_broken|user_ambiguous\",\"reason\":\"…\",\"replan_hint\":\"…\"}"
                                        .to_string(),
                                });
                                continue;
                            }
                            request.messages.push(me_core::ai::AiWireMessage::User {
                                content: "请按 [结果] 格式给出最终答复，把没做到的写进「未完成」。"
                                    .to_string(),
                            });
                            phase = Phase::Final;
                            continue;
                        }
                    }
                }

                // 重列阶段只认 `plan_submit`。模型改说人话就先提醒一次，
                // 第二次不再等——上限是「两次重列」，不是「两次聊天」。
                if phase == Phase::Replan {
                    if replan_rejections == 0 {
                        request.messages.push(me_core::ai::AiWireMessage::User {
                            content: "请调用 plan_submit 提交**新的**计划（不要重复已经失败过的调用参数）；如果确实无法继续，再直接说明结论。"
                                .to_string(),
                        });
                        replan_rejections = 1;
                        continue;
                    }
                    request.messages.push(me_core::ai::AiWireMessage::User {
                        content: "请按 [结果] 格式给出最终答复。".to_string(),
                    });
                    phase = Phase::Final;
                    continue;
                }

                if phase == Phase::Plan {
                    // PLAN 阶段模型直接说了一段话，没有提交计划。
                    // 只在「这句话看起来是在说『我打算去查…』」时才追问一次；
                    // 寒暄与纯知识问答直接当最终答复，不逼它编一份计划出来。
                    if plan_rounds < MAX_PLAN_ROUNDS && promises_action(&turn.text) {
                        plan_rounds += 1;
                        request.messages.push(me_core::ai::AiWireMessage::User {
                            content: "你还没有提交计划。这次请求需要查数据或做动作，请先调用 \
plan_submit 把要做的事写成 1–8 步（每步写清 tool / args / expect），我确认后再执行；\
如果确实不需要任何工具，请调用 answer_directly 并说明理由。"
                                .to_string(),
                        });
                        continue;
                    }
                    if plan_rounds < MAX_PLAN_ROUNDS && is_blank_answer(&turn.text) {
                        plan_rounds += 1;
                        request.messages.push(me_core::ai::AiWireMessage::User {
                            content: "你还没有回答。请先调用 plan_submit 给出计划，\
或者调用 answer_directly 说明这次为什么不需要工具。"
                                .to_string(),
                        });
                        continue;
                    }
                    // 第二道兜底：已经问过一次、模型仍然只给文本 → 引擎合成单步清单。
                    // 首轮就不追问（问过不算），所以寒暄与纯问答不会凭空多出一份计划。
                    if plan_rounds > 0 && plan.is_none() && !answered_directly {
                        plan = Some(synthetic_plan());
                        error_text = Some("模型未提交计划，已按单步处理".to_string());
                    }
                    // 不像任务、也没计划：这段文本就是答复，直接收尾。
                    break;
                }

                // 调用过工具却一个字没说：界面会停在一块空白上，用户既不知道做成了什么，
                // 也不知道为什么没结果。先补一句提醒让它交代，只给一次机会。
                if blank_retries < MAX_BLANK_RETRIES
                    && (executed_calls > 0 || plan.is_some())
                    && is_blank_answer(&turn.text)
                {
                    blank_retries += 1;
                    request.messages.push(me_core::ai::AiWireMessage::User {
                        content: "你还没有把结果告诉用户。请用自然语言说明：找到了什么、\
没找到什么、下一步建议是什么；本轮调用过工具时按 [结果] 格式收尾。"
                            .to_string(),
                    });
                    continue;
                }
                // 催过一次还是空白：不再催（再催就是逼它编），交给兜底摘要。
                if is_blank_answer(&turn.text) {
                    break;
                }
                // 说了一句「我这就去查」然后停手：清单还开着，但一步都没做。
                // 这是唯一值得催的情形——再给它一轮，而不是直接收尾把清单丢在半路。
                //
                // 判据收得很窄（`promises_action`）：模型已经给出实质回答时再催，
                // 只会让它把好好的答案重写一遍，用户白等一轮。
                if executed_calls == 0
                    && promises_action(&turn.text)
                    && nudges < MAX_PLAN_NUDGES
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

            // 模型这一段想完了，接下来要调工具：**收段 + 换段号**。
            //
            // 顺序不能反：先把节流窗口里的残余顶着**旧段号**发出去，
            // 再 cut，最后自增。否则残余会跟在新段号后面到达 UI，被塞进
            // 错误的段里（只在节流窗口恰跨边界时偶发，极难复现）。
            if let Some(tail) = thinking_throttle
                .lock()
                .ok()
                .and_then(|mut state| state.flush())
                && !tail.is_empty()
            {
                let segment = thinking.lock().map(|acc| acc.segment_no).unwrap_or(1);
                self.emit(Event::AiStreamDelta {
                    conversation,
                    message,
                    seq: seq_counter.fetch_add(1, Ordering::Relaxed) + 1,
                    delta: tail,
                    kind: me_core::command::AiDeltaKind::Thinking,
                    segment,
                })
                .await;
            }
            if let Ok(mut acc) = thinking.lock() {
                acc.cut(true);
                acc.next_segment();
            }

            let mut results: Vec<AiToolResult> = Vec::new();
            for call in &turn.tool_calls {
                // 动机在**执行前**就要发出去：界面靠它解释「为什么现在调这个工具」。
                let (motive, motive_source) = me_core::ai::tool_motive(&call.arguments);

                // 同参数 + 不可重试错误已经失败过：不再执行，直接回灌「换个参数」。
                // 必须放在 `executed_calls` 计数之前——被拦下的调用没有产生任何 IO，
                // 不该消耗那 24 次预算；否则模型连撞几次就把预算烧光了。
                if let Some(previous) = budget.previous_failure(call).map(str::to_string) {
                    self.emit(Event::AiToolStarted {
                        conversation,
                        call_id: call.id.clone(),
                        tool: call.name,
                        motive: motive.clone(),
                        motive_source,
                    })
                    .await;
                    let result =
                        crate::ai_tools::intercepted_result(call, motive, motive_source, &previous);
                    self.emit(Event::AiToolCompleted {
                        conversation,
                        call_id: result.call_id.clone(),
                        tool: result.tool,
                        status: result.trace.status,
                        summary: result.trace.summary.clone(),
                        duration_ms: result.trace.duration_ms,
                        motive: result.trace.motive.clone(),
                        motive_source: result.trace.motive_source,
                        step_id: result.trace.step_id,
                        attempt: result.trace.attempt,
                    })
                    .await;
                    traces.push(result.trace.clone());
                    results.push(result);
                    continue;
                }

                // 防御性拦截：PLAN 阶段根本没下发业务工具，模型仍可能凭历史记忆去调。
                // 这里拒绝**不消耗**预算（没有产生 IO），并告诉它现在该做什么。
                if phase == Phase::Plan && !call.name.is_bookkeeping() {
                    self.emit(Event::AiToolStarted {
                        conversation,
                        call_id: call.id.clone(),
                        tool: call.name,
                        motive: motive.clone(),
                        motive_source,
                    })
                    .await;
                    let result = crate::ai_tools::phase_rejected_result(
                        call,
                        motive,
                        motive_source,
                        "现在还在**制定计划**阶段，不能执行工具。请先调用 plan_submit 把要做的事\
写成 1–8 步（每步写清 tool / args / expect），我会在计划阶段就帮你校验参数；\
若确实不需要任何工具，请调用 answer_directly 并说明理由。",
                        "计划阶段不能执行工具",
                    );
                    self.emit(Event::AiToolCompleted {
                        conversation,
                        call_id: result.call_id.clone(),
                        tool: result.tool,
                        status: result.trace.status,
                        summary: result.trace.summary.clone(),
                        duration_ms: result.trace.duration_ms,
                        motive: result.trace.motive.clone(),
                        motive_source: result.trace.motive_source,
                        step_id: result.trace.step_id,
                        attempt: result.trace.attempt,
                    })
                    .await;
                    traces.push(result.trace.clone());
                    results.push(result);
                    continue;
                }

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
                // 绑定到清单里的某一步：模型可以显式给 `step_id`，也可以让引擎按
                // motive / 工具名兜底。绑定失败**不拒绝执行**——报错换不来让它改对。
                let bound = if phase == Phase::Execute && !call.name.is_bookkeeping() {
                    plan.as_ref().and_then(|current| {
                        let id = crate::ai_tools::bind_step(current, call, &motive)?;
                        Some(id)
                    })
                } else {
                    None
                };
                if let (Some(id), Some(current)) = (bound, plan.as_mut()) {
                    current.begin_step(id);
                }
                // 清单类工具成功时不进行为条：清单条本身就是它的展示位，
                // 再在活动条里刷一条「更新清单」只会把真正的工具调用挤下去。
                let visible = !call.name.is_bookkeeping();
                if visible {
                    self.emit(Event::AiToolStarted {
                        conversation,
                        call_id: call.id.clone(),
                        tool: call.name,
                        motive: motive.clone(),
                        motive_source,
                    })
                    .await;
                }
                let plan_before = plan.clone();
                // 引擎在同一轮里自己重试，不把重试甩给模型。
                //
                // 两类重试：
                // 1. 临时故障（网络抖动、超时）→ **同参数**重试，退避 300ms → 900ms；
                // 2. 0 命中且 `on_empty = narrow` → **放宽条件**重试。
                //
                // 参数错误、权限错误、写操作失败一律不重试：参数不变结果就不会变，
                // 而写操作重试一次可能就真的做两遍。
                let quota_before = budget.quota_snapshot();
                // 本步剩余的机会次数（引擎重试与模型调用共用）。
                let mut tries_left = bound
                    .and_then(|id| {
                        plan.as_ref()
                            .and_then(|current| current.step(id))
                            .map(|step| step.attempts)
                    })
                    .map(|used| {
                        me_core::ai_plan::MAX_STEP_ATTEMPTS
                            .saturating_sub(used)
                            .max(1) as usize
                    })
                    .unwrap_or(me_core::ai_plan::MAX_STEP_ATTEMPTS as usize);
                let mut retry_no = 0usize;
                // 物理尝试次数（临时重试 + 放宽重试都算）：这一步真正花掉的次数。
                let mut physical_tries = 0u8;
                // 放宽后用来重试的参数：`None` 表示还在用原来的参数。
                let mut relaxed_args: Option<serde_json::Value> = None;
                let result = loop {
                    tries_left = tries_left.saturating_sub(1);
                    physical_tries = physical_tries.saturating_add(1);
                    // 重试时用放宽后的参数构造一次新的调用。
                    let attempt_call = match relaxed_args.as_ref() {
                        Some(args) => {
                            let mut clone = call.clone();
                            clone.arguments = args.clone();
                            clone
                        }
                        None => call.clone(),
                    };
                    let attempt = tokio::select! {
                        biased;
                        _ = token.cancelled() => {
                            status = AiMessageStatus::Cancelled;
                            break 'rounds;
                        }
                        result = self.execute_ai_tool(conversation, &attempt_call, &mut budget, &mut plan) => result,
                    };
                    let transient = attempt
                        .failure
                        .is_some_and(|failure| failure.is_auto_retryable())
                        && !call.name.has_side_effect()
                        && retry_no < RETRY_BACKOFF.len()
                        && tries_left > 0;
                    if transient {
                        retry_no += 1;
                        // 重试是**同一次逻辑调用**：配额退回去，
                        // 否则第二次就会撞上「本次消息已查询过天气」这类限额错误，
                        // 把真正的网络故障盖掉。
                        budget.restore_quota(quota_before);
                        tokio::time::sleep(RETRY_BACKOFF[retry_no - 1]).await;
                        continue;
                    }
                    // 0 命中：按 `on_empty` 决定是放宽重试、问用户，还是接受。
                    let empty = !attempt.is_error
                        && crate::ai_tools::is_empty_result(&attempt.trace.summary);
                    if empty && tries_left > 0 {
                        // 放宽基于**这次实际调用的参数**，不是计划里声明的那份：
                        // 两者可以不同（模型在计划里写 `days:7`，执行时加了
                        // `unread_only`），按声明放宽会把多出来的条件丢掉。
                        let next = me_core::ai_plan::relax_args(&attempt_call.arguments);
                        if let Some(args) = next {
                            // 放宽后的参数写回清单：用户看到的是**真正跑过**的条件。
                            let note = format!(
                                "0 命中，条件已放宽为 {} 后重试",
                                args.as_object()
                                    .map(|map| {
                                        map.iter()
                                            .map(|(key, value)| format!("{key}={value}"))
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    })
                                    .unwrap_or_default()
                            );
                            if let (Some(id), Some(current)) = (bound, plan.as_mut()) {
                                current.set_step_args(id, args.clone(), &note);
                            }
                            relaxed_args = Some(args);
                            continue;
                        }
                    }
                    break attempt;
                };
                if let Some(id) = bound
                    && let Some(current) = plan.as_mut()
                {
                    // 记耗时：这一趟（含所有引擎重试）花了多久。
                    // 用来调 `MAX_STEP_ATTEMPTS` 与各工具的超时。
                    current.add_duration(id, u64::from(result.trace.duration_ms));
                    let empty =
                        !result.is_error && crate::ai_tools::is_empty_result(&result.trace.summary);
                    if empty {
                        // 0 命中不是成功也不是失败：按 `on_empty` 决定这一步的终态。
                        let action = current.apply_empty(id, &result.trace.summary, physical_tries);
                        if action == me_core::ai_plan::EmptyAction::Ask {
                            // 需要用户补充：这一轮就停在这里等用户回答。
                            awaiting_user = true;
                        }
                    } else {
                        let outcome = if result.is_error {
                            me_core::ai_plan::AttemptOutcome::Failed(
                                crate::ai_tools::classify_failure(
                                    call.name,
                                    result
                                        .failure
                                        .unwrap_or(me_core::ai::ToolFailure::InvalidArgs),
                                    &result.trace.summary,
                                ),
                            )
                        } else {
                            me_core::ai_plan::AttemptOutcome::Done {
                                evidence: result.trace.summary.clone(),
                            }
                        };
                        // 引擎重试也是「尝试」：一次模型调用可能对应多次物理尝试，
                        // 都要算进这一步的机会预算里。
                        current.record_attempts(id, physical_tries, outcome);
                    }
                    // 被引用步失败 → 依赖它的步自动跳过，别拿空参数再撞一次。
                    current.skip_dependents();
                }
                if call.name == AiToolName::AnswerDirectly && !result.is_error {
                    answered_directly = true;
                }
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
                // 先把绑定信息算好，事件与落库用的是同一份，不会前后不一致。
                let mut trace = result.trace.clone();
                trace.step_id = trace.step_id.or(bound);
                trace.attempt = trace
                    .step_id
                    .and_then(|id| plan.as_ref().and_then(|current| current.step(id)))
                    .map(|step| step.attempts);
                // 清单类工具成功时不显示，但**失败必须显示**。
                //
                // 2026-11-07 的事故：模型为了一份清单连续改了 4 次参数
                // （capability 省略 / 空串 / 改成 tool 步），4 次工具往返全废在
                // 参数上——而这些失败在界面上一个字都没显示。用户只看到最后
                // 成功的那一次，完全不知道模型挣扎了多久、错在哪。
                //
                // 失败的清单类调用同样发 Started：活动条是「先 Started 再
                // Completed」成对的，少了 Started 会让 UI 的实时工具条目
                // 悬着不消。
                let failed = result.is_error || result.trace.status == AiToolStatus::Error;
                let show = visible || failed;
                if show {
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
                        step_id: trace.step_id,
                        attempt: trace.attempt,
                    })
                    .await;
                }
                // 每一步的尝试都发一次事件：清单条靠它显示「第 2/3 次」角标。
                // 这条事件是**可丢**的（丢一次只是少一个角标）。
                if let Some(id) = bound
                    && let Some(step) = plan.as_ref().and_then(|current| current.step(id))
                {
                    self.emit(Event::AiPlanStepAttempt {
                        conversation,
                        message,
                        step_id: id,
                        attempt: step.attempts,
                        max_attempts: me_core::ai_plan::MAX_STEP_ATTEMPTS,
                        tool: step.tool,
                        args_digest: step.args_digest(),
                        error: step.failure.as_ref().map(|failure| failure.human()),
                    })
                    .await;
                }
                // 落库的 trace 同理：失败的清单类调用要留下痕迹，
                // 成功的不要（否则活动区会被「更新清单」刷满）。
                if show {
                    traces.push(trace);
                }
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

            // 阶段推进放在工具结果**之后**：`assistant(tool_calls)` 后面必须紧跟
            // 对应的 tool 消息，中间插一条 user 会让协议对不上号。

            // 计划到手 → 进入执行阶段，并把「接下来做什么」写清楚。
            //
            // 这里只要求「有清单」，不要求 `is_executable()`：模型用旧接口
            // （`plan_update`）交上来的 v1 清单也算数。它的步骤没有工具与参数，
            // 清单条只能显示标题与状态——但那是模型的表达质量问题，
            // 不该让整轮卡在 PLAN 阶段反复催。
            if phase == Phase::Plan && plan.is_some() {
                phase = Phase::Execute;
                if let Some(current) = plan.as_ref() {
                    request.messages.push(me_core::ai::AiWireMessage::User {
                        content: format!(
                            "计划已收到（{} 步）。现在开始执行，工具调用请带上 step_id。{}",
                            current.steps.len(),
                            current.summary_line()
                        ),
                    });
                }
            } else if phase == Phase::Plan && answered_directly {
                // 模型声明「不需要工具」：不再给工具，逼它把话说出来。
                phase = Phase::Final;
                request.messages.push(me_core::ai::AiWireMessage::User {
                    content: "好，请直接回答用户。".to_string(),
                });
            }

            // 重列：新计划要过「历史失败指纹」这道闸。
            if phase == Phase::Replan
                && let Some(candidate) = plan.clone()
                && let Some(previous) = prev_plan.clone()
            {
                let meta = replan_meta.clone().unwrap_or(ReplanMeta {
                    diagnosis: me_core::ai_plan::Diagnosis::WrongArgs,
                    reason: "上一轮的执行没有达到目标".to_string(),
                    changes: "换一组参数重试".to_string(),
                    failed_step: None,
                });
                let outcome = me_core::ai_plan::validate_replan(
                    &previous,
                    &candidate,
                    &me_core::ai_plan::ReplanRequest {
                        diagnosis: meta.diagnosis,
                        reason: &meta.reason,
                        changes: &meta.changes,
                        at: now_unix(),
                    },
                    &budget.failed_fingerprints(),
                );
                match outcome {
                    Ok(outcome) => {
                        let dropped = outcome.dropped.len();
                        // 一步不剩时保留**旧清单**：用户需要看到「哪些步失败了」，
                        // 而不是让清单凭空消失。
                        plan = outcome.plan.clone().or_else(|| Some(previous.clone()));
                        match outcome.plan {
                            Some(current) => {
                                self.emit(Event::AiPlanUpdated {
                                    conversation,
                                    message,
                                    plan: current.clone(),
                                })
                                .await;
                                // 重列记录：界面据此显示「第 N 次计划」与上一轮的问题。
                                if let Some(record) = current.replans.last() {
                                    let failed_step_titles = record
                                        .failed_steps
                                        .iter()
                                        .filter_map(|id| {
                                            current
                                                .steps
                                                .iter()
                                                .find(|step| step.id == *id)
                                                .map(|step| step.title.clone())
                                        })
                                        .collect();
                                    self.emit(Event::AiPlanReplanned {
                                        conversation,
                                        message,
                                        round: record.round,
                                        diagnosis: record.diagnosis,
                                        changes: record.changes.clone(),
                                        failed_step_titles,
                                    })
                                    .await;
                                }
                                phase = Phase::Execute;
                                request.messages.push(me_core::ai::AiWireMessage::User {
                                    content: format!(
                                        "已换成第 {} 次计划（砍掉 {dropped} 个与历史失败重复的步骤）。\
继续执行，工具调用请带上 step_id。{}",
                                        current.replans.len(),
                                        current.summary_line()
                                    ),
                                });
                            }
                            None => {
                                // 一步不剩：新计划里全是失败过的调用，再试也是一样的结果。
                                phase = Phase::Final;
                                request.messages.push(me_core::ai::AiWireMessage::User {
                                    content: "新计划里的步骤全都和之前失败过的调用一样，再试也不会\
有不同结果。请直接说明当前能给出的最好结果，不要再提交计划。"
                                        .to_string(),
                                });
                            }
                        }
                    }
                    Err(err) => {
                        // 退回旧清单：宁可显示「上一轮失败了」，也不要把一份
                        // 没过校验的计划留在界面上。
                        plan = Some(previous.clone());
                        replan_rejections += 1;
                        let exhausted = replan_rejections >= MAX_REPLAN_REJECTIONS;
                        if exhausted {
                            phase = Phase::Final;
                        }
                        request.messages.push(me_core::ai::AiWireMessage::User {
                            content: if exhausted {
                                format!(
                                    "新计划没有通过校验：{}。重列次数已用完，请直接给出最终答复。",
                                    err.user_message()
                                )
                            } else {
                                format!(
                                    "新计划没有通过校验：{}。请改参数后重新提交（或直接说明结论）。",
                                    err.user_message()
                                )
                            },
                        });
                    }
                }
            }

            // 这一步要求用户补充信息：整轮停下来，把问题交给用户。
            if awaiting_user {
                phase = Phase::Final;
                request.messages.push(me_core::ai::AiWireMessage::User {
                    content: "有一步检索不到结果且该步声明了「需要用户补充」。请直接向用户说明需要什么信息（例如该用哪个邮箱、要看哪个时间范围），并停下来等用户回答，不要继续执行后面的步骤。"
                        .to_string(),
                });
            }

            // 计划跑完了 → 先自检（只为「有失败步」的情况），再写答复。
            //
            // 全部成功就不必让模型自评：那只会多花一轮，还可能把一份好好的
            // 结果自贬成 partial。
            if phase == Phase::Execute
                && plan
                    .as_ref()
                    .is_some_and(|current| !current.has_open_steps())
            {
                let failed = plan.as_ref().is_some_and(|current| current.any_failed());
                let rounds_left = plan.as_ref().map_or(0, |current| current.replans.len())
                    < me_core::ai_plan::MAX_REPLANS as usize;
                if failed && rounds_left {
                    phase = Phase::Reflect;
                    if let Some(current) = plan.as_ref() {
                        request.messages.push(me_core::ai::AiWireMessage::User {
                            content: reflect_prompt(current, &user_input),
                        });
                    }
                } else {
                    phase = Phase::Final;
                    request.messages.push(me_core::ai::AiWireMessage::User {
                        content: "计划已执行完。请按 [结果] 格式给出最终答复，不要只重复工具回执。"
                            .to_string(),
                    });
                }
            }
        }

        // 收尾：两条通道各自把未满 50ms 的残余发出去，正文同时作为落库内容。
        let (mut content, answer_tail) = {
            let Ok(mut state) = answer_throttle.lock() else {
                return;
            };
            let tail = state.flush().unwrap_or_default();
            (state.total.clone(), tail)
        };
        if !answer_tail.is_empty() {
            self.emit(Event::AiStreamDelta {
                conversation,
                message,
                seq: seq_counter.fetch_add(1, Ordering::Relaxed) + 1,
                delta: answer_tail,
                kind: me_core::command::AiDeltaKind::Answer,
                segment: 0,
            })
            .await;
        }
        if let Some(tail) = thinking_throttle
            .lock()
            .ok()
            .and_then(|mut state| state.flush())
            && !tail.is_empty()
        {
            let segment = thinking.lock().map(|acc| acc.segment_no).unwrap_or(1);
            self.emit(Event::AiStreamDelta {
                conversation,
                message,
                seq: seq_counter.fetch_add(1, Ordering::Relaxed) + 1,
                delta: tail,
                kind: me_core::command::AiDeltaKind::Thinking,
                segment,
            })
            .await;
        }
        // 收最后一段（没有后续工具），并生成要落库的思考块。
        let thinking_for_db = match thinking.lock() {
            Ok(mut acc) => std::mem::replace(&mut *acc, ThinkingAccumulator::new()).finish(),
            Err(_) => None,
        };

        // 兜底：整轮调用过工具却没有任何正文 → 用执行回执拼一段，绝不留白。
        //
        // 状态仍是 `complete`：`ai_messages.status` 的 CHECK 只有 streaming/complete/
        // failed/cancelled 四个值，加 `partial` 需要一次迁移。「没拿到结果」这层语义
        // 由正文首句与 `error` 字段承担，UI 重载后用户能看懂发生了什么。
        if status == AiMessageStatus::Complete
            && (executed_calls > 0 || plan.is_some())
            && is_blank_answer(&content)
        {
            content = fallback_answer(plan.as_ref(), &traces);
            error_text = Some("模型未输出正文，已用执行回执兜底".to_string());
        }

        let usage = *usage.lock().unwrap_or_else(|e| e.into_inner());
        let error_for_db = error_text.clone();
        // 清单必须是**最终态**再落库：取消时只把「进行中」的步标跳过，
        // 还没开始的步保持 pending（用户可能接着说「继续」）。
        let plan_for_db = finalize_plan(plan, status);
        // 清单统计：调 `MAX_STEP_ATTEMPTS` 与各工具超时时看这行。
        if let Some(current) = plan_for_db.as_ref() {
            tracing::info!(
                conversation = conversation.0,
                stats = %current.stats().summary(),
                "AI 清单统计"
            );
        }
        // 收尾结论：有清单才算（模型声明「不需要工具」的对话没有清单）。
        if let Some(current) = plan_for_db.as_ref() {
            let (done, total) = current.progress();
            let verdict = if current.any_failed() {
                me_core::ai_plan::Verdict::Partial
            } else if done == total {
                me_core::ai_plan::Verdict::Done
            } else {
                me_core::ai_plan::Verdict::Partial
            };
            self.emit(Event::AiPlanFinished {
                conversation,
                message,
                verdict,
                done,
                total,
                replans: current.replans.len(),
            })
            .await;
        }
        let result = self
            .db(move |s| {
                s.finish_assistant_message(
                    message,
                    &content,
                    status,
                    Some(usage),
                    &traces,
                    plan_for_db.as_ref(),
                    thinking_for_db.as_ref(),
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

/// 一次回复内部的阶段。**工具集随阶段变化**，这是「强制清单」的落地方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// 只能提交计划（`plan_submit`）或声明不需要（`answer_directly`）。
    Plan,
    /// 按计划执行：能力工具 + `plan_update`。
    Execute,
    /// 自检：把执行事实回灌给模型，要它判断「这次到底做完没有」。不给工具。
    Reflect,
    /// 重列：只给 `plan_submit`，拿到新计划后由引擎按「历史失败指纹」裁剪。
    Replan,
    /// 收尾：不下发任何工具，模型必须把话说出来。
    Final,
}

/// REFLECT 判定「要重列」时带过来的三样东西。
///
/// 为什么不把它们塞进 `plan_submit` 的参数：那是模型每次都要写的协议字段，
/// 多三个字段就多三处可能写错的地方；而这些信息本来就是自检那一轮的产物，
/// 由引擎带过来既省一次往返，也不会被模型「忘掉」。
#[derive(Debug, Clone)]
struct ReplanMeta {
    diagnosis: me_core::ai_plan::Diagnosis,
    reason: String,
    changes: String,
    failed_step: Option<u8>,
}

/// 自检要的回灌事实表。**不含正文原文**：8 步全跑完也只有几百字，
/// 既不会把上下文顶爆，也不会把邮件内容当成指令送进模型。
fn reflect_prompt(plan: &me_core::ai_plan::Plan, user_input: &str) -> String {
    format!(
        "【本轮执行事实】\n目标：{}\n{}\n用户原始需求：{}\n\n请判断：这次任务完成了吗？只输出一段 JSON，不要解释、不要代码块：\n{{\"verdict\":\"done|partial|failed\",\"failed_step\":<失败的步号，完成时不填>,\
\"diagnosis\":\"wrong_plan|wrong_args|missing_data|tool_broken|user_ambiguous\",\
\"reason\":\"一句话说清哪里不对（至少 8 字）\",\
\"replan_hint\":\"如果重来一次该怎么改（至少 4 字；verdict=failed 时必填）\"}}\n\
判定标准：全部步骤都完成、且结果确实回答了用户的需求 → done；\n有步骤失败但对用户的问题仍给出了可用的答案 → partial；\n拿不到能回答用户的结果 → failed。不要为了显得勤奋而判 failed。",
        plan.goal,
        plan.facts_block(),
        me_core::ai::truncate_chars(user_input, 200)
    )
}

/// 重列要的指令：把「错在哪、怎么改」说清楚，否则模型只会重抄一遍。
fn replan_prompt(meta: &ReplanMeta) -> String {
    format!(
        "这一轮没有达到目标：{}\n诊断：{}\n改法建议：{}\n\n请调用 plan_submit 提交一份**新的**计划。要求：\n- 不要重复已经失败过的调用参数（换关键词、放宽时间窗、去掉某个限定，或换一条路）；\n\
- 如果问题在于本地没有这份数据，就加一步先让用户打开/同步；\n\
- 如果需求本身含糊，就把最后一步写成 kind=ask 停下来问用户。\n\
重列次数有限（最多 {} 次），这一次要真的换一个思路。",
        meta.failed_step
            .map(|id| format!("第 {id} 步没做成"))
            .unwrap_or_else(|| "有步骤没有做成".to_string()),
        meta.diagnosis.label(),
        meta.changes,
        me_core::ai_plan::MAX_REPLANS
    )
}

/// 模型两轮都不提交计划时的兜底：合成一份「直接回答」的单步清单。
///
/// 有清单比没有强：界面至少能显示「这一步做了什么」，用户不会面对一块空白，
/// 事后也能看出「模型没配合」，而不是以为工具全挂了。
fn synthetic_plan() -> me_core::ai_plan::Plan {
    let now = now_unix();
    let mut step = me_core::ai_plan::PlanStep::new(
        1,
        "直接回答",
        me_core::ai_plan::Capability::Mail,
        me_core::ai_plan::PlanStatus::Running,
    );
    step.kind = me_core::ai_plan::StepKind::Reason;
    step.expect = Some("给出当前能给出的最好答案".to_string());
    me_core::ai_plan::Plan::v2("按单步直接回答", vec![step], now)
}

/// 模型是不是在说「我这就去办」——只有这种空头承诺值得追一句。
///
/// 判据故意窄：把一段实质回答误判成「还没开始」会白烧一轮，还会让模型
/// 把刚写好的答案重写一遍。宁可不催，也不要打断一个已经答完的模型。
fn promises_action(text: &str) -> bool {
    if is_blank_answer(text) {
        return false;
    }
    [
        "我先",
        "我帮",
        "马上",
        "稍等",
        "这就",
        "接下来",
        "让我",
        "我看看",
        "我去",
        "我来",
    ]
    .iter()
    .any(|hint| text.contains(hint))
}

/// 一个可见字符都没有才算「没说话」——那次事故的正文就是 13 个换行。
///
/// **不要**提高阈值去判「回答得够不够好」：`记住了。`、`已删除。` 这类短回执是
/// 完全合法的回答，用字符数卡会误伤它们（而且会把正常回复也塞进兜底摘要）。
/// 引擎只负责「一个字都没说」这一种情况。
fn is_blank_answer(text: &str) -> bool {
    text.chars().all(char::is_whitespace)
}

/// 模型整轮没写出正文时的兜底：只陈述**已经发生的事实**，不编造结论。
///
/// 空白比错误更糟——用户既不知道做成了什么，也不知道为什么没结果。
fn fallback_answer(plan: Option<&me_core::ai_plan::Plan>, traces: &[AiToolTrace]) -> String {
    let mut out = String::from("这次没有拿到可用的结果。\n\n");
    match plan {
        Some(plan) => {
            let (done, total) = plan.progress();
            out.push_str(&format!("计划：完成 {done}/{total} 步\n"));
            for step in &plan.steps {
                out.push_str(&format!(
                    "· {}：{} —— {}\n",
                    step.title,
                    step.status.label(),
                    step.evidence.as_deref().unwrap_or("没有留下回执"),
                ));
            }
        }
        None if !traces.is_empty() => {
            out.push_str("执行过的操作：\n");
            for trace in traces {
                out.push_str(&format!("· {}：{}\n", trace.tool.label(), trace.summary));
            }
        }
        None => {}
    }
    out.push_str(
        "\n你可以：① 让我先同步最近的邮件；② 换一个更具体的说法（例如「这周未读的邮件」）。",
    );
    out
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
        AiMessageStatus::Complete => {
            // 清单的所有权在引擎，所以收尾也得由引擎把状态收干净：
            // 留一个 `running` 在界面上就是永远转圈的圆圈。
            for step in current.steps.iter_mut() {
                if step.status != me_core::ai_plan::PlanStatus::Running {
                    continue;
                }
                if step.kind == me_core::ai_plan::StepKind::Reason || step.evidence.is_some() {
                    // 推理步把答案写在正文里；工具步有回执（含「0 命中」）就是执行过了。
                    step.status = me_core::ai_plan::PlanStatus::Done;
                    if step.evidence.is_none() {
                        step.evidence = Some("已直接回答".to_string());
                    }
                } else {
                    // 一次都没执行过：如实说没做，不要假装完成。
                    step.status = me_core::ai_plan::PlanStatus::Skipped;
                    step.evidence = Some("本轮没有执行".to_string());
                }
            }
        }
        AiMessageStatus::Cancelled => {
            current.skip_running("用户停止了生成");
        }
        AiMessageStatus::Failed | AiMessageStatus::Streaming => {
            current.skip_open("回复中断，这一步没有执行");
        }
    }
    plan
}

#[cfg(test)]
mod throttle_tests {
    use super::*;

    /// flush 在没有残余时返回 None **并且不动时钟**：否则空 flush 会把
    /// 下一批增量硬生生再压 50ms，切段之后的首字会明显卡一下。
    #[test]
    fn flush_returns_none_when_empty() {
        let mut t = DeltaThrottle::new();
        assert_eq!(t.flush(), None);
        // 立刻 push 仍应因为「初始允许立刻发送」而直接产出。
        assert_eq!(t.push("首字"), Some("首字".to_string()));
        assert_eq!(t.flush(), None);
    }

    /// 切段前 flush 必须取走攒着的残余，否则它会顶着新段号到达 UI。
    #[test]
    fn flush_drains_pending_before_segment_switch() {
        let mut t = DeltaThrottle::new();
        assert_eq!(t.push("第一段"), Some("第一段".to_string()));
        // 上一次 take 刚刷新时钟，这一片会攒在窗口里。
        assert_eq!(t.push("尾巴"), None);
        assert_eq!(t.flush(), Some("尾巴".to_string()));
        assert_eq!(t.flush(), None, "取过一次之后不能再取到");
        assert_eq!(t.total, "第一段尾巴");
    }

    /// 超长思考被截断，后续增量直接丢弃，并带上 truncated 标记。
    #[test]
    fn thinking_accumulator_truncates_at_limit() {
        let mut acc = ThinkingAccumulator::new();
        let long = "想".repeat(me_core::ai::MAX_THINKING_CHARS + 100);
        let kept = acc.push(&long);
        assert_eq!(kept.chars().count(), me_core::ai::MAX_THINKING_CHARS);
        assert!(acc.truncated);
        assert!(acc.push("再来一段").is_empty(), "截断后不该继续累积");
        let block = acc.finish().expect("截断也要留下标记");
        assert!(block.truncated);
        assert_eq!(block.segments.len(), 1);
    }

    /// 段数超上限时同样置 truncated，而不是无限加段。
    #[test]
    fn thinking_accumulator_caps_segment_count() {
        let mut acc = ThinkingAccumulator::new();
        for i in 0..me_core::ai::MAX_THINKING_SEGMENTS {
            acc.push(&format!("第 {i} 段"));
            acc.cut(true);
            acc.next_segment();
        }
        assert!(acc.push("第 17 段").is_empty());
        let block = acc.finish().expect("有内容");
        assert_eq!(block.segments.len(), me_core::ai::MAX_THINKING_SEGMENTS);
        assert!(block.truncated);
    }

    /// 没有任何思考时 finish 返回 None（界面不画这一块）。
    #[test]
    fn thinking_accumulator_returns_none_when_empty() {
        assert!(ThinkingAccumulator::new().finish().is_none());
    }
}
