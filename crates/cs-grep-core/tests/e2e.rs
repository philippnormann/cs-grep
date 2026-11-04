//! End-to-end pipeline: dblp JSON -> upsert -> parse query -> search -> render.

use cs_grep_core::config::Config;
use cs_grep_core::db::{Database, Search};
use cs_grep_core::output::{render, Format};
use cs_grep_core::{dblp, query};
use serde_json::json;

fn fixture() -> serde_json::Value {
    let row = |publ: &str, title: &str, year: &str, authors: &str, url: &str| {
        json!({
            "publ": {"value": publ},
            "t": {"value": title},
            "y": {"value": year},
            "a": {"value": authors},
            "u": {"value": url},
        })
    };
    json!({
        "results": {"bindings": [
            row("p1", "Fuzzing the Linux kernel", "2024", "1\tAlice Smith 0001\n2\tBob Jones", "https://doi.org/10.1/a"),
            row("p2", "Side channel attacks on caches", "2023", "1\tCarol Chen", "https://doi.org/10.1/b"),
            row("p3", "A formal model of TLS", "2022", "1\tDave Diaz", "https://doi.org/10.1/c"),
        ]}
    })
}

#[test]
fn full_pipeline() {
    // ingest
    let papers = dblp::parse_results(&fixture(), "NDSS").unwrap();
    assert_eq!(papers.len(), 3);
    let mut db = Database::open_in_memory().unwrap();
    let n = db.upsert_papers(&papers).unwrap();
    assert_eq!(n, 3);
    assert_eq!(db.count().unwrap(), 3);
    let config = Config::defaults().unwrap();

    // boolean full-text search
    let parsed = query::parse("fuzzing AND kernel", &config).unwrap();
    let hits = db
        .search(&Search {
            fts: parsed.fts,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "Fuzzing the Linux kernel");
    assert_eq!(hits[0].authors, "Alice Smith, Bob Jones");
    assert_eq!(hits[0].doi.as_deref(), Some("10.1/a"));

    // phrase search
    let parsed = query::parse("\"side channel\"", &config).unwrap();
    let hits = db
        .search(&Search {
            fts: parsed.fts,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].dblp_key, "p2");

    // full-text and metadata query
    let parsed = query::parse("attacks WHERE year:2023- AND venue:NDSS", &config).unwrap();
    let recent = db
        .search(&Search {
            fts: parsed.fts,
            filter: parsed.filter,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].dblp_key, "p2");

    // bibtex render
    let bib = render(&papers, Format::Bibtex, None).unwrap();
    assert!(bib.contains("@inproceedings{ndss:2024:smith,"));
    assert!(bib.contains("author    = {Alice Smith and Bob Jones}"));
}
