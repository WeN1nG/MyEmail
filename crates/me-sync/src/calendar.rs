//! 日历用例层：写操作的**唯一实现**（`doc/v03架构设计.md` §2.7）。
//!
//! `dispatch` 与未来的 `ai_tools` 都只是薄壳：把参数转成
//! [`CalendarEventDraft`]、调用这里的用例、把错误翻成各自的反馈形态。
//! 「算 `recurrence_until`、校验、来源归属、事务内落库、发事件」只在这里做一次。
//!
//! 为什么 AI 侧不复用 `Command`：`Command` 是无界通道的 fire-and-forget，
//! 拿不到返回值（工具调用必须同步返回事件 id），且 `ai_tools` 本身就跑在同一个
//! `SyncService` 里，发 `Command` 等于让服务给自己排队。

use std::time::Duration;

use me_core::calendar::{
    Calendar, CalendarEvent, CalendarEventDraft, CalendarEventId, CalendarId, CalendarSource,
    MAX_REMINDER_MINUTES, validate_draft,
};
use me_core::model::now_unix;
use me_core::port::{ImapSessionFactory, SmtpSender, StorePort};
use me_core::{Command, CoreError, Event};

use crate::SyncService;

/// 写操作的来源。只影响审计、去重与专属事件，**不改变核心语义**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    /// 用户经 `Command` 发起。
    User,
    /// AI 工具发起（v0.3 尚未接入 `ai_tools`，先把形态定下来）。
    Ai { conversation: i64 },
    /// 提醒循环、迁移、批量导入。
    System,
}

impl Actor {
    /// 事件来源。`Ics` 由导入流程显式传入，不走 `Actor`。
    fn source(self) -> CalendarSource {
        match self {
            Actor::Ai { .. } => CalendarSource::Ai,
            // 系统侧当前只用于提醒扫描，不写事件；保守记为本地。
            Actor::User | Actor::System => CalendarSource::Local,
        }
    }
}

/// 计算冗余上界列。`Unsupported`（超出子集）按无限重复处理：保留原文，
/// 但写入 `NULL`，让系列始终可见——这与 `expand_event` 的降级口径一致。
fn recurrence_upper_bound(draft: &CalendarEventDraft) -> Result<Option<i64>, CoreError> {
    match me_core::recurrence_until(
        draft.rrule.as_deref(),
        draft.start_utc,
        draft.timezone.as_deref(),
    ) {
        Ok(until) => Ok(until),
        Err(err) if err.is_degradable() => Ok(None),
        Err(err) => Err(CoreError::validation(format!("{err}"))),
    }
}

impl<S, F, M> SyncService<S, F, M>
where
    S: StorePort + Clone,
    F: ImapSessionFactory + 'static,
    M: SmtpSender + 'static,
{
    /// 新建日程。UI 命令与（未来的）AI 工具共用。
    pub(crate) async fn create_event(
        &self,
        draft: CalendarEventDraft,
        actor: Actor,
    ) -> Result<CalendarEvent, CoreError> {
        validate_draft(&draft)?;
        let until = recurrence_upper_bound(&draft)?;
        let source = actor.source();
        let event = self
            .db(move |store| {
                store.ensure_default_calendar()?;
                store.create_event(&draft, until, source)
            })
            .await?;
        self.emit(Event::CalendarEventsChanged {
            calendar: event.calendar_id,
        })
        .await;
        Ok(event)
    }

    /// 更新日程。保留原 `source`（由仓储保证），`sequence` 自动 +1。
    pub(crate) async fn update_event(
        &self,
        id: CalendarEventId,
        draft: CalendarEventDraft,
        _actor: Actor,
    ) -> Result<CalendarEvent, CoreError> {
        validate_draft(&draft)?;
        let until = recurrence_upper_bound(&draft)?;
        let event = self
            .db(move |store| store.update_event(id, &draft, until))
            .await?;
        self.emit(Event::CalendarEventsChanged {
            calendar: event.calendar_id,
        })
        .await;
        Ok(event)
    }

    pub(crate) async fn delete_event(
        &self,
        id: CalendarEventId,
        _actor: Actor,
    ) -> Result<(), CoreError> {
        let calendar = self
            .db(move |store| {
                let event = store.get_event(id)?;
                store.soft_delete_event(id)?;
                Ok(event.map(|event| event.calendar_id))
            })
            .await?;
        if let Some(calendar) = calendar {
            self.emit(Event::CalendarEventsChanged { calendar }).await;
        }
        Ok(())
    }

    pub(crate) async fn create_calendar(
        &self,
        name: String,
        color: String,
        _actor: Actor,
    ) -> Result<Calendar, CoreError> {
        if name.trim().is_empty() {
            return Err(CoreError::validation("日历名称不能为空"));
        }
        let calendar = self
            .db(move |store| store.create_calendar(name.trim(), &color))
            .await?;
        self.emit_calendars().await?;
        Ok(calendar)
    }

    pub(crate) async fn set_calendar_visible(
        &self,
        id: CalendarId,
        visible: bool,
        _actor: Actor,
    ) -> Result<(), CoreError> {
        self.db(move |store| store.set_calendar_visible(id, visible))
            .await?;
        self.emit_calendars().await
    }

    /// 重新读取日历集合并广播。界面据此刷新左侧日历列表。
    pub(crate) async fn emit_calendars(&self) -> Result<(), CoreError> {
        let calendars = self.db(|store| store.list_calendars()).await?;
        self.emit(Event::CalendarsChanged { calendars }).await;
        Ok(())
    }

    /// 启动时确保有默认日历，并把集合广播给界面。
    pub(crate) async fn calendar_bootstrap(&self) {
        if let Err(err) = self
            .db(|store| store.ensure_default_calendar().map(|_| ()))
            .await
        {
            tracing::warn!(error = %err, "初始化默认日历失败");
            return;
        }
        if let Err(err) = self.emit_calendars().await {
            tracing::warn!(error = %err, "读取日历列表失败");
        }
    }

    /// 提醒循环：每 30 秒扫一次，命中即发应用内 toast。
    ///
    /// 去重依赖 `calendar_reminder_fires` 的主键，所以重复扫描不会重复弹窗。
    pub(crate) fn start_reminder_loop(&self) {
        let service = self.clone();
        tokio::spawn(async move {
            loop {
                service.tick_reminders().await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
    }

    async fn tick_reminders(&self) {
        let now = now_unix();
        // 只认「刚刚到点」的提醒：允许 90 秒抖动，避免启动瞬间补发一堆旧提醒。
        let window_start = now - 90;
        let window_end = now;
        // 候选事件：从 1 小时前开始（跨过开始时刻的事件仍提醒）到 7 天后。
        let from = now - 3_600;
        let to = now + MAX_REMINDER_MINUTES as i64 * 60;

        let expanded = self
            .db(move |store| {
                let series = store.list_reminder_candidates(from, to, 200)?;
                let ids = series.iter().map(|item| item.id).collect::<Vec<_>>();
                let exceptions = store.list_exceptions(&ids)?;
                let mut due = Vec::new();
                for item in &series {
                    let own = exceptions
                        .iter()
                        .filter(|(id, _)| *id == item.id)
                        .map(|(_, exception)| exception.clone())
                        .collect::<Vec<_>>();
                    let result = me_core::expand_event(
                        item,
                        &own,
                        from,
                        to,
                        me_core::RecurrenceLimits::default(),
                    );
                    for reminder in me_core::due_reminders(
                        &result.occurrences,
                        &item.reminders,
                        window_start,
                        window_end,
                    ) {
                        if store.record_reminder_fire(
                            item.id,
                            reminder.occurrence_at,
                            reminder.minutes_before,
                        )? {
                            due.push((
                                item.id,
                                item.summary.clone(),
                                reminder.occurrence_at,
                                reminder.minutes_before,
                            ));
                        }
                    }
                }
                Ok(due)
            })
            .await;

        let Ok(due) = expanded else {
            return;
        };
        for (event, summary, start_utc, minutes_before) in due {
            self.emit(Event::CalendarReminder {
                event,
                summary,
                start_utc,
                minutes_before,
            })
            .await;
        }
    }

    /// 命令入口：只做转发，业务语义都在上面的用例里。
    pub(crate) async fn handle_calendar_command(&self, command: Command) {
        let result = match command {
            Command::CreateCalendarEvent { draft } => {
                self.create_event(draft, Actor::User).await.map(|_| ())
            }
            Command::UpdateCalendarEvent { id, draft } => {
                self.update_event(id, draft, Actor::User).await.map(|_| ())
            }
            Command::DeleteCalendarEvent { id } => self.delete_event(id, Actor::User).await,
            Command::CreateCalendar { name, color } => self
                .create_calendar(name, color, Actor::User)
                .await
                .map(|_| ()),
            Command::SetCalendarVisible { id, visible } => {
                self.set_calendar_visible(id, visible, Actor::User).await
            }
            _ => Ok(()),
        };
        if let Err(err) = result {
            self.emit(Event::Error {
                scope: me_core::ErrorScope::Global,
                message: err.user_message(),
            })
            .await;
        }
    }
}
