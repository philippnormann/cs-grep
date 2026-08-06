//! Optional abstract enrichment.
//!
//! Two tiers, each a fallback for the previous:
//!   1. APIs: DOI, source URL, then guarded metadata lookup
//!   2. Static URL extraction (publisher page)
//!
//! The pure parsing/extraction helpers are unit-tested; the networked
//! orchestration is exercised end-to-end via the CLI.

use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    sync::OnceLock,
    time::Duration,
};

use futures::stream::{self, StreamExt};
use reqwest::{header, Url};
use scraper::{ElementRef, Html, Selector};
use serde_json::Value;

use crate::config::Secrets;
use crate::{Paper, Result};

const MAX_JSON_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_HTML_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_STATIC_REDIRECTS: usize = 5;
const MAX_RATE_LIMIT_SLEEP: Duration = Duration::from_secs(65);
const OPENREVIEW_PAGE_SIZE: usize = 500;
const OPENREVIEW_LOGIN_EXPIRES_IN: u64 = 7 * 24 * 60 * 60;
const SEMANTIC_SCHOLAR_BATCH_SIZE: usize = 500;
const OPENALEX_BATCH_SIZE: usize = 100;
const TRANSIENT_RETRY_DELAY: Duration = Duration::from_millis(250);
const MIN_ABSTRACT_CHARS: usize = 80;
const MAX_ABSTRACT_CHARS: usize = 6_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum AbstractSource {
    Acl,
    Acm,
    Cvf,
    Ieee,
    Ijcai,
    Ndss,
    Neurips,
    Openreview,
    Pmlr,
    Springer,
    Usenix,
}

/// Reconstruct plain text from an OpenAlex `abstract_inverted_index`,
/// or read a plain `abstract` string if present.
fn abstract_from_openalex(work: &Value) -> Option<String> {
    if let Some(s) = work.get("abstract").and_then(|v| v.as_str()) {
        return non_empty_text(s);
    }
    let idx = work.get("abstract_inverted_index")?.as_object()?;
    let mut positioned: Vec<(u64, &str)> = Vec::new();
    for (word, positions) in idx {
        for p in positions.as_array()? {
            if let Some(pos) = p.as_u64() {
                positioned.push((pos, word.as_str()));
            }
        }
    }
    if positioned.is_empty() {
        return None;
    }
    positioned.sort_by_key(|(p, _)| *p);
    non_empty_text(
        positioned
            .into_iter()
            .map(|(_, w)| w)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn abstract_from_semantic_scholar(paper: &Value) -> Option<String> {
    paper
        .get("abstract")
        .and_then(|v| v.as_str())
        .and_then(non_empty_text)
}

fn abstract_from_openalex_title_search(results: &Value, paper: &Paper) -> Option<String> {
    results
        .get("results")?
        .as_array()?
        .iter()
        .find(|work| openalex_identity_matches(work, paper))
        .and_then(abstract_from_openalex)
}

fn abstract_from_semantic_scholar_title_search(results: &Value, paper: &Paper) -> Option<String> {
    results
        .get("data")?
        .as_array()?
        .iter()
        .find(|work| semantic_scholar_identity_matches(work, paper))
        .and_then(abstract_from_semantic_scholar)
}

fn abstracts_from_openalex_works(
    results: &Value,
    expected_titles: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(results) = results.get("results").and_then(|v| v.as_array()) else {
        return out;
    };
    for work in results {
        let Some(doi) = work
            .get("doi")
            .and_then(|v| v.as_str())
            .and_then(normalized_doi)
            .filter(|doi| {
                expected_titles
                    .get(doi)
                    .is_some_and(|title| json_doi_title_matches(work, title))
            })
        else {
            continue;
        };
        if let Some(abs) = abstract_from_openalex(work)
            .and_then(|abs| usable_abstract_for_title(abs, &expected_titles[&doi]))
        {
            out.insert(doi, abs);
        }
    }
    out
}

fn abstracts_from_semantic_scholar_batch(
    results: &Value,
    expected_titles: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(results) = results.as_array() else {
        return out;
    };
    for paper in results {
        let Some(doi) = paper
            .get("externalIds")
            .and_then(|ids| ids.get("DOI"))
            .and_then(|v| v.as_str())
            .and_then(normalized_doi)
            .filter(|doi| {
                expected_titles
                    .get(doi)
                    .is_some_and(|title| json_doi_title_matches(paper, title))
            })
        else {
            continue;
        };
        if let Some(abs) = abstract_from_semantic_scholar(paper)
            .and_then(|abs| usable_abstract_for_title(abs, &expected_titles[&doi]))
        {
            out.insert(doi, abs);
        }
    }
    out
}

fn openalex_identity_matches(work: &Value, paper: &Paper) -> bool {
    let title = work
        .get("title")
        .or_else(|| work.get("display_name"))
        .and_then(|v| v.as_str());
    let year = work.get("publication_year").and_then(|v| v.as_i64());
    let author = work
        .get("authorships")
        .and_then(|v| v.as_array())
        .and_then(|authors| authors.first())
        .and_then(|authorship| authorship.get("author"))
        .and_then(|author| author.get("display_name"))
        .and_then(|v| v.as_str());
    paper_identity_matches(title, year, author, paper)
}

fn semantic_scholar_identity_matches(work: &Value, paper: &Paper) -> bool {
    let title = work.get("title").and_then(|v| v.as_str());
    let year = work.get("year").and_then(|v| v.as_i64());
    let author = work
        .get("authors")
        .and_then(|v| v.as_array())
        .and_then(|authors| authors.first())
        .and_then(|author| author.get("name"))
        .and_then(|v| v.as_str());
    paper_identity_matches(title, year, author, paper)
}

fn paper_identity_matches(
    title: Option<&str>,
    year: Option<i64>,
    first_author: Option<&str>,
    paper: &Paper,
) -> bool {
    let Some(title) = title else {
        return false;
    };
    let Some(year) = year else {
        return false;
    };
    let Some(first_author) = first_author else {
        return false;
    };
    let Some(paper_first_author) = paper
        .authors
        .split(',')
        .next()
        .filter(|s| !s.trim().is_empty())
    else {
        return false;
    };
    normalized_match_text(title) == normalized_match_text(&paper.title)
        && (year - i64::from(paper.year)).abs() <= 1
        && normalized_match_text(first_author) == normalized_match_text(paper_first_author)
}

fn normalized_match_text(text: &str) -> String {
    decode_html_entities(text)
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn titles_match(actual: &str, expected: &str) -> bool {
    normalized_match_text(actual) == normalized_match_text(expected)
}

fn doi_titles_match(actual: &str, expected: &str) -> bool {
    titles_match(actual, expected)
        || subtitle_prefix_matches(actual, expected)
        || subtitle_prefix_matches(expected, actual)
}

fn subtitle_prefix_matches(longer: &str, shorter: &str) -> bool {
    [":", " - ", " – ", " — "].iter().any(|delimiter| {
        longer
            .match_indices(delimiter)
            .any(|(index, _)| titles_match(&longer[..index], shorter))
    })
}

fn json_doi_title_matches(value: &Value, expected: &str) -> bool {
    value
        .get("title")
        .or_else(|| value.get("display_name"))
        .and_then(|v| v.as_str())
        .is_some_and(|title| doi_titles_match(title, expected))
}

fn usable_abstract_for_title(raw: String, title: &str) -> Option<String> {
    let abstract_text = abstract_text(raw)?;
    (!titles_match(&abstract_text, title)).then_some(abstract_text)
}

fn normalized_doi(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('.');
    let lower = trimmed.to_ascii_lowercase();
    for prefix in [
        "https://doi.org/",
        "http://doi.org/",
        "https://dx.doi.org/",
        "http://dx.doi.org/",
        "doi:",
    ] {
        if lower.starts_with(prefix) {
            let doi = trimmed[prefix.len()..].trim();
            return (!doi.is_empty()).then(|| doi.to_ascii_lowercase());
        }
    }
    (!trimmed.is_empty()).then(|| trimmed.to_ascii_lowercase())
}

fn abstracts_from_openreview_notes(notes: &Value) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Some(notes) = notes.get("notes").and_then(|v| v.as_array()) else {
        return out;
    };
    for note in notes {
        let Some(content) = note.get("content") else {
            continue;
        };
        let Some(abs) = content.get("abstract").and_then(openreview_content_value) else {
            continue;
        };
        for key in ["id", "forum"] {
            if let Some(id) = note.get(key).and_then(|v| v.as_str()) {
                out.insert(id.to_string(), abs.clone());
            }
        }
    }
    out
}

fn openreview_content_value(value: &Value) -> Option<String> {
    value
        .as_str()
        .or_else(|| value.get("value").and_then(|v| v.as_str()))
        .and_then(non_empty_text)
}

/// Crossref returns JATS-flavored XML in `message.abstract`; strip tags.
fn abstract_from_crossref(message: &Value) -> Option<String> {
    let raw = message.get("abstract").and_then(|v| v.as_str())?;
    let doc = Html::parse_fragment(raw);
    element_text(doc.root_element())
}

fn crossref_title_matches(message: &Value, expected: &str) -> bool {
    message
        .get("title")
        .and_then(|v| v.as_array())
        .and_then(|titles| titles.first())
        .and_then(|v| v.as_str())
        .is_some_and(|title| doi_titles_match(title, expected))
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Extract abstract candidates from a publisher page in priority order.
fn abstract_candidates(html: &str, source: Option<AbstractSource>) -> Vec<String> {
    let doc = Html::parse_document(html);

    let mut candidates = match source {
        Some(AbstractSource::Acl) => selector_texts(&doc, &[".acl-abstract span"]),
        Some(AbstractSource::Acm) => {
            selector_texts(&doc, &["div.abstractInFull", "div.abstractSection"])
        }
        Some(AbstractSource::Cvf) => selector_texts(&doc, &["div#abstract"]),
        Some(AbstractSource::Ieee) => selector_texts(&doc, &["div.abstract-text"]),
        Some(AbstractSource::Ijcai) => {
            selector_texts(&doc, &[".proceedings-detail hr + .row > .col-md-12"])
        }
        Some(AbstractSource::Ndss) => extract_ndss_abstract(&doc).into_iter().collect(),
        Some(AbstractSource::Neurips) => extract_neurips_abstract(&doc).into_iter().collect(),
        Some(AbstractSource::Openreview) => selector_texts(&doc, &[".abstract-text-inner"]),
        Some(AbstractSource::Pmlr) => selector_texts(&doc, &["div#abstract"]),
        Some(AbstractSource::Springer) => selector_texts(
            &doc,
            &[
                "section[data-title='Abstract'] div.c-article-section__content",
                "#Abs1-content",
            ],
        ),
        Some(AbstractSource::Usenix) => selector_texts(
            &doc,
            &[
                "div.field-name-field-paper-description",
                "div.field-type-text-with-summary",
            ],
        ),
        _ => Vec::new(),
    };

    candidates.extend(meta_contents(&doc));
    candidates
}

fn extract_ndss_abstract(doc: &Html) -> Option<String> {
    let mut text = first_selector_text(doc, &["div.paper-data"])?;
    if let Some(authors) = first_selector_text(doc, &["div.paper-data strong"]) {
        if let Some(rest) = text.strip_prefix(&authors) {
            text = rest
                .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ':' | '-'))
                .to_string();
        }
    }
    non_empty_text(text)
}

fn extract_neurips_abstract(doc: &Html) -> Option<String> {
    let selector = Selector::parse("section.paper-section").ok()?;
    for section in doc.select(&selector) {
        let text = collapse_ws(&section.text().collect::<String>());
        if let Some(stripped) = text.strip_prefix("Abstract") {
            return non_empty_text(stripped);
        }
    }
    let selector = Selector::parse("*").ok()?;
    let mut after_abstract_heading = false;
    for element in doc.select(&selector) {
        match element.value().name() {
            "h4" => {
                after_abstract_heading =
                    collapse_ws(&element.text().collect::<String>()) == "Abstract"
            }
            "p" if after_abstract_heading => {
                if let Some(text) = element_text(element) {
                    return Some(text);
                }
            }
            _ => {}
        }
    }
    None
}

fn first_selector_text(doc: &Html, selectors: &[&str]) -> Option<String> {
    selector_texts(doc, selectors).into_iter().next()
}

fn selector_texts(doc: &Html, selectors: &[&str]) -> Vec<String> {
    let mut texts = Vec::new();
    for selector in selectors {
        let Ok(selector) = Selector::parse(selector) else {
            continue;
        };
        for element in doc.select(&selector) {
            if let Some(text) = element_text(element) {
                texts.push(text);
            }
        }
    }
    texts
}

fn meta_contents(doc: &Html) -> Vec<String> {
    let selectors = [
        "meta[name='citation_abstract']",
        "meta[property='og:description']",
        "meta[name='description']",
    ];

    let mut texts = Vec::new();
    for selector in selectors {
        let Ok(selector) = Selector::parse(selector) else {
            continue;
        };
        for element in doc.select(&selector) {
            if let Some(content) = element.value().attr("content") {
                if let Some(text) = non_empty_text(content) {
                    texts.push(text);
                }
            }
        }
    }
    texts
}

fn is_boilerplate_abstract(text: &str) -> bool {
    let year = text.trim_end_matches('.').rsplit(' ').next().unwrap_or("");
    text.starts_with("Promoting openness in scientific communication and the peer-review process")
        || text.starts_with("Electronic proceedings of ")
        || text.starts_with("Paper presented at ")
        || text.starts_with("This repository contains ")
        || (text.len() < 300 && text.starts_with("Warning:"))
        || (text.len() < 300 && text.contains(" to me ") && text.contains("I am "))
        || (text.contains("Authors Info & Claims")
            && text.contains("View Profile")
            && text.contains("Get Citation Alerts"))
        || text
            .strip_prefix('\'')
            .and_then(|text| text.strip_suffix('\''))
            .and_then(|text| text.split_once("' published in '"))
            .is_some_and(|(title, publication)| !title.is_empty() && !publication.is_empty())
        || ((text.contains("Proceedings of ") || text.contains("Findings of "))
            && ((year.len() == 4 && year.chars().all(|c| c.is_ascii_digit()))
                || text.contains(". In: ")))
}

fn is_url_only(text: &str) -> bool {
    let lower = text.trim().trim_end_matches('.').to_ascii_lowercase();
    !lower.contains(char::is_whitespace)
        && (lower.starts_with("http:") || lower.starts_with("https:") || lower.starts_with("doi:"))
}

fn trim_standard_suffix(text: &str) -> &str {
    let lower = text.to_ascii_lowercase();
    ["ccs concepts", "acm reference format:", "© 20", "©20"]
        .iter()
        .filter_map(|marker| lower.find(marker))
        .min()
        .map_or(text, |index| &text[..index])
        .trim_end()
}

fn element_text(element: ElementRef) -> Option<String> {
    non_empty_text(element.text().collect::<String>())
}

fn abstract_text(text: impl AsRef<str>) -> Option<String> {
    let text = non_empty_text(text)?;
    let text = if text.contains("</") || text.contains("<br") {
        element_text(Html::parse_fragment(&text).root_element())?
    } else {
        text
    };
    let text = trim_standard_suffix(&text);
    let chars = text.chars().count();
    ((MIN_ABSTRACT_CHARS..=MAX_ABSTRACT_CHARS).contains(&chars)
        && !text.ends_with("...")
        && !text.ends_with('…')
        && !text.contains('�')
        && !is_boilerplate_abstract(text)
        && !text.contains("Copy to Clipboard")
        && !text.contains("Download Related Material")
        && (chars >= 120 || text.ends_with(['.', '!', '?']))
        && !is_url_only(text))
    .then(|| text.to_string())
}

fn non_empty_text(text: impl AsRef<str>) -> Option<String> {
    let decoded = decode_html_entities(text.as_ref());
    let decoded = decode_html_entities(&decoded);
    let text = strip_abstract_heading(&collapse_ws(&decoded)).to_string();
    text.chars().any(char::is_alphabetic).then_some(text)
}

fn strip_abstract_heading(text: &str) -> &str {
    let Some(rest) = text
        .get(.."Abstract".len())
        .filter(|prefix| prefix.eq_ignore_ascii_case("Abstract"))
        .and_then(|_| text.get("Abstract".len()..))
    else {
        return text;
    };
    if let Some(rest) = [":", ".", " -", " –", " —"]
        .iter()
        .find_map(|separator| rest.strip_prefix(separator))
    {
        return rest.trim_start();
    }
    rest.strip_prefix(' ')
        .filter(|rest| {
            let mut words = rest.split_whitespace();
            words
                .next()
                .is_some_and(|word| word.starts_with(char::is_uppercase))
                && !words
                    .next()
                    .is_some_and(|word| word.starts_with(char::is_uppercase))
        })
        .unwrap_or(text)
}

fn static_page_title_matches(html: &str, paper: &Paper) -> bool {
    let doc = Html::parse_document(html);
    let selector = Selector::parse("meta[name='citation_title']").expect("valid selector");
    let Some(title) = doc
        .select(&selector)
        .next()
        .and_then(|element| element.value().attr("content"))
    else {
        return true;
    };
    if titles_match(title, &paper.title) {
        return true;
    }
    let Some(doi) = paper.doi.as_deref().and_then(normalized_doi) else {
        return false;
    };
    let selector = Selector::parse("meta[name='citation_doi']").expect("valid selector");
    doc.select(&selector)
        .filter_map(|element| element.value().attr("content"))
        .filter_map(normalized_doi)
        .any(|page_doi| page_doi == doi)
        && doi_titles_match(title, &paper.title)
}

fn decode_html_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    while let Some(idx) = rest.find('&') {
        out.push_str(&rest[..idx]);
        let entity_start = idx + 1;
        let Some(entity_len) = rest[entity_start..].find(';') else {
            out.push_str(&rest[idx..]);
            return out;
        };
        let entity_end = entity_start + entity_len;
        let entity = &rest[entity_start..entity_end];
        match decode_entity(entity) {
            Some(decoded) => out.push_str(&decoded),
            None => out.push_str(&rest[idx..=entity_end]),
        }
        rest = &rest[entity_end + 1..];
    }

    out.push_str(rest);
    out
}

fn decode_entity(entity: &str) -> Option<String> {
    if let Some(hex) = entity
        .strip_prefix("#x")
        .or_else(|| entity.strip_prefix("#X"))
    {
        let code = u32::from_str_radix(hex, 16).ok()?;
        return char::from_u32(code).map(|c| c.to_string());
    }
    if let Some(dec) = entity.strip_prefix('#') {
        let code = dec.parse::<u32>().ok()?;
        return char::from_u32(code).map(|c| c.to_string());
    }
    match entity {
        "amp" => Some("&".to_string()),
        "apos" => Some("'".to_string()),
        "gt" => Some(">".to_string()),
        "lt" => Some("<".to_string()),
        "nbsp" => Some(" ".to_string()),
        "quot" => Some("\"".to_string()),
        _ => None,
    }
}

/// Networked abstract enrichment using the configured API keys.
pub struct Enricher {
    client: reqwest::Client,
    secrets: Secrets,
    openreview_cache: HashMap<(String, i32), HashMap<String, String>>,
    doi_cache: HashMap<String, Option<String>>,
    openreview_login_token: OnceLock<String>,
}

pub enum EnrichResult {
    Found(String),
    Missing(String),
}

#[derive(Clone, Copy)]
enum RateLimitRetry {
    Enabled,
    Disabled,
}

fn store_doi_batch_results(
    cache: &mut HashMap<String, Option<String>>,
    dois: &[String],
    mut hits: HashMap<String, String>,
    retry_individually: &HashSet<String>,
) {
    for doi in dois {
        if let Some(abs) = hits.remove(doi) {
            cache.insert(doi.clone(), Some(abs));
        } else if !retry_individually.contains(doi) {
            cache.insert(doi.clone(), None);
        }
    }
}

impl Enricher {
    pub fn new(secrets: Secrets) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("cs-grep/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("reqwest client");
        Self {
            client,
            secrets,
            openreview_cache: HashMap::new(),
            doi_cache: HashMap::new(),
            openreview_login_token: OnceLock::new(),
        }
    }

    pub async fn enrich_many(
        &mut self,
        inputs: Vec<Paper>,
        jobs: usize,
    ) -> Vec<(Paper, Result<EnrichResult>)> {
        self.prefetch_api_batches(&inputs).await;
        let openreview_cache = &self.openreview_cache;
        let doi_cache = &self.doi_cache;
        let enricher = &*self;
        stream::iter(inputs.into_iter().map(|paper| async move {
            let doi_prefetch_complete = paper
                .doi
                .as_deref()
                .and_then(normalized_doi)
                .is_some_and(|doi| doi_cache.contains_key(&doi));
            let result = match cached_doi_abstract(doi_cache, &paper)
                .or_else(|| cached_openreview_abstract(openreview_cache, &paper))
            {
                Some(abs) => Ok(EnrichResult::Found(abs)),
                None => {
                    enricher
                        .enrich_after_prefetch(&paper, doi_prefetch_complete)
                        .await
                }
            };
            (paper, result)
        }))
        .buffer_unordered(jobs.max(1))
        .collect()
        .await
    }

    async fn prefetch_api_batches(&mut self, inputs: &[Paper]) {
        self.prefetch_doi_batches(inputs).await;
        let mut needed = Vec::new();
        let mut seen = HashSet::new();
        for paper in inputs {
            if paper_source(paper) == Some(AbstractSource::Openreview) {
                let key = (paper.venue.clone(), paper.year);
                if !self.openreview_cache.contains_key(&key) && seen.insert(key.clone()) {
                    needed.push(key);
                }
            }
        }
        for key in needed {
            match self.openreview_accepted_abstracts(&key.0, key.1).await {
                Ok(abstracts) => {
                    self.openreview_cache.insert(key, abstracts);
                }
                Err(e) => eprintln!(
                    "warning: OpenReview batch lookup failed for {} {}: {e}",
                    key.0, key.1
                ),
            }
        }
    }

    async fn prefetch_doi_batches(&mut self, inputs: &[Paper]) {
        let mut needed = Vec::new();
        let mut seen = HashSet::new();
        let mut expected_titles = HashMap::new();
        for paper in inputs {
            let Some(doi) = paper.doi.as_deref().and_then(normalized_doi) else {
                continue;
            };
            expected_titles
                .entry(doi.clone())
                .or_insert_with(|| paper.title.clone());
            if !self.doi_cache.contains_key(&doi) && seen.insert(doi.clone()) {
                needed.push(doi);
            }
        }

        for chunk in needed.chunks(SEMANTIC_SCHOLAR_BATCH_SIZE) {
            let mut retry_individually = HashSet::new();
            let mut hits = match self
                .semantic_scholar_doi_abstracts(chunk, &expected_titles)
                .await
            {
                Ok(hits) => hits,
                Err(e) => {
                    eprintln!("warning: Semantic Scholar DOI batch lookup failed: {e}");
                    retry_individually.extend(chunk.iter().cloned());
                    HashMap::new()
                }
            };
            let missing = chunk
                .iter()
                .filter(|doi| !hits.contains_key(*doi))
                .cloned()
                .collect::<Vec<_>>();
            for missing in missing.chunks(OPENALEX_BATCH_SIZE) {
                match self.openalex_doi_abstracts(missing, &expected_titles).await {
                    Ok(openalex_hits) => hits.extend(openalex_hits),
                    Err(e) => {
                        eprintln!("warning: OpenAlex DOI batch lookup failed: {e}");
                        retry_individually.extend(missing.iter().cloned());
                    }
                }
            }
            store_doi_batch_results(&mut self.doi_cache, chunk, hits, &retry_individually);
        }
    }

    async fn enrich_after_prefetch(
        &self,
        paper: &Paper,
        doi_prefetch_complete: bool,
    ) -> Result<EnrichResult> {
        if !doi_prefetch_complete {
            if let Some(abs) = self.api_by_doi(paper).await? {
                return Ok(EnrichResult::Found(abs));
            }
        }
        if let Some(abs) = self.openreview_api_abstract(paper).await {
            return Ok(EnrichResult::Found(abs));
        }
        self.fallback_enrich(paper).await
    }

    async fn fallback_enrich(&self, paper: &Paper) -> Result<EnrichResult> {
        let source = paper_source(paper);
        let mut static_miss = None;
        if source.is_some_and(|source| source != AbstractSource::Openreview) && paper.url.is_some()
        {
            match self.static_scrape(paper, source).await? {
                EnrichResult::Found(abs) => return Ok(EnrichResult::Found(abs)),
                EnrichResult::Missing(reason) => {
                    static_miss = Some(reason);
                }
            }
        }

        if paper.doi.as_deref().and_then(normalized_doi).is_none() {
            if let Some(abs) = self.api_by_title(paper).await? {
                return Ok(EnrichResult::Found(abs));
            }
        }
        if let Some(reason) = static_miss {
            return Ok(EnrichResult::Missing(reason));
        }
        if paper.url.is_some() {
            return self.static_scrape(paper, source).await;
        }
        Ok(EnrichResult::Missing(
            "API metadata lookup missed and paper has no URL".into(),
        ))
    }

    async fn api_by_doi(&self, paper: &Paper) -> Result<Option<String>> {
        let Some(doi) = paper.doi.as_deref().and_then(normalized_doi) else {
            return Ok(None);
        };
        let s2 = format!(
            "https://api.semanticscholar.org/graph/v1/paper/DOI:{doi}?fields=title,abstract"
        );
        if let Some(abs) = self
            .fetch_semantic_scholar_json(self.client.get(&s2))
            .await
            .ok()
            .filter(|json| json_doi_title_matches(json, &paper.title))
            .and_then(|json| abstract_from_semantic_scholar(&json))
            .and_then(|abs| usable_abstract_for_title(abs, &paper.title))
        {
            return Ok(Some(abs));
        }

        if let Some(abs) = self
            .fetch_openalex_json(&format!("https://api.openalex.org/works/doi:{doi}"), &[])
            .await
            .ok()
            .filter(|json| json_doi_title_matches(json, &paper.title))
            .and_then(|json| abstract_from_openalex(&json))
            .and_then(|abs| usable_abstract_for_title(abs, &paper.title))
        {
            return Ok(Some(abs));
        }

        let cr = self
            .client
            .get(format!("https://api.crossref.org/works/{doi}"));
        Ok(self
            .fetch_json(cr, RateLimitRetry::Enabled)
            .await
            .ok()
            .and_then(|json| json.get("message").cloned())
            .filter(|message| crossref_title_matches(message, &paper.title))
            .and_then(|message| abstract_from_crossref(&message))
            .and_then(|abs| usable_abstract_for_title(abs, &paper.title)))
    }

    async fn openalex_doi_abstracts(
        &self,
        dois: &[String],
        expected_titles: &HashMap<String, String>,
    ) -> Result<HashMap<String, String>> {
        let filter = format!("doi:{}", dois.join("|"));
        let per_page = dois.len().to_string();
        let json = self
            .fetch_openalex_json(
                "https://api.openalex.org/works",
                &[("filter", filter.as_str()), ("per-page", per_page.as_str())],
            )
            .await
            .map_err(crate::Error::Other)?;
        Ok(abstracts_from_openalex_works(&json, expected_titles))
    }

    async fn semantic_scholar_doi_abstracts(
        &self,
        dois: &[String],
        expected_titles: &HashMap<String, String>,
    ) -> Result<HashMap<String, String>> {
        let ids = dois
            .iter()
            .map(|doi| format!("DOI:{doi}"))
            .collect::<Vec<_>>();
        let req = self
            .client
            .post("https://api.semanticscholar.org/graph/v1/paper/batch")
            .query(&[("fields", "externalIds,title,abstract")])
            .json(&serde_json::json!({ "ids": ids }));
        let json = self
            .fetch_semantic_scholar_json(req)
            .await
            .map_err(crate::Error::Other)?;
        Ok(abstracts_from_semantic_scholar_batch(
            &json,
            expected_titles,
        ))
    }

    async fn api_by_title(&self, paper: &Paper) -> Result<Option<String>> {
        if let Some(abs) = self
            .fetch_openalex_json(
                "https://api.openalex.org/works",
                &[("search", paper.title.as_str()), ("per-page", "10")],
            )
            .await
            .ok()
            .and_then(|json| abstract_from_openalex_title_search(&json, paper))
            .and_then(|abs| usable_abstract_for_title(abs, &paper.title))
        {
            return Ok(Some(abs));
        }

        let s2_req = self
            .client
            .get("https://api.semanticscholar.org/graph/v1/paper/search/match")
            .query(&[
                ("query", paper.title.as_str()),
                ("limit", "5"),
                ("fields", "title,year,authors,abstract"),
            ]);
        Ok(self
            .fetch_semantic_scholar_json(s2_req)
            .await
            .ok()
            .and_then(|json| abstract_from_semantic_scholar_title_search(&json, paper))
            .and_then(|abs| usable_abstract_for_title(abs, &paper.title)))
    }

    async fn openreview_api_abstract(&self, paper: &Paper) -> Option<String> {
        if paper_source(paper) != Some(AbstractSource::Openreview) {
            return None;
        }
        let url = paper.url.as_deref()?;
        let forum_id = openreview_forum_id(url)?;
        for base in [
            "https://api2.openreview.net/notes",
            "https://api.openreview.net/notes",
        ] {
            let req = self
                .openreview_get(base)
                .await
                .query(&[("id", forum_id.as_str())]);
            let Ok(json) = self.fetch_json(req, RateLimitRetry::Enabled).await else {
                continue;
            };
            if let Some(abs) = abstracts_from_openreview_notes(&json)
                .get(&forum_id)
                .cloned()
                .and_then(|abs| usable_abstract_for_title(abs, &paper.title))
            {
                return Some(abs);
            }
        }
        None
    }

    async fn openreview_accepted_abstracts(
        &self,
        venue: &str,
        year: i32,
    ) -> Result<HashMap<String, String>> {
        let venue_id = format!("{venue}.cc/{year}/Conference");
        let mut last_err = None;
        for base in [
            "https://api2.openreview.net/notes",
            "https://api.openreview.net/notes",
        ] {
            match self
                .openreview_accepted_abstracts_from(base, &venue_id)
                .await
            {
                Ok(abstracts) if !abstracts.is_empty() => return Ok(abstracts),
                Ok(_) => {}
                Err(e) => last_err = Some(e),
            }
        }
        match last_err {
            Some(e) => Err(e),
            None => Ok(HashMap::new()),
        }
    }

    async fn openreview_accepted_abstracts_from(
        &self,
        base: &str,
        venue_id: &str,
    ) -> Result<HashMap<String, String>> {
        let mut out = HashMap::new();
        let mut offset = 0usize;
        loop {
            let req = self.openreview_get(base).await.query(&[
                ("content.venueid", venue_id),
                ("limit", &OPENREVIEW_PAGE_SIZE.to_string()),
                ("offset", &offset.to_string()),
            ]);
            let json = self
                .fetch_json(req, RateLimitRetry::Enabled)
                .await
                .map_err(crate::Error::Other)?;
            let page_len = json
                .get("notes")
                .and_then(|v| v.as_array())
                .map_or(0, Vec::len);
            out.extend(abstracts_from_openreview_notes(&json));
            if page_len < OPENREVIEW_PAGE_SIZE {
                break;
            }
            offset += OPENREVIEW_PAGE_SIZE;
        }
        Ok(out)
    }

    async fn openreview_get(&self, url: &str) -> reqwest::RequestBuilder {
        let req = self.client.get(url);
        if url.contains("api2.openreview.net") {
            if let Some(token) = self.openreview_login_token().await {
                return req.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
        }
        req
    }

    async fn openreview_login_token(&self) -> Option<String> {
        if let Some(token) = self.openreview_login_token.get() {
            return Some(token.clone());
        }
        let token = self.openreview_login().await;
        if let Some(token) = &token {
            let _ = self.openreview_login_token.set(token.clone());
        }
        token
    }

    async fn openreview_login(&self) -> Option<String> {
        let username = self.secrets.openreview_username.as_deref()?;
        let password = self.secrets.openreview_password.as_deref()?;
        let req = self
            .client
            .post("https://api2.openreview.net/login")
            .json(&serde_json::json!({
                "id": username,
                "password": password,
                "expiresIn": OPENREVIEW_LOGIN_EXPIRES_IN,
            }));
        let json = self.fetch_json(req, RateLimitRetry::Disabled).await.ok()?;
        json.get("token")
            .and_then(|token| token.as_str())
            .map(str::to_string)
    }

    async fn fetch_openalex_json(
        &self,
        url: &str,
        query: &[(&str, &str)],
    ) -> std::result::Result<Value, String> {
        let plain_req = || self.client.get(url).query(query);
        let req = match &self.secrets.openalex_api_key {
            Some(key) => plain_req().query(&[("api_key", key.as_str())]),
            None => plain_req(),
        };
        match self.fetch_json(req, RateLimitRetry::Disabled).await {
            Err(reason)
                if self.secrets.openalex_api_key.is_some()
                    && should_retry_without_api_key(&reason) =>
            {
                self.fetch_json(plain_req(), RateLimitRetry::Disabled).await
            }
            other => other,
        }
    }

    async fn fetch_semantic_scholar_json(
        &self,
        plain_req: reqwest::RequestBuilder,
    ) -> std::result::Result<Value, String> {
        let Some(key) = &self.secrets.semantic_scholar_key else {
            return self.fetch_json(plain_req, RateLimitRetry::Enabled).await;
        };
        let fallback = plain_req.try_clone();
        let req = plain_req.header("x-api-key", key);
        match self.fetch_json(req, RateLimitRetry::Enabled).await {
            Err(reason) if should_retry_without_api_key(&reason) => match fallback {
                Some(req) => self.fetch_json(req, RateLimitRetry::Enabled).await,
                None => Err(reason),
            },
            other => other,
        }
    }

    async fn fetch_json(
        &self,
        req: reqwest::RequestBuilder,
        retry_rate_limits: RateLimitRetry,
    ) -> std::result::Result<Value, String> {
        let rate_limit_req = req.try_clone();
        let mut resp = self.send(req).await?;
        if matches!(retry_rate_limits, RateLimitRetry::Enabled)
            && resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            if let Some(req) = rate_limit_req {
                tokio::time::sleep(rate_limit_delay(&resp)).await;
                resp = self.send(req).await?;
            }
        }
        if !resp.status().is_success() {
            return Err(http_status_reason("HTTP", resp.status()));
        }
        let Some(bytes) = read_body_limited(resp, MAX_JSON_BODY_BYTES).await else {
            return Err("response too large or unreadable".into());
        };
        serde_json::from_slice::<Value>(&bytes).map_err(|_| "invalid JSON response".into())
    }

    async fn send(
        &self,
        req: reqwest::RequestBuilder,
    ) -> std::result::Result<reqwest::Response, String> {
        let retry_req = req.try_clone();
        match req.send().await {
            Ok(resp) => Ok(resp),
            Err(_) => {
                let Some(req) = retry_req else {
                    return Err("request failed".into());
                };
                tokio::time::sleep(TRANSIENT_RETRY_DELAY).await;
                req.send()
                    .await
                    .map_err(|_| "request failed after retry".into())
            }
        }
    }

    async fn static_scrape(
        &self,
        paper: &Paper,
        source: Option<AbstractSource>,
    ) -> Result<EnrichResult> {
        let url = paper.url.as_deref().expect("paper URL checked by caller");
        let mut url = match allowed_static_url(url).await {
            Ok(url) => url,
            Err(reason) => return Ok(EnrichResult::Missing(reason.into())),
        };

        for _ in 0..=MAX_STATIC_REDIRECTS {
            let resp = match self.send(self.client.get(url.clone())).await {
                Ok(resp) => resp,
                Err(reason) => return Ok(EnrichResult::Missing(format!("static scrape {reason}"))),
            };
            if resp.status().is_redirection() {
                let Some(next_url) = redirect_url(&url, &resp) else {
                    return Ok(EnrichResult::Missing(
                        "redirect without valid location".into(),
                    ));
                };
                let next_url = match allowed_static_url(next_url.as_str()).await {
                    Ok(url) => url,
                    Err(reason) => return Ok(EnrichResult::Missing(reason.into())),
                };
                url = next_url;
                continue;
            }
            if !resp.status().is_success() {
                return Ok(EnrichResult::Missing(http_status_reason(
                    "static scrape HTTP",
                    resp.status(),
                )));
            }
            let Some(html) = read_text_limited(resp, MAX_HTML_BODY_BYTES).await else {
                return Ok(EnrichResult::Missing(
                    "static page too large or unreadable".into(),
                ));
            };
            if !static_page_title_matches(&html, paper) {
                return Ok(EnrichResult::Missing(
                    "static page title did not match paper".into(),
                ));
            }
            let source = source_from_static_url(&url).or(source);
            return Ok(
                match abstract_candidates(&html, source)
                    .into_iter()
                    .find_map(|abs| usable_abstract_for_title(abs, &paper.title))
                {
                    Some(abs) => EnrichResult::Found(abs),
                    None => EnrichResult::Missing("static page had no extractable abstract".into()),
                },
            );
        }

        Ok(EnrichResult::Missing("too many redirects".into()))
    }
}

fn source_from_static_url(url: &Url) -> Option<AbstractSource> {
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    match host.as_str() {
        "aclanthology.org" | "www.aclanthology.org" => Some(AbstractSource::Acl),
        "dl.acm.org" => Some(AbstractSource::Acm),
        "ieeexplore.ieee.org" => Some(AbstractSource::Ieee),
        "ijcai.org" | "www.ijcai.org" => Some(AbstractSource::Ijcai),
        "openaccess.thecvf.com" => Some(AbstractSource::Cvf),
        "openreview.net" | "www.openreview.net" => Some(AbstractSource::Openreview),
        "proceedings.mlr.press" => Some(AbstractSource::Pmlr),
        "neurips.cc" | "nips.cc" => Some(AbstractSource::Neurips),
        "link.springer.com" => Some(AbstractSource::Springer),
        "ndss-symposium.org" | "www.ndss-symposium.org" => Some(AbstractSource::Ndss),
        "usenix.org" | "www.usenix.org" => Some(AbstractSource::Usenix),
        _ if host.ends_with(".neurips.cc") || host.ends_with(".nips.cc") => {
            Some(AbstractSource::Neurips)
        }
        _ => None,
    }
}

fn source_from_paper_url(raw: &str) -> Option<AbstractSource> {
    let url = parse_static_url(raw)?;
    source_from_doi_url(&url).or_else(|| source_from_static_url(&url))
}

fn source_from_doi_url(url: &Url) -> Option<AbstractSource> {
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    if host != "doi.org" && host != "dx.doi.org" {
        return None;
    }
    let doi = url.path().trim_start_matches('/').to_ascii_lowercase();
    match doi.as_str() {
        doi if doi.starts_with("10.1145/") => Some(AbstractSource::Acm),
        doi if doi.starts_with("10.18653/") => Some(AbstractSource::Acl),
        doi if doi.starts_with("10.1109/") => Some(AbstractSource::Ieee),
        doi if doi.starts_with("10.24963/") => Some(AbstractSource::Ijcai),
        doi if doi.starts_with("10.1007/") => Some(AbstractSource::Springer),
        doi if doi.starts_with("10.14722/") => Some(AbstractSource::Ndss),
        _ => None,
    }
}

fn paper_source(paper: &Paper) -> Option<AbstractSource> {
    paper.url.as_deref().and_then(source_from_paper_url)
}

fn cached_openreview_abstract(
    cache: &HashMap<(String, i32), HashMap<String, String>>,
    paper: &Paper,
) -> Option<String> {
    if paper_source(paper) != Some(AbstractSource::Openreview) {
        return None;
    }
    let forum_id = openreview_forum_id(paper.url.as_deref()?)?;
    cache
        .get(&(paper.venue.clone(), paper.year))?
        .get(&forum_id)
        .cloned()
        .and_then(|abs| usable_abstract_for_title(abs, &paper.title))
}

fn cached_doi_abstract(cache: &HashMap<String, Option<String>>, paper: &Paper) -> Option<String> {
    let doi = paper.doi.as_deref().and_then(normalized_doi)?;
    cache.get(&doi).cloned().flatten()
}

fn http_status_reason(prefix: &str, status: reqwest::StatusCode) -> String {
    let label = match status.as_u16() {
        401 | 403 => "blocked",
        429 => "rate limited",
        500..=599 => "server error",
        _ => "request failed",
    };
    format!("{prefix} {} ({label})", status.as_u16())
}

fn should_retry_without_api_key(reason: &str) -> bool {
    reason.starts_with("HTTP 401")
        || reason.starts_with("HTTP 403")
        || reason.starts_with("HTTP 429")
}

fn rate_limit_delay(resp: &reqwest::Response) -> Duration {
    let headers = resp.headers();
    let secs = header_seconds(headers, header::RETRY_AFTER)
        .or_else(|| header_seconds(headers, "ratelimit-reset"))
        .unwrap_or(60);
    Duration::from_secs(secs.clamp(1, MAX_RATE_LIMIT_SLEEP.as_secs()))
}

fn header_seconds(headers: &header::HeaderMap, name: impl header::AsHeaderName) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

async fn allowed_static_url(raw: &str) -> std::result::Result<Url, &'static str> {
    let url = parse_static_url(raw).ok_or("URL rejected by static scraper")?;
    let host = url.host_str().ok_or("URL rejected by static scraper")?;
    let port = url
        .port_or_known_default()
        .ok_or("URL rejected by static scraper")?;
    if let Some(ip) = parse_host_ip(host) {
        return is_public_ip(ip)
            .then_some(url)
            .ok_or("URL rejected by static scraper");
    }
    let addrs = match tokio::net::lookup_host((host, port)).await {
        Ok(addrs) => addrs,
        Err(_) => {
            tokio::time::sleep(TRANSIENT_RETRY_DELAY).await;
            tokio::net::lookup_host((host, port))
                .await
                .map_err(|_| "static URL DNS lookup failed")?
        }
    };
    let mut has_addr = false;
    for addr in addrs {
        has_addr = true;
        if !is_public_ip(addr.ip()) {
            return Err("URL rejected by static scraper");
        }
    }
    has_addr
        .then_some(url)
        .ok_or("static URL DNS lookup returned no addresses")
}

fn parse_static_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let host = url.host_str()?;
    if is_localhost(host) {
        return None;
    }
    if let Some(ip) = parse_host_ip(host) {
        return is_public_ip(ip).then_some(url);
    }
    Some(url)
}

fn openreview_forum_id(raw: &str) -> Option<String> {
    let url = parse_static_url(raw)?;
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    if host != "openreview.net" && host != "www.openreview.net" {
        return None;
    }
    if url.path() != "/forum" {
        return None;
    }
    url.query_pairs()
        .find_map(|(key, value)| (key == "id" && !value.is_empty()).then(|| value.into_owned()))
}

fn parse_host_ip(host: &str) -> Option<IpAddr> {
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.parse().ok()
}

fn is_localhost(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "localhost" || host.ends_with(".localhost")
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_multicast()
                || ip.is_documentation())
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            let first = segments[0];
            let is_unique_local = (first & 0xfe00) == 0xfc00;
            let is_link_local = (first & 0xffc0) == 0xfe80;
            let is_documentation = segments[0] == 0x2001 && segments[1] == 0x0db8;
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || is_unique_local
                || is_link_local
                || is_documentation)
        }
    }
}

fn redirect_url(base: &Url, resp: &reqwest::Response) -> Option<Url> {
    let location = resp.headers().get(header::LOCATION)?.to_str().ok()?;
    base.join(location).ok()
}

async fn read_text_limited(resp: reqwest::Response, limit: usize) -> Option<String> {
    let bytes = read_body_limited(resp, limit).await?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

async fn read_body_limited(mut resp: reqwest::Response, limit: usize) -> Option<Vec<u8>> {
    if resp.content_length().is_some_and(|len| len > limit as u64) {
        return None;
    }

    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.ok()? {
        if body.len().checked_add(chunk.len())? > limit {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paper(title: &str) -> Paper {
        Paper {
            dblp_key: "conf/iclr/example".into(),
            venue: "ICLR".into(),
            year: 2024,
            title: title.into(),
            authors: "Nicklas Hansen, Hao Su, Xiaolong Wang".into(),
            doi: None,
            url: Some("https://openreview.net/forum?id=Oxh5CstDJU".into()),
            abstract_text: None,
        }
    }

    fn fixture_abstract(prefix: &str) -> String {
        format!(
            "{prefix} This additional sentence provides enough detail to look like a real paper abstract. It also states the result and its significance."
        )
    }

    fn extract_abstract_html(html: &str, source: Option<AbstractSource>) -> Option<String> {
        abstract_candidates(html, source).into_iter().next()
    }

    #[test]
    fn http_status_reason_labels_blocked_and_rate_limited() {
        assert_eq!(
            http_status_reason("HTTP", reqwest::StatusCode::TOO_MANY_REQUESTS),
            "HTTP 429 (rate limited)"
        );
        assert_eq!(
            http_status_reason("static scrape HTTP", reqwest::StatusCode::FORBIDDEN),
            "static scrape HTTP 403 (blocked)"
        );
    }

    #[test]
    fn api_key_fallback_is_limited_to_auth_or_rate_statuses() {
        assert!(should_retry_without_api_key("HTTP 403 (blocked)"));
        assert!(should_retry_without_api_key("HTTP 429 (rate limited)"));
        assert!(!should_retry_without_api_key("HTTP 404 (request failed)"));
    }

    #[test]
    fn numeric_dois_are_normalized() {
        assert_eq!(
            normalized_doi("https://doi.org/10.1145/3611643.3613892").as_deref(),
            Some("10.1145/3611643.3613892")
        );
        assert_eq!(normalized_doi(" "), None);
    }

    #[test]
    fn header_seconds_parses_retry_after() {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::RETRY_AFTER, header::HeaderValue::from_static("120"));
        assert_eq!(header_seconds(&headers, header::RETRY_AFTER), Some(120));
    }

    #[test]
    fn openalex_inverted_index() {
        let work = json!({
            "abstract_inverted_index": {
                "We": [0], "fuzz": [1], "the": [2], "kernel": [3]
            }
        });
        assert_eq!(
            abstract_from_openalex(&work).as_deref(),
            Some("We fuzz the kernel")
        );
    }

    #[test]
    fn openalex_plain_abstract() {
        let work = json!({ "abstract": "Direct text." });
        assert_eq!(
            abstract_from_openalex(&work).as_deref(),
            Some("Direct text.")
        );
    }

    #[test]
    fn text_sanitization_preserves_angle_bracket_notation() {
        assert_eq!(
            non_empty_text("Use <EOS> when k<n, Vec<T>, and &amp; delimiters.").as_deref(),
            Some("Use <EOS> when k<n, Vec<T>, and & delimiters.")
        );
    }

    #[test]
    fn rejects_observed_placeholder_abstracts() {
        for placeholder in [
            "Abstract",
            "TODO",
            "International audience",
            "peer reviewed",
            "Challenge Report",
            "No abstract available.",
            "status: Published",
            "Warning: This paper contains examples of language that some people may find offensive.",
            "Explain this framework to me in detail and in chronological order. I am an aspiring consultant and I need to know this.",
            "Explain this framework to me in detail and in chronological order.I am an aspiring consultant and I need to know this.",
            "Promoting openness in scientific communication and the peer-review process",
            "Promoting openness in scientific communication and the peer-review process | OpenReview",
            "Electronic proceedings of IJCAI 2024 with additional publication information",
            "Paper presented at a workshop with enough bibliographic details to exceed the minimum accepted abstract length by a wide margin.",
            "This repository contains code, datasets, devices, and usage instructions for reproducing the paper's experiments and released models.",
            "'Artificial Intelligence Enhancement for Remote Virtual Consultations in Healthcare Provision for Patients with Chronic Conditions' published in 'HCI International 2023 Posters'",
            "extended-abstract Share on Paper title Authors Info & Claims Alice Example View Profile Article No.: 1 Get Citation Alerts Save to Binder Export Citation Publisher Site Get Access",
            "Alice Example. A paper title. In: Findings of the Association for Computational Linguistics: EMNLP 2023. 2023: 1-10.",
            "https:",
            "https://doi.org/10.18653/v1/2023.acl-long.361",
            "http://10.0.72.221/v1/2023.emnlp-main.194",
        ] {
            assert!(abstract_text(placeholder).is_none(), "{placeholder}");
        }
    }

    #[test]
    fn sanitizes_and_rejects_observed_bad_abstracts() {
        let abs = fixture_abstract("Clean abstract.");
        assert_eq!(
            abstract_text(format!("Abstract. {abs}")).as_deref(),
            Some(abs.as_str())
        );
        assert_eq!(
            abstract_text(format!("ABSTRACT {abs}")).as_deref(),
            Some(abs.as_str())
        );
        assert_eq!(
            abstract_text(format!("{abs} CCS CONCEPTS • Security and privacy")).as_deref(),
            Some(abs.as_str())
        );
        assert_eq!(
            abstract_text(format!("{abs} Ccs Concepts• General and reference")).as_deref(),
            Some(abs.as_str())
        );
        assert_eq!(
            abstract_text(format!(
                "{abs} © 2024 Association for Computational Linguistics."
            ))
            .as_deref(),
            Some(abs.as_str())
        );
        assert!(
            abstract_text(format!("{abs} Copy to Clipboard Download Related Material")).is_none()
        );
        assert_eq!(
            abstract_text(format!("<p>{abs}</p>")).as_deref(),
            Some(abs.as_str())
        );
        assert!(abstract_text(format!("{abs}...")).is_none());
        assert!(abstract_text("x".repeat(MAX_ABSTRACT_CHARS + 1)).is_none());
        assert!(abstract_text(format!("{abs} invalid � character")).is_none());
        let short = "We introduce a compact benchmark and show that it improves accuracy on three evaluation datasets.";
        assert_eq!(abstract_text(short).as_deref(), Some(short));
        assert!(abstract_text(short.trim_end_matches('.')).is_none());
        assert!(usable_abstract_for_title(
            "A Paper: With Punctuation.".into(),
            "A Paper - With Punctuation"
        )
        .is_none());
    }

    #[test]
    fn abstract_length_limits_count_characters() {
        assert!(abstract_text(format!("{}.", "界".repeat(30))).is_none());
        assert!(abstract_text(format!("{}.", "界".repeat(2_000))).is_some());
    }

    #[test]
    fn doi_title_matching_accepts_only_subtitle_boundaries() {
        assert!(doi_titles_match(
            "Privacy Interventions and Education",
            "Privacy Interventions and Education: Encouraging Protective Behavior"
        ));
        assert!(doi_titles_match(
            "A Study: Methods",
            "A Study: Methods: Methods"
        ));
        assert!(!doi_titles_match(
            "Privacy Interventions",
            "Privacy Interventions and Education"
        ));
        assert!(!doi_titles_match("A Study: Methods", "A Study: Results"));
    }

    #[test]
    fn doi_batch_helpers_require_matching_doi_and_title() {
        let expected = HashMap::from([("10.1000/right".to_string(), "Expected Title".to_string())]);
        let right = fixture_abstract("Right abstract.");
        let wrong = fixture_abstract("Wrong abstract.");
        let openalex = json!({
            "results": [
                {"doi": "https://doi.org/10.1000/wrong", "title": "Expected Title", "abstract": wrong},
                {"doi": "https://doi.org/10.1000/right", "title": "Wrong Title", "abstract": wrong},
                {"doi": "https://doi.org/10.1000/right", "title": "Expected Title.", "abstract": right}
            ]
        });
        assert_eq!(
            abstracts_from_openalex_works(&openalex, &expected)
                .get("10.1000/right")
                .map(String::as_str),
            Some(right.as_str())
        );

        let s2 = json!([
            {"externalIds": {"DOI": "10.1000/wrong"}, "title": "Expected Title", "abstract": wrong},
            {"externalIds": {"DOI": "10.1000/right"}, "title": "Wrong Title", "abstract": wrong},
            {"externalIds": {"DOI": "10.1000/right"}, "title": "Expected Title.", "abstract": right}
        ]);
        assert_eq!(
            abstracts_from_semantic_scholar_batch(&s2, &expected)
                .get("10.1000/right")
                .map(String::as_str),
            Some(right.as_str())
        );
    }

    #[test]
    fn failed_doi_batches_remain_retryable() {
        let dois = ["hit", "miss", "retry"].map(str::to_string);
        let mut cache = HashMap::new();
        store_doi_batch_results(
            &mut cache,
            &dois,
            HashMap::from([("hit".into(), "abstract".into())]),
            &HashSet::from(["retry".into()]),
        );

        assert_eq!(cache.get("hit"), Some(&Some("abstract".into())));
        assert_eq!(cache.get("miss"), Some(&None));
        assert!(!cache.contains_key("retry"));
    }

    #[test]
    fn static_page_citation_title_must_match() {
        let mut expected = paper("A Paper: With Punctuation.");
        assert!(static_page_title_matches(
            r#"<meta name="citation_title" content="A Paper - With Punctuation">"#,
            &expected
        ));
        assert!(!static_page_title_matches(
            r#"<meta name="citation_title" content="Another Paper">"#,
            &expected
        ));
        assert!(!static_page_title_matches(
            r#"<meta name="citation_title" content="A Paper">"#,
            &expected
        ));
        expected.doi = Some("10.1000/example".into());
        assert!(static_page_title_matches(
            r#"<meta name="citation_title" content="A Paper"><meta name="citation_doi" content="https://doi.org/10.1000/example">"#,
            &expected
        ));
        assert!(!static_page_title_matches(
            r#"<meta name="citation_title" content="A Paper"><meta name="citation_doi" content="10.1000/wrong">"#,
            &expected
        ));
        assert!(static_page_title_matches("<html></html>", &expected));
    }

    #[test]
    fn openalex_title_search_requires_identity_match() {
        let results = json!({
            "results": [
                {
                    "title": "TD-MPC2: Scalable, Robust World Models for Continuous Control",
                    "publication_year": 2024,
                    "authorships": [{"author": {"display_name": "Wrong Author"}}],
                    "abstract": "Wrong author abstract."
                },
                {
                    "title": "TD-MPC2: Scalable, Robust World Models for Continuous Control",
                    "publication_year": 2023,
                    "authorships": [{"author": {"display_name": "Nicklas Hansen"}}],
                    "abstract": "Right abstract."
                }
            ]
        });
        assert_eq!(
            abstract_from_openalex_title_search(
                &results,
                &paper("TD-MPC2: Scalable, Robust World Models for Continuous Control.")
            )
            .as_deref(),
            Some("Right abstract.")
        );
        assert_eq!(
            abstract_from_openalex_title_search(&results, &paper("Different Title")),
            None
        );
    }

    #[test]
    fn semantic_scholar_title_search_requires_identity_match() {
        let results = json!({
            "data": [
                {
                    "title": "TD-MPC2: Scalable, Robust World Models for Continuous Control",
                    "year": 2024,
                    "authors": [{"name": "Wrong Author"}],
                    "abstract": "Wrong author abstract."
                },
                {
                    "title": "TD-MPC2: Scalable, Robust World Models for Continuous Control",
                    "year": 2023,
                    "authors": [{"name": "Nicklas Hansen"}],
                    "abstract": "Right abstract."
                }
            ]
        });
        assert_eq!(
            abstract_from_semantic_scholar_title_search(
                &results,
                &paper("TD-MPC2: Scalable, Robust World Models for Continuous Control.")
            )
            .as_deref(),
            Some("Right abstract.")
        );
    }

    #[test]
    fn semantic_scholar_abstract() {
        assert_eq!(
            abstract_from_semantic_scholar(&json!({"abstract": "Semantic Scholar abstract."}))
                .as_deref(),
            Some("Semantic Scholar abstract.")
        );
        assert!(abstract_from_semantic_scholar(&json!({"abstract": null})).is_none());
    }

    #[test]
    fn openreview_batch_abstracts_indexes_id_and_forum() {
        let notes = json!({
            "notes": [
                {"id": "v2-id", "forum": "v2-forum", "content": {"abstract": {"value": "V2 abstract."}}},
                {"id": "v1-id", "forum": "v1-forum", "content": {"abstract": "V1 abstract."}}
            ]
        });
        let abstracts = abstracts_from_openreview_notes(&notes);
        assert_eq!(
            abstracts.get("v2-id").map(String::as_str),
            Some("V2 abstract.")
        );
        assert_eq!(
            abstracts.get("v2-forum").map(String::as_str),
            Some("V2 abstract.")
        );
        assert_eq!(
            abstracts.get("v1-id").map(String::as_str),
            Some("V1 abstract.")
        );
        assert_eq!(
            abstracts.get("v1-forum").map(String::as_str),
            Some("V1 abstract.")
        );
    }

    #[test]
    fn cached_openreview_abstract_uses_forum_id() {
        let abs = fixture_abstract("Cached abstract.");
        let mut cache = HashMap::new();
        cache.insert(
            ("ICLR".to_string(), 2024),
            HashMap::from([("Oxh5CstDJU".to_string(), abs.clone())]),
        );
        assert_eq!(
            cached_openreview_abstract(
                &cache,
                &paper("TD-MPC2: Scalable, Robust World Models for Continuous Control.")
            )
            .as_deref(),
            Some(abs.as_str())
        );
        let mut pmlr = paper("TD-MPC2: Scalable, Robust World Models for Continuous Control.");
        pmlr.url = Some("https://proceedings.mlr.press/v1/example.html".into());
        assert_eq!(cached_openreview_abstract(&cache, &pmlr), None);
    }

    #[tokio::test]
    async fn enrich_many_uses_cached_openreview_abstracts() {
        let abs = fixture_abstract("Cached abstract.");
        let mut enricher = Enricher::new(Secrets::default());
        enricher.openreview_cache.insert(
            ("ICLR".to_string(), 2024),
            HashMap::from([("Oxh5CstDJU".to_string(), abs.clone())]),
        );

        let results = enricher
            .enrich_many(
                vec![paper(
                    "TD-MPC2: Scalable, Robust World Models for Continuous Control.",
                )],
                1,
            )
            .await;

        let (_, result) = results.into_iter().next().unwrap();
        match result.unwrap() {
            EnrichResult::Found(found) => assert_eq!(found, abs),
            EnrichResult::Missing(reason) => panic!("expected cached abstract, got {reason}"),
        }
    }

    #[test]
    fn openreview_forum_id_from_url() {
        assert_eq!(
            openreview_forum_id("https://openreview.net/forum?id=8EtSBX41mt").as_deref(),
            Some("8EtSBX41mt")
        );
        assert!(openreview_forum_id("https://example.com/forum?id=8EtSBX41mt").is_none());
    }

    #[test]
    fn static_source_uses_resolved_url_provider() {
        assert_eq!(
            source_from_static_url(&Url::parse("https://www.usenix.org/conference/x").unwrap()),
            Some(AbstractSource::Usenix)
        );
        assert_eq!(
            source_from_static_url(&Url::parse("https://dl.acm.org/doi/10.1145/x").unwrap()),
            Some(AbstractSource::Acm)
        );
        assert_eq!(
            source_from_static_url(&Url::parse("https://aclanthology.org/D19-1593/").unwrap()),
            Some(AbstractSource::Acl)
        );
        assert_eq!(
            source_from_static_url(
                &Url::parse("https://www.ijcai.org/proceedings/2024/1").unwrap()
            ),
            Some(AbstractSource::Ijcai)
        );
        assert_eq!(
            source_from_static_url(&Url::parse("https://papers.nips.cc/paper/1").unwrap()),
            Some(AbstractSource::Neurips)
        );
        assert_eq!(
            source_from_static_url(&Url::parse("https://example.com/paper").unwrap()),
            None
        );
    }

    #[test]
    fn paper_url_source_handles_mixed_provider_venues() {
        assert_eq!(
            source_from_paper_url("https://www.usenix.org/conference/raid2019/presentation/chiba"),
            Some(AbstractSource::Usenix)
        );
        assert_eq!(
            source_from_paper_url("https://doi.org/10.1007/978-3-030-00470-5_1"),
            Some(AbstractSource::Springer)
        );
        assert_eq!(
            source_from_paper_url("https://doi.org/10.1145/3678890.3678926"),
            Some(AbstractSource::Acm)
        );
        assert_eq!(
            source_from_paper_url("https://doi.org/10.18653/V1/D19-1593"),
            Some(AbstractSource::Acl)
        );
        assert_eq!(
            source_from_paper_url("https://doi.org/10.1109/RAID67961.2025.00012"),
            Some(AbstractSource::Ieee)
        );
        assert_eq!(
            source_from_paper_url("https://doi.org/10.24963/ijcai.2024/1"),
            Some(AbstractSource::Ijcai)
        );
    }

    #[test]
    fn crossref_strips_jats() {
        let msg = json!({"abstract": "<jats:p>A <jats:bold>bold</jats:bold> claim.</jats:p>"});
        assert_eq!(
            abstract_from_crossref(&msg).as_deref(),
            Some("A bold claim.")
        );
    }

    #[test]
    fn html_acm_source_specific_selector() {
        let html = r#"<html><body>
            <div class="abstractInFull"><p>This is the ACM abstract.</p></div>
        </body></html>"#;
        assert_eq!(
            extract_abstract_html(html, Some(AbstractSource::Acm)).as_deref(),
            Some("This is the ACM abstract.")
        );
    }

    #[test]
    fn html_acl_source_specific_selector() {
        let html = r#"<html><body>
            <meta property="og:description" content="Alice. Proceedings of the 2024 Conference. 2024.">
            <div class="card-body acl-abstract">
                <h5>Abstract</h5><span>This is the ACL abstract.</span>
            </div>
        </body></html>"#;
        assert_eq!(
            extract_abstract_html(html, Some(AbstractSource::Acl)).as_deref(),
            Some("This is the ACL abstract.")
        );
    }

    #[test]
    fn html_acl_source_ignores_citation_meta() {
        let html = r#"<html><head>
            <meta property="og:description" content="Alice. Proceedings of the 2024 Conference. 2024.">
        </head><body></body></html>"#;
        assert!(extract_abstract_html(html, Some(AbstractSource::Acl))
            .and_then(|raw| usable_abstract_for_title(raw, "A paper title"))
            .is_none());
    }

    #[test]
    fn html_skips_unusable_candidates() {
        let valid = fixture_abstract("Valid fallback.");
        let html = format!(
            r#"<html><head>
                <meta name="citation_abstract" content="TODO">
                <meta property="og:description" content="{valid}">
            </head><body><div id="abstract">Abstract</div></body></html>"#
        );
        assert_eq!(
            abstract_candidates(&html, Some(AbstractSource::Cvf))
                .into_iter()
                .find_map(|raw| usable_abstract_for_title(raw, "A paper title"))
                .as_deref(),
            Some(valid.as_str())
        );
    }

    #[test]
    fn html_ijcai_source_specific_selector() {
        let html = r#"<div class="proceedings-detail">
                <hr><div class="row">
                    <div class="col-md-12">This is the IJCAI abstract.</div>
                    <div class="col-md-12"><div class="keywords">Keywords: planning</div></div>
                </div>
            </div>"#;
        assert_eq!(
            extract_abstract_html(html, Some(AbstractSource::Ijcai)).as_deref(),
            Some("This is the IJCAI abstract.")
        );
    }

    #[test]
    fn html_ndss_strips_authors_from_paper_data() {
        let html = r#"<html><body>
            <div class="paper-data">
                <p><strong><p>Alice A (Example U), Bob B (Example Labs)</p></strong></p>
                <p>
                    <p>First abstract paragraph.</p>
                    <p>Second abstract paragraph.</p>
                </p>
            </div>
        </body></html>"#;
        let abstract_text = extract_abstract_html(html, Some(AbstractSource::Ndss)).unwrap();
        assert_eq!(
            abstract_text,
            "First abstract paragraph. Second abstract paragraph."
        );
        assert!(!abstract_text.contains("Alice A"));
        assert!(!abstract_text.contains("Bob B"));
    }

    #[test]
    fn html_usenix_source_specific_selector() {
        let html = r#"<html><body>
            <div class="field field-name-field-paper-people-text"><p>Alice A and Bob B</p></div>
            <div class="field field-name-field-paper-description"><p>This is the USENIX abstract.</p></div>
        </body></html>"#;
        assert_eq!(
            extract_abstract_html(html, Some(AbstractSource::Usenix)).as_deref(),
            Some("This is the USENIX abstract.")
        );
    }

    #[test]
    fn html_neurips_source_specific_selector() {
        let html = r#"<html><body>
            <section class="paper-section">
                <h2 class="section-label">Abstract</h2>
                <p class="paper-abstract">
                    This avoids <em>loss of plasticity</em>, and proposes
                    <strong>DASH</strong>, a robust method.
                </p>
            </section>
        </body></html>"#;
        assert_eq!(
            extract_abstract_html(html, Some(AbstractSource::Neurips)).as_deref(),
            Some("This avoids loss of plasticity, and proposes DASH, a robust method.")
        );
    }

    #[test]
    fn html_neurips_datasets_benchmarks_abstract() {
        let html = r#"<html><body>
            <h4>Abstract</h4>
            <p><p>Datasets and Benchmarks abstract text.</p></p>
        </body></html>"#;
        assert_eq!(
            extract_abstract_html(html, Some(AbstractSource::Neurips)).as_deref(),
            Some("Datasets and Benchmarks abstract text.")
        );
    }

    #[test]
    fn html_pmlr_prefers_full_page_abstract_over_truncated_meta() {
        let html = r#"<html><head>
            <meta name="description" content="Truncated PMLR abstract...">
        </head><body>
            <h4>Abstract</h4>
            <div id="abstract" class="abstract">
                First full ICML abstract sentence. Second full sentence.
            </div>
        </body></html>"#;
        assert_eq!(
            extract_abstract_html(html, Some(AbstractSource::Pmlr)).as_deref(),
            Some("First full ICML abstract sentence. Second full sentence.")
        );
    }

    #[test]
    fn html_openreview_source_reads_iclr_virtual_abstract() {
        let html = r#"<html><body>
            <div class="abstract-section">
                <div class="abstract-text-inner">
                    <p>ICLR virtual abstract text.</p>
                </div>
            </div>
        </body></html>"#;
        assert_eq!(
            extract_abstract_html(html, Some(AbstractSource::Openreview)).as_deref(),
            Some("ICLR virtual abstract text.")
        );
    }

    #[test]
    fn html_ieee_prefers_abstract_text_selector() {
        let html = r#"<html><head>
            <meta property="og:description" content="IEEE fallback abstract.">
        </head><body>
            <div class="abstract-text">This is the IEEE abstract.</div>
        </body></html>"#;
        assert_eq!(
            extract_abstract_html(html, Some(AbstractSource::Ieee)).as_deref(),
            Some("This is the IEEE abstract.")
        );
    }

    #[test]
    fn html_springer_prefers_full_abstract_section_over_truncated_meta() {
        let html = r#"<html><head>
            <meta property="og:description" content="Truncated Springer abstract...">
        </head><body>
            <section data-title="Abstract">
                <div class="c-article-section__content">
                    <p>This is the full Springer abstract.</p>
                    <p>It has a second sentence.</p>
                </div>
            </section>
        </body></html>"#;
        assert_eq!(
            extract_abstract_html(html, Some(AbstractSource::Springer)).as_deref(),
            Some("This is the full Springer abstract. It has a second sentence.")
        );
    }

    #[test]
    fn html_generic_meta_fallback() {
        let html = r#"<html><head>
            <meta property="og:description" content="Fallback abstract here.">
        </head><body></body></html>"#;
        assert_eq!(
            extract_abstract_html(html, None).as_deref(),
            Some("Fallback abstract here.")
        );
    }

    #[test]
    fn html_cvf_source_specific_selector() {
        let html = r#"<html><body>
            <div id="abstract">CVF-style abstract text.</div>
        </body></html>"#;
        assert_eq!(
            extract_abstract_html(html, Some(AbstractSource::Cvf)).as_deref(),
            Some("CVF-style abstract text.")
        );
    }

    #[test]
    fn html_meta_fallback_decodes_entities() {
        let html = r#"<html><head>
            <meta property="og:description" content="A&amp;#160;B &amp;amp; C &#8217;">
        </head><body></body></html>"#;
        assert_eq!(
            extract_abstract_html(html, None).as_deref(),
            Some("A B & C ’")
        );
    }

    #[test]
    fn html_ignores_punctuation_only_text() {
        assert!(non_empty_text(",").is_none());
    }

    #[test]
    fn html_ignores_proceedings_boilerplate() {
        assert!(abstract_text("Electronic proceedings of IJCAI 2024").is_none());
    }

    #[test]
    fn html_strips_abstract_heading_prefix() {
        assert_eq!(
            non_empty_text("Abstract: This is the real abstract.").as_deref(),
            Some("This is the real abstract.")
        );
        assert_eq!(
            non_empty_text("Abstract meaning representation is a graph.").as_deref(),
            Some("Abstract meaning representation is a graph.")
        );
        assert_eq!(
            non_empty_text("Abstract Meaning Representation is a graph.").as_deref(),
            Some("Abstract Meaning Representation is a graph.")
        );
        assert_eq!(
            non_empty_text("Abstract We present a new method.").as_deref(),
            Some("We present a new method.")
        );
    }

    #[test]
    fn html_no_abstract() {
        assert!(extract_abstract_html("<html></html>", None).is_none());
    }

    #[test]
    fn static_url_rejects_local_or_non_http_targets() {
        assert!(parse_static_url("https://example.com/paper").is_some());
        assert!(parse_static_url("file:///etc/passwd").is_none());
        assert!(parse_static_url("http://localhost/paper").is_none());
        assert!(parse_static_url("http://127.0.0.1/paper").is_none());
        assert!(parse_static_url("http://[::1]/paper").is_none());
        assert!(parse_static_url("https://user@example.com/paper").is_none());
    }
}
