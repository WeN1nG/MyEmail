//! 用固定 `.eml` 夹具验证 MIME 解析与 RFC822 构建。

use me_core::model::{AccountId, Address, OutgoingMessage};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[test]
fn plain_text_headers_and_body() {
    let raw = fixture("plain.eml");
    let header = me_proto::parse_headers(&raw[..raw.len().min(600)]).unwrap();
    assert_eq!(header.message_id.as_deref(), Some("plain-1@example.com"));
    assert_eq!(header.subject.as_deref(), Some("Plain hello"));
    assert_eq!(
        header.from.as_ref().map(|a| a.addr.as_str()),
        Some("alice@example.com")
    );
    assert_eq!(
        header.to.first().map(|a| a.addr.as_str()),
        Some("bob@example.com")
    );
    assert!(header.date.is_some());

    let body = me_proto::parse_body(&raw).unwrap();
    assert!(body.text.contains("Hello world."));
    assert!(body.attachments.is_empty());
    assert!(body.html.is_none(), "plain text message has no HTML part");
}

#[test]
fn multipart_attachment_is_detected() {
    let raw = fixture("multipart_attachment.eml");
    let header = me_proto::parse_headers(&raw[..raw.len().min(800)]).unwrap();
    // RFC 2047 解码。
    assert_eq!(header.subject.as_deref(), Some("中文主题"));
    assert_eq!(
        header.from.as_ref().and_then(|a| a.name.as_deref()),
        Some("张三")
    );
    // 显示名中的逗号不应把地址拆开。
    assert_eq!(
        header.to.first().and_then(|a| a.name.as_deref()),
        Some("Doe, John")
    );

    let body = me_proto::parse_body(&raw).unwrap();
    assert!(body.text.contains("正文正文"));
    assert_eq!(body.attachments.len(), 1);
    let attachment = &body.attachments[0];
    assert_eq!(attachment.filename.as_deref(), Some("report.pdf"));
    assert_eq!(attachment.mime_type.as_deref(), Some("application/pdf"));
    assert!(!attachment.is_inline);
}

#[test]
fn html_only_is_converted_to_text_and_preserves_html() {
    let body = me_proto::parse_body(&fixture("html_only.eml")).unwrap();
    assert!(body.text.contains("Hello"), "text was {:?}", body.text);
    assert!(
        body.text.contains("HTML body text."),
        "text was {:?}",
        body.text
    );
    let html = body
        .html
        .expect("html part must be preserved for rich rendering");
    assert!(html.contains("<h1>Hello</h1>"), "html was {html:?}");
}

#[test]
fn inline_cid_image_is_extracted_with_bytes() {
    let body = me_proto::parse_body(&fixture("multipart_related.eml")).unwrap();
    let html = body.html.expect("html part");
    assert!(html.contains("cid:logo@example.com"));
    assert_eq!(
        body.inline_images.len(),
        1,
        "images: {:?}",
        body.inline_images
            .iter()
            .map(|i| &i.content_id)
            .collect::<Vec<_>>()
    );
    let image = &body.inline_images[0];
    assert_eq!(image.content_id.as_deref(), Some("logo@example.com"));
    assert_eq!(image.mime_type.as_deref(), Some("image/png"));
    assert!(
        image.bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        "decoded PNG signature"
    );
}

#[test]
fn missing_date_is_none_not_error() {
    let header = me_proto::parse_headers(&fixture("missing_date.eml")).unwrap();
    assert!(header.date.is_none());
    assert_eq!(header.subject.as_deref(), Some("Missing Date"));
}

#[test]
fn malformed_message_never_panics() {
    let raw = fixture("malformed.eml");
    // 解析成功或返回 Parse 错误都可以，但绝不能 panic。
    let _ = me_proto::parse_headers(&raw);
    let _ = me_proto::parse_body(&raw);

    let random = [0u8, 159, 146, 150, 13, 10, 13, 10];
    let _ = me_proto::parse_body(&random);
}

#[test]
fn build_rfc822_has_no_bcc_and_encodes_attachments() {
    let dir = std::env::temp_dir().join(format!("myemail-mime-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let attachment_path = dir.join("note.txt");
    std::fs::write(&attachment_path, b"attachment body").unwrap();

    let message = OutgoingMessage {
        account: AccountId(1),
        draft_id: None,
        message_id: "fixed-id@example.com".into(),
        from: Address::new(Some("张三".into()), "alice@example.com"),
        to: vec![Address::new(Some("Doe, John".into()), "john@example.com")],
        cc: vec![Address::bare("carol@example.com")],
        bcc: vec![Address::bare("hidden@example.com")],
        subject: "中文主题".into(),
        body_text: "正文\n第二行".into(),
        attachments: vec![attachment_path],
    };

    let raw = me_proto::build_rfc822(&message).unwrap();
    let text = String::from_utf8_lossy(&raw);
    assert!(text.contains("Message-ID: <fixed-id@example.com>"));
    assert!(text.contains("multipart/mixed"));
    assert!(text.contains("Content-Disposition: attachment"));
    assert!(!text.contains("\r\nBcc:"), "Bcc must not be emitted");
    assert!(!text.contains("\nBcc:"));
    // 主题与文件名使用 RFC 2047 编码。
    assert!(text.contains("=?UTF-8?B?"));

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// parse_addresses：从完整邮件里提取收发件人（联系人累积用）
// ---------------------------------------------------------------------------

#[test]
fn parse_addresses_reads_from_to_and_cc() {
    let raw = fixture("plain.eml");
    let (from, to, cc) = me_proto::parse_addresses(&raw).unwrap();
    assert_eq!(
        from.as_ref().map(|a| a.addr.as_str()),
        Some("alice@example.com")
    );
    assert_eq!(from.as_ref().and_then(|a| a.name.as_deref()), Some("Alice"));
    assert_eq!(to.first().map(|a| a.addr.as_str()), Some("bob@example.com"));
    assert!(cc.is_empty());
}

#[test]
fn parse_addresses_on_malformed_mail_returns_empty_not_error() {
    // 联系人累积是**附带行为**——畸形邮件不该让解析失败。
    let raw = fixture("malformed.eml");
    let result = me_proto::parse_addresses(&raw);
    // 要么成功（拿到空或部分地址），要么是可识别的错误——但不能 panic。
    if let Ok((from, to, cc)) = result {
        assert!(from.is_none() || !from.unwrap().addr.is_empty());
        assert!(to.iter().all(|a| !a.addr.is_empty()));
        assert!(cc.iter().all(|a| !a.addr.is_empty()));
    }
}

#[test]
fn parse_addresses_handles_empty_input() {
    let (from, to, cc) = me_proto::parse_addresses(b"").unwrap();
    assert!(from.is_none());
    assert!(to.is_empty());
    assert!(cc.is_empty());
}

// --- 会话线程头（v0.4 C5）---

/// 拼一份只有头部的报文，便于精确构造回复头。
fn header_only(extra: &str) -> Vec<u8> {
    format!(
        "Subject: Re: 报价\r\n\
         From: bob@example.com\r\n\
         To: alice@example.com\r\n\
         Date: Mon, 01 Jan 2024 10:00:00 +0000\r\n\
         Message-ID: <c@example.com>\r\n\
         {extra}\r\n"
    )
    .into_bytes()
}

#[test]
fn in_reply_to_and_references_are_extracted() {
    let raw = header_only(
        "In-Reply-To: <b@example.com>\r\n\
         References: <a@example.com> <b@example.com>",
    );
    let header = me_proto::parse_headers(&raw).unwrap();
    assert_eq!(header.in_reply_to.as_deref(), Some("b@example.com"));
    assert_eq!(
        header.references.as_deref(),
        Some(["a@example.com".to_string(), "b@example.com".to_string()].as_slice()),
        "References 必须按空白分隔成多条"
    );
}

#[test]
fn missing_thread_headers_are_none() {
    let raw = header_only("");
    let header = me_proto::parse_headers(&raw).unwrap();
    assert!(header.in_reply_to.is_none());
    assert!(header.references.is_none());
}

#[test]
fn references_without_brackets_fall_back_to_whitespace_split() {
    // 真实邮件里存在裸地址（没有尖括号）的 References。
    let raw = header_only("References: a@example.com b@example.com");
    let header = me_proto::parse_headers(&raw).unwrap();
    assert_eq!(
        header.references.as_deref(),
        Some(["a@example.com".to_string(), "b@example.com".to_string()].as_slice())
    );
}

#[test]
fn references_are_deduped_and_capped() {
    // 病态邮件可以带几百个 id——去重 + 截断，且保留**根**（第一个）。
    let ids: Vec<String> = (0..40).map(|i| format!("<id{i}@example.com>")).collect();
    let raw = header_only(&format!("References: {} <id0@example.com>", ids.join(" ")));
    let header = me_proto::parse_headers(&raw).unwrap();
    let references = header.references.expect("references");
    assert_eq!(references.len(), me_proto::mime::MAX_REFERENCES);
    assert_eq!(references[0], "id0@example.com", "根必须保留");
    assert_eq!(
        references
            .iter()
            .filter(|id| *id == "id0@example.com")
            .count(),
        1,
        "重复的 id 只留一次"
    );
}

#[test]
fn unterminated_bracket_does_not_break_parsing() {
    // 头部被截断/损坏时不该 panic，也不该把半截当 id。
    let raw = header_only("References: <a@example.com");
    let header = me_proto::parse_headers(&raw).unwrap();
    assert!(header.references.is_none());
    assert_eq!(
        header.subject.as_deref(),
        Some("Re: 报价"),
        "其余头部照常解析"
    );
}
