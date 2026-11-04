//! dblp SPARQL ingestion: fetch the papers of a venue stream.

use std::time::Duration;

use serde_json::Value;

use crate::abstracts::{collapse_ws, decode_html_entities};
use crate::config::Venue;
use crate::{Error, Paper, Result};

pub const DEFAULT_ENDPOINT: &str = "https://sparql.dblp.org/sparql";

/// Build the SPARQL query for one DBLP venue stream from `min_year` on, without
/// papers also listed in one of the `later` streams (they belong there).
/// One row per publication; authors are "ordinal\tname" lines.
pub fn build_query(stream: &str, min_year: i32, later: &[&str]) -> String {
    // shortcut: each later stream adds ~55 bytes to the GET URL; past ~120 configured
    // venues switch the DBLP request to POST (application/sparql-query)
    let later: String = later
        .iter()
        .map(|s| format!(" <https://dblp.org/streams/{s}>"))
        .collect();
    format!(
        r#"PREFIX dblp: <https://dblp.org/rdf/schema#>
PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
SELECT ?publ (MIN(?title) AS ?t) (MIN(?year) AS ?y) (MIN(STR(?url)) AS ?u) (MIN(STR(?doi)) AS ?d)
       (GROUP_CONCAT(DISTINCT CONCAT(STR(?ordinal), "\t", ?author); SEPARATOR="\n") AS ?a) WHERE {{
  VALUES ?type {{ dblp:Inproceedings dblp:Article }}
  ?publ dblp:publishedInStream <https://dblp.org/streams/{stream}> ;
        rdf:type ?type ;
        dblp:title ?title ;
        dblp:yearOfPublication ?pubYear ;
        dblp:hasSignature ?sig .
  ?sig dblp:signatureDblpName ?author ;
       dblp:signatureOrdinal ?ordinal .
  OPTIONAL {{ ?publ dblp:primaryDocumentPage ?url . }}
  OPTIONAL {{ ?publ dblp:doi ?doi . }}
  OPTIONAL {{ ?publ dblp:yearOfEvent ?eventYear . }}
  FILTER NOT EXISTS {{ VALUES ?later {{{later} }} ?publ dblp:publishedInStream ?later }}
  BIND(COALESCE(?eventYear, ?pubYear) AS ?year)
  FILTER(?year >= "{min_year}"^^xsd:gYear)
}}
GROUP BY ?publ"#,
    )
}

/// dblp disambiguates homonyms with a trailing 4-digit id ("Name 0001");
/// strip it to keep the display name.
fn clean_author(author: &str) -> String {
    let trimmed = author.trim();
    if let Some((name, suffix)) = trimmed.rsplit_once(' ') {
        if suffix.len() == 4 && suffix.chars().all(|c| c.is_ascii_digit()) {
            return name.trim_end().to_string();
        }
    }
    trimmed.to_string()
}

/// Extract a DOI from a publisher URL if it is a doi.org / DOI link.
fn extract_doi(url: &str) -> Option<String> {
    let lower = url.to_lowercase();
    for marker in ["doi.org/", "doi/"] {
        if let Some(idx) = lower.find(marker) {
            let doi = &url[idx + marker.len()..];
            if doi.starts_with("10.") {
                return Some(doi.trim_end_matches('/').to_string());
            }
        }
    }
    None
}

fn binding_str<'a>(binding: &'a Value, key: &str) -> Option<&'a str> {
    binding.get(key)?.get("value")?.as_str()
}

/// Parse a SPARQL JSON results document into papers for a venue.
pub fn parse_results(json: &Value, venue_id: &str) -> Result<Vec<Paper>> {
    let bindings = json
        .get("results")
        .and_then(|r| r.get("bindings"))
        .and_then(|b| b.as_array())
        .ok_or_else(|| Error::Other("dblp response has no results.bindings".into()))?;
    Ok(bindings
        .iter()
        .filter_map(|b| {
            let publ = binding_str(b, "publ")?;
            let url = binding_str(b, "u").map(str::to_string);
            let doi = url
                .as_deref()
                .and_then(extract_doi)
                .or_else(|| binding_str(b, "d").and_then(extract_doi));
            let mut authors: Vec<(i64, &str)> = binding_str(b, "a")
                .unwrap_or_default()
                .lines()
                .filter_map(|l| l.split_once('\t'))
                .map(|(o, a)| (o.parse().unwrap_or(i64::MAX), a))
                .collect();
            authors.sort_unstable();
            Some(Paper {
                dblp_key: publ.to_string(),
                venue: venue_id.to_string(),
                year: binding_str(b, "y")
                    .and_then(|y| y.parse().ok())
                    .unwrap_or(0),
                title: collapse_ws(&decode_html_entities(
                    binding_str(b, "t").unwrap_or_default(),
                )),
                authors: authors
                    .into_iter()
                    .map(|(_, a)| clean_author(a))
                    .collect::<Vec<_>>()
                    .join(", "),
                doi,
                url,
                abstract_text: None,
            })
        })
        .collect())
}

/// SPARQL client over the dblp endpoint.
pub struct Dblp {
    endpoint: String,
    client: reqwest::Client,
}

impl Default for Dblp {
    fn default() -> Self {
        Self::new(DEFAULT_ENDPOINT)
    }
}

impl Dblp {
    pub fn new(endpoint: &str) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("cs-grep/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        Self {
            endpoint: endpoint.to_string(),
            client,
        }
    }

    /// Fetch and parse the papers of a venue from `min_year` on, except those
    /// also listed in one of the `later` streams.
    pub async fn fetch_venue(
        &self,
        venue: &Venue,
        min_year: i32,
        later: &[&str],
    ) -> Result<Vec<Paper>> {
        let query = build_query(&venue.dblp_stream, min_year, later);
        let resp = self
            .client
            .get(&self.endpoint)
            .query(&[("query", query.as_str())])
            .header("Accept", "application/sparql-results+json")
            .send()
            .await?
            .error_for_status()?;
        let json: Value = resp.json().await?;
        parse_results(&json, &venue.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn binding(
        publ: &str,
        title: &str,
        year: &str,
        authors: &str,
        url: Option<&str>,
        doi: Option<&str>,
    ) -> Value {
        let mut b = json!({
            "publ": {"value": publ},
            "t": {"value": title},
            "y": {"value": year},
            "a": {"value": authors},
        });
        if let Some(u) = url {
            b["u"] = json!({"value": u});
        }
        if let Some(d) = doi {
            b["d"] = json!({"value": d});
        }
        b
    }

    #[test]
    fn query_contains_stream_types_years_and_later_streams() {
        let q = build_query("conf/ndss", 2000, &["conf/soups"]);
        assert!(q.contains("https://dblp.org/streams/conf/ndss"));
        assert!(q.contains("VALUES ?type { dblp:Inproceedings dblp:Article }"));
        assert!(q.contains("\"2000\"^^xsd:gYear"));
        assert!(q.contains("GROUP_CONCAT(DISTINCT"));
        assert!(q.contains("VALUES ?later { <https://dblp.org/streams/conf/soups> }"));
    }

    #[test]
    fn query_gives_event_year_precedence_over_publication_year() {
        let q = build_query("conf/esorics", 2000, &[]);
        assert!(q.contains("COALESCE(?eventYear, ?pubYear) AS ?year"));
    }

    #[test]
    fn clean_author_strips_homonym_suffix() {
        assert_eq!(clean_author("Jane Doe 0001"), "Jane Doe");
        assert_eq!(clean_author("Jane Doe"), "Jane Doe");
        assert_eq!(clean_author("R2 D2"), "R2 D2");
    }

    #[test]
    fn extract_doi_from_urls() {
        assert_eq!(
            extract_doi("https://doi.org/10.1145/123.456"),
            Some("10.1145/123.456".to_string())
        );
        assert_eq!(
            extract_doi("https://dl.acm.org/doi/10.1145/abc"),
            Some("10.1145/abc".to_string())
        );
        assert_eq!(extract_doi("https://www.usenix.org/x"), None);
    }

    #[test]
    fn parse_sorts_authors_by_ordinal() {
        let doc = json!({"results": {"bindings": [
            binding("p1", "Title One", "2020", "2\tBob B\n10\tDan D\n1\tAlice A 0001", Some("https://doi.org/10.1/a"), None),
            binding("p2", "Title Two", "2021", "1\tCarol C", None, None),
        ]}});
        let papers = parse_results(&doc, "NDSS").unwrap();
        assert_eq!(papers.len(), 2);
        assert_eq!(papers[0].authors, "Alice A, Bob B, Dan D");
        assert_eq!(papers[0].venue, "NDSS");
        assert_eq!(papers[0].year, 2020);
        assert_eq!(papers[0].doi.as_deref(), Some("10.1/a"));
        assert_eq!(papers[1].authors, "Carol C");
        assert!(papers[1].url.is_none());
    }

    #[test]
    fn parse_prefers_url_doi_over_dblp_doi() {
        let doc = json!({"results": {"bindings": [
            binding("p1", "T", "2020", "1\tA", Some("https://doi.org/10.1145/x"), Some("https://doi.org/10.1109/y")),
            binding("p2", "T", "2020", "1\tA", Some("https://openaccess.thecvf.com/x.html"), Some("https://doi.org/10.1109/y")),
        ]}});
        let papers = parse_results(&doc, "NDSS").unwrap();
        assert_eq!(papers[0].doi.as_deref(), Some("10.1145/x"));
        assert_eq!(papers[1].doi.as_deref(), Some("10.1109/y"));
    }

    #[test]
    fn parse_cleans_title_entities_and_whitespace() {
        let doc = json!({"results": {"bindings": [binding("p1", "A &amp; B  C", "2020", "1\tA", None, None)]}});
        assert_eq!(parse_results(&doc, "NDSS").unwrap()[0].title, "A & B C");
    }

    #[test]
    fn parse_empty_results() {
        let doc = json!({"results": {"bindings": []}});
        assert!(parse_results(&doc, "NDSS").unwrap().is_empty());
        assert!(parse_results(&json!({}), "NDSS").is_err());
    }
}
