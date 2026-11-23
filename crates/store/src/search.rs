//! Search pipeline: empty -> list, short -> LIKE, otherwise FTS5 plus a title fuzzy pass.

use std::collections::HashMap;

use aw_core::{SessionId, WorkspaceId};
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use rusqlite::{params, Connection};

use crate::reader::{card_from_row, list_sessions, CARD_COLS};
use crate::{paginate, Page, SessionCard, StoreError};

const MIN_FTS_CHARS: usize = 3;
const FTS_CANDIDATE_CAP: i64 = 500;
/// Minimum nucleo score per query character for a fuzzy-only title hit.
const FUZZY_MIN_SCORE_PER_CHAR: u32 = 14;

pub(crate) fn search(
    conn: &Connection,
    scope: Option<WorkspaceId>,
    query: &str,
    page: u32,
    page_size: u32,
) -> Result<Page<SessionCard>, StoreError> {
    let q = query.trim();
    if q.is_empty() {
        return list_sessions(conn, scope, page, page_size);
    }
    let scope = scope.map(|w| w.0.to_string());
    let cards = if q.chars().count() < MIN_FTS_CHARS {
        like_search(conn, scope.as_deref(), q)?
    } else {
        ranked_search(conn, scope.as_deref(), q)?
    };
    Ok(paginate(cards, page, page_size))
}

fn scoped_cards(conn: &Connection, scope: Option<&str>) -> Result<Vec<SessionCard>, StoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CARD_COLS} FROM sessions WHERE (?1 IS NULL OR workspace_id = ?1)
         ORDER BY updated_at DESC, id"
    ))?;
    let rows = stmt.query_map([scope], card_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn like_search(
    conn: &Connection,
    scope: Option<&str>,
    q: &str,
) -> Result<Vec<SessionCard>, StoreError> {
    let escaped = q
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let mut stmt = conn.prepare(&format!(
        "SELECT {CARD_COLS} FROM sessions
         WHERE (?1 IS NULL OR workspace_id = ?1)
           AND (title LIKE ?2 ESCAPE '\\' OR preview_snippet LIKE ?2 ESCAPE '\\')
         ORDER BY updated_at DESC, id"
    ))?;
    let rows = stmt.query_map(params![scope, format!("%{escaped}%")], card_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Quotes `q` as an FTS5 string literal so operators and punctuation are plain text.
fn fts_literal(q: &str) -> String {
    format!("\"{}\"", q.replace('"', "\"\""))
}

struct FtsHit {
    bm25: f64,
    excerpt: Option<String>,
}

fn fts_hits(
    conn: &Connection,
    scope: Option<&str>,
    q: &str,
) -> Result<HashMap<SessionId, FtsHit>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT search_index.session_id, search_index.source,
                snippet(search_index, 0, '', '', '…', 12), bm25(search_index)
         FROM search_index
         JOIN sessions s ON s.id = search_index.session_id
         WHERE search_index MATCH ?1 AND (?2 IS NULL OR s.workspace_id = ?2)
         ORDER BY bm25(search_index)
         LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![fts_literal(q), scope, FTS_CANDIDATE_CAP], |r| {
        Ok((
            crate::schema::session_id(0, &r.get::<_, String>(0)?)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, f64>(3)?,
        ))
    })?;
    // Rows arrive best-first: the first row per session sets its score, and the
    // first message row supplies the excerpt.
    let mut hits: HashMap<SessionId, FtsHit> = HashMap::new();
    for row in rows {
        let (id, source, snippet, bm25) = row?;
        let hit = hits.entry(id).or_insert(FtsHit {
            bm25,
            excerpt: None,
        });
        if source == "msg" && hit.excerpt.is_none() {
            hit.excerpt = Some(snippet);
        }
    }
    Ok(hits)
}

fn fuzzy_scores(cards: &[SessionCard], q: &str) -> HashMap<SessionId, u32> {
    let pattern = Pattern::new(
        q,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut matcher = Matcher::new(Config::DEFAULT);
    let min = FUZZY_MIN_SCORE_PER_CHAR * q.chars().filter(|c| !c.is_whitespace()).count() as u32;
    let mut buf = Vec::new();
    cards
        .iter()
        .filter_map(|c| {
            let score = pattern.score(Utf32Str::new(&c.title, &mut buf), &mut matcher)?;
            (score >= min).then_some((c.id, score))
        })
        .collect()
}

/// Ranking: exact title substring, then FTS by bm25, then fuzzy-only by score;
/// ties go to the most recently updated.
fn ranked_search(
    conn: &Connection,
    scope: Option<&str>,
    q: &str,
) -> Result<Vec<SessionCard>, StoreError> {
    let cards = scoped_cards(conn, scope)?;
    let mut fts = fts_hits(conn, scope, q)?;
    let fuzzy = fuzzy_scores(&cards, q);
    let needle = q.to_lowercase();

    let mut ranked: Vec<(u8, f64, SessionCard)> = Vec::new();
    for mut card in cards {
        let hit = fts.remove(&card.id);
        let exact = card.title.to_lowercase().contains(&needle);
        let (tier, key) = match (&hit, fuzzy.get(&card.id)) {
            _ if exact => (0, 0.0),
            (Some(h), _) => (1, h.bm25),
            (None, Some(score)) => (2, -f64::from(*score)),
            (None, None) => continue,
        };
        card.excerpt = hit.and_then(|h| h.excerpt);
        ranked.push((tier, key, card));
    }
    ranked.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.total_cmp(&b.1))
            .then(b.2.updated_at.cmp(&a.2.updated_at))
    });
    Ok(ranked.into_iter().map(|(_, _, c)| c).collect())
}
