//! RFC 3501 modified UTF-7（IMAP 文件夹名编码）。
//!
//! 规则：
//! - 可打印 ASCII（0x20..=0x7e，`&` 除外）原样输出；
//! - `&` 写为 `&-`；
//! - 其他字符以 `&` 开始、`-` 结束，中间是 UTF-16BE 的 modified BASE64
//!   （`/` 换成 `,`，无 `=` 填充）。

use base64::Engine as _;

use me_core::CoreError;

/// 把 UTF-8 文件夹名编码为 modified UTF-7。
pub fn encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut shifted: Vec<u16> = Vec::new();

    fn flush(out: &mut String, shifted: &mut Vec<u16>) {
        if shifted.is_empty() {
            return;
        }
        let mut bytes = Vec::with_capacity(shifted.len() * 2);
        for unit in shifted.drain(..) {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        let encoded = base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes);
        out.push('&');
        for ch in encoded.chars() {
            out.push(if ch == '/' { ',' } else { ch });
        }
        out.push('-');
    }

    for ch in input.chars() {
        if ch == '&' {
            flush(&mut out, &mut shifted);
            out.push_str("&-");
        } else if (' '..='~').contains(&ch) {
            flush(&mut out, &mut shifted);
            out.push(ch);
        } else {
            let mut buf = [0u16; 2];
            shifted.extend_from_slice(ch.encode_utf16(&mut buf));
        }
    }
    flush(&mut out, &mut shifted);
    out
}

/// 把 modified UTF-7 解码为 UTF-8。畸形输入返回 `CoreError::Parse`。
pub fn decode(input: &str) -> Result<String, CoreError> {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '&' {
            out.push(ch);
            continue;
        }

        if chars.peek() == Some(&'-') {
            chars.next();
            out.push('&');
            continue;
        }

        let mut encoded = String::new();
        let mut terminated = false;
        for inner in chars.by_ref() {
            if inner == '-' {
                terminated = true;
                break;
            }
            encoded.push(if inner == ',' { '/' } else { inner });
        }
        if !terminated {
            return Err(CoreError::parse("modified UTF-7 缺少结束符"));
        }
        if encoded.is_empty() {
            return Err(CoreError::parse("modified UTF-7 空序列"));
        }

        let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(encoded.as_bytes())
            .map_err(|_| CoreError::parse("modified UTF-7 base64 无效"))?;
        if bytes.len() % 2 != 0 {
            return Err(CoreError::parse("modified UTF-7 UTF-16 长度非法"));
        }
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_be_bytes(*pair))
            .collect();
        let decoded = String::from_utf16(&units)
            .map_err(|_| CoreError::parse("modified UTF-7 UTF-16 无效"))?;
        out.push_str(&decoded);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_round_trip() {
        for name in ["INBOX", "Sent", "Archive/2025", "a b c", "~!@#$%^*()"] {
            assert_eq!(encode(name), name);
            assert_eq!(decode(name).unwrap(), name);
        }
    }

    #[test]
    fn ampersand_round_trip() {
        assert_eq!(encode("Tom & Jerry"), "Tom &- Jerry");
        assert_eq!(decode("Tom &- Jerry").unwrap(), "Tom & Jerry");
        assert_eq!(decode("&-").unwrap(), "&");
    }

    #[test]
    fn chinese_round_trip() {
        let names = [
            "收件箱",
            "已发送",
            "工作/草稿箱",
            "邮件 & 归档",
            "日本語フォルダ",
        ];
        for name in names {
            let encoded = encode(name);
            assert!(encoded.starts_with('&') || encoded.contains('&'));
            assert_eq!(decode(&encoded).unwrap(), name);
        }
    }

    #[test]
    fn known_rfc_example() {
        // RFC 3501 示例：~peter/mail/台北/日本語 -> ~peter/mail/&U,BTFw-/&ZeVnLIqe-
        let encoded = encode("~peter/mail/台北/日本語");
        assert_eq!(encoded, "~peter/mail/&U,BTFw-/&ZeVnLIqe-");
        assert_eq!(decode(&encoded).unwrap(), "~peter/mail/台北/日本語");
    }

    #[test]
    fn malformed_input_is_error_not_panic() {
        assert!(decode("&").is_err());
        assert!(decode("&AAAA").is_err());
        assert!(decode("&!!!!-").is_err());
    }
}
