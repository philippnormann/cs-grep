//! SQLite storage with an FTS5 full-text index over papers.

use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::{params_from_iter, types::Value, Connection, OpenFlags, OptionalExtension, Row};

use crate::config::RankSortOrder;
use crate::query::{FilterExpr, YearRange};
use crate::{Paper, Result};

// shortcut: user_version holds ENRICH_VERSION; move both into a small meta table when a
// non-additive schema change is needed.
const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS papers (
    id         INTEGER PRIMARY KEY,
    dblp_key   TEXT UNIQUE NOT NULL,
    venue      TEXT NOT NULL,
    year       INTEGER NOT NULL,
    title      TEXT NOT NULL,
    authors    TEXT NOT NULL,
    doi        TEXT,
    url        TEXT,
    abstract   TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_papers_venue_year ON papers(venue, year);
CREATE INDEX IF NOT EXISTS idx_papers_year_venue ON papers(year DESC, venue ASC, id);

CREATE VIRTUAL TABLE IF NOT EXISTS papers_fts USING fts5(
    title, authors, abstract,
    content = 'papers',
    content_rowid = 'id',
    tokenize = 'porter unicode61'
);

CREATE TRIGGER IF NOT EXISTS papers_ai AFTER INSERT ON papers BEGIN
    INSERT INTO papers_fts(rowid, title, authors, abstract)
    VALUES (new.id, new.title, new.authors, new.abstract);
END;

CREATE TRIGGER IF NOT EXISTS papers_ad AFTER DELETE ON papers BEGIN
    INSERT INTO papers_fts(papers_fts, rowid, title, authors, abstract)
    VALUES ('delete', old.id, old.title, old.authors, old.abstract);
END;

CREATE TRIGGER IF NOT EXISTS papers_au AFTER UPDATE ON papers BEGIN
    INSERT INTO papers_fts(papers_fts, rowid, title, authors, abstract)
    VALUES ('delete', old.id, old.title, old.authors, old.abstract);
    INSERT INTO papers_fts(rowid, title, authors, abstract)
    VALUES (new.id, new.title, new.authors, new.abstract);
END;

-- Papers no abstract source had.
CREATE TABLE IF NOT EXISTS abstract_misses (
    paper_id   INTEGER PRIMARY KEY REFERENCES papers(id) ON DELETE CASCADE,
    checked_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TRIGGER IF NOT EXISTS abstract_misses_stale AFTER UPDATE OF title, doi, url ON papers
WHEN old.title IS NOT new.title OR old.doi IS NOT new.doi OR old.url IS NOT new.url BEGIN
    DELETE FROM abstract_misses WHERE paper_id = new.id;
END;
"#;

const PAPER_COLUMNS_WITH_ALIAS: &str =
    "p.dblp_key, p.venue, p.year, p.title, p.authors, p.doi, p.url, p.abstract";

/// How to order search results.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Sort {
    /// BM25 relevance for full-text searches; otherwise year descending.
    #[default]
    Relevance,
    Year,
    Venue,
    /// Venue groups in priority order; venues outside the groups sort last.
    Rank(RankSortOrder),
}

/// A compiled search request.
#[derive(Debug, Clone, Default)]
pub struct Search {
    pub fts: Option<String>,
    pub filter: Option<FilterExpr>,
    pub sort: Sort,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

pub struct Database {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct MissingPaper {
    pub id: i64,
    pub paper: Paper,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_existing(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        Self::init(conn)
    }

    pub fn open_read_only(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        // Fail now, not on the first search, if this isn't a cs-grep database.
        conn.prepare(&format!(
            "SELECT {PAPER_COLUMNS_WITH_ALIAS} FROM papers_fts f JOIN papers p ON p.id = f.rowid \
             WHERE papers_fts MATCH ?1 LIMIT 0"
        ))?;
        Ok(Self { conn })
    }

    /// Interrupt statements that are still running after `limit`.
    pub fn set_time_limit(&self, limit: Duration) -> Result<()> {
        let deadline = Instant::now() + limit;
        self.conn
            .progress_handler(1000, Some(move || Instant::now() >= deadline))?;
        Ok(())
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM papers", [], |r| r.get(0))?)
    }

    /// Insert or update papers keyed by `dblp_key`. Returns rows affected.
    pub fn upsert_papers(&mut self, papers: &[Paper]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                r#"
                INSERT INTO papers (dblp_key, venue, year, title, authors, doi, url, abstract, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, datetime('now'))
                ON CONFLICT(dblp_key) DO UPDATE SET
                    venue = excluded.venue,
                    year = excluded.year,
                    title = excluded.title,
                    authors = excluded.authors,
                    doi = excluded.doi,
                    url = excluded.url,
                    abstract = COALESCE(excluded.abstract, papers.abstract),
                    updated_at = datetime('now')
                WHERE
                    papers.venue IS NOT excluded.venue OR
                    papers.year IS NOT excluded.year OR
                    papers.title IS NOT excluded.title OR
                    papers.authors IS NOT excluded.authors OR
                    papers.doi IS NOT excluded.doi OR
                    papers.url IS NOT excluded.url OR
                    (excluded.abstract IS NOT NULL AND papers.abstract IS NOT excluded.abstract)
                "#,
            )?;
            for p in papers {
                n += stmt.execute(rusqlite::params![
                    p.dblp_key,
                    p.venue,
                    p.year,
                    p.title,
                    p.authors,
                    p.doi,
                    p.url,
                    p.abstract_text,
                ])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn set_abstracts(&mut self, abstracts: &[(String, String)]) -> Result<()> {
        if abstracts.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE papers SET abstract = ?2, updated_at = datetime('now') WHERE dblp_key = ?1",
            )?;
            for (dblp_key, abstract_text) in abstracts {
                stmt.execute(rusqlite::params![dblp_key, abstract_text])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Remember papers no source had an abstract for, by dblp_key.
    pub fn record_abstract_misses(&mut self, keys: &[String]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO abstract_misses (paper_id) \
                 SELECT id FROM papers WHERE dblp_key = ?1",
            )?;
            for dblp_key in keys {
                stmt.execute([dblp_key])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The first time `version` runs on this database: re-check every stored
    /// abstract with `clean` (`None` clears it) and forget all remembered misses.
    /// Returns the number of changed abstracts, or `None` if `version` already ran.
    pub fn begin_enrich_version(
        &mut self,
        version: i32,
        clean: impl Fn(String, &str) -> Option<String>,
    ) -> Result<Option<usize>> {
        let tx = self.conn.transaction()?;
        let current: i32 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if current >= version {
            return Ok(None);
        }
        let mut changed = Vec::new();
        {
            let mut stmt =
                tx.prepare("SELECT id, title, abstract FROM papers WHERE abstract <> ''")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let old: String = row.get(2)?;
                let new = clean(old.clone(), &row.get::<_, String>(1)?);
                if new.as_ref() != Some(&old) {
                    changed.push((row.get::<_, i64>(0)?, new));
                }
            }
            let mut stmt = tx.prepare(
                "UPDATE papers SET abstract = ?2, updated_at = datetime('now') WHERE id = ?1",
            )?;
            for (id, abstract_text) in &changed {
                stmt.execute(rusqlite::params![id, abstract_text])?;
            }
        }
        tx.execute("DELETE FROM abstract_misses", [])?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        Ok(Some(changed.len()))
    }

    pub fn papers_missing_abstract_batch(
        &self,
        venues: &[String],
        years: &[YearRange],
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<MissingPaper>> {
        let mut parts = missing_abstract_parts(venues, years, Some(after_id));
        let next = parts.args.len() + 1;
        let sql = format!(
            "SELECT p.id, {PAPER_COLUMNS_WITH_ALIAS} FROM papers p {} \
             ORDER BY p.id ASC LIMIT ?{next}",
            parts.where_sql
        );
        parts.args.push((limit as i64).into());
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(parts.args.iter()), row_to_missing_paper)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    pub fn count_missing_abstracts(&self, venues: &[String], years: &[YearRange]) -> Result<usize> {
        let parts = missing_abstract_parts(venues, years, None);
        let sql = format!("SELECT COUNT(*) FROM papers p {}", parts.where_sql);
        let count: i64 = self
            .conn
            .query_row(&sql, params_from_iter(parts.args.iter()), |r| r.get(0))?;
        Ok(count as usize)
    }

    pub fn search(&self, q: &Search) -> Result<Vec<Paper>> {
        let mut parts = search_query_parts(q);
        let order = order_clause(q, &mut parts.args);
        // bm25() needs the FTS table in the outer query, so relevance pages sort in place.
        let id_page = q.limit.is_some() && !(q.fts.is_some() && matches!(q.sort, Sort::Relevance));
        let columns = if id_page {
            "p.id"
        } else {
            PAPER_COLUMNS_WITH_ALIAS
        };

        let mut sql = format!(
            "SELECT {columns} FROM {} {} {order}",
            parts.from, parts.where_sql
        );
        if let Some(n) = q.limit {
            let next = parts.args.len() + 1;
            sql.push_str(&format!(" LIMIT ?{next}"));
            parts.args.push((n as i64).into());
        }
        if let Some(n) = q.offset {
            if q.limit.is_none() {
                sql.push_str(" LIMIT -1");
            }
            let next = parts.args.len() + 1;
            sql.push_str(&format!(" OFFSET ?{next}"));
            parts.args.push((n as i64).into());
        }
        if id_page {
            // Sort and page ids first so abstracts stay out of the sort.
            sql = format!(
                "SELECT {PAPER_COLUMNS_WITH_ALIAS} FROM papers p WHERE p.id IN ({sql}) {order}"
            );
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(parts.args.iter()), row_to_paper)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    pub fn search_count(&self, q: &Search) -> Result<usize> {
        let parts = search_query_parts(q);
        let sql = format!("SELECT COUNT(*) FROM {} {}", parts.from, parts.where_sql);
        let count: i64 = self
            .conn
            .query_row(&sql, params_from_iter(parts.args.iter()), |r| r.get(0))?;
        Ok(count as usize)
    }

    /// Look up a single paper by its DBLP key.
    pub fn get_by_key(&self, dblp_key: &str) -> Result<Option<Paper>> {
        Ok(self
            .conn
            .query_row(
                "SELECT dblp_key, venue, year, title, authors, doi, url, abstract \
                 FROM papers WHERE dblp_key = ?1",
                [dblp_key],
                row_to_paper,
            )
            .optional()?)
    }
}

fn in_clause(column: &str, start: usize, value_count: usize) -> String {
    format!("{column} IN ({})", placeholders(start, value_count))
}

fn placeholders(start: usize, count: usize) -> String {
    (start..start + count)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn append_string_args(args: &mut Vec<Value>, values: &[String]) {
    args.extend(values.iter().cloned().map(Value::from));
}

fn year_ranges_clause(column: &str, ranges: &[YearRange], args: &mut Vec<Value>) -> String {
    let mut next = args.len() + 1;
    let clauses = ranges
        .iter()
        .map(|range| match range.bounds() {
            (Some(min), Some(max)) if min == max => {
                let placeholder = next;
                next += 1;
                args.push((min as i64).into());
                format!("{column} = ?{placeholder}")
            }
            (Some(min), Some(max)) => {
                let min_placeholder = next;
                let max_placeholder = next + 1;
                next += 2;
                args.push((min as i64).into());
                args.push((max as i64).into());
                format!("({column} >= ?{min_placeholder} AND {column} <= ?{max_placeholder})")
            }
            (Some(min), None) => {
                let placeholder = next;
                next += 1;
                args.push((min as i64).into());
                format!("{column} >= ?{placeholder}")
            }
            (None, Some(max)) => {
                let placeholder = next;
                next += 1;
                args.push((max as i64).into());
                format!("{column} <= ?{placeholder}")
            }
            (None, None) => unreachable!("year parser rejects empty ranges"),
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    format!("({clauses})")
}

fn order_clause(q: &Search, args: &mut Vec<Value>) -> String {
    let order = match &q.sort {
        Sort::Relevance if q.fts.is_some() => "ORDER BY bm25(papers_fts), p.year DESC".to_string(),
        Sort::Venue => "ORDER BY p.venue ASC, p.year DESC".to_string(),
        Sort::Rank(order) if order.iter().any(|venues| !venues.is_empty()) => {
            let rank_expr = rank_groups_clause("p.venue", args.len() + 1, order);
            for venues in order {
                append_string_args(args, venues);
            }
            format!("ORDER BY {rank_expr}, p.year DESC, p.venue ASC")
        }
        Sort::Relevance | Sort::Year | Sort::Rank(_) => {
            "ORDER BY p.year DESC, p.venue ASC".to_string()
        }
    };
    // Tie-break on id so pages neither repeat nor skip rows.
    format!("{order}, p.id ASC")
}

fn rank_groups_clause(column: &str, start: usize, groups: &[Vec<String>]) -> String {
    let mut next = start;
    let mut clauses = Vec::new();
    for (rank_index, venues) in groups.iter().enumerate() {
        if venues.is_empty() {
            continue;
        }
        let clause = in_clause(column, next, venues.len());
        next += venues.len();
        clauses.push(format!("WHEN {clause} THEN {rank_index}"));
    }
    format!("CASE {} ELSE {} END", clauses.join(" "), groups.len())
}

struct SearchQueryParts {
    from: String,
    where_sql: String,
    args: Vec<Value>,
}

fn search_query_parts(q: &Search) -> SearchQueryParts {
    let mut args: Vec<Value> = Vec::new();
    let mut where_clauses: Vec<String> = Vec::new();

    let from = if let Some(fts) = &q.fts {
        args.push(fts.clone().into());
        where_clauses.push("papers_fts MATCH ?1".to_string());
        "papers_fts f JOIN papers p ON p.id = f.rowid".to_string()
    } else {
        "papers p".to_string()
    };

    if let Some(filter) = &q.filter {
        where_clauses.push(filter_clause(filter, &mut args));
    }

    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", where_clauses.join(" AND "))
    };

    SearchQueryParts {
        from,
        where_sql,
        args,
    }
}

fn filter_clause(filter: &FilterExpr, args: &mut Vec<Value>) -> String {
    match filter {
        FilterExpr::Venue(venues) => {
            if venues.is_empty() {
                return "0".to_string();
            }
            let clause = in_clause("p.venue", args.len() + 1, venues.len());
            append_string_args(args, venues);
            clause
        }
        FilterExpr::Year(range) => year_ranges_clause("p.year", std::slice::from_ref(range), args),
        FilterExpr::Doi(term) => {
            let next = args.len() + 1;
            args.push(format!("%{}%", escape_like(term)).into());
            format!("(COALESCE(p.doi, '') LIKE ?{next} ESCAPE '\\')")
        }
        FilterExpr::And(filters) => boolean_filter_clause("AND", filters, args),
        FilterExpr::Or(filters) => boolean_filter_clause("OR", filters, args),
        FilterExpr::Not(filter) => format!("NOT ({})", filter_clause(filter, args)),
    }
}

fn boolean_filter_clause(operator: &str, filters: &[FilterExpr], args: &mut Vec<Value>) -> String {
    let clauses = filters
        .iter()
        .map(|filter| filter_clause(filter, args))
        .collect::<Vec<_>>();
    format!("({})", clauses.join(&format!(" {operator} ")))
}

fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

fn missing_abstract_parts(
    venues: &[String],
    years: &[YearRange],
    after_id: Option<i64>,
) -> SearchQueryParts {
    let mut args: Vec<Value> = Vec::new();
    let mut where_clauses = vec![
        "(p.abstract IS NULL OR p.abstract = '')".to_string(),
        // Recheck misses weekly for this year's papers, monthly for last year's, else twice a year.
        "NOT EXISTS (SELECT 1 FROM abstract_misses m WHERE m.paper_id = p.id \
         AND m.checked_at > datetime('now', CASE \
         WHEN p.year >= CAST(strftime('%Y', 'now') AS INTEGER) THEN '-7 days' \
         WHEN p.year = CAST(strftime('%Y', 'now') AS INTEGER) - 1 THEN '-30 days' \
         ELSE '-180 days' END))"
            .to_string(),
    ];
    if !venues.is_empty() {
        where_clauses.push(in_clause("p.venue", args.len() + 1, venues.len()));
        append_string_args(&mut args, venues);
    }
    if !years.is_empty() {
        where_clauses.push(year_ranges_clause("p.year", years, &mut args));
    }
    if let Some(after_id) = after_id {
        let next = args.len() + 1;
        where_clauses.push(format!("p.id > ?{next}"));
        args.push(after_id.into());
    }

    SearchQueryParts {
        from: String::new(),
        where_sql: format!("WHERE {}", where_clauses.join(" AND ")),
        args,
    }
}

fn row_to_paper(row: &Row) -> rusqlite::Result<Paper> {
    Ok(Paper {
        dblp_key: row.get(0)?,
        venue: row.get(1)?,
        year: row.get(2)?,
        title: row.get(3)?,
        authors: row.get(4)?,
        doi: row.get(5)?,
        url: row.get(6)?,
        abstract_text: row.get(7)?,
    })
}

fn row_to_missing_paper(row: &Row) -> rusqlite::Result<MissingPaper> {
    Ok(MissingPaper {
        id: row.get(0)?,
        paper: Paper {
            dblp_key: row.get(1)?,
            venue: row.get(2)?,
            year: row.get(3)?,
            title: row.get(4)?,
            authors: row.get(5)?,
            doi: row.get(6)?,
            url: row.get(7)?,
            abstract_text: row.get(8)?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paper(key: &str, venue: &str, year: i32, title: &str, abs: Option<&str>) -> Paper {
        Paper {
            dblp_key: key.into(),
            venue: venue.into(),
            year,
            title: title.into(),
            authors: "Alice Smith, Bob Jones".into(),
            doi: Some("10.1/x".into()),
            url: Some("https://example.com".into()),
            abstract_text: abs.map(|s| s.into()),
        }
    }

    fn seeded() -> Database {
        let mut db = Database::open_in_memory().unwrap();
        db.upsert_papers(&[
            paper(
                "k1",
                "NDSS",
                2020,
                "Fuzzing the Linux kernel",
                Some("we fuzz kernels"),
            ),
            paper("k2", "CCS", 2021, "Side channel attacks on caches", None),
            paper(
                "k3",
                "SP",
                2019,
                "Kernel exploitation techniques",
                Some("rop chains"),
            ),
        ])
        .unwrap();
        db
    }

    fn paper_by_key(db: &Database, key: &str) -> Paper {
        db.search(&Search::default())
            .unwrap()
            .into_iter()
            .find(|paper| paper.dblp_key == key)
            .unwrap()
    }

    #[test]
    fn read_only_database_requires_valid_schema_and_rejects_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("papers.db");
        let original = paper("k1", "NDSS", 2026, "Kernel", Some("Original abstract"));
        {
            let mut db = Database::open(&path).unwrap();
            db.upsert_papers(std::slice::from_ref(&original)).unwrap();
        }
        let mut db = Database::open_read_only(&path).unwrap();
        assert_eq!(
            db.search(&Search::default()).unwrap(),
            vec![original.clone()]
        );
        assert!(db
            .set_abstracts(&[("k1".into(), "Replacement".into())])
            .is_err());
        assert_eq!(db.get_by_key("k1").unwrap(), Some(original));

        assert!(Database::open_read_only(&dir.path().join("missing.db")).is_err());
        let invalid = dir.path().join("invalid.db");
        std::fs::write(&invalid, "").unwrap();
        assert!(Database::open_read_only(&invalid).is_err());
    }

    #[test]
    fn upsert_is_idempotent_and_updates() {
        let mut db = seeded();
        db.upsert_papers(&[paper(
            "k1",
            "NDSS",
            2020,
            "Fuzzing the Linux kernel v2",
            None,
        )])
        .unwrap();
        assert_eq!(db.count().unwrap(), 3);
        let p = paper_by_key(&db, "k1");
        assert_eq!(p.title, "Fuzzing the Linux kernel v2");
        assert_eq!(p.abstract_text.as_deref(), Some("we fuzz kernels"));
    }

    #[test]
    fn upsert_skips_unchanged_rows() {
        let mut db = seeded();
        let rows = db
            .upsert_papers(&[paper("k1", "NDSS", 2020, "Fuzzing the Linux kernel", None)])
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn fts_search_matches_title_and_abstract() {
        let db = seeded();
        let hits = db
            .search(&Search {
                fts: Some("kernel".into()),
                ..Default::default()
            })
            .unwrap();
        let keys: Vec<_> = hits.iter().map(|p| p.dblp_key.as_str()).collect();
        assert!(keys.contains(&"k1"));
        assert!(keys.contains(&"k3"));
        assert!(!keys.contains(&"k2"));
    }

    #[test]
    fn metadata_filters() {
        let db = seeded();
        let by_venue = db
            .search(&Search {
                filter: Some(FilterExpr::Venue(vec!["NDSS".into(), "SP".into()])),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_venue.len(), 2);

        let by_year = db
            .search(&Search {
                filter: Some(FilterExpr::Year(YearRange::new(Some(2020), None).unwrap())),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_year.len(), 2);
    }

    #[test]
    fn repeated_year_filters_match_any_range() {
        let db = seeded();
        let hits = db
            .search(&Search {
                filter: Some(FilterExpr::Or(vec![
                    FilterExpr::Year(YearRange::single(2019)),
                    FilterExpr::Year(YearRange::single(2021)),
                ])),
                ..Default::default()
            })
            .unwrap();
        let keys = hits
            .iter()
            .map(|paper| paper.dblp_key.as_str())
            .collect::<Vec<_>>();
        assert_eq!(keys, vec!["k2", "k3"]);
    }

    #[test]
    fn boolean_metadata_filters() {
        let db = seeded();
        let intersection = db
            .search(&Search {
                filter: Some(FilterExpr::And(vec![
                    FilterExpr::Venue(vec!["NDSS".into(), "SP".into()]),
                    FilterExpr::Venue(vec!["SP".into(), "CCS".into()]),
                ])),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(intersection[0].dblp_key, "k3");

        let excluded = db
            .search(&Search {
                filter: Some(FilterExpr::Not(Box::new(FilterExpr::Venue(vec![
                    "CCS".into()
                ])))),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(excluded.len(), 2);
        assert!(excluded.iter().all(|paper| paper.venue != "CCS"));
    }

    #[test]
    fn rank_sort_groups_venues_before_year() {
        let db = seeded();
        let hits = db
            .search(&Search {
                sort: Sort::Rank(vec![vec!["NDSS".into(), "SP".into()], vec!["CCS".into()]]),
                ..Default::default()
            })
            .unwrap();
        let keys = hits
            .iter()
            .map(|paper| paper.dblp_key.as_str())
            .collect::<Vec<_>>();
        assert_eq!(keys, vec!["k1", "k3", "k2"]);
    }

    #[test]
    fn rank_sort_without_ranked_venues_falls_back_to_year() {
        let db = seeded();
        let hits = db
            .search(&Search {
                sort: Sort::Rank(Vec::new()),
                ..Default::default()
            })
            .unwrap();
        let keys = hits
            .iter()
            .map(|paper| paper.dblp_key.as_str())
            .collect::<Vec<_>>();
        assert_eq!(keys, vec!["k2", "k1", "k3"]);
    }

    #[test]
    fn doi_filter_matches_substrings() {
        let db = seeded();
        let hits = db
            .search(&Search {
                filter: Some(FilterExpr::Doi("10.1/x".into())),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn negated_doi_filter_includes_missing_dois() {
        let mut db = seeded();
        let mut missing = paper("k4", "NDSS", 2022, "Paper without DOI", None);
        missing.doi = None;
        db.upsert_papers(&[missing]).unwrap();

        let hits = db
            .search(&Search {
                filter: Some(FilterExpr::Not(Box::new(FilterExpr::Doi("10.1/x".into())))),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].dblp_key, "k4");
    }

    #[test]
    fn search_count_and_offset() {
        let db = seeded();
        let search = Search {
            limit: Some(1),
            offset: Some(1),
            ..Default::default()
        };
        assert_eq!(db.search_count(&search).unwrap(), 3);
        let hits = db.search(&search).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].dblp_key, "k1");
    }

    #[test]
    fn pagination_matches_slices_of_the_full_result() {
        let mut db = Database::open_in_memory().unwrap();
        db.upsert_papers(&[
            paper("z", "NDSS", 2026, "Kernel", Some("Findings")),
            paper("a", "NDSS", 2026, "Kernel", Some("Findings")),
            paper("x", "SP", 2025, "Other research", None),
            paper("b", "NDSS", 2026, "Kernel", Some("Findings")),
            paper("c", "CCS", 2024, "Kernel", Some("Findings")),
        ])
        .unwrap();
        // Ties must not depend on SQLite's scan order.
        db.conn
            .pragma_update(None, "reverse_unordered_selects", true)
            .unwrap();
        let filter = FilterExpr::And(vec![
            FilterExpr::Or(vec![
                FilterExpr::Year(YearRange::single(2026)),
                FilterExpr::Venue(vec!["CCS".into()]),
            ]),
            FilterExpr::Not(Box::new(FilterExpr::Doi("excluded".into()))),
        ]);
        for sort in [
            Sort::Relevance,
            Sort::Year,
            Sort::Venue,
            Sort::Rank(vec![vec!["SP".into(), "NDSS".into()], vec!["CCS".into()]]),
        ] {
            for fts in [None, Some("kernel".into())] {
                for filter in [None, Some(filter.clone())] {
                    let mut search = Search {
                        sort: sort.clone(),
                        fts: fts.clone(),
                        filter,
                        ..Default::default()
                    };
                    let all = db.search(&search).unwrap();
                    for offset in [0, 1, all.len()] {
                        for limit in [None, Some(1), Some(2)] {
                            search.limit = limit;
                            search.offset = Some(offset);
                            let expected: Vec<_> = all
                                .iter()
                                .skip(offset)
                                .take(limit.unwrap_or(usize::MAX))
                                .cloned()
                                .collect();
                            assert_eq!(db.search(&search).unwrap(), expected, "{search:?}");
                            assert_eq!(db.search_count(&search).unwrap(), all.len());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn empty_venue_filter_matches_nothing() {
        let db = seeded();
        let search = Search {
            filter: Some(FilterExpr::Venue(Vec::new())),
            ..Default::default()
        };
        assert_eq!(db.search_count(&search).unwrap(), 0);
        assert!(db.search(&search).unwrap().is_empty());
    }

    #[test]
    fn missing_abstract_batch_uses_keyset_bound() {
        let db = seeded();
        assert_eq!(db.count_missing_abstracts(&[], &[]).unwrap(), 1);
        let missing = db.papers_missing_abstract_batch(&[], &[], 0, 10).unwrap();
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].paper.dblp_key, "k2");
        let next = db
            .papers_missing_abstract_batch(&[], &[], missing[0].id, 10)
            .unwrap();
        assert!(next.is_empty());
    }

    #[test]
    fn missing_abstract_batch_includes_papers_without_url() {
        let mut db = seeded();
        let no_url = Paper {
            url: None,
            ..paper("k4", "AAAI", 2004, "A paper without a URL", None)
        };
        db.upsert_papers(&[no_url]).unwrap();
        assert_eq!(db.count_missing_abstracts(&[], &[]).unwrap(), 2);
    }

    #[test]
    fn missing_abstract_batch_filters_years() {
        let db = seeded();
        assert_eq!(
            db.count_missing_abstracts(&[], &[YearRange::single(2020)])
                .unwrap(),
            0
        );
        assert_eq!(
            db.papers_missing_abstract_batch(&[], &[YearRange::single(2021)], 0, 10)
                .unwrap()[0]
                .paper
                .dblp_key,
            "k2"
        );
    }

    #[test]
    fn remembered_misses_wait_for_recheck_or_new_inputs() {
        let mut db = seeded();
        let current_year: i32 = db
            .conn
            .query_row("SELECT CAST(strftime('%Y', 'now') AS INTEGER)", [], |r| {
                r.get(0)
            })
            .unwrap();
        let pending_after = |db: &Database, year: i32, days: i32| {
            db.conn
                .execute_batch(&format!(
                    "UPDATE papers SET year = {year} WHERE dblp_key = 'k2';
                     UPDATE abstract_misses SET checked_at = datetime('now', '-{days} days');"
                ))
                .unwrap();
            db.count_missing_abstracts(&[], &[]).unwrap()
        };
        db.record_abstract_misses(&["k2".to_string()]).unwrap();
        for (year, days, pending) in [
            (current_year + 1, 6, 0),
            (current_year, 8, 1),
            (current_year - 1, 8, 0),
            (current_year - 1, 31, 1),
            (2001, 179, 0),
            (2001, 181, 1),
        ] {
            assert_eq!(pending_after(&db, year, days), pending, "{year} {days}");
        }

        db.conn
            .execute(
                "UPDATE papers SET doi = '10.1/new' WHERE dblp_key = 'k2'",
                [],
            )
            .unwrap();
        assert_eq!(db.count_missing_abstracts(&[], &[]).unwrap(), 1, "new DOI");
    }

    #[test]
    fn set_abstracts_updates_fts() {
        let mut db = seeded();
        db.set_abstracts(&[("k2".into(), "a batched cache timing leak".into())])
            .unwrap();
        let hits = db
            .search(&Search {
                fts: Some("batched".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].dblp_key, "k2");
    }

    #[test]
    fn begin_enrich_version_rechecks_stored_abstracts_once() {
        let mut db = seeded();
        let clean =
            |abs: String, _: &str| (abs != "rop chains").then(|| abs.replace("fuzz", "test"));
        db.record_abstract_misses(&["k2".to_string()]).unwrap();
        assert_eq!(db.count_missing_abstracts(&[], &[]).unwrap(), 0);
        assert_eq!(db.begin_enrich_version(1, clean).unwrap(), Some(2));
        assert_eq!(
            paper_by_key(&db, "k1").abstract_text.as_deref(),
            Some("we test kernels")
        );
        assert_eq!(paper_by_key(&db, "k3").abstract_text, None);
        assert_eq!(db.count_missing_abstracts(&[], &[]).unwrap(), 2);
        assert!(db
            .search(&Search {
                fts: Some("rop".into()),
                ..Default::default()
            })
            .unwrap()
            .is_empty());
        let misses: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM abstract_misses", [], |r| r.get(0))
            .unwrap();
        assert_eq!(misses, 0);
        assert_eq!(db.begin_enrich_version(1, clean).unwrap(), None);
    }
}
