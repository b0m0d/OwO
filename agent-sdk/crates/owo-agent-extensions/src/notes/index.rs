//! 全文索引：内存分词 + SQLite FTS5（零新增依赖，从 notes/mod.rs 拆出）。

use super::*;
use std::collections::BTreeMap;
use std::path::Path;

// ----------------------------------------------------------------------------
// 全文索引：内存分词 + SQLite FTS5（零新增依赖）
// ----------------------------------------------------------------------------

/// 检索命中。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub doc_id: String,
    pub block_id: BlockId,
    /// 命中词（FTS 片段或内存命中的原词）。
    pub snippet: String,
}

/// 从块提取可检索/可展示文本（递归含子块；供索引、渲染器、测试使用）。
pub fn block_text(doc: &NoteDoc, block: &Block) -> String {
    let mut text = String::new();
    match &block.kind {
        BlockKind::Paragraph { text: t } => text.push_str(t),
        BlockKind::Heading { text: t, .. } => text.push_str(t),
        BlockKind::ListItem { text: t } => text.push_str(t),
        BlockKind::Code { text: t, .. } => text.push_str(t),
        BlockKind::Table { rows } => {
            for row in rows {
                text.push_str(&row.join(" "));
            }
        }
        BlockKind::Image { alt, .. } => text.push_str(alt),
        BlockKind::File { path, .. } => text.push_str(path),
        BlockKind::Quote { text: t } => text.push_str(t),
        BlockKind::HtmlEmbed { html } => text.push_str(html),
        BlockKind::Canvas { data } => {
            for note in &data.notes {
                text.push_str(&note.text);
            }
        }
        BlockKind::AiGenerated {
            prompt, text: t, ..
        } => {
            text.push_str(prompt);
            text.push(' ');
            text.push_str(t);
        }
        BlockKind::List { .. } => {}
    }
    // 递归子块
    for child_id in &block.children {
        if let Some(child) = doc.blocks.get(child_id) {
            text.push(' ');
            text.push_str(&block_text(doc, child));
        }
    }
    text
}

/// 简单分词：按非字母数字切分 + 小写。
fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.push(ch.to_lowercase().next().unwrap_or(ch));
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// 索引 trait：内存实现与 FTS5 实现共用。
pub trait NoteIndex: Send {
    /// 全量重建文档索引（幂等）。
    fn index_doc(&mut self, doc: &NoteDoc) -> Result<(), String>;
    /// 检索，返回命中的块（按文档内顺序）。
    fn search(&self, query: &str) -> Vec<SearchHit>;
}

/// 内存分词索引（简单、无文件依赖）。
#[derive(Default)]
pub struct InMemoryNoteIndex {
    /// 词 → (doc_id, block_id, snippet)
    map: BTreeMap<String, Vec<(String, String, String)>>,
}

impl InMemoryNoteIndex {
    pub fn new() -> Self {
        Self::default()
    }
}

impl NoteIndex for InMemoryNoteIndex {
    fn index_doc(&mut self, doc: &NoteDoc) -> Result<(), String> {
        self.map.clear();
        for block in walk(doc, &doc.root) {
            let text = block_text(doc, block);
            if text.trim().is_empty() {
                continue;
            }
            let snippet: String = text.chars().take(60).collect();
            for word in tokenize(&text) {
                if word.len() < 2 {
                    continue;
                }
                self.map.entry(word).or_default().push((
                    doc.id.clone(),
                    block.id.clone(),
                    snippet.clone(),
                ));
            }
        }
        Ok(())
    }

    fn search(&self, query: &str) -> Vec<SearchHit> {
        let words: Vec<String> = tokenize(query)
            .into_iter()
            .filter(|w| w.len() >= 2)
            .collect();
        if words.is_empty() {
            return Vec::new();
        }
        let mut hits = Vec::new();
        for (word, entries) in &self.map {
            if words
                .iter()
                .any(|w| word.starts_with(w) || w.starts_with(word))
            {
                for (doc_id, block_id, snippet) in entries {
                    hits.push(SearchHit {
                        doc_id: doc_id.clone(),
                        block_id: block_id.clone(),
                        snippet: snippet.clone(),
                    });
                }
            }
        }
        hits.sort_by(|a, b| a.doc_id.cmp(&b.doc_id).then(a.block_id.cmp(&b.block_id)));
        hits.dedup();
        hits
    }
}

/// SQLite FTS5 索引（`<db_path>` 单文件；重建时清空重插）。
/// tokenizer 用 trigram：对中文/无空格语言的子串检索友好；<3 字符查询回退 LIKE。
pub struct FtsNoteIndex {
    conn: std::sync::Mutex<rusqlite::Connection>,
}

impl FtsNoteIndex {
    pub fn open(db_path: &Path) -> Result<Self, String> {
        let conn = rusqlite::Connection::open(db_path).map_err(|e| e.to_string())?;
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS note_fts USING fts5(doc_id, block_id UNINDEXED, text, tokenize='trigram');",
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            conn: std::sync::Mutex::new(conn),
        })
    }
}

impl NoteIndex for FtsNoteIndex {
    fn index_doc(&mut self, doc: &NoteDoc) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("DELETE FROM note_fts;")
            .map_err(|e| e.to_string())?;
        for block in walk(doc, &doc.root) {
            let text = block_text(doc, block);
            if text.trim().is_empty() {
                continue;
            }
            conn.execute(
                "INSERT INTO note_fts(doc_id, block_id, text) VALUES (?1, ?2, ?3)",
                rusqlite::params![doc.id, block.id, text],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn search(&self, query: &str) -> Vec<SearchHit> {
        let query = query.trim();
        if query.is_empty() {
            return Vec::new();
        }
        let conn = self.conn.lock().unwrap();
        // 短查询（trigram 要求 ≥3 字符）：LIKE 回退
        let chars: usize = query.chars().count();
        if chars < 3 {
            let like = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
            let mut stmt = match conn.prepare(
                "SELECT doc_id, block_id, substr(text, 1, 60) FROM note_fts WHERE text LIKE ?1 ESCAPE '\\'",
            ) {
                Ok(stmt) => stmt,
                Err(_) => return Vec::new(),
            };
            let rows = stmt.query_map(rusqlite::params![like], |row| {
                Ok(SearchHit {
                    doc_id: row.get(0)?,
                    block_id: row.get(1)?,
                    snippet: row.get(2)?,
                })
            });
            return match rows {
                Ok(rows) => rows.flatten().collect(),
                Err(_) => Vec::new(),
            };
        }
        // 查询词转 FTS 短语（trigram 子串匹配）
        let phrase = format!("\"{}\"", query.replace('"', "\"\""));
        let mut stmt = match conn.prepare(
            "SELECT doc_id, block_id, snippet(note_fts, 2, '[', ']', '...', 12) FROM note_fts WHERE note_fts MATCH ?1",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(rusqlite::params![phrase], |row| {
            Ok(SearchHit {
                doc_id: row.get(0)?,
                block_id: row.get(1)?,
                snippet: row.get(2)?,
            })
        });
        match rows {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// 便捷入口：索引器（内存为主，可选 FTS）。
pub struct NoteIndexer {
    inner: Box<dyn NoteIndex>,
}

impl NoteIndexer {
    pub fn in_memory() -> Self {
        Self {
            inner: Box::new(InMemoryNoteIndex::new()),
        }
    }

    pub fn fts(db_path: &Path) -> Result<Self, String> {
        Ok(Self {
            inner: Box::new(FtsNoteIndex::open(db_path)?),
        })
    }

    pub fn index_doc(&mut self, doc: &NoteDoc) -> Result<(), String> {
        self.inner.index_doc(doc)
    }
}

impl NoteIndex for NoteIndexer {
    fn index_doc(&mut self, doc: &NoteDoc) -> Result<(), String> {
        self.inner.index_doc(doc)
    }

    fn search(&self, query: &str) -> Vec<SearchHit> {
        self.inner.search(query)
    }
}

/// 便捷函数：内存索引检索。
pub fn search_notes(index: &NoteIndexer, query: &str) -> Vec<SearchHit> {
    index.search(query)
}
