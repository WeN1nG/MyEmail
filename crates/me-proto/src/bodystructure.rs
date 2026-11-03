//! BODYSTRUCTURE 解析与附件判定。
//!
//! 服务器在 `FETCH ... BODYSTRUCTURE` 中返回带括号的 S-表达式。这里提供一个
//! 独立的递归 tokenizer/parser，用于在缺少类型化接口或测试场景下判断附件；
//! 运行时优先使用 `imap_proto` 已解析好的 `BodyStructure`，但两者共享同一套
//! 判定规则（`is_leaf_attachment`）。
//!
//! 解析器对畸形输入只返回 `None`，不 panic。

use async_imap::imap_proto::{BodyContentCommon, BodyStructure};

/// 纯文本 BODYSTRUCTURE 的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
enum BsValue {
    List(Vec<BsValue>),
    Str(String),
    Nil,
}

const MAX_DEPTH: usize = 64;

fn parse_value(input: &[u8], pos: &mut usize, depth: usize) -> Option<BsValue> {
    if depth > MAX_DEPTH {
        return None;
    }
    skip_ws(input, pos);
    match input.get(*pos)? {
        b'(' => {
            *pos += 1;
            let mut items = Vec::new();
            loop {
                skip_ws(input, pos);
                match input.get(*pos)? {
                    b')' => {
                        *pos += 1;
                        return Some(BsValue::List(items));
                    }
                    _ => items.push(parse_value(input, pos, depth + 1)?),
                }
            }
        }
        b')' => None,
        b'"' => {
            *pos += 1;
            let mut out = String::new();
            loop {
                let byte = *input.get(*pos)?;
                *pos += 1;
                match byte {
                    b'"' => return Some(BsValue::Str(out)),
                    b'\\' => {
                        let escaped = *input.get(*pos)?;
                        *pos += 1;
                        out.push(escaped as char);
                    }
                    other => out.push(other as char),
                }
            }
        }
        _ => {
            let start = *pos;
            while let Some(byte) = input.get(*pos) {
                if matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b'(' | b')') {
                    break;
                }
                *pos += 1;
            }
            if start == *pos {
                return None;
            }
            let atom = String::from_utf8_lossy(&input[start..*pos]).into_owned();
            if atom.eq_ignore_ascii_case("NIL") {
                Some(BsValue::Nil)
            } else {
                Some(BsValue::Str(atom))
            }
        }
    }
}

fn skip_ws(input: &[u8], pos: &mut usize) {
    while matches!(input.get(*pos), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        *pos += 1;
    }
}

/// 解析 BODYSTRUCTURE 文本。返回 `None` 表示输入畸形。
#[allow(dead_code)]
fn parse_text(text: &str) -> Option<BsValue> {
    let full = text.as_bytes();
    // 允许调用方传入带或不带前导 `(` 的文本。
    let start = full.iter().position(|b| *b == b'(')?;
    let mut pos = start;
    parse_value(full, &mut pos, 0)
}

fn as_str(value: &BsValue) -> Option<&str> {
    match value {
        BsValue::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

fn parse_params(value: Option<&BsValue>) -> Vec<(String, String)> {
    let Some(BsValue::List(items)) = value else {
        return Vec::new();
    };
    items
        .as_chunks::<2>()
        .0
        .iter()
        .filter_map(|pair| match (&pair[0], &pair[1]) {
            (BsValue::Str(name), BsValue::Str(value)) => {
                Some((name.to_ascii_lowercase(), value.clone()))
            }
            _ => None,
        })
        .collect()
}

fn find_disposition(items: &[BsValue]) -> Option<(String, Vec<(String, String)>)> {
    for item in items.iter().skip(7) {
        let BsValue::List(disp) = item else {
            continue;
        };
        let Some(kind) = disp.first().and_then(as_str) else {
            continue;
        };
        // 只把 attachment/inline 当作 disposition；避免把嵌套 message 的
        // body-structure 误认为 disposition。
        if !kind.eq_ignore_ascii_case("attachment") && !kind.eq_ignore_ascii_case("inline") {
            continue;
        }
        let params = parse_params(disp.get(1));
        return Some((kind.to_ascii_lowercase(), params));
    }
    None
}

#[allow(dead_code)]
fn is_leaf_attachment(
    content_type: &str,
    params: &[(String, String)],
    disposition: Option<&(String, Vec<(String, String)>)>,
) -> bool {
    let content_type = content_type.to_ascii_lowercase();
    if disposition.is_some_and(|(kind, _)| kind.eq_ignore_ascii_case("attachment")) {
        return true;
    }
    let named = params
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("name"))
        || disposition.is_some_and(|(_, disp_params)| {
            disp_params
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("filename"))
        });
    named && content_type != "text/plain" && content_type != "text/html"
}

#[allow(dead_code)]
fn value_has_attachment(value: &BsValue) -> bool {
    let BsValue::List(items) = value else {
        return false;
    };
    let Some(first) = items.first() else {
        return false;
    };
    if matches!(first, BsValue::List(_)) {
        // multipart：前导的若干 List 是子部件。
        return items
            .iter()
            .take_while(|item| matches!(item, BsValue::List(_)))
            .any(value_has_attachment);
    }

    let Some(ty) = as_str(first) else {
        return false;
    };
    let subtype = items.get(1).and_then(as_str).unwrap_or("");
    let content_type = format!("{ty}/{subtype}");
    let params = parse_params(items.get(2));
    let disposition = find_disposition(items);
    is_leaf_attachment(&content_type, &params, disposition.as_ref())
}

/// 从原始 BODYSTRUCTURE 文本判断是否含附件。畸形输入返回 `false`。
#[allow(dead_code)]
pub(crate) fn has_attachment_in_text(text: &str) -> bool {
    parse_text(text).is_some_and(|value| value_has_attachment(&value))
}

fn typed_leaf_is_attachment(common: &BodyContentCommon<'_>) -> bool {
    let content_type = format!(
        "{}/{}",
        common.ty.ty.to_ascii_lowercase(),
        common.ty.subtype.to_ascii_lowercase()
    );
    let params: Vec<(String, String)> = common
        .ty
        .params
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.to_string()))
        .collect();
    let disposition = common.disposition.as_ref().map(|disp| {
        let params = disp
            .params
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value.to_string()))
            .collect();
        (disp.ty.to_ascii_lowercase(), params)
    });
    is_leaf_attachment(&content_type, &params, disposition.as_ref())
}

/// 判断类型化 BODYSTRUCTURE 是否含附件。
pub(crate) fn has_attachment(structure: &BodyStructure<'_>) -> bool {
    match structure {
        BodyStructure::Multipart { bodies, .. } => bodies.iter().any(has_attachment),
        BodyStructure::Basic { common, .. }
        | BodyStructure::Text { common, .. }
        | BodyStructure::Message { common, .. } => typed_leaf_is_attachment(common),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_not_attachment() {
        assert!(!has_attachment_in_text(
            "(\"text\" \"plain\" (\"charset\" \"utf-8\") NIL NIL \"7bit\" 12 1)"
        ));
    }

    #[test]
    fn multipart_with_named_pdf_is_attachment() {
        let text = "((\"text\" \"plain\" (\"charset\" \"utf-8\") NIL NIL \"7bit\" 12 1)(\"application\" \"pdf\" (\"name\" \"a.pdf\") NIL NIL \"base64\" 100 NIL (\"attachment\" (\"filename\" \"a.pdf\")) NIL NIL) \"mixed\" (\"boundary\" \"x\"))";
        assert!(has_attachment_in_text(text));
    }

    #[test]
    fn inline_html_name_is_not_attachment() {
        assert!(!has_attachment_in_text(
            "(\"text\" \"html\" (\"name\" \"mail.html\" \"charset\" \"utf-8\") NIL NIL \"7bit\" 42)"
        ));
    }

    #[test]
    fn disposition_attachment_wins() {
        assert!(has_attachment_in_text(
            "(\"text\" \"plain\" NIL NIL NIL \"7bit\" 1 NIL (\"attachment\" (\"filename\" \"x.txt\")) NIL)"
        ));
    }

    #[test]
    fn malformed_input_does_not_panic() {
        for text in ["", "(", "()", "(\"text\"", ")", "\u{0}\u{1}", "(((((((\""] {
            let _ = has_attachment_in_text(text);
        }
    }

    #[test]
    fn deeply_nested_input_is_rejected() {
        let text = "(".repeat(200);
        assert!(!has_attachment_in_text(&text));
    }
}
