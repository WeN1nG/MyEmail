//! 本机嵌入推理（candle）与模型下载。
//!
//! # 为什么自己写 BERT 前向
//!
//! `candle-nn 0.11` 不再带 `models` 模块（那部分搬去了 `candle-transformers`），
//! 而后者会把 `hf-hub` / `image` / 一堆模型实现拖进依赖树。这里只需要
//! 「BERT encoder + CLS pooling」，用 `candle-nn` 的 `linear` / `layer_norm` /
//! `embedding` 手写约一百行就够，体积也更可控。
//!
//! # 三个写错也不报错的地方（决策 O-21）
//!
//! 1. **pooling 用 `[CLS]`，不是 mean**。用错照样出 512 维向量，只是召回变差。
//! 2. **必须 L2 归一化**。`vec0` 自己会算 cosine，但归一化后内积 = cosine，数值更稳。
//! 3. **query 加指令前缀、document 不加**。所以端口拆成两个方法，禁止混用。
//!
//! 另外 tokenizer 对超过 512 token 的输入**静默截断**——所以语块硬上限是 480 字，
//! 不是 600（见 `me_core::knowledge::MAX_CHUNK_CHARS`）。

use std::path::{Path, PathBuf};

use candle_core::{D, DType, Device, Tensor};
use candle_nn::ops::softmax_last_dim;
use candle_nn::{Embedding, LayerNorm, Linear, Module, VarBuilder, embedding, layer_norm, linear};
use me_core::CoreError;
use me_core::knowledge::{BGE_QUERY_PREFIX, EMBED_DIM, EMBED_MODEL_FILES, EMBED_MODEL_ID};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// tokenizer 的位置编码上限。与 bge 的 512 一致。
const MAX_TOKENS: usize = 512;

fn model_err(message: impl std::fmt::Display) -> CoreError {
    CoreError::storage(format!("嵌入模型: {message}"))
}

// ---------------------------------------------------------------------------
// BERT encoder
// ---------------------------------------------------------------------------

/// 模型结构参数（从 `config.json` 读）。
///
/// 字段名与 HuggingFace 的 `BertConfig` 一致，所以直接 serde 反序列化，
/// 不用自己映射——多一个字段就少一处「两边名字不一样」的坑。
#[derive(Debug, Clone, Deserialize)]
struct ModelConfig {
    #[serde(default = "default_hidden")]
    hidden_size: usize,
    #[serde(default = "default_intermediate")]
    intermediate_size: usize,
    #[serde(default = "default_heads")]
    num_attention_heads: usize,
    #[serde(default = "default_layers")]
    num_hidden_layers: usize,
    #[serde(default = "default_vocab")]
    vocab_size: usize,
    #[serde(default = "default_max_pos")]
    max_position_embeddings: usize,
    #[serde(default = "default_type_vocab")]
    type_vocab_size: usize,
    #[serde(default = "default_eps")]
    layer_norm_eps: f64,
}

fn default_hidden() -> usize {
    512
}
fn default_intermediate() -> usize {
    2048
}
fn default_heads() -> usize {
    8
}
fn default_layers() -> usize {
    4
}
fn default_vocab() -> usize {
    21128
}
fn default_max_pos() -> usize {
    512
}
fn default_type_vocab() -> usize {
    2
}
fn default_eps() -> f64 {
    1e-12
}

fn read_model_config(dir: &Path) -> Result<ModelConfig, CoreError> {
    let path = dir.join("config.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| model_err(format!("读 config.json 失败: {e}")))?;
    serde_json::from_str(&text).map_err(|e| model_err(format!("解析 config.json 失败: {e}")))
}

/// 探测权重文件里的公共前缀（`"bert"` 或空串）。
///
/// safetensors 的文件头是一段 JSON：8 字节长度 + `{"张量名": {"dtype":…, "shape":…}}`。
/// 读它只要读头部，不用把 96 MB 拉进内存。
///
/// 判据：找 `embeddings.word_embeddings.weight` 这个必然存在的张量，
/// 看它前面有没有公共前缀。`bert.embeddings.…` → 前缀是 `bert`；
/// `embeddings.…` → 没有前缀。
fn detect_weight_prefix(weights_path: &Path) -> Result<&'static str, CoreError> {
    use std::io::Read;
    let mut file = std::fs::File::open(weights_path)
        .map_err(|e| model_err(format!("打开权重文件失败: {e}")))?;
    let mut len_bytes = [0u8; 8];
    file.read_exact(&mut len_bytes)
        .map_err(|e| model_err(format!("读权重文件头失败: {e}")))?;
    let header_len = u64::from_le_bytes(len_bytes) as usize;
    // 头部不该有几十 MB；超过 8 MB 就说明文件格式不对，直接报错。
    if header_len > 8 * 1024 * 1024 {
        return Err(model_err("权重文件头长度异常，文件可能不是 safetensors"));
    }
    let mut header = vec![0u8; header_len];
    file.read_exact(&mut header)
        .map_err(|e| model_err(format!("读权重文件头失败: {e}")))?;
    let names: serde_json::Value = serde_json::from_slice(&header)
        .map_err(|e| model_err(format!("解析权重文件头失败: {e}")))?;
    let Some(map) = names.as_object() else {
        return Err(model_err("权重文件头不是 JSON 对象"));
    };
    if map.contains_key("embeddings.word_embeddings.weight") {
        return Ok("");
    }
    if map.contains_key("bert.embeddings.word_embeddings.weight") {
        return Ok("bert");
    }
    Err(model_err(
        "权重文件里找不到 embeddings.word_embeddings.weight（也没有 bert. 前缀版本）",
    ))
}

struct BertLayer {
    query: Linear,
    key: Linear,
    value: Linear,
    output: Linear,
    attn_ln: LayerNorm,
    ffn1: Linear,
    ffn2: Linear,
    out_ln: LayerNorm,
}

impl BertLayer {
    fn load(hidden: usize, intermediate: usize, vb: VarBuilder<'_>) -> Result<Self, CoreError> {
        let attn = vb.pp("attention");
        let self_attn = attn.pp("self");
        let out = attn.pp("output");
        Ok(BertLayer {
            query: linear(hidden, hidden, self_attn.pp("query")).map_err(model_err)?,
            key: linear(hidden, hidden, self_attn.pp("key")).map_err(model_err)?,
            value: linear(hidden, hidden, self_attn.pp("value")).map_err(model_err)?,
            output: linear(hidden, hidden, out.pp("dense")).map_err(model_err)?,
            attn_ln: layer_norm(hidden, 1e-12, out.pp("LayerNorm")).map_err(model_err)?,
            ffn1: linear(hidden, intermediate, vb.pp("intermediate").pp("dense"))
                .map_err(model_err)?,
            ffn2: linear(intermediate, hidden, vb.pp("output").pp("dense")).map_err(model_err)?,
            out_ln: layer_norm(hidden, 1e-12, vb.pp("output").pp("LayerNorm"))
                .map_err(model_err)?,
        })
    }

    /// 后置 LayerNorm 的 BERT 层：`x = LN(x)` → attn → `+residual` → `LN` → FFN → `+residual`。
    fn forward(
        &self,
        x: &Tensor,
        additive_mask: &Tensor,
        heads: usize,
    ) -> Result<Tensor, CoreError> {
        let (batch, seq, hidden) = x.dims3().map_err(model_err)?;
        let head_dim = hidden / heads;

        let residual = x.clone();
        let normed = self.attn_ln.forward(x).map_err(model_err)?;
        let q = self
            .query
            .forward(&normed)
            .and_then(|t| t.reshape((batch, seq, heads, head_dim)))
            .and_then(|t| t.transpose(1, 2))
            .and_then(|t| t.contiguous())
            .map_err(model_err)?;
        let k = self
            .key
            .forward(&normed)
            .and_then(|t| t.reshape((batch, seq, heads, head_dim)))
            .and_then(|t| t.transpose(1, 2))
            .and_then(|t| t.contiguous())
            .map_err(model_err)?;
        let v = self
            .value
            .forward(&normed)
            .and_then(|t| t.reshape((batch, seq, heads, head_dim)))
            .and_then(|t| t.transpose(1, 2))
            .and_then(|t| t.contiguous())
            .map_err(model_err)?;

        let scale = 1.0 / (head_dim as f64).sqrt();
        // `transpose` 出来的张量是**非连续**的，candle 的 matmul 不接受
        // 非连续的左操作数（报 `MatMulUnexpectedStriding`）。
        // 单条查询时 batch=1，形状恰好连续，所以只有批量推理才会炸——
        // 这类「只在批量下暴露」的坑必须在这里就修掉。
        let k = k
            .transpose(2, 3)
            .and_then(|t| t.contiguous())
            .map_err(model_err)?;
        let scores = q
            .matmul(&k)
            .and_then(|t| t.affine(scale, 0.0))
            .and_then(|t| t.broadcast_add(additive_mask))
            .map_err(model_err)?;
        let probs = softmax_last_dim(&scores).map_err(model_err)?;
        let context = probs
            .matmul(&v)
            .and_then(|t| t.transpose(1, 2))
            // 同上：transpose 之后不连续，reshape 前必须先 contiguous。
            .and_then(|t| t.contiguous())
            .and_then(|t| t.reshape((batch, seq, hidden)))
            .map_err(model_err)?;
        let context = self.output.forward(&context).map_err(model_err)?;
        let x = context.add(&residual).map_err(model_err)?;

        let residual = x.clone();
        let normed = self.out_ln.forward(&x).map_err(model_err)?;
        let ffn = self
            .ffn1
            .forward(&normed)
            .and_then(|t| t.gelu_erf())
            .and_then(|t| self.ffn2.forward(&t))
            .map_err(model_err)?;
        ffn.add(&residual).map_err(model_err)
    }
}

struct BertEncoder {
    word: Embedding,
    position: Embedding,
    token_type: Embedding,
    ln: LayerNorm,
    layers: Vec<BertLayer>,
    heads: usize,
}

impl BertEncoder {
    fn load(dir: &Path, device: &Device) -> Result<(Self, usize, usize), CoreError> {
        // 权重是 F32（config.json 里 `torch_dtype: float32`），直接 mmap 不转换。
        let weights_path = dir.join("model.safetensors");
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(
                std::slice::from_ref(&weights_path),
                DType::F32,
                device,
            )
            .map_err(model_err)?
        };

        // **前缀不是固定的**：HuggingFace 导出的权重挂在 `bert.` 下，
        // 而 ModelScope 上的 bge-small-zh-v1.5 是 sentence-transformers 转换过的版本，
        // 前缀被剥掉了（`embeddings.…` / `encoder.…` 直接挂在根下）。
        // 写死 `vb.pp("bert")` 的后果是「找不到张量 bert.embeddings.…」——
        // 报错信息里没有「前缀不对」这层意思，排查起来很绕。
        // 所以先读 safetensors 的头部（文件名 → 张量名的映射），
        // 看清楚前缀长什么样再取。
        let root = detect_weight_prefix(&weights_path)?;
        let bert = if root.is_empty() {
            vb.clone()
        } else {
            vb.pp(root)
        };
        let emb = bert.pp("embeddings");

        // 结构参数从 config.json 读，不硬编码：
        // 换模型（哪怕只是换层数）时硬编码会静默错位，出来的向量全是垃圾。
        let config = read_model_config(dir)?;
        let vocab = config.vocab_size;
        let hidden = config.hidden_size;
        let intermediate = config.intermediate_size;
        let heads = config.num_attention_heads;
        let layers = config.num_hidden_layers;

        if hidden != EMBED_DIM {
            return Err(model_err(format!(
                "模型隐层维度是 {hidden}，与约定的 {EMBED_DIM} 不符。换模型必须整库重建向量。"
            )));
        }

        let word = embedding(vocab, hidden, emb.pp("word_embeddings")).map_err(model_err)?;
        let position = embedding(
            config.max_position_embeddings,
            hidden,
            emb.pp("position_embeddings"),
        )
        .map_err(model_err)?;
        let token_type = embedding(
            config.type_vocab_size,
            hidden,
            emb.pp("token_type_embeddings"),
        )
        .map_err(model_err)?;
        let ln =
            layer_norm(hidden, config.layer_norm_eps, emb.pp("LayerNorm")).map_err(model_err)?;

        let encoder = bert.pp("encoder");
        let mut stack = Vec::with_capacity(layers);
        for index in 0..layers {
            stack.push(BertLayer::load(
                hidden,
                intermediate,
                encoder.pp(format!("layer.{index}")),
            )?);
        }
        Ok((
            BertEncoder {
                word,
                position,
                token_type,
                ln,
                layers: stack,
                heads,
            },
            hidden,
            heads,
        ))
    }

    fn forward(&self, ids: &Tensor, mask: &Tensor) -> Result<Tensor, CoreError> {
        let (batch, seq) = ids.dims2().map_err(model_err)?;
        let device = ids.device();
        let positions = Tensor::arange(0u32, seq as u32, device)
            .and_then(|t| t.unsqueeze(0))
            .and_then(|t| t.broadcast_as((batch, seq)))
            .map_err(model_err)?;
        let type_ids = Tensor::zeros((batch, seq), DType::U32, device).map_err(model_err)?;

        let mut x = self
            .word
            .forward(ids)
            .and_then(|t| t.add(&self.position.forward(&positions)?))
            .and_then(|t| t.add(&self.token_type.forward(&type_ids)?))
            .map_err(model_err)?;
        x = self.ln.forward(&x).map_err(model_err)?;

        // 加性掩码：mask=0 的位置加一个足够大的负数，softmax 后趋 0。
        let additive = mask
            .to_dtype(DType::F32)
            .and_then(|t| t.affine(-1.0, 1.0))
            .and_then(|t| t.affine(10_000.0, 0.0))
            .and_then(|t| t.reshape((batch, 1, 1, seq)))
            .map_err(model_err)?;

        for layer in &self.layers {
            x = layer.forward(&x, &additive, self.heads)?;
        }
        Ok(x)
    }
}

// ---------------------------------------------------------------------------
// 嵌入器
// ---------------------------------------------------------------------------

/// 生产实现：本机 CPU 推理。
pub struct CandleEmbedder {
    encoder: BertEncoder,
    tokenizer: tokenizers::Tokenizer,
    device: Device,
    dim: usize,
}

impl CandleEmbedder {
    /// 加载模型。目录缺失或文件损坏都返回错误——上层据此降级成「仅关键词检索」。
    pub fn load(dir: &Path) -> Result<Self, CoreError> {
        let device = Device::Cpu;
        let tokenizer_path = dir.join("tokenizer.json");
        let mut tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| model_err(format!("读取 tokenizer 失败: {e}")))?;
        // **必须设截断**：不设的话超长输入会被静默截到模型上限，
        // 而且不会告诉我们截了。
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(|e| model_err(format!("设置 tokenizer 截断失败: {e}")))?;
        let (encoder, dim, _heads) = BertEncoder::load(dir, &device)?;
        Ok(CandleEmbedder {
            encoder,
            tokenizer,
            device,
            dim,
        })
    }

    fn encode(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, CoreError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| model_err(format!("分词失败: {e}")))?;

        let max_len = encodings
            .iter()
            .map(|item| item.get_ids().len())
            .max()
            .unwrap_or(0)
            .max(1);
        let batch = encodings.len();
        let mut ids = vec![0u32; batch * max_len];
        let mut mask = vec![0u32; batch * max_len];
        for (row, encoding) in encodings.iter().enumerate() {
            let tokens = encoding.get_ids();
            let attention = encoding.get_attention_mask();
            for (col, id) in tokens.iter().enumerate() {
                ids[row * max_len + col] = *id;
                mask[row * max_len + col] = attention.get(col).copied().unwrap_or(1);
            }
        }

        let ids = Tensor::from_vec(ids, (batch, max_len), &self.device).map_err(model_err)?;
        let mask = Tensor::from_vec(mask, (batch, max_len), &self.device).map_err(model_err)?;
        let hidden = self.encoder.forward(&ids, &mask)?;

        // CLS pooling：取第 0 个位置。**不是 mean pooling**（决策见模块注释）。
        let cls = hidden
            .narrow(D::Minus2, 0, 1)
            .and_then(|t| t.squeeze(D::Minus2))
            .map_err(model_err)?;
        let vectors = cls.to_vec2::<f32>().map_err(model_err)?;
        let mut out = Vec::with_capacity(vectors.len());
        for vector in vectors {
            out.push(l2_normalize(vector));
        }
        Ok(out)
    }
}

/// L2 归一化。零向量原样返回（归一化会除 0，产出 NaN 向量，之后 KNN 全崩）。
fn l2_normalize(mut vector: Vec<f32>) -> Vec<f32> {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 1e-12 {
        for value in &mut vector {
            *value /= norm;
        }
    }
    vector
}

impl me_core::knowledge::Embedder for CandleEmbedder {
    fn model_id(&self) -> &str {
        EMBED_MODEL_ID
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn embed_documents(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, CoreError> {
        // 索引路径：不加指令前缀。
        self.encode(texts)
    }

    fn embed_query(&self, query: &str) -> Result<Vec<f32>, CoreError> {
        // 查询路径：加 BGE 的指令前缀。与 document 混淆会系统性拉低相似度。
        let prefixed = format!("{BGE_QUERY_PREFIX}{query}");
        let mut vectors = self.encode(&[prefixed.as_str()])?;
        Ok(vectors.pop().unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// 模型文件与下载
// ---------------------------------------------------------------------------

/// 目录里三个文件都在就算「已就绪」。**不重算 sha**：每次启动校验 95 MB 太贵，
/// 校验只在下载完成时做一次。
pub fn model_ready(dir: &Path) -> bool {
    EMBED_MODEL_FILES.iter().all(|name| {
        dir.join(name)
            .metadata()
            .map(|meta| meta.len() > 0)
            .unwrap_or(false)
    })
}

/// 下载三个文件到 `dir`。`progress(done, total)` 每 1 MB 调一次。
///
/// **不做断点续传**（第一版）：失败就整文件重下。这样实现简单，
/// 也避免「半截文件 + Range 请求」引入的一类难查问题。
pub async fn download_model(
    client: &reqwest::Client,
    base_url: &str,
    dir: &Path,
    progress: impl Fn(u64, Option<u64>),
) -> Result<(), CoreError> {
    std::fs::create_dir_all(dir)
        .map_err(|e| CoreError::storage(format!("创建模型目录失败: {e}")))?;

    for name in EMBED_MODEL_FILES {
        let url = format!("{base_url}&FilePath={name}");
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|e| CoreError::network(true, format!("下载 {name} 失败（网络）: {e}")))?;
        if !response.status().is_success() {
            return Err(CoreError::network(
                true,
                format!("下载 {name} 失败：HTTP {}", response.status()),
            ));
        }
        let total = response.content_length();
        let target = dir.join(name);
        let tmp = dir.join(format!("{name}.part"));
        write_stream(response, &tmp, total, &progress).await?;

        if name == "model.safetensors" {
            let actual = sha256_file(&tmp)?;
            if actual != me_core::knowledge::EMBED_MODEL_SHA256 {
                let _ = std::fs::remove_file(&tmp);
                return Err(CoreError::storage(format!(
                    "模型文件校验失败（sha256 不符），已删除。实际 {actual}"
                )));
            }
        }
        std::fs::rename(&tmp, &target)
            .map_err(|e| CoreError::storage(format!("保存 {name} 失败: {e}")))?;
    }
    progress(1, Some(1));
    Ok(())
}

async fn write_stream(
    mut response: reqwest::Response,
    path: &Path,
    total: Option<u64>,
    progress: &impl Fn(u64, Option<u64>),
) -> Result<(), CoreError> {
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| CoreError::storage(format!("创建下载文件失败: {e}")))?;
    let mut done = 0u64;
    let mut last_report = 0u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| CoreError::network(true, format!("下载中断: {e}")))?
    {
        file.write_all(&chunk)
            .await
            .map_err(|e| CoreError::storage(format!("写入下载文件失败: {e}")))?;
        done += chunk.len() as u64;
        // 每 1 MB 报一次：95 MB 的文件报 95 次，界面进度条够平滑，
        // 又不会把事件通道打满。
        if done - last_report >= 1 << 20 {
            last_report = done;
            progress(done, total);
        }
    }
    file.flush()
        .await
        .map_err(|e| CoreError::storage(format!("刷新下载文件失败: {e}")))?;
    file.sync_all()
        .await
        .map_err(|e| CoreError::storage(format!("落盘下载文件失败: {e}")))?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, CoreError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)
        .map_err(|e| CoreError::storage(format!("校验时打不开模型文件: {e}")))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| CoreError::storage(format!("校验读取失败: {e}")))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// 模型目录（`<root>/models/<id>/`）。
pub fn model_dir(root: &Path) -> PathBuf {
    root.join("models").join(EMBED_MODEL_ID)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l2_normalize_makes_unit_length() {
        let normalized = l2_normalize(vec![3.0, 4.0]);
        let norm = (normalized[0].powi(2) + normalized[1].powi(2)).sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
    }

    #[test]
    fn l2_normalize_keeps_zero_vector() {
        // 全零向量归一化会除 0 变成 NaN，KNN 之后全崩。
        assert_eq!(l2_normalize(vec![0.0, 0.0]), vec![0.0, 0.0]);
    }

    #[test]
    fn model_not_ready_for_empty_dir() {
        let dir = tempfile::tempdir().expect("tmp");
        assert!(!model_ready(dir.path()));
    }
}
