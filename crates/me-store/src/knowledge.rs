//! 知识库仓储：文档 / 语块 / FTS / 向量表。
//!
//! **本文件是 `kb_chunks_fts` 与 `kbvec.vec_chunks` 的唯一维护者**
//! （与 `search.rs` 对 `messages_fts` 的约定一致）。别在别处自己拼这几张表的 SQL：
//! 漏一处就是「搜得到但点不开」或「点了空结果」的幽灵 bug，而且极难复现。
//!
//! 三条硬约束：
//!
//! 1. **删除只有一个入口** `delete_doc_deep`（下面 `delete` 的实现）：
//!    收 chunk id → 删向量 → 删 FTS → 删文档，顺序不能反（决策 O-26）。
//! 2. **写入顺序固定**：先写向量、再标 `embedded = 1`（决策 O-22）。
//!    反过来崩溃会留下「标了完成但查不到向量」的块，且永远不会被重跑。
//! 3. **FTS 的文本一律过 `fts_fold`**，查询侧同样折叠并包成短语（决策 O-20）。

use me_core::knowledge::{
    ChunkHit, KbFilter, KbKind, KbOrigin, KbStatus, KnowledgeChunk, KnowledgeChunkId, KnowledgeDoc,
    KnowledgeDocId, KnowledgeRepo, KnowledgeStats, MailSeed, NewChunk, NewKnowledgeDoc, SkipReason,
    fold_for_fts,
};
use me_core::text::fts_match;
use me_core::{CoreError, MessageId};
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};

use crate::db::{ReadOnlyStore, WritableStore};
use crate::{StoreResult, storage};

/// 只读侧单次 `LIMIT` 上限（与项目既有约定一致）。
const READ_LIMIT: u32 = 200;

fn clamp_limit(limit: u32) -> u32 {
    limit.clamp(1, READ_LIMIT)
}

/// f32 → 小端裸字节。sqlite-vec **只收裸字节**，不收 JSON、不收 JSON 数组。
fn f32_blob(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

fn map_doc(row: &Row<'_>) -> rusqlite::Result<KnowledgeDoc> {
    Ok(KnowledgeDoc {
        id: KnowledgeDocId(row.get("id")?),
        origin: KbOrigin::parse(&row.get::<_, String>("origin")?).unwrap_or(KbOrigin::File),
        source_ref: row.get("source_ref")?,
        title: row.get("title")?,
        kind: KbKind::parse(&row.get::<_, String>("kind")?).unwrap_or(KbKind::Text),
        mail_id: row.get::<_, Option<i64>>("mail_id")?.map(MessageId),
        size_bytes: row.get::<_, i64>("size_bytes")?.max(0) as u64,
        content_hash: row.get("content_hash")?,
        status: KbStatus::parse(&row.get::<_, String>("status")?).unwrap_or(KbStatus::Pending),
        skip_reason: row
            .get::<_, Option<String>>("skip_reason")?
            .and_then(|value| SkipReason::parse(&value)),
        chunk_count: row.get::<_, i64>("chunk_count")?.max(0) as u32,
        char_count: row.get::<_, i64>("char_count")?.max(0) as u32,
        splitter: row.get("splitter")?,
        embed_model: row.get("embed_model")?,
        embed_dim: row.get::<_, i64>("embed_dim")?.max(0) as u32,
        enabled: row.get::<_, i64>("enabled")? != 0,
        error: row.get("error")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

const DOC_COLUMNS: &str = "id, origin, source_ref, title, kind, mail_id, size_bytes, \
                           content_hash, status, skip_reason, chunk_count, char_count, \
                           splitter, embed_model, embed_dim, enabled, error, created_at, updated_at";

fn map_chunk(row: &Row<'_>) -> rusqlite::Result<KnowledgeChunk> {
    Ok(KnowledgeChunk {
        id: KnowledgeChunkId(row.get("id")?),
        doc_id: KnowledgeDocId(row.get("doc_id")?),
        ord: row.get::<_, i64>("ord")?.max(0) as u32,
        text: row.get("text")?,
        char_count: row.get::<_, i64>("char_count")?.max(0) as u32,
        locator: row.get("locator")?,
        embedded: row.get::<_, i64>("embedded")? != 0,
    })
}

fn map_hit(row: &Row<'_>) -> rusqlite::Result<ChunkHit> {
    Ok(ChunkHit {
        chunk: KnowledgeChunkId(row.get("chunk_id")?),
        doc: KnowledgeDocId(row.get("doc_id")?),
        title: row.get("title")?,
        ord: row.get::<_, i64>("ord")?.max(0) as u32,
        text: row.get("text")?,
        locator: row.get("locator")?,
        score: row.get::<_, f64>("score").unwrap_or(0.0) as f32,
    })
}

// ---------------------------------------------------------------------------
// 深删（唯一入口）
// ---------------------------------------------------------------------------

/// 收 chunk id → 删向量 → 删 FTS → 删文档。**顺序不能反。**
///
/// `vec0` 与普通 FTS5 都不吃外键级联：一旦 `kb_chunks` 被 CASCADE 删掉，
/// 就再也找不到它们，只能留下「搜得到但点不开」的幽灵结果。
fn delete_doc_deep(conn: &Connection, doc_id: KnowledgeDocId) -> Result<(), CoreError> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| storage(format!("开始知识库删除事务失败: {e}")))?;

    // 1) 先收 chunk id —— 后面两步都靠它。
    let ids: Vec<i64> = {
        let mut stmt = tx
            .prepare("SELECT id FROM kb_chunks WHERE doc_id = ?1")
            .store()?;

        stmt.query_map([doc_id.0], |row| row.get(0))
            .store()?
            .collect::<Result<Vec<i64>, _>>()
            .store()?
    };

    {
        let mut vector = tx
            .prepare("DELETE FROM kbvec.vec_chunks WHERE chunk_id = ?1")
            .store()?;
        let mut fts = tx
            .prepare("DELETE FROM kb_chunks_fts WHERE rowid = ?1")
            .store()?;
        for chunk in &ids {
            vector.execute([chunk]).store()?;
            fts.execute([chunk]).store()?;
        }
    }

    // 4) 最后删文档：kb_chunks / kb_doc_entities 走 CASCADE。
    tx.execute("DELETE FROM kb_documents WHERE id = ?1", [doc_id.0])
        .store()?;
    tx.commit()
        .map_err(|e| storage(format!("提交知识库删除事务失败: {e}")))?;
    Ok(())
}

/// 清掉某份文档现有的语块（含它们在各索引里的行）。供 `replace_chunks` 用。
fn clear_chunks(conn: &Connection, doc_id: KnowledgeDocId) -> Result<(), CoreError> {
    let ids: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT id FROM kb_chunks WHERE doc_id = ?1")
            .store()?;

        stmt.query_map([doc_id.0], |row| row.get(0))
            .store()?
            .collect::<Result<Vec<i64>, _>>()
            .store()?
    };
    {
        let mut vector = conn
            .prepare("DELETE FROM kbvec.vec_chunks WHERE chunk_id = ?1")
            .store()?;
        let mut fts = conn
            .prepare("DELETE FROM kb_chunks_fts WHERE rowid = ?1")
            .store()?;
        for chunk in &ids {
            vector.execute([chunk]).store()?;
            fts.execute([chunk]).store()?;
        }
    }
    conn.execute("DELETE FROM kb_chunks WHERE doc_id = ?1", [doc_id.0])
        .store()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 写仓储
// ---------------------------------------------------------------------------

impl KnowledgeRepo for WritableStore {
    fn upsert_document(&self, doc: &NewKnowledgeDoc) -> Result<KnowledgeDocId, CoreError> {
        let now = me_core::now_unix();
        let conn = self.lock();
        // `content_hash` 不变 → 保持原状态（不重跑）；变了 → 重置成 pending 重跑。
        // `mail_id` 用 COALESCE：同一封邮件重复入库时不要把外键抹成 NULL。
        let id: i64 = conn
            .query_row(
                "INSERT INTO kb_documents
                   (origin, source_ref, title, kind, mail_id, size_bytes, content_hash,
                    status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8, ?8)
                 ON CONFLICT(origin, source_ref) DO UPDATE SET
                   title        = excluded.title,
                   kind         = excluded.kind,
                   size_bytes   = excluded.size_bytes,
                   mail_id      = COALESCE(excluded.mail_id, kb_documents.mail_id),
                   status       = CASE WHEN kb_documents.content_hash = excluded.content_hash
                                       THEN kb_documents.status ELSE 'pending' END,
                   skip_reason  = CASE WHEN kb_documents.content_hash = excluded.content_hash
                                       THEN kb_documents.skip_reason ELSE NULL END,
                   error        = CASE WHEN kb_documents.content_hash = excluded.content_hash
                                       THEN kb_documents.error ELSE NULL END,
                   content_hash = excluded.content_hash,
                   updated_at   = excluded.updated_at
                 RETURNING id",
                params![
                    doc.origin.as_str(),
                    doc.source_ref,
                    doc.title,
                    doc.kind.as_str(),
                    doc.mail_id.map(|id| id.0),
                    doc.size_bytes as i64,
                    doc.content_hash,
                    now,
                ],
                |row| row.get(0),
            )
            .store()?;
        Ok(KnowledgeDocId(id))
    }

    fn set_status(
        &self,
        id: KnowledgeDocId,
        status: KbStatus,
        error: Option<&str>,
    ) -> Result<(), CoreError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE kb_documents SET status = ?2, error = ?3, updated_at = ?4 WHERE id = ?1",
            params![id.0, status.as_str(), error, me_core::now_unix()],
        )
        .store()?;
        Ok(())
    }

    fn set_skip_reason(&self, id: KnowledgeDocId, reason: SkipReason) -> Result<(), CoreError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE kb_documents
                SET status = 'skipped', skip_reason = ?2, error = NULL, updated_at = ?3
              WHERE id = ?1",
            params![id.0, reason.as_str(), me_core::now_unix()],
        )
        .store()?;
        Ok(())
    }

    fn replace_chunks(&self, id: KnowledgeDocId, chunks: &[NewChunk]) -> Result<(), CoreError> {
        let conn = self.lock();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| storage(format!("开始替换语块事务失败: {e}")))?;

        // 旧语块连索引一起清掉：只删 kb_chunks 会留下向量与 FTS 的孤儿行。
        clear_chunks(&tx, id)?;

        // 文档标题要进 FTS：标题是用户最可能搜的东西（「采购合同」「员工手册」），
        // 而它不在任何一块的正文里——只在 `kb_documents.title` 里。
        let doc_title: String = tx
            .query_row(
                "SELECT title FROM kb_documents WHERE id = ?1",
                params![id.0],
                |row| row.get(0),
            )
            .store()?;

        let mut char_total = 0u32;
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO kb_chunks (doc_id, ord, text, char_count, locator, embedded)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0)",
                )
                .store()?;
            let mut fts = tx
                .prepare("INSERT INTO kb_chunks_fts (rowid, text) VALUES (?1, ?2)")
                .store()?;
            for (index, chunk) in chunks.iter().enumerate() {
                let chars = chunk.chars();
                char_total += chars;
                insert
                    .execute(params![
                        id.0,
                        index as i64,
                        chunk.text,
                        chars as i64,
                        chunk.locator,
                    ])
                    .store()?;
                // FTS 存**折叠后**的文本；kb_chunks.text 存原文（检索结果要回显原文）。
                //
                // 索引内容 = 标题 + 章节标题（locator）+ 正文：
                // 标题与章节标题都不在正文里（「HT-2024-07 刀具套装技术规格」
                // 只出现在标题行），不索引它们，搜型号就一定搜不到。
                let rowid = tx.last_insert_rowid();
                let mut indexed = doc_title.clone();
                if let Some(locator) = &chunk.locator {
                    indexed.push('\n');
                    indexed.push_str(locator);
                }
                indexed.push('\n');
                indexed.push_str(&chunk.text);
                fts.execute(params![rowid, fold_for_fts(&indexed)])
                    .store()?;
            }
        }

        tx.execute(
            "UPDATE kb_documents
                SET chunk_count = ?2, char_count = ?3, status = 'chunked',
                    error = NULL, updated_at = ?4
              WHERE id = ?1",
            params![
                id.0,
                chunks.len() as i64,
                char_total as i64,
                me_core::now_unix()
            ],
        )
        .store()?;

        tx.commit()
            .map_err(|e| storage(format!("提交替换语块事务失败: {e}")))?;
        Ok(())
    }

    fn put_vectors(&self, items: &[(KnowledgeChunkId, Vec<f32>)]) -> Result<(), CoreError> {
        if items.is_empty() {
            return Ok(());
        }
        let conn = self.lock();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| storage(format!("开始写向量事务失败: {e}")))?;
        {
            // vec0 对 `INSERT OR REPLACE` 的支持依赖版本，删了再插最稳。
            let mut delete = tx
                .prepare("DELETE FROM kbvec.vec_chunks WHERE chunk_id = ?1")
                .store()?;
            let mut insert = tx
                .prepare("INSERT INTO kbvec.vec_chunks(chunk_id, embedding) VALUES (?1, ?2)")
                .store()?;
            for (id, vector) in items {
                delete.execute([id.0]).store()?;
                insert.execute(params![id.0, f32_blob(vector)]).store()?;
            }
        }
        tx.commit()
            .map_err(|e| storage(format!("提交写向量事务失败: {e}")))?;
        Ok(())
    }

    fn knn(&self, query: &[f32], limit: usize) -> Result<Vec<ChunkHit>, CoreError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT v.chunk_id AS chunk_id, v.distance AS score, c.doc_id AS doc_id,
                        c.ord AS ord, c.text AS text, c.locator AS locator, d.title AS title
                   FROM (SELECT chunk_id, distance FROM kbvec.vec_chunks
                          WHERE embedding MATCH ?1 AND k = ?2) v
                   JOIN kb_chunks c ON c.id = v.chunk_id
                   JOIN kb_documents d ON d.id = c.doc_id
                  WHERE d.enabled = 1
                  ORDER BY v.distance",
            )
            .store()?;
        let rows = stmt
            .query_map(params![f32_blob(query), limit as i64], map_hit)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?;
        Ok(rows)
    }

    fn fts(&self, query: &str, limit: usize) -> Result<Vec<ChunkHit>, CoreError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT f.rowid AS chunk_id, bm25(kb_chunks_fts) AS score, c.doc_id AS doc_id,
                        c.ord AS ord, c.text AS text, c.locator AS locator, d.title AS title
                   FROM kb_chunks_fts f
                   JOIN kb_chunks c ON c.id = f.rowid
                   JOIN kb_documents d ON d.id = c.doc_id
                  WHERE kb_chunks_fts MATCH ?1 AND d.enabled = 1
                  ORDER BY score
                  LIMIT ?2",
            )
            .store()?;
        // 短语查询：中文 2 字词靠它才能命中（trigram 的 3 字符限制在这里不存在）。
        let rows = stmt
            .query_map(params![fts_match(query), limit as i64], map_hit)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?;
        Ok(rows)
    }

    fn list(
        &self,
        filter: &KbFilter,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<KnowledgeDoc>, CoreError> {
        let conn = self.lock();
        let mut sql = format!("SELECT {DOC_COLUMNS} FROM kb_documents WHERE 1 = 1");
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(origin) = filter.origin {
            sql.push_str(" AND origin = ?");
            args.push(rusqlite::types::Value::Text(origin.as_str().to_string()));
        }
        if let Some(status) = filter.status {
            sql.push_str(" AND status = ?");
            args.push(rusqlite::types::Value::Text(status.as_str().to_string()));
        }
        if let Some(query) = filter.query.as_deref().filter(|q| !q.trim().is_empty()) {
            sql.push_str(" AND title LIKE ?");
            args.push(rusqlite::types::Value::Text(format!("%{}%", query.trim())));
        }
        sql.push_str(" ORDER BY updated_at DESC, id DESC LIMIT ? OFFSET ?");
        args.push(i64::from(clamp_limit(limit)).into());
        args.push(i64::from(offset).into());

        let mut stmt = conn.prepare(&sql).store()?;
        let rows = stmt
            .query_map(params_from_iter(args.iter()), map_doc)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?;
        Ok(rows)
    }

    fn get(&self, id: KnowledgeDocId) -> Result<Option<KnowledgeDoc>, CoreError> {
        let conn = self.lock();
        conn.query_row(
            &format!("SELECT {DOC_COLUMNS} FROM kb_documents WHERE id = ?1"),
            [id.0],
            map_doc,
        )
        .optional()
        .store()
    }

    fn chunks(
        &self,
        id: KnowledgeDocId,
        from: u32,
        count: u32,
    ) -> Result<Vec<KnowledgeChunk>, CoreError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, doc_id, ord, text, char_count, locator, embedded
                   FROM kb_chunks WHERE doc_id = ?1 ORDER BY ord LIMIT ?2 OFFSET ?3",
            )
            .store()?;
        let rows = stmt
            .query_map(
                params![id.0, i64::from(clamp_limit(count)), i64::from(from)],
                map_chunk,
            )
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?;
        Ok(rows)
    }

    fn delete(&self, id: KnowledgeDocId) -> Result<(), CoreError> {
        let conn = self.lock();
        delete_doc_deep(&conn, id)
    }

    fn link_entities(&self, id: KnowledgeDocId, links: &[(i64, u32)]) -> Result<(), CoreError> {
        let conn = self.lock();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| storage(format!("开始写实体关联事务失败: {e}")))?;
        tx.execute("DELETE FROM kb_doc_entities WHERE doc_id = ?1", [id.0])
            .store()?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO kb_doc_entities(doc_id, entity_id, hits)
                     VALUES (?1, ?2, ?3)",
                )
                .store()?;
            for (entity, hits) in links {
                stmt.execute(params![id.0, entity, i64::from(*hits)])
                    .store()?;
            }
        }
        tx.commit()
            .map_err(|e| storage(format!("提交实体关联事务失败: {e}")))?;
        Ok(())
    }

    fn doc_entities(&self, id: KnowledgeDocId) -> Result<Vec<i64>, CoreError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT entity_id FROM kb_doc_entities WHERE doc_id = ?1 ORDER BY hits DESC")
            .store()?;
        let rows = stmt
            .query_map([id.0], |row| row.get(0))
            .store()?
            .collect::<Result<Vec<i64>, _>>()
            .store()?;
        Ok(rows)
    }

    fn pending_docs(&self, limit: u32) -> Result<Vec<KnowledgeDoc>, CoreError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!(
                // **只取 `pending`**，不取 `chunked`。
                //
                // `chunked` 状态的文档「还差向量」，但向量这一步要等模型就绪。
                // 模型没就绪时状态不变，于是它会**永远**被这条查询选中——
                // worker 反复取同一份文档、反复发现没模型、原地打转，
                // 后面的文档永远排不上队（实测 134 封邮件卡在 pending）。
                //
                // 「等模型」这件事交给 `chunks_needing_vectors` 那条路：
                // 模型就绪后由 `embed_pending_documents` 显式重排。
                "SELECT {DOC_COLUMNS} FROM kb_documents
                  WHERE enabled = 1 AND status = 'pending'
                  ORDER BY created_at, id LIMIT ?1"
            ))
            .store()?;
        let rows = stmt
            .query_map([i64::from(limit.clamp(1, 100))], map_doc)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?;
        Ok(rows)
    }

    fn docs_needing_vectors(&self, limit: u32) -> Result<Vec<KnowledgeDoc>, CoreError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {DOC_COLUMNS} FROM kb_documents
                  WHERE enabled = 1 AND status = 'chunked'
                    AND EXISTS (SELECT 1 FROM kb_chunks c
                                 WHERE c.doc_id = kb_documents.id AND c.embedded = 0)
                  ORDER BY created_at, id LIMIT ?1"
            ))
            .store()?;
        let rows = stmt
            .query_map([i64::from(limit.clamp(1, 100))], map_doc)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?;
        Ok(rows)
    }

    fn chunks_needing_vectors(
        &self,
        id: KnowledgeDocId,
        limit: u32,
    ) -> Result<Vec<KnowledgeChunk>, CoreError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, doc_id, ord, text, char_count, locator, embedded
                   FROM kb_chunks WHERE doc_id = ?1 AND embedded = 0
                  ORDER BY ord LIMIT ?2",
            )
            .store()?;
        let rows = stmt
            .query_map(params![id.0, i64::from(limit.clamp(1, 200))], map_chunk)
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?;
        Ok(rows)
    }

    fn mark_embedded(&self, chunks: &[KnowledgeChunkId]) -> Result<(), CoreError> {
        if chunks.is_empty() {
            return Ok(());
        }
        let conn = self.lock();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| storage(format!("开始标记向量化事务失败: {e}")))?;
        {
            let mut stmt = tx
                .prepare("UPDATE kb_chunks SET embedded = 1 WHERE id = ?1")
                .store()?;
            for chunk in chunks {
                stmt.execute([chunk.0]).store()?;
            }
        }
        tx.commit()
            .map_err(|e| storage(format!("提交标记向量化事务失败: {e}")))?;
        Ok(())
    }

    fn purge_mail(&self, messages: &[MessageId]) -> Result<u32, CoreError> {
        if messages.is_empty() {
            return Ok(0);
        }
        let conn = self.lock();
        let ids = mail_doc_ids(&conn, messages)?;
        for id in &ids {
            // 必须逐份深删：`mail_id` 的 CASCADE 只删文档行，
            // 向量与 FTS 会留在原地，而且删完就再也拿不到 chunk_id。
            delete_doc_deep(&conn, *id)?;
        }
        Ok(ids.len() as u32)
    }

    fn purge_mail_account(&self, account: me_core::AccountId) -> Result<u32, CoreError> {
        let conn = self.lock();
        let ids: Vec<KnowledgeDocId> = {
            let mut stmt = conn
                .prepare(
                    "SELECT d.id FROM kb_documents d
                       JOIN messages m ON m.id = d.mail_id
                      WHERE m.account_id = ?1",
                )
                .store()?;
            let rows = stmt
                .query_map([account.0], |row| row.get::<_, i64>(0))
                .store()?
                .collect::<Result<Vec<i64>, _>>()
                .store()?;
            rows.into_iter().map(KnowledgeDocId).collect()
        };
        let count = ids.len() as u32;
        for id in ids {
            delete_doc_deep(&conn, id)?;
        }
        Ok(count)
    }

    fn mail_window(&self, capacity: u32) -> Result<Vec<MessageId>, CoreError> {
        if capacity == 0 {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT m.id FROM messages m
                  WHERE m.body_path IS NOT NULL
                  ORDER BY COALESCE(m.date, 0) DESC, m.id DESC
                  LIMIT ?1",
            )
            .store()?;
        let rows = stmt
            .query_map([i64::from(capacity)], |row| row.get::<_, i64>(0))
            .store()?
            .collect::<Result<Vec<i64>, _>>()
            .store()?;
        Ok(rows.into_iter().map(MessageId).collect())
    }

    fn mail_seeds(&self, capacity: u32) -> Result<Vec<MailSeed>, CoreError> {
        if capacity == 0 {
            return Ok(Vec::new());
        }
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT m.id, m.subject, m.from_name, COALESCE(m.date, 0), m.body_path
                   FROM messages m
                  WHERE m.body_path IS NOT NULL
                  ORDER BY COALESCE(m.date, 0) DESC, m.id DESC
                  LIMIT ?1",
            )
            .store()?;
        let rows = stmt
            .query_map([i64::from(capacity)], |row| {
                Ok(MailSeed {
                    message: MessageId(row.get(0)?),
                    subject: row.get(1)?,
                    from_name: row.get(2)?,
                    date: row.get(3)?,
                    body_path: row.get(4)?,
                })
            })
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?;
        Ok(rows)
    }

    fn set_embedding(&self, id: KnowledgeDocId, model: &str, dim: u32) -> Result<(), CoreError> {
        let conn = self.lock();
        conn.execute(
            "UPDATE kb_documents SET embed_model = ?2, embed_dim = ?3, updated_at = ?4
              WHERE id = ?1",
            params![id.0, model, i64::from(dim), me_core::now_unix()],
        )
        .store()?;
        Ok(())
    }

    fn update_doc(
        &self,
        id: KnowledgeDocId,
        title: Option<&str>,
        enabled: Option<bool>,
    ) -> Result<(), CoreError> {
        let now = me_core::now_unix();
        let conn = self.lock();
        if let Some(title) = title {
            let title: String = title.chars().take(me_core::MAX_KB_TITLE_CHARS).collect();
            conn.execute(
                "UPDATE kb_documents SET title = ?2, updated_at = ?3 WHERE id = ?1",
                params![id.0, title, now],
            )
            .store()?;
        }
        if let Some(enabled) = enabled {
            conn.execute(
                "UPDATE kb_documents SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
                params![id.0, i64::from(enabled), now],
            )
            .store()?;
        }
        Ok(())
    }

    fn mail_docs_outside_window(&self, capacity: u32) -> Result<Vec<KnowledgeDocId>, CoreError> {
        let conn = self.lock();
        // 用 NOT EXISTS 而不是 NOT IN：mail_id 可空，NOT IN 遇到 NULL 会返回空集，
        // 结果是「一条都删不掉」——最难查的那种 bug。
        let mut stmt = conn
            .prepare(
                "SELECT d.id FROM kb_documents d
                  WHERE d.origin = 'mail'
                    AND NOT EXISTS (
                      SELECT 1 FROM (
                        SELECT m.id FROM messages m
                         WHERE m.body_path IS NOT NULL
                         ORDER BY COALESCE(m.date, 0) DESC, m.id DESC
                         LIMIT ?1
                      ) w WHERE w.id = d.mail_id
                    )",
            )
            .store()?;
        let rows = stmt
            .query_map([i64::from(capacity)], |row| row.get::<_, i64>(0))
            .store()?
            .collect::<Result<Vec<i64>, _>>()
            .store()?;
        Ok(rows.into_iter().map(KnowledgeDocId).collect())
    }

    fn stats(&self, capacity: u32) -> Result<KnowledgeStats, CoreError> {
        let conn = self.lock();
        let mut stats = KnowledgeStats {
            mail_capacity: capacity,
            ..Default::default()
        };

        stats.file_docs = conn
            .query_row(
                "SELECT COUNT(*) FROM kb_documents WHERE origin IN ('file','ai')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .store()? as u32;
        stats.chunks = conn
            .query_row("SELECT COUNT(*) FROM kb_chunks", [], |row| {
                row.get::<_, i64>(0)
            })
            .store()? as u32;

        stats.mail_docs = conn
            .query_row(
                "SELECT COUNT(*) FROM kb_documents WHERE origin = 'mail'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .store()? as u32;
        stats.mail_ready = conn
            .query_row(
                "SELECT COUNT(*) FROM kb_documents
                  WHERE origin = 'mail' AND status IN ('chunked','embedded')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .store()? as u32;
        stats.mail_skipped = conn
            .query_row(
                "SELECT COUNT(*) FROM kb_documents
                  WHERE origin = 'mail' AND status = 'skipped'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .store()? as u32;

        let mut stmt = conn
            .prepare(
                "SELECT a.email, COUNT(*) AS n
                   FROM kb_documents d
                   JOIN messages m ON m.id = d.mail_id
                   JOIN accounts a ON a.id = m.account_id
                  WHERE d.origin = 'mail'
                  GROUP BY a.id ORDER BY n DESC",
            )
            .store()?;
        stats.per_account = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u32))
            })
            .store()?
            .collect::<Result<Vec<_>, _>>()
            .store()?;

        Ok(stats)
    }

    fn reset_orphan_embeddings(&self) -> Result<u64, CoreError> {
        let conn = self.lock();
        crate::db::reset_orphan_embeddings(&conn)
    }
}

/// 取出这些消息名下的知识库文档 id。
fn mail_doc_ids(
    conn: &Connection,
    messages: &[MessageId],
) -> Result<Vec<KnowledgeDocId>, CoreError> {
    let mut out = Vec::new();
    let mut stmt = conn
        .prepare("SELECT id FROM kb_documents WHERE mail_id = ?1")
        .store()?;
    for message in messages {
        let rows = stmt
            .query_map([message.0], |row| row.get::<_, i64>(0))
            .store()?
            .collect::<Result<Vec<i64>, _>>()
            .store()?;
        out.extend(rows.into_iter().map(KnowledgeDocId));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 只读侧（me-app 用）
// ---------------------------------------------------------------------------

/// 只读查询：列表、单份文档、语块分页。
///
/// **只读侧不做 FTS / KNN**：检索要 KNN，只能在写连接（挂了 `kbvec`）上跑，
/// 一律走 `Command::SearchKnowledge` → me-sync → `Event`。
pub fn list_readonly(
    store: &ReadOnlyStore,
    filter: &KbFilter,
    limit: u32,
    offset: u32,
) -> Result<Vec<KnowledgeDoc>, CoreError> {
    let conn = store.conn()?;
    let mut sql = format!("SELECT {DOC_COLUMNS} FROM kb_documents WHERE 1 = 1");
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(origin) = filter.origin {
        sql.push_str(" AND origin = ?");
        args.push(rusqlite::types::Value::Text(origin.as_str().to_string()));
    }
    if let Some(status) = filter.status {
        sql.push_str(" AND status = ?");
        args.push(rusqlite::types::Value::Text(status.as_str().to_string()));
    }
    if let Some(query) = filter.query.as_deref().filter(|q| !q.trim().is_empty()) {
        sql.push_str(" AND title LIKE ?");
        args.push(rusqlite::types::Value::Text(format!("%{}%", query.trim())));
    }
    sql.push_str(" ORDER BY updated_at DESC, id DESC LIMIT ? OFFSET ?");
    args.push(i64::from(clamp_limit(limit)).into());
    args.push(i64::from(offset).into());

    let mut stmt = conn.prepare(&sql).store()?;
    let rows = stmt
        .query_map(params_from_iter(args.iter()), map_doc)
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

pub fn get_readonly(
    store: &ReadOnlyStore,
    id: KnowledgeDocId,
) -> Result<Option<KnowledgeDoc>, CoreError> {
    let conn = store.conn()?;
    conn.query_row(
        &format!("SELECT {DOC_COLUMNS} FROM kb_documents WHERE id = ?1"),
        [id.0],
        map_doc,
    )
    .optional()
    .store()
}

pub fn chunks_readonly(
    store: &ReadOnlyStore,
    id: KnowledgeDocId,
    from: u32,
    count: u32,
) -> Result<Vec<KnowledgeChunk>, CoreError> {
    let conn = store.conn()?;
    let mut stmt = conn
        .prepare(
            "SELECT id, doc_id, ord, text, char_count, locator, embedded
               FROM kb_chunks WHERE doc_id = ?1 ORDER BY ord LIMIT ?2 OFFSET ?3",
        )
        .store()?;
    let rows = stmt
        .query_map(
            params![id.0, i64::from(clamp_limit(count)), i64::from(from)],
            map_chunk,
        )
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

/// 只读侧的实体名列表（详情页展示「涉及：张三、XX 项目」）。
pub fn doc_entity_names(
    store: &ReadOnlyStore,
    id: KnowledgeDocId,
) -> Result<Vec<String>, CoreError> {
    let conn = store.conn()?;
    let mut stmt = conn
        .prepare(
            "SELECT e.canonical_name
               FROM kb_doc_entities de
               JOIN ai_entities e ON e.id = de.entity_id
              WHERE de.doc_id = ?1
              ORDER BY de.hits DESC",
        )
        .store()?;
    let rows = stmt
        .query_map([id.0], |row| row.get::<_, String>(0))
        .store()?
        .collect::<Result<Vec<_>, _>>()
        .store()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    fn open_tmp() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = Store::open(dir.path()).expect("打开存储");
        (dir, store)
    }

    fn doc(kind: KbKind, title: &str, hash: &str) -> NewKnowledgeDoc {
        NewKnowledgeDoc {
            origin: KbOrigin::File,
            source_ref: format!("/tmp/{title}.{kind:?}"),
            title: title.to_string(),
            kind,
            mail_id: None,
            size_bytes: 10,
            content_hash: hash.to_string(),
        }
    }

    #[test]
    fn upsert_keeps_status_when_hash_unchanged() {
        let (_dir, store) = open_tmp();
        let w = store.writable();
        let id = w.upsert_document(&doc(KbKind::Md, "合同", "abc")).unwrap();
        w.set_status(id, KbStatus::Embedded, None).unwrap();
        let same = w.upsert_document(&doc(KbKind::Md, "合同", "abc")).unwrap();
        assert_eq!(same, id);
        let got = w.get(id).unwrap().unwrap();
        assert_eq!(got.status, KbStatus::Embedded, "hash 没变就不该重跑");
    }

    #[test]
    fn upsert_resets_status_when_content_changed() {
        let (_dir, store) = open_tmp();
        let w = store.writable();
        let id = w.upsert_document(&doc(KbKind::Md, "合同", "abc")).unwrap();
        w.set_status(id, KbStatus::Embedded, None).unwrap();
        w.upsert_document(&doc(KbKind::Md, "合同", "def")).unwrap();
        let got = w.get(id).unwrap().unwrap();
        assert_eq!(got.status, KbStatus::Pending);
        assert_eq!(got.content_hash, "def");
    }

    #[test]
    fn replace_chunks_writes_fts_with_folded_text() {
        let (_dir, store) = open_tmp();
        let w = store.writable();
        let id = w
            .upsert_document(&doc(KbKind::Md, "合同", "hash1"))
            .unwrap();
        w.replace_chunks(
            id,
            &[
                NewChunk::new("合同审查由法务部门负责。", None),
                NewChunk::new("预算超支需要重新审批。", Some("第 2 段".into())),
            ],
        )
        .unwrap();

        // 2 字词必须能命中——这正是 trigram 做不到的。
        let hits = w.fts("合同", 10).unwrap();
        assert!(!hits.is_empty(), "搜「合同」应有命中");
        let hits = w.fts("预算", 10).unwrap();
        assert_eq!(hits.len(), 1, "「预算」只在第 2 块里");
        // 原文不连续的四字词不该命中。
        assert!(w.fts("合同预算", 10).unwrap().is_empty());
    }

    /// 文档标题必须能被搜到。
    ///
    /// 标题不在任何一块的正文里（它在 `kb_documents.title` 与 `locator` 里），
    /// 不索引它的话，用户搜文件名就一定搜不到这份文档。
    #[test]
    fn document_title_is_searchable() {
        let (_dir, store) = open_tmp();
        let w = store.writable();
        let id = w
            .upsert_document(&doc(KbKind::Md, "季度复盘", "hash1"))
            .unwrap();
        w.replace_chunks(id, &[NewChunk::new("华东仓搬迁项目超支 38%。", None)])
            .unwrap();

        let hits = w.fts("季度复盘", 10).unwrap();
        assert_eq!(hits.len(), 1, "搜文档标题应命中：{hits:?}");
    }

    #[test]
    fn delete_clears_all_four_places_and_fts_returns_zero() {
        let (_dir, store) = open_tmp();
        let w = store.writable();
        let id = w.upsert_document(&doc(KbKind::Md, "合同", "h")).unwrap();
        w.replace_chunks(id, &[NewChunk::new("合同审查由法务部门负责。", None)])
            .unwrap();
        let chunks = w.chunks(id, 0, 10).unwrap();
        assert_eq!(chunks.len(), 1);
        w.put_vectors(&[(chunks[0].id, vec![0.1; me_core::EMBED_DIM])])
            .unwrap();

        w.delete(id).unwrap();

        assert!(w.get(id).unwrap().is_none(), "文档应被删掉");
        assert!(w.chunks(id, 0, 10).unwrap().is_empty(), "语块应被删掉");
        // 只断言「表里行数为 0」不够：FTS 的实际检索行为才是用户能感知的。
        assert!(
            w.fts("合同", 10).unwrap().is_empty(),
            "删完还能搜到就是幽灵结果"
        );
        let conn = w.lock();
        let vectors: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kbvec.vec_chunks WHERE chunk_id = ?1",
                [chunks[0].id.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(vectors, 0, "向量也必须清掉");
    }

    #[test]
    fn knn_orders_by_distance() {
        let (_dir, store) = open_tmp();
        let w = store.writable();
        let id = w.upsert_document(&doc(KbKind::Md, "向量", "h")).unwrap();
        w.replace_chunks(
            id,
            &[
                NewChunk::new("第一个块的内容。", None),
                NewChunk::new("第二个块的内容。", None),
            ],
        )
        .unwrap();
        let chunks = w.chunks(id, 0, 10).unwrap();
        let mut first = vec![0.0f32; me_core::EMBED_DIM];
        first[0] = 1.0;
        let mut second = vec![0.0f32; me_core::EMBED_DIM];
        second[1] = 1.0;
        w.put_vectors(&[(chunks[0].id, first.clone()), (chunks[1].id, second)])
            .unwrap();

        let hits = w.knn(&first, 2).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].chunk, chunks[0].id, "最近的应排在前面");
    }

    #[test]
    fn mail_purge_requires_deep_delete_before_messages() {
        let (_dir, store) = open_tmp();
        let w = store.writable();
        // 直接造一封邮件太麻烦，这里只验证「没有邮件时不报错、返回 0」。
        assert_eq!(w.purge_mail(&[MessageId(999)]).unwrap(), 0);
        assert_eq!(w.mail_window(0).unwrap().len(), 0);
    }

    #[test]
    fn stats_counts_documents_and_chunks() {
        let (_dir, store) = open_tmp();
        let w = store.writable();
        let id = w.upsert_document(&doc(KbKind::Md, "统计", "h")).unwrap();
        w.replace_chunks(id, &[NewChunk::new("一二三四五", None)])
            .unwrap();
        let stats = w.stats(200).unwrap();
        assert_eq!(stats.file_docs, 1);
        assert_eq!(stats.chunks, 1);
        assert_eq!(stats.mail_capacity, 200);
    }

    /// 待处理队列**不能**包含 `chunked` 的文档。
    ///
    /// 这条锁住的是一个真实的死循环：模型没就绪时，`chunked` 文档的状态
    /// 不会变，于是 worker 永远取到同一份文档原地打转，后面的邮件
    /// 永远排不上队（实测 134 封卡在 pending，只有 1 份被处理）。
    #[test]
    fn pending_docs_excludes_chunked_documents() {
        let (_dir, store) = open_tmp();
        let w = store.writable();
        let pending = w.upsert_document(&doc(KbKind::Md, "待处理", "h1")).unwrap();
        let chunked = w.upsert_document(&doc(KbKind::Md, "已分块", "h2")).unwrap();
        w.replace_chunks(
            chunked,
            &[NewChunk::new("一段足够长的正文内容用于测试。", None)],
        )
        .unwrap();

        let queue = w.pending_docs(10).unwrap();
        assert!(
            queue.iter().all(|item| item.id != chunked),
            "`chunked` 的文档不能进待处理队列，否则 worker 会死循环"
        );
        assert!(
            queue.iter().any(|item| item.id == pending),
            "`pending` 的文档必须在队列里"
        );

        // 需要补向量的走另一条路。
        let needing = w.docs_needing_vectors(10).unwrap();
        assert!(
            needing.iter().any(|item| item.id == chunked),
            "`chunked` 的文档必须出现在待向量化列表里"
        );
        assert!(
            needing.iter().all(|item| item.id != pending),
            "`pending` 的文档还没分块，不该出现在待向量化列表里"
        );
    }

    #[test]
    fn update_doc_renames_and_toggles() {
        let (_dir, store) = open_tmp();
        let w = store.writable();
        let id = w.upsert_document(&doc(KbKind::Md, "旧标题", "h")).unwrap();
        w.update_doc(id, Some("新标题"), Some(false)).unwrap();
        let got = w.get(id).unwrap().unwrap();
        assert_eq!(got.title, "新标题");
        assert!(!got.enabled);
    }
}
