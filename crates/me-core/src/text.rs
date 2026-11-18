//! 文本判定的公共部分：CJK 判定、标点、FTS5 折叠。
//!
//! 这些函数原来是两份（`me-core/src/ai.rs` 里判「中文该不该加空格」、
//! `me-store/src/cache.rs` 里判「文件名里能不能留这个字符」），
//! 知识库的 FTS 折叠又要第三份。v0.5 统一到这里，**别再各写一份**——
//! 三处的口径不一致时，表现是「搜索结果对不上」，而不是报错。

/// 是否是 CJK 字符（含汉字、日文假名、韩文音节、CJK 标点与全角形式）。
///
/// 覆盖面刻意包含扩展 A/B~F：中文人名与生僻字经常落在这里，
/// 漏掉的后果是 FTS 不为它们分词，2 字词直接搜不到。
///
/// 这是**唯一**一份判定：`me-core/src/ai.rs` 的 token 估算与
/// `me-store/src/cache.rs` 的文件名净化都引用它，别再各写一份。
pub fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x3000..=0x303F      // CJK 标点（、。「」等）
            | 0x3400..=0x4DBF // CJK 扩展 A
            | 0x4E00..=0x9FFF // CJK 基本区
            | 0xF900..=0xFAFF // CJK 兼容表意文字
            | 0x3040..=0x30FF // 平假名 / 片假名
            | 0xAC00..=0xD7AF // 韩文音节
            | 0xFF00..=0xFFEF // 全角形式
            | 0x20000..=0x2FA1F // CJK 扩展 B~F 与兼容补充
    )
}

/// 常见中英文标点。`is_meaningful` 用它区分「正文」与「提取出的乱码」。
///
/// 只收**真正会出现在正文里**的标点：`<>|` 这类在正常文本里很少见，
/// 收进来会让乱码判定变松。
pub fn is_common_punct(c: char) -> bool {
    matches!(
        c,
        ',' | '.'
            | ';'
            | ':'
            | '!'
            | '?'
            | '\''
            | '"'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '-'
            | '_'
            | '/'
            | '\\'
            | '@'
            | '#'
            | '%'
            | '&'
            | '*'
            | '+'
            | '='
            | '~'
            | '^'
            | '`'
            | '，'
            | '。'
            | '；'
            | '：'
            | '！'
            | '？'
            | '、'
            | '“'
            | '”'
            | '‘'
            | '’'
            | '（'
            | '）'
            | '【'
            | '】'
            | '《'
            | '》'
            | '「'
            | '」'
            | '『'
            | '』'
            | '—'
            | '…'
            | '·'
            | '～'
            | '／'
            | '＃'
            | '％'
            | '＠'
            | '＆'
            | '＊'
    )
}

/// 把 CJK 字符之间插入空格，其余原样（ASCII 单词不动：`T-1000` 不能被拆开）。
///
/// **写入与查询必须走同一个函数**。`kb_chunks.text` 存原文，只有 FTS 表存折叠版；
/// 查询侧忘了折叠，`MATCH '合同'` 会返回 0 行——而这是中文最常见的查询单位。
///
/// 为什么需要它：`unicode61` 把连续的汉字当成**一个 token**，
/// 于是「合同审查」只有一个 token，搜「合同」永远不命中。
/// 逐字切开后每个汉字都是 token，再配合短语查询就等价于「连续子串」。
pub fn fts_fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    let mut prev_cjk = false;
    for ch in text.chars() {
        let cjk = is_cjk(ch);
        if cjk && prev_cjk {
            out.push(' ');
        }
        out.push(ch);
        prev_cjk = cjk;
    }
    out
}

/// 把用户查询折成 FTS5 的**短语**查询串（`合同` → `"合 同"`）。
///
/// 短语语法保证「相邻」，否则 `MATCH '合 同'` 会命中「合…同」任意共现。
/// 输入里的双引号按 FTS5 的规则用两个双引号转义。
pub fn fts_phrase(query: &str) -> String {
    let folded = fts_fold(query);
    let mut escaped = String::with_capacity(folded.len() + 4);
    escaped.push('"');
    for ch in folded.chars() {
        if ch == '"' {
            escaped.push('"');
        }
        escaped.push(ch);
    }
    escaped.push('"');
    escaped
}

/// 一段查询里，token 数不超过这个值时用**精确短语**。
///
/// 4 个 token ≈ 4 个汉字（「付款周期」「华东仓」），中文查询最常用的长度就是
/// 2~4 字，这种长度用精确短语的精度明显高于二元组 OR：
/// 实测「付款周期」用短语只命中 1 块，用二元组会带出 3 块（多出来的两块
/// 只是恰好含「付款」或「周期」）。
///
/// 5 个字以上就不行了——原文不会逐字重复查询，短语必然落空，
/// 只能退回二元组。
const PHRASE_MAX_TOKENS: usize = 4;

/// 拼出 FTS5 的 MATCH 表达式。
///
/// **为什么不直接用整句短语**：`fts_phrase` 把整个查询包成一个短语，
/// 于是「采购合同的付款周期是多久」会变成 13 个字的连续短语，
/// 而原文里这 13 个字当然不连续——整句查询**必然 0 命中**。
/// 这一点在实测里很致命：短关键词搜得到，整句话搜不到。
///
/// 这里的策略分三层：
///
/// 1. **按空白与标点切段**。`HT-2024-07 的涂层厚度` 切成两段，
///    段与段之间是 AND（两个条件都满足才算命中）。
/// 2. **短段用精确短语**（`合同` → `"合 同"`）：保证「相邻」，
///    不让「合…同」任意共现混进来。
/// 3. **长段用二元组 OR**：`涂层厚度是多少` 折成
///    `"涂 层" OR "层 厚" OR "厚 度" OR "度 是" OR "是多" OR "多少"`。
///    这是中文没有分词器时的标准做法——用相邻二元组近似「词」，
///    靠 BM25 排序把命中最多的排前面。
///    纯短语在这个长度上会漏掉一大半（原文不会逐字重复查询里的字）。
///
/// 段内 OR、段间 AND：FTS5 的 `OR` 优先级高于隐式 AND，
/// 显式写出来是为了不依赖它的默认解析顺序。
pub fn fts_match(query: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for segment in split_query_segments(query) {
        let folded = fts_fold(&segment);
        let tokens: Vec<&str> = folded.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }
        if tokens.len() <= PHRASE_MAX_TOKENS {
            parts.push(quote_tokens(&tokens));
        } else {
            // 二元组 OR。ASCII 词（`T-1000`）只有一个 token，
            // 落到上面的精确短语分支，不会被切开。
            let mut alternatives = Vec::with_capacity(tokens.len() - 1);
            for window in tokens.windows(2) {
                alternatives.push(quote_tokens(window));
            }
            parts.push(alternatives.join(" OR "));
        }
    }
    if parts.is_empty() {
        // 空查询（或全是标点）：退化成「匹配一切」是不可能的，
        // 这里给一个必然不匹配的空短语，调用方据此返回空结果。
        return "\"\"".to_string();
    }
    // 段与段之间用 **OR**，不是 AND。
    //
    // 实测踩过的坑：查询「合同编号 CG-2024-0371 的金额」拆成三段，
    // 其中「的金额」在原文里不存在（原文写的是「合同总额」），
    // AND 之下整条查询直接 0 命中——**用户明明搜到了正确的那份文档**。
    // 改成 OR 之后，「合同编号」与「CG-2024-0371」任意一段命中即可，
    // 而 BM25 会把同时含多段的行排到前面，精度由排序保证，不由 AND 保证。
    parts.join(" OR ")
}

/// 把 token 列表包成一个 FTS5 短语（内部引号按 FTS5 规则转义成两个）。
///
/// **token 之间必须用空格分隔**：FTS5 的短语语法里空格才是「相邻」的意思，
/// 直接拼起来（`合` + `同` = `合同`）会被当成**一个** token，
/// 而 FTS 表里存的是逐字拆开的版本，于是永远匹配不上。
fn quote_tokens(tokens: &[&str]) -> String {
    let mut out = String::with_capacity(tokens.iter().map(|t| t.len() + 1).sum::<usize>() + 2);
    out.push('"');
    for (index, token) in tokens.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        if token.contains('"') {
            out.push_str(&token.replace('"', "\"\""));
        } else {
            out.push_str(token);
        }
    }
    out.push('"');
    out
}

/// 按空白与标点把查询切成段。
///
/// 标点包含 ASCII 的 `,.;:!?()[]{}"'` 与中文的 `，。；：！？、（）【】《》“”‘’`。
fn split_query_segments(query: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    for ch in query.chars() {
        if ch.is_whitespace() || is_query_punct(ch) {
            if !current.is_empty() {
                segments.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// 查询切段时算作分隔符的标点。
fn is_query_punct(ch: char) -> bool {
    matches!(
        ch,
        ',' | '.'
            | ';'
            | ':'
            | '!'
            | '?'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '"'
            | '\''
            | '，'
            | '。'
            | '；'
            | '：'
            | '！'
            | '？'
            | '、'
            | '（'
            | '）'
            | '【'
            | '】'
            | '《'
            | '》'
            | '“'
            | '”'
            | '‘'
            | '’'
            | '·'
            | '…'
            | '—'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_covers_common_and_rare_ranges() {
        assert!(is_cjk('中'));
        assert!(is_cjk('あ'));
        assert!(is_cjk('한'));
        assert!(is_cjk('\u{20000}')); // 扩展 B
        assert!(!is_cjk('a'));
        assert!(!is_cjk('1'));
        assert!(!is_cjk(','));
    }

    #[test]
    fn fts_fold_splits_cjk_only() {
        assert_eq!(fts_fold("合同审查"), "合 同 审 查");
        // ASCII 与型号不能被拆开（`T-1000` 必须整体成一个 token），
        // 汉字则逐字切开——「型号」本身当然也拆，查询侧同样折叠，短语仍然命中。
        assert_eq!(fts_fold("型号 T-1000 的合同"), "型 号 T-1000 的 合 同");
        assert_eq!(fts_fold(""), "");
    }

    #[test]
    fn fts_phrase_quotes_and_escapes() {
        assert_eq!(fts_phrase("合同"), "\"合 同\"");
        // 用户输入引号时不能拼出非法 FTS 表达式。
        assert_eq!(fts_phrase("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn fts_match_keeps_short_segments_as_phrases() {
        // 2 个 token 以内：精确短语，保证「相邻」。
        assert_eq!(fts_match("合同"), "\"合 同\"");
        // ASCII 词不被拆开。
        assert_eq!(fts_match("T-1000"), "\"T-1000\"");
    }

    /// 短语里的 token 必须空格分隔：直接拼起来会被当成一个 token。
    #[test]
    fn fts_match_phrase_keeps_token_separator() {
        let expr = fts_match("合同");
        assert!(expr.contains("合 同"), "token 之间要有空格：{expr}");
        assert!(!expr.contains("合同"), "不能把两个 token 拼成一个：{expr}");
    }

    #[test]
    fn fts_match_splits_long_segments_into_bigrams() {
        // 4 个 token 以内：精确短语。
        assert_eq!(fts_match("涂层厚度"), "\"涂 层 厚 度\"");
        // 5 个 token 起：走二元组 OR。
        assert_eq!(
            fts_match("涂层厚度是多少"),
            "\"涂 层\" OR \"层 厚\" OR \"厚 度\" OR \"度 是\" OR \"是 多\" OR \"多 少\""
        );
    }

    #[test]
    fn fts_match_ors_across_segments() {
        // 段间是 OR：任一段命中即可，AND 会让「原文没有那个词」的查询整条落空。
        let expr = fts_match("HT-2024-07 涂层");
        assert_eq!(expr, "\"HT-2024-07\" OR \"涂 层\"");
    }

    #[test]
    fn fts_match_splits_on_punctuation() {
        // 标点是分隔符：问句不能整句当短语，否则必然 0 命中。
        let expr = fts_match("合同，付款");
        assert_eq!(expr, "\"合 同\" OR \"付 款\"");
    }
}
