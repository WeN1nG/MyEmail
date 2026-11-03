//! MyEmail 存储层。
//!
//! 职责（见 `doc/v01架构设计.md` §3.5）：
//! - 单一写连接 + 只读连接池；
//! - SQLite 仓储（账户 / 文件夹 / 邮件 / 附件 / 草稿 / 待同步队列 / 搜索 / meta）；
//! - 文件缓存（正文、原始邮件、附件、草稿原文）。
//!
//! 约束：
//! - 本 crate 不依赖 tokio，所有仓储方法同步执行；
//! - 不向调用方暴露 `rusqlite` 类型；
//! - 事务内不做网络 IO（本 crate 也没有网络能力）。

mod accounts;
mod attachments;
pub mod cache;
mod calendars;
mod db;
mod drafts;
mod folders;
mod messages;
mod migrations;
mod pending;
mod search;
mod weather;

use std::fmt::Display;

use me_core::CoreError;

pub use cache::CachePaths;
pub use db::{ReadOnlyStore, Store, WritableStore};

/// 把任意底层错误折叠成 `CoreError::Storage`，避免 `rusqlite` / `io` 类型越过本层。
pub(crate) fn storage(err: impl Display) -> CoreError {
    CoreError::storage(err.to_string())
}

/// 便捷扩展：`conn.execute(...).store()?`。
pub(crate) trait StoreResult<T> {
    fn store(self) -> Result<T, CoreError>;
}

impl<T, E: Display> StoreResult<T> for Result<T, E> {
    fn store(self) -> Result<T, CoreError> {
        self.map_err(storage)
    }
}
