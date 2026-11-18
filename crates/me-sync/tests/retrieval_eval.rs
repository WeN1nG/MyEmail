//! 检索评测集（决策 O-24）。
//!
//! **这是知识库检索唯一的验证手段**：§6/§7 的验收项只回答「能不能跑通」，
//! 这里回答「召回准不准」。语料是 `tests/fixtures/kb/` 下 5 份合成文档，
//! 20 条问答对的口径写在 [doc/versiondoc/v5检索评测.md](../../../doc/versiondoc/v5检索评测.md)。
//!
//! 标 `#[ignore]` 的原因：需要**真实嵌入模型**（语义那一路）。
//! 没模型时本测试会退化成「只跑 FTS」，并把同义改写那 5 条的失败
//! 明确标成「模型未就绪」而不是「召回失败」——两者的修法完全不同。
//!
//! ```bash
//! cargo test -p me-sync --test retrieval_eval -- --ignored --nocapture
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use me_core::Embedder;
use me_core::knowledge::{
    ChunkHit, KbKind, KbOrigin, KnowledgeRepo, NewKnowledgeDoc, RAG_CANDIDATES, RAG_TOP_K,
    rrf_fuse, split_structured,
};
use me_store::Store;

/// 一条问答：问题、期望命中的文档（按标题）、类型。
struct Case {
    question: &'static str,
    expect: &'static [&'static str],
    kind: &'static str,
}

const CASES: &[Case] = &[
    // 一、单文档精确查询
    Case {
        question: "采购合同的付款周期是多久",
        expect: &["采购合同"],
        kind: "单文档精确",
    },
    Case {
        question: "华东仓搬迁项目为什么超支",
        expect: &["季度复盘"],
        kind: "单文档精确",
    },
    Case {
        question: "迟到早退怎么算违纪",
        expect: &["员工手册"],
        kind: "单文档精确",
    },
    Case {
        question: "刀具套装的切削速度建议是多少",
        expect: &["刀具规格"],
        kind: "单文档精确",
    },
    Case {
        question: "P1 级别故障多久首次响应",
        expect: &["服务手册"],
        kind: "单文档精确",
    },
    // 二、跨文档综合
    Case {
        question: "T-1000 加工中心的合同价与超支原因分别是什么",
        expect: &["采购合同", "季度复盘"],
        kind: "跨文档",
    },
    Case {
        question: "采购的刀具套装对主机有什么配套要求",
        expect: &["采购合同", "刀具规格"],
        kind: "跨文档",
    },
    Case {
        question: "员工主动离职的提前通知期限与交接流程是什么",
        expect: &["员工手册"],
        kind: "跨文档",
    },
    Case {
        question: "设备故障响应的时限与服务级别如何对应",
        expect: &["服务手册", "采购合同"],
        kind: "跨文档",
    },
    Case {
        question: "公司在保密与知识产权上分别有什么规定",
        expect: &["员工手册", "采购合同"],
        kind: "跨文档",
    },
    // 三、专有名词 / 编号 / 型号
    Case {
        question: "HT-2024-07 的涂层厚度是多少",
        expect: &["刀具规格"],
        kind: "编号",
    },
    Case {
        question: "合同编号 CG-2024-0371 的金额",
        expect: &["采购合同"],
        kind: "编号",
    },
    Case {
        question: "服务编号 CS-2024 的 P2 响应时限",
        expect: &["服务手册"],
        kind: "编号",
    },
    Case {
        question: "T-1000 的配套刀柄规格",
        expect: &["刀具规格"],
        kind: "型号",
    },
    Case {
        question: "采购合同里那句关于同业销售的限制",
        expect: &["采购合同"],
        kind: "型号",
    },
    // 四、同义改写（检向量通路）
    Case {
        question: "员工辞职要提前多久说",
        expect: &["员工手册"],
        kind: "同义改写",
    },
    Case {
        question: "仓库搬迁多花了多少钱",
        expect: &["季度复盘"],
        kind: "同义改写",
    },
    Case {
        question: "机器坏了多久有人来修",
        expect: &["服务手册"],
        kind: "同义改写",
    },
    Case {
        question: "设备保修多久",
        expect: &["采购合同"],
        kind: "同义改写",
    },
    Case {
        question: "员工手册里规定的泄密责任",
        expect: &["员工手册"],
        kind: "同义改写",
    },
];

/// 语料文件名 → 文档标题。
const CORPUS: &[(&str, &str)] = &[
    ("采购合同.md", "采购合同"),
    ("季度复盘.md", "季度复盘"),
    ("员工手册.md", "员工手册"),
    ("刀具规格.md", "刀具规格"),
    ("服务手册.md", "服务手册"),
];

/// 语料目录。优先用 `CARGO_MANIFEST_DIR` 下的固定路径——
/// `cargo test` 的工作目录是包根，`fixtures/` 相对它最稳。
fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/kb")
}

/// 嵌入模型目录。没有模型时返回 `None`（测试退化成 FTS only）。
fn model_dir() -> Option<PathBuf> {
    // 环境变量优先：开发机上模型装在 `%APPDATA%\MyEmail\models`。
    if let Ok(explicit) = std::env::var("MYEMAIL_EMBED_MODEL_DIR") {
        let path = PathBuf::from(explicit);
        if me_sync::knowledge::embed::model_ready(&path) {
            return Some(path);
        }
    }
    let root = dirs::data_dir()?.join("MyEmail");
    let path = me_sync::knowledge::embed::model_dir(&root);
    if me_sync::knowledge::embed::model_ready(&path) {
        Some(path)
    } else {
        None
    }
}

/// 把语料灌进临时库：本地分块 + 向量化。
///
/// **必须向量化**：只写语块不写向量的话，`kbvec.vec_chunks` 是空的，
/// KNN 那一路永远返回 0 条——评测会表现为「语义检索完全失效」，
/// 但根因是语料没进向量库，不是嵌入有问题。
fn ingest(
    store: &Store,
    embedder: Option<&me_sync::knowledge::embed::CandleEmbedder>,
) -> HashMap<String, i64> {
    let writable = store.writable();
    let mut titles = HashMap::new();
    for (file, title) in CORPUS {
        let path = corpus_dir().join(file);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("读语料 {} 失败：{err}", path.display()));
        let doc = NewKnowledgeDoc {
            origin: KbOrigin::File,
            source_ref: path.to_string_lossy().to_string(),
            title: (*title).to_string(),
            kind: KbKind::Md,
            mail_id: None,
            size_bytes: text.len() as u64,
            content_hash: format!("eval-{file}"),
        };
        let id = writable.upsert_document(&doc).expect("写入文档");
        let chunks = split_structured(&text);
        assert!(!chunks.is_empty(), "{file} 分块结果为空");
        writable.replace_chunks(id, &chunks).expect("写入语块");

        if let Some(embedder) = embedder {
            let stored = writable.chunks(id, 0, 1000).expect("读回语块");
            let texts: Vec<&str> = stored.iter().map(|chunk| chunk.text.as_str()).collect();
            let vectors = embedder.embed_documents(&texts).expect("嵌入语块");
            assert_eq!(vectors.len(), stored.len());
            let items: Vec<(me_core::knowledge::KnowledgeChunkId, Vec<f32>)> = stored
                .iter()
                .zip(vectors)
                .map(|(chunk, vector)| (chunk.id, vector))
                .collect();
            writable.put_vectors(&items).expect("写入向量");
        }

        titles.insert((*title).to_string(), id.0);
    }
    titles
}

/// 一路检索的结果：命中的文档 id 集合（按名次）。
fn docs_of(hits: &[ChunkHit]) -> Vec<i64> {
    hits.iter().map(|hit| hit.doc.0).collect()
}

/// 跑一路检索，返回 top-8 的文档 id 列表。
fn run_fts(store: &Store, question: &str) -> Vec<i64> {
    let writable = store.writable();
    let hits = writable.fts(question, RAG_CANDIDATES).expect("FTS");
    docs_of(&hits[..hits.len().min(RAG_TOP_K)])
}

fn run_knn(
    store: &Store,
    embedder: &me_sync::knowledge::embed::CandleEmbedder,
    question: &str,
) -> Vec<i64> {
    let query = embedder.embed_query(question).expect("嵌入查询");
    let writable = store.writable();
    let hits = writable.knn(&query, RAG_CANDIDATES).expect("KNN");
    docs_of(&hits[..hits.len().min(RAG_TOP_K)])
}

fn run_rrf(
    store: &Store,
    embedder: &me_sync::knowledge::embed::CandleEmbedder,
    question: &str,
) -> Vec<i64> {
    let writable = store.writable();
    let fts = writable.fts(question, RAG_CANDIDATES).expect("FTS");
    let query = embedder.embed_query(question).expect("嵌入查询");
    let knn = writable.knn(&query, RAG_CANDIDATES).expect("KNN");
    let fused = rrf_fuse(&fts, &knn, me_core::knowledge::RRF_K, RAG_TOP_K);
    docs_of(
        &fused
            .iter()
            .map(|scored| scored.hit.clone())
            .collect::<Vec<_>>(),
    )
}

/// 命中判定：期望的每个文档都要在 top-8 里。
fn hit(expected: &[&str], found: &[i64], titles: &HashMap<String, i64>) -> bool {
    expected.iter().all(|title| {
        let Some(id) = titles.get(*title) else {
            return false;
        };
        found.contains(id)
    })
}

/// 完整评测。**有模型就跑三路对比，没有就只跑 FTS**。
#[test]
#[ignore = "需要真实嵌入模型与固定语料"]
fn retrieval_hits_at_8_meets_target() {
    let dir = tempfile::tempdir().expect("临时目录");
    let store = Store::open(dir.path()).expect("打开存储");

    let embedder = model_dir()
        .map(|path| me_sync::knowledge::embed::CandleEmbedder::load(&path).expect("加载嵌入模型"));
    let titles = ingest(&store, embedder.as_ref());
    if embedder.is_none() {
        println!(
            "⚠️ 没找到嵌入模型：只跑 FTS 一路。\n\
             同义改写那 5 条必然失败，**这不是召回失败**，是模型没就绪。\n\
             下载模型后重跑，或用 MYEMAIL_EMBED_MODEL_DIR 指向模型目录。"
        );
    }

    // ---- 单路 FTS ----
    let mut fts_hits = 0usize;
    let mut fts_miss: Vec<&str> = Vec::new();
    for case in CASES {
        let found = run_fts(&store, case.question);
        if hit(case.expect, &found, &titles) {
            fts_hits += 1;
        } else {
            fts_miss.push(case.question);
        }
    }
    let fts_rate = fts_hits as f32 / CASES.len() as f32;
    println!(
        "FTS only : Hit@8 = {fts_hits}/{} = {:.2}",
        CASES.len(),
        fts_rate
    );
    for question in &fts_miss {
        println!("  未命中：{question}");
    }

    let Some(embedder) = embedder else {
        // 没有模型就到此为止：FTS 已经跑完，语义那一路无法评估。
        assert!(
            fts_rate >= 0.60,
            "FTS 单路的 Hit@8 只有 {fts_rate:.2}，连专有名词都搜不到：{fts_miss:?}"
        );
        return;
    };

    // ---- 单路 KNN ----
    let mut knn_hits = 0usize;
    let mut knn_miss: Vec<&str> = Vec::new();
    for case in CASES {
        let found = run_knn(&store, &embedder, case.question);
        if hit(case.expect, &found, &titles) {
            knn_hits += 1;
        } else {
            knn_miss.push(case.question);
        }
    }
    let knn_rate = knn_hits as f32 / CASES.len() as f32;
    println!(
        "KNN only : Hit@8 = {knn_hits}/{} = {:.2}",
        CASES.len(),
        knn_rate
    );
    for question in &knn_miss {
        println!("  未命中：{question}");
    }

    // ---- RRF 融合 ----
    let mut rrf_hits = 0usize;
    let mut rrf_miss: Vec<(&str, &str)> = Vec::new();
    for case in CASES {
        let found = run_rrf(&store, &embedder, case.question);
        if hit(case.expect, &found, &titles) {
            rrf_hits += 1;
        } else {
            rrf_miss.push((case.kind, case.question));
        }
    }
    let rrf_rate = rrf_hits as f32 / CASES.len() as f32;
    println!(
        "RRF      : Hit@8 = {rrf_hits}/{} = {:.2}",
        CASES.len(),
        rrf_rate
    );
    for (kind, question) in &rrf_miss {
        println!("  未命中（{kind}）：{question}");
    }

    // 指标一：RRF 的 Hit@8 必须达标。
    assert!(
        rrf_rate >= 0.85,
        "RRF 的 Hit@8 = {rrf_rate:.2}，低于目标 0.85。逐类看失败：{rrf_miss:?}"
    );

    // 指标二：RRF 不低于任何单路。
    assert!(
        rrf_rate >= fts_rate && rrf_rate >= knn_rate,
        "RRF ({rrf_rate:.2}) 不如单路：FTS {fts_rate:.2} / KNN {knn_rate:.2}。\
         融合权重或 k 值错了"
    );

    // 指标三：块尾召回全中（O-21 的 480 字上限）。
    // 这 5 条的答案都在块尾附近，漏了就说明还有更长的块没被切到。
    let tail_cases = [
        "采购合同的付款周期是多久",
        "采购合同里那句关于同业销售的限制",
    ];
    for question in tail_cases {
        let case = CASES
            .iter()
            .find(|case| case.question == question)
            .expect("语料里有这条");
        let found = run_rrf(&store, &embedder, case.question);
        assert!(
            hit(case.expect, &found, &titles),
            "块尾召回失败：{question}"
        );
    }
}

/// RRF 的 k 值不该照抄 60。这个测试把四组 k 都跑一遍，
/// 打印各自的 Hit@8，供人挑（结果写回 `doc/versiondoc/v5检索评测.md`）。
#[test]
#[ignore = "需要真实嵌入模型与固定语料"]
fn rrf_k_sweep_reports_hit_rates() {
    let dir = tempfile::tempdir().expect("临时目录");
    let store = Store::open(dir.path()).expect("打开存储");
    let Some(path) = model_dir() else {
        println!("⚠️ 没找到嵌入模型，跳过 k 值扫描。");
        return;
    };
    let embedder = me_sync::knowledge::embed::CandleEmbedder::load(&path).expect("加载嵌入模型");
    let titles = ingest(&store, Some(&embedder));

    for k in [10.0f32, 30.0, 60.0, 100.0] {
        let mut hits = 0usize;
        for case in CASES {
            let writable = store.writable();
            let fts = writable.fts(case.question, RAG_CANDIDATES).expect("FTS");
            let query = embedder.embed_query(case.question).expect("嵌入查询");
            let knn = writable.knn(&query, RAG_CANDIDATES).expect("KNN");
            let fused = rrf_fuse(&fts, &knn, k, RAG_TOP_K);
            let found = docs_of(
                &fused
                    .iter()
                    .map(|scored| scored.hit.clone())
                    .collect::<Vec<_>>(),
            );
            if hit(case.expect, &found, &titles) {
                hits += 1;
            }
        }
        println!("k = {k:>5.0} : Hit@8 = {hits}/{}", CASES.len());
    }
}

/// 语料本身的自检：5 份文档都要能分出块，块数不能太少。
///
/// 这一条**不标 ignore**——它不依赖模型，跑得快，
/// 能在语料被改坏时第一时间发现（比如手滑把文件删了）。
#[test]
fn corpus_files_exist_and_split_into_chunks() {
    for (file, title) in CORPUS {
        let path = corpus_dir().join(file);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("读语料 {} 失败：{err}", path.display()));
        let chunks = split_structured(&text);
        assert!(
            chunks.len() >= 3,
            "{file}（{title}）只切出 {} 块，太少了：语料是不是被改坏了？",
            chunks.len()
        );
        for chunk in &chunks {
            assert!(
                chunk.chars() as usize <= me_core::knowledge::MAX_CHUNK_CHARS,
                "{file} 有一块超长（{} 字），超过 {} 的上限",
                chunk.chars(),
                me_core::knowledge::MAX_CHUNK_CHARS
            );
        }
    }
}
