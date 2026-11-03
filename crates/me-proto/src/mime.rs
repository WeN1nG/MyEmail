//! MIME 解析与 RFC822 构建。
//!
//! - 头部：`mail-parser` → `ParsedHeader`（Date 失败时留 `None`，由调用方回退 INTERNALDATE）。
//! - 正文：优先 `text/plain`，缺失时用 `html2text` 把 HTML 转成可读纯文本。
//! - 发信：手工构建 RFC822，CRLF 行尾，附件大小先校验，绝不输出 `Bcc`。

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use chrono::{FixedOffset, NaiveDate, TimeZone};
use mail_parser::{
    Address as MailAddress, ContentType, DateTime as MailDateTime, HeaderName, Message,
    MessageParser, MessagePart, PartType,
};
use me_core::model::{
    Address, Attachment, InlineImage, OutgoingMessage, check_attachment_limits,
    generate_message_id, normalize_message_id,
};
use me_core::{CoreError, LimitKind, MAX_PARSE_BYTES};

/// 解析后的头部字段。`date` 为 unix 秒，失败或缺失时为 `None`。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedHeader {
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub from: Option<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub date: Option<i64>,
}

/// 解析后的正文与附件元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedBody {
    /// 纯文本，或 HTML 降级结果。
    pub text: String,
    /// 原始 HTML 正文（用于阅读页富文本渲染），不参与全文索引。
    pub html: Option<String>,
    /// `cid:` / inline 图片（已解码字节），供阅读页直接渲染。
    pub inline_images: Vec<InlineImage>,
    pub attachments: Vec<Attachment>,
    pub raw_len: usize,
}

/// 已解码的附件内容，供服务器草稿导入到 `drafts/<id>/attachments/`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedAttachment {
    pub attachment: Attachment,
    pub data: Vec<u8>,
}

/// 草稿导入所需的一次解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDraft {
    pub header: ParsedHeader,
    pub body: ParsedBody,
    pub attachments: Vec<DecodedAttachment>,
}

fn parser() -> MessageParser {
    MessageParser::default()
        .with_mime_headers()
        .with_date_headers()
        .with_address_headers()
        .header_text(HeaderName::Subject)
        .header_id(HeaderName::MessageId)
}

/// 解析仅头部字节（`BODY.PEEK[HEADER]`）。
pub fn parse_headers(header: &[u8]) -> Result<ParsedHeader, CoreError> {
    let parsed = std::panic::catch_unwind(AssertUnwindSafe(|| parser().parse_headers(header)))
        .map_err(|_| CoreError::parse("邮件头部解析崩溃"))?
        .ok_or_else(|| CoreError::parse("无法解析邮件头部"))?;

    let message_id = parsed.message_id().and_then(normalize_message_id);

    let subject = parsed
        .subject()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    let from = parsed.from().and_then(first_address);

    let mut to = Vec::new();
    for address in parsed.all_to() {
        push_addresses(&mut to, address);
    }
    let mut cc = Vec::new();
    for address in parsed.all_cc() {
        push_addresses(&mut cc, address);
    }

    Ok(ParsedHeader {
        message_id,
        subject,
        from,
        to,
        cc,
        date: parsed.date().and_then(mail_date_to_unix),
    })
}

fn first_address(address: &MailAddress<'_>) -> Option<Address> {
    let addr = address.iter().next()?;
    core_address(addr)
}

fn push_addresses(target: &mut Vec<Address>, address: &MailAddress<'_>) {
    for addr in address.iter() {
        if let Some(converted) = core_address(addr) {
            target.push(converted);
        }
    }
}

fn core_address(addr: &mail_parser::Addr<'_>) -> Option<Address> {
    let email = addr.address()?.trim();
    if email.is_empty() {
        return None;
    }
    Some(Address::new(
        addr.name()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string),
        email,
    ))
}

fn mail_date_to_unix(date: &MailDateTime) -> Option<i64> {
    let year = i32::from(date.year);
    let month = u32::from(date.month);
    let day = u32::from(date.day);
    let naive = NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(
        u32::from(date.hour),
        u32::from(date.minute),
        u32::from(date.second),
    )?;
    let sign = if date.tz_before_gmt { -1 } else { 1 };
    let offset_seconds = sign * (i32::from(date.tz_hour) * 3600 + i32::from(date.tz_minute) * 60);
    let offset = FixedOffset::east_opt(offset_seconds)?;
    offset
        .from_local_datetime(&naive)
        .single()
        .map(|value| value.timestamp())
}

/// 解析整封邮件。超过 `MAX_PARSE_BYTES` 直接返回 `Limit(ParseSize)`；
/// 解析过程中的 panic 会转换为 `CoreError::Parse`。
pub fn parse_body(raw: &[u8]) -> Result<ParsedBody, CoreError> {
    if raw.len() > MAX_PARSE_BYTES {
        return Err(CoreError::limit(LimitKind::ParseSize));
    }
    // 整段解析 + 提取都置于 catch_unwind 下：畸形邮件只能变成 Parse 错误。
    std::panic::catch_unwind(AssertUnwindSafe(|| parse_body_inner(raw)))
        .map_err(|_| CoreError::parse("邮件正文解析崩溃"))?
}

/// 解析服务器草稿：一次解析得到头部、正文与**已解码附件字节**。
pub fn parse_draft(raw: &[u8]) -> Result<ParsedDraft, CoreError> {
    if raw.len() > MAX_PARSE_BYTES {
        return Err(CoreError::limit(LimitKind::ParseSize));
    }
    std::panic::catch_unwind(AssertUnwindSafe(|| parse_draft_inner(raw)))
        .map_err(|_| CoreError::parse("草稿解析崩溃"))?
}

fn parse_draft_inner(raw: &[u8]) -> Result<ParsedDraft, CoreError> {
    let parsed = parser()
        .parse(raw)
        .ok_or_else(|| CoreError::parse("无法解析草稿"))?;

    let message_id = parsed.message_id().and_then(normalize_message_id);
    let subject = parsed
        .subject()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let from = parsed.from().and_then(first_address);
    let mut to = Vec::new();
    for address in parsed.all_to() {
        push_addresses(&mut to, address);
    }
    let mut cc = Vec::new();
    for address in parsed.all_cc() {
        push_addresses(&mut cc, address);
    }

    let body = ParsedBody {
        text: extract_text(&parsed)?,
        html: extract_html(&parsed),
        inline_images: extract_inline_images(&parsed),
        attachments: extract_attachments(&parsed),
        raw_len: raw.len(),
    };
    Ok(ParsedDraft {
        header: ParsedHeader {
            message_id,
            subject,
            from,
            to,
            cc,
            date: parsed.date().and_then(mail_date_to_unix),
        },
        body,
        attachments: extract_decoded_attachments(&parsed),
    })
}

fn parse_body_inner(raw: &[u8]) -> Result<ParsedBody, CoreError> {
    let parsed = parser()
        .parse(raw)
        .ok_or_else(|| CoreError::parse("无法解析邮件正文"))?;

    let text = extract_text(&parsed)?;
    let html = extract_html(&parsed);
    let inline_images = extract_inline_images(&parsed);
    let attachments = extract_attachments(&parsed);

    Ok(ParsedBody {
        text,
        html,
        inline_images,
        attachments,
        raw_len: raw.len(),
    })
}

/// 提取可供阅读页渲染的 HTML。超过上限时不返回，避免事件通道被巨型正文拖垮。
fn extract_html(message: &Message<'_>) -> Option<String> {
    const MAX_HTML_BYTES: usize = 2 * 1024 * 1024;
    for part in message.html_bodies() {
        if let PartType::Html(html) = &part.body
            && !html.trim().is_empty()
            && html.len() <= MAX_HTML_BYTES
        {
            return Some(html.to_string());
        }
    }
    None
}

fn extract_text(message: &Message<'_>) -> Result<String, CoreError> {
    for part in message.text_bodies() {
        if let PartType::Text(value) = &part.body {
            return Ok(value.to_string());
        }
    }

    for part in message.html_bodies() {
        if let PartType::Html(html) = &part.body {
            return html2text::from_read(html.as_bytes(), 80)
                .map_err(|err| CoreError::parse(format!("HTML 转纯文本失败: {err}")));
        }
    }

    Ok(String::new())
}

/// 内嵌图片：限制单张与总量，避免正文事件携带巨量二进制。
fn extract_inline_images(message: &Message<'_>) -> Vec<InlineImage> {
    const MAX_ONE: usize = 4 * 1024 * 1024;
    const MAX_TOTAL: usize = 8 * 1024 * 1024;

    let mut out: Vec<InlineImage> = Vec::new();
    let mut total = 0usize;
    for part in &message.parts {
        let Some(content_type) = part_content_type(part) else {
            continue;
        };
        if !content_type.ctype().eq_ignore_ascii_case("image") {
            continue;
        }
        let Some(subtype) = content_type.subtype() else {
            continue;
        };
        let bytes = match &part.body {
            PartType::Binary(bytes) | PartType::InlineBinary(bytes) => bytes.to_vec(),
            _ => continue,
        };
        if bytes.is_empty() || bytes.len() > MAX_ONE {
            continue;
        }
        let disposition = part_content_disposition(part);
        let content_id = part_content_id(part).map(|value| {
            value
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        });
        let is_inline = content_id.is_some() || disposition.is_some_and(|value| value.is_inline());
        if !is_inline {
            continue;
        }
        if out
            .iter()
            .any(|existing| existing.content_id.is_some() && existing.content_id == content_id)
        {
            continue;
        }
        if total.saturating_add(bytes.len()) > MAX_TOTAL {
            break;
        }
        total += bytes.len();
        out.push(InlineImage {
            content_id,
            filename: part_filename(Some(content_type), disposition),
            mime_type: Some(format!("image/{}", subtype.to_ascii_lowercase())),
            bytes,
        });
    }
    out
}

fn attachment_from_part(
    message: &Message<'_>,
    pattern: &HashMap<u32, u32>,
    part_id: &u32,
) -> Option<Attachment> {
    let part = message.parts.get(*part_id as usize)?;
    if matches!(&part.body, PartType::Multipart(_)) {
        return None;
    }

    let content_type = part_content_type(part);
    let disposition = part_content_disposition(part);
    let content_id = part_content_id(part);
    let filename = part_filename(content_type, disposition);

    let is_inline = disposition.is_some_and(|value| value.is_inline())
        || matches!(&part.body, PartType::InlineBinary(_))
        || (content_id.is_some() && !disposition.is_some_and(|value| value.is_attachment()));

    let mime_type = content_type.map(|content_type| match content_type.subtype() {
        Some(subtype) => format!("{}/{}", content_type.ctype(), subtype),
        None => content_type.ctype().to_string(),
    });

    Some(Attachment {
        part_index: pattern.get(part_id).copied().unwrap_or(*part_id + 1),
        filename,
        mime_type,
        size: part_size(part),
        is_inline,
        content_id,
        local_path: None,
    })
}

fn extract_attachments(message: &Message<'_>) -> Vec<Attachment> {
    let index_map = top_level_index_map(message);
    let mut attachments = Vec::new();
    for part_id in &message.attachments {
        if let Some(attachment) = attachment_from_part(message, &index_map, part_id) {
            attachments.push(attachment);
        }
    }
    attachments
}

fn extract_decoded_attachments(message: &Message<'_>) -> Vec<DecodedAttachment> {
    let index_map = top_level_index_map(message);
    let mut attachments = Vec::new();
    for part_id in &message.attachments {
        let Some(attachment) = attachment_from_part(message, &index_map, part_id) else {
            continue;
        };
        let Some(part) = message.parts.get(*part_id as usize) else {
            continue;
        };
        attachments.push(DecodedAttachment {
            attachment,
            data: part.contents().to_vec(),
        });
    }
    attachments
}

fn part_content_type<'a>(part: &'a MessagePart<'a>) -> Option<&'a ContentType<'a>> {
    part.headers
        .iter()
        .find(|header| header.name == HeaderName::ContentType)
        .and_then(|header| header.value.as_content_type())
}

fn part_content_disposition<'a>(part: &'a MessagePart<'a>) -> Option<&'a ContentType<'a>> {
    part.headers
        .iter()
        .find(|header| header.name == HeaderName::ContentDisposition)
        .and_then(|header| header.value.as_content_type())
}

fn part_content_id(part: &MessagePart<'_>) -> Option<String> {
    part.headers
        .iter()
        .find(|header| header.name == HeaderName::ContentId)
        .and_then(|header| header.value.as_text())
        .map(|value| {
            value
                .trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .trim()
                .to_string()
        })
        .filter(|value| !value.is_empty())
}

fn part_filename(
    content_type: Option<&ContentType<'_>>,
    disposition: Option<&ContentType<'_>>,
) -> Option<String> {
    disposition
        .and_then(|value| value.attribute("filename"))
        .or_else(|| content_type.and_then(|value| value.attribute("name")))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn part_size(part: &MessagePart<'_>) -> Option<u32> {
    let len = match &part.body {
        PartType::Text(value) | PartType::Html(value) => value.len(),
        PartType::Binary(value) | PartType::InlineBinary(value) => value.len(),
        PartType::Message(message) => message.raw_message.len(),
        PartType::Multipart(_) => return None,
    };
    u32::try_from(len).ok()
}

fn top_level_index_map(message: &Message<'_>) -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    let Some(root) = message.parts.first() else {
        return map;
    };
    match &root.body {
        PartType::Multipart(children) => {
            for (position, child) in children.iter().enumerate() {
                mark_index(message, *child, position as u32 + 1, 0, &mut map);
            }
        }
        _ => {
            map.insert(0, 1);
        }
    }
    map
}

fn mark_index(
    message: &Message<'_>,
    part_id: u32,
    top_level: u32,
    depth: usize,
    map: &mut HashMap<u32, u32>,
) {
    if depth > 64 || map.insert(part_id, top_level).is_some() {
        return;
    }
    if let Some(part) = message.parts.get(part_id as usize)
        && let PartType::Multipart(children) = &part.body
    {
        for child in children {
            mark_index(message, *child, top_level, depth + 1, map);
        }
    }
}

// ---------------------------------------------------------------------------
// RFC822 构建
// ---------------------------------------------------------------------------

struct PreparedAttachment {
    filename: String,
    mime_type: &'static str,
    data: Vec<u8>,
}

/// 构建待投递的 RFC822 字节。CRLF 行尾；`Bcc` 只进入信封，不写入头部。
pub fn build_rfc822(message: &OutgoingMessage) -> Result<Vec<u8>, CoreError> {
    let attachments = prepare_attachments(&message.attachments)?;

    let mut out = Vec::new();
    push_header(&mut out, "From", &format_address(&message.from));
    if !message.to.is_empty() {
        push_header(&mut out, "To", &format_address_list(&message.to));
    }
    if !message.cc.is_empty() {
        push_header(&mut out, "Cc", &format_address_list(&message.cc));
    }
    push_header(&mut out, "Subject", &encode_unstructured(&message.subject));

    let message_id = normalize_message_id(&message.message_id)
        .unwrap_or_else(|| generate_message_id(&message.from.addr));
    push_header(&mut out, "Message-ID", &format!("<{message_id}>"));
    push_header(&mut out, "Date", &chrono::Utc::now().to_rfc2822());
    push_header(&mut out, "MIME-Version", "1.0");

    if attachments.is_empty() {
        push_header(&mut out, "Content-Type", "text/plain; charset=utf-8");
        push_header(&mut out, "Content-Transfer-Encoding", "base64");
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(base64_body(message.body_text.as_bytes()).as_bytes());
    } else {
        let boundary = make_boundary();
        push_header(
            &mut out,
            "Content-Type",
            &format!("multipart/mixed; boundary=\"{boundary}\""),
        );
        out.extend_from_slice(b"\r\n");

        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        push_header(&mut out, "Content-Type", "text/plain; charset=utf-8");
        push_header(&mut out, "Content-Transfer-Encoding", "base64");
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(base64_body(message.body_text.as_bytes()).as_bytes());

        for attachment in &attachments {
            out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            push_header(
                &mut out,
                "Content-Type",
                &format!(
                    "{}; name=\"{}\"",
                    attachment.mime_type,
                    encode_parameter(&attachment.filename)
                ),
            );
            push_header(&mut out, "Content-Transfer-Encoding", "base64");
            push_header(
                &mut out,
                "Content-Disposition",
                &format!(
                    "attachment; filename=\"{}\"",
                    encode_parameter(&attachment.filename)
                ),
            );
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(base64_body(&attachment.data).as_bytes());
        }
        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    }

    Ok(out)
}

fn prepare_attachments(paths: &[PathBuf]) -> Result<Vec<PreparedAttachment>, CoreError> {
    let mut declared_sizes = Vec::with_capacity(paths.len());
    for path in paths {
        declared_sizes.push(attachment_size(path)?);
    }
    check_attachment_limits(&declared_sizes)?;

    let mut prepared = Vec::with_capacity(paths.len());
    let mut actual_sizes = Vec::with_capacity(paths.len());
    for path in paths {
        let data = std::fs::read(path)
            .map_err(|err| CoreError::storage(format!("读取附件失败: {err}")))?;
        actual_sizes.push(data.len() as u64);
        let filename = path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "attachment".to_string());
        prepared.push(PreparedAttachment {
            mime_type: guess_mime_type(&filename),
            filename,
            data,
        });
    }
    // 文件可能在读取期间变大，按实际字节再校验一次。
    check_attachment_limits(&actual_sizes)?;
    Ok(prepared)
}

fn attachment_size(path: &Path) -> Result<u64, CoreError> {
    let metadata = std::fs::metadata(path)
        .map_err(|err| CoreError::storage(format!("读取附件元数据失败: {err}")))?;
    if !metadata.is_file() {
        return Err(CoreError::storage("附件路径不是普通文件"));
    }
    Ok(metadata.len())
}

fn push_header(out: &mut Vec<u8>, name: &str, value: &str) {
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b": ");
    out.extend_from_slice(sanitize_header_value(value).as_bytes());
    out.extend_from_slice(b"\r\n");
}

fn sanitize_header_value(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch == '\r' || ch == '\n' { ' ' } else { ch })
        .collect()
}

fn format_address_list(addresses: &[Address]) -> String {
    addresses
        .iter()
        .map(format_address)
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_address(address: &Address) -> String {
    let email = sanitize_header_value(address.addr.trim());
    match address
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        Some(name) => {
            let name = sanitize_header_value(name);
            let phrase = if name.is_ascii() {
                quote_phrase(&name)
            } else {
                encode_rfc2047(&name)
            };
            format!("{phrase} <{email}>")
        }
        None => email,
    }
}

fn quote_phrase(value: &str) -> String {
    let simple = !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    ' ' | '!'
                        | '#'
                        | '$'
                        | '%'
                        | '&'
                        | '\''
                        | '*'
                        | '+'
                        | '-'
                        | '/'
                        | '='
                        | '?'
                        | '^'
                        | '_'
                        | '`'
                        | '{'
                        | '|'
                        | '}'
                        | '~'
                )
        });
    if simple {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn encode_unstructured(value: &str) -> String {
    let value = sanitize_header_value(value);
    if value.is_ascii() {
        value
    } else {
        encode_rfc2047(&value)
    }
}

fn base64_encode(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn encode_rfc2047(value: &str) -> String {
    format!("=?UTF-8?B?{}?=", base64_encode(value.as_bytes()))
}

fn encode_parameter(value: &str) -> String {
    if value.is_ascii() {
        value.replace('\\', "\\\\").replace('"', "\\\"")
    } else {
        encode_rfc2047(value)
    }
}

fn base64_body(data: &[u8]) -> String {
    let encoded = base64_encode(data);
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 76 * 2 + 2);
    if encoded.is_empty() {
        out.push_str("\r\n");
        return out;
    }
    for chunk in encoded.as_bytes().chunks(76) {
        // base64 字母表全为 ASCII，from_utf8 不会失败。
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push_str("\r\n");
    }
    out
}

fn make_boundary() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = chrono::Utc::now().timestamp_micros();
    format!("----=_MyEmail_{nanos}_{sequence}")
}

fn guess_mime_type(filename: &str) -> &'static str {
    let extension = Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "txt" | "text" => "text/plain",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        "ics" => "text/calendar",
        "json" => "application/json",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "eml" => "message/rfc822",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "svg" => "image/svg+xml",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use me_core::model::AccountId;

    const PLAIN_TEXT: &[u8] = b"From: Alice <alice@example.com>\r\n\
To: Bob <bob@example.com>\r\n\
Subject: Plain hello\r\n\
Date: Tue, 14 Jan 2025 10:20:30 +0800\r\n\
Message-ID: <plain@example.com>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Transfer-Encoding: 7bit\r\n\
\r\n\
Hello from the plain text fixture.\r\n";

    const WITH_ATTACHMENT: &[u8] = b"From: Alice <alice@example.com>\r\n\
To: Bob <bob@example.com>\r\n\
Subject: With attachment\r\n\
Date: Tue, 14 Jan 2025 10:20:30 +0800\r\n\
Message-ID: <attach@example.com>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\
\r\n\
--b1\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Transfer-Encoding: 7bit\r\n\
\r\n\
see attached\r\n\
--b1\r\n\
Content-Type: text/plain; name=\"note.txt\"\r\n\
Content-Transfer-Encoding: 7bit\r\n\
Content-Disposition: attachment; filename=\"note.txt\"\r\n\
\r\n\
attached text\r\n\
--b1--\r\n";

    const HTML_ONLY: &[u8] = b"From: Alice <alice@example.com>\r\n\
To: Bob <bob@example.com>\r\n\
Subject: HTML only\r\n\
Date: Tue, 14 Jan 2025 10:20:30 +0800\r\n\
Message-ID: <html@example.com>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Content-Transfer-Encoding: 7bit\r\n\
\r\n\
<html><body><h1>HTML only</h1><p>converted paragraph</p></body></html>\r\n";

    const CHINESE_SUBJECT: &[u8] = b"From: Alice <alice@example.com>\r\n\
To: Bob <bob@example.com>\r\n\
Subject: =?UTF-8?B?5Lit5paH5Li76aKY?=\r\n\
Date: Tue, 14 Jan 2025 10:20:30 +0800\r\n\
Message-ID: <cn@example.com>\r\n\
\r\n";

    const MISSING_DATE: &[u8] = b"From: Alice <alice@example.com>\r\n\
To: Bob <bob@example.com>\r\n\
Subject: No date\r\n\
Message-ID: <nodate@example.com>\r\n\
\r\n";

    const EMPTY_MESSAGE_ID: &[u8] = b"From: Alice <alice@example.com>\r\n\
To: Bob <bob@example.com>\r\n\
Subject: Empty id\r\n\
Date: Tue, 14 Jan 2025 10:20:30 +0800\r\n\
Message-ID: <>\r\n\
\r\n";

    fn header_block(raw: &[u8]) -> &[u8] {
        match raw.windows(4).position(|window| window == b"\r\n\r\n") {
            Some(position) => &raw[..position],
            None => raw,
        }
    }

    #[test]
    fn plain_text_headers_and_body() {
        let header = parse_headers(header_block(PLAIN_TEXT)).unwrap();
        assert_eq!(header.subject.as_deref(), Some("Plain hello"));
        assert_eq!(
            header.from.as_ref().map(|a| a.addr.as_str()),
            Some("alice@example.com")
        );
        assert_eq!(header.to.len(), 1);
        assert!(header.date.is_some_and(|date| date > 1_700_000_000));

        let body = parse_body(PLAIN_TEXT).unwrap();
        assert!(body.text.contains("Hello from the plain text fixture."));
        assert!(body.attachments.is_empty());
    }

    #[test]
    fn attachment_fixture_reports_part_index() {
        let body = parse_body(WITH_ATTACHMENT).unwrap();
        assert_eq!(body.attachments.len(), 1);
        let attachment = &body.attachments[0];
        assert_eq!(attachment.part_index, 2);
        assert_eq!(attachment.filename.as_deref(), Some("note.txt"));
        assert_eq!(attachment.mime_type.as_deref(), Some("text/plain"));
        assert!(!attachment.is_inline);
    }

    #[test]
    fn parse_draft_returns_decoded_attachment_bytes() {
        let draft = parse_draft(WITH_ATTACHMENT).unwrap();
        assert_eq!(
            draft.header.message_id.as_deref(),
            Some("attach@example.com")
        );
        assert_eq!(draft.header.subject.as_deref(), Some("With attachment"));
        assert_eq!(draft.attachments.len(), 1);
        assert_eq!(
            draft.attachments[0].attachment.filename.as_deref(),
            Some("note.txt")
        );
        let data = String::from_utf8_lossy(&draft.attachments[0].data);
        assert!(data.contains("attached text"));
    }

    #[test]
    fn html_only_uses_html2text() {
        let body = parse_body(HTML_ONLY).unwrap();
        assert!(body.text.contains("HTML only"));
        assert!(body.text.contains("converted paragraph"));
        assert!(body.attachments.is_empty());
    }

    #[test]
    fn chinese_subject_is_decoded() {
        let header = parse_headers(header_block(CHINESE_SUBJECT)).unwrap();
        assert_eq!(header.subject.as_deref(), Some("中文主题"));
    }

    #[test]
    fn gbk_headers_and_body_are_decoded() {
        // 163.com 实际样本：RFC 2047 GBK 编码的 From/Subject + charset=gbk 正文。
        let raw = b"From: =?GBK?B?z8rA9ruq?= <xianlihua1021@163.com>\r\n\
Subject: =?GBK?B?tcfUqtS+x6ijqLPJtrzK0KOpyv3X1r/GvLzT0M/euavLvi1GREW42s67suLK1Mzi?=\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/plain; charset=gbk\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
xPq6w6Oh\r\n";
        let header = parse_headers(raw).expect("headers");
        assert_eq!(
            header.subject.as_deref(),
            Some("登元跃迁（成都市）数字科技有限公司-FDE岗位测试题")
        );
        let from = header.from.expect("from");
        assert_eq!(from.name.as_deref(), Some("鲜丽华"));
        assert_eq!(from.addr, "xianlihua1021@163.com");
        let body = parse_body(raw).expect("body");
        assert_eq!(body.text, "您好！");
    }

    #[test]
    fn gbk_html_body_is_decoded() {
        let raw = b"MIME-Version: 1.0\r\n\
Content-Type: text/html; charset=gbk\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
PHA+1tDOxDwvcD4=\r\n";
        let body = parse_body(raw).expect("body");
        assert_eq!(body.html.as_deref(), Some("<p>中文</p>"));
        assert_eq!(body.text.trim(), "中文");
    }

    #[test]
    fn missing_date_stays_none() {
        let header = parse_headers(header_block(MISSING_DATE)).unwrap();
        assert!(header.date.is_none());
    }

    #[test]
    fn empty_message_id_is_none() {
        let header = parse_headers(header_block(EMPTY_MESSAGE_ID)).unwrap();
        assert!(header.message_id.is_none());
    }

    #[test]
    fn malformed_bytes_do_not_panic() {
        let _ = parse_body(b"\x00\xff\xfe garbage without headers\r\n\r\n\x01\x02");
        let _ = parse_headers(b"\x00\xff");
        let _ = parse_body(b"");
        let _ = parse_headers(b"");
    }

    #[test]
    fn oversized_body_is_rejected() {
        let raw = vec![b'x'; MAX_PARSE_BYTES + 1];
        assert!(matches!(
            parse_body(&raw),
            Err(CoreError::Limit {
                what: LimitKind::ParseSize
            })
        ));
    }

    #[test]
    fn build_without_attachment() {
        let message = outgoing(vec![]);
        let text = String::from_utf8(build_rfc822(&message).unwrap()).unwrap();
        assert!(text.starts_with("From: Alice <alice@example.com>\r\n"));
        assert!(text.contains("\r\nTo: bob@example.com\r\n"));
        assert!(text.contains("\r\nSubject: Hello\r\n"));
        assert!(text.contains("\r\nMessage-ID: <fixed@example.com>\r\n"));
        assert!(text.contains("\r\nMIME-Version: 1.0\r\n"));
        assert!(text.contains("Content-Type: text/plain; charset=utf-8\r\n"));
        assert!(!text.contains("\r\nBcc:"));
        assert_eq!(decode_first_body(&text), "body text");
    }

    #[test]
    fn build_encodes_non_ascii_subject_as_rfc2047() {
        let mut message = outgoing(vec![]);
        message.subject = "中文主题".to_string();
        let text = String::from_utf8(build_rfc822(&message).unwrap()).unwrap();
        assert!(text.contains("Subject: =?UTF-8?B?5Lit5paH5Li76aKY?=\r\n"));
        assert!(!text.contains("Subject: 中文主题"));
    }

    #[test]
    fn build_with_attachment_uses_multipart_and_no_bcc() {
        let path = std::env::temp_dir().join(format!(
            "me-proto-test-{}-{}.txt",
            std::process::id(),
            chrono::Utc::now().timestamp_micros()
        ));
        std::fs::write(&path, b"attachment body").unwrap();

        let message = outgoing(vec![path.clone()]);
        let text = String::from_utf8(build_rfc822(&message).unwrap()).unwrap();
        assert!(text.contains("Content-Type: multipart/mixed; boundary="));
        assert!(text.contains("Content-Disposition: attachment; filename=\""));
        assert!(text.contains("Content-Type: text/plain; name=\""));
        assert!(!text.contains("\r\nBcc:"));
        assert!(
            text.contains(&base64::engine::general_purpose::STANDARD.encode(b"attachment body"))
        );
        let _ = std::fs::remove_file(path);
    }

    fn outgoing(attachments: Vec<PathBuf>) -> OutgoingMessage {
        OutgoingMessage {
            account: AccountId(1),
            draft_id: None,
            message_id: "fixed@example.com".to_string(),
            from: Address::new(Some("Alice".to_string()), "alice@example.com"),
            to: vec![Address::bare("bob@example.com")],
            cc: vec![],
            bcc: vec![Address::bare("hidden@example.com")],
            subject: "Hello".to_string(),
            body_text: "body text".to_string(),
            attachments,
        }
    }

    fn decode_first_body(raw: &str) -> String {
        let body = raw.split("\r\n\r\n").nth(1).unwrap();
        let encoded: String = body.chars().filter(|ch| !ch.is_whitespace()).collect();
        String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
        )
        .unwrap()
    }
}
