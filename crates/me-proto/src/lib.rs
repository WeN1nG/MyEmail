//! MyEmail 协议适配层。
//!
//! 唯一允许 `use async_imap / lettre` 的 crate。对外只暴露 `me-core::port` 中的
//! 契约类型与纯编解码函数；任何 `async-imap` / `lettre` 类型都不出现在公共签名中。

pub mod imap;
pub mod mime;
pub mod oauth;
pub mod qweather;
pub mod smtp;
pub mod utf7;

mod bodystructure;

pub use imap::{ImapConnection, ProtoFactory, connect_imap};
pub use me_core::port::ImapCapabilities;
pub use mime::{
    DecodedAttachment, ParsedBody, ParsedDraft, ParsedHeader, build_rfc822, parse_body,
    parse_draft, parse_headers,
};
pub use oauth::{
    BuildTimeClient, DeviceCodeResponse, DeviceTokenOutcome, OAuthClient, Pkce, ProviderConfig,
    TokenResponse, UserInfo, build_time_client, random_urlsafe,
};
pub use qweather::QWeatherProvider;
pub use smtp::ProtoSmtp;
