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
