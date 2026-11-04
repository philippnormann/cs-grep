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
use crate::output::terminal_safe;
use crate::{Paper, Result};

const MAX_JSON_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_HTML_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_STATIC_REDIRECTS: usize = 5;
const MAX_RATE_LIMIT_SLEEP: Duration = Duration::from_secs(65);
const OPENREVIEW_BATCH_SIZE: usize = 500;
const OPENREVIEW_LOGIN_EXPIRES_IN: u64 = 7 * 24 * 60 * 60;
const OPENREVIEW_API_V1: usize = 0;
const OPENREVIEW_API_V2: usize = 1;
const SEMANTIC_SCHOLAR_BATCH_SIZE: usize = 500;
const OPENALEX_BATCH_SIZE: usize = 50;
const TRANSIENT_RETRY_DELAY: Duration = Duration::from_millis(250);
const STATIC_DNS_FAILED: &str = "static URL DNS lookup failed";
const MIN_ABSTRACT_CHARS: usize = 80;
const MAX_ABSTRACT_CHARS: usize = 6_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum AbstractSource {
    Acl,
    Cvf,
    Iclr,
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

/// Work ids of the first three autocomplete hits whose title matches.
fn openalex_title_hits(results: &Value, title: &str) -> Vec<String> {
    results
        .get("results")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter(|hit| {
            hit.get("display_name")
                .and_then(|v| v.as_str())
                .is_some_and(|name| titles_match(name, title))
        })
        .filter_map(|hit| {
            hit.get("id")?
                .as_str()?
                .rsplit('/')
                .next()
                .map(str::to_string)
        })
        .take(3)
        .collect()
}

/// Same title, year within one, a DBLP surname among the first three OpenAlex
/// authors, and not a thesis, editorial or the like.
fn openalex_identity_matches(work: &Value, paper: &Paper) -> bool {
    let title = work.get("title").and_then(|v| v.as_str());
    let year = work.get("publication_year").and_then(|v| v.as_i64());
    let api_authors = normalized_match_text(
        &work
            .get("authorships")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .take(3)
            .filter_map(|a| a.get("author")?.get("display_name")?.as_str())
            .collect::<Vec<_>>()
            .join(" "),
    );
    !matches!(
        work.get("type").and_then(|v| v.as_str()),
        Some("dissertation" | "editorial" | "erratum" | "paratext" | "retraction" | "peer-review")
    ) && title.is_some_and(|title| titles_match(title, &paper.title))
        && year.is_some_and(|year| (year - i64::from(paper.year)).abs() <= 1)
        && paper.authors.split(',').any(|author| {
            normalized_match_text(author)
                .split_whitespace()
                .last()
                .is_some_and(|surname| api_authors.split_whitespace().any(|name| name == surname))
        })
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

/// Titles compare without spaces or punctuation: "BIS i" = "BISi", "3-D" = "3D".
fn titles_match(actual: &str, expected: &str) -> bool {
    normalized_match_text(actual).replace(' ', "")
        == normalized_match_text(expected).replace(' ', "")
}

fn doi_titles_match(actual: &str, expected: &str) -> bool {
    titles_match(actual, expected)
        || subtitle_prefix_matches(actual, expected)
        || subtitle_prefix_matches(expected, actual)
}

fn subtitle_prefix_matches(longer: &str, shorter: &str) -> bool {
    [":", " - ", " – ", " — ", "?"].iter().any(|delimiter| {
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

pub(crate) fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Extract abstract candidates from a publisher page in priority order.
fn abstract_candidates(html: &str, source: Option<AbstractSource>) -> Vec<String> {
    let doc = Html::parse_document(html);

    let mut candidates = match source {
        Some(AbstractSource::Acl) => selector_texts(&doc, &[".acl-abstract span"]),
        Some(AbstractSource::Cvf) => selector_texts(&doc, &["div#abstract"]),
        Some(AbstractSource::Ijcai) => {
            let text = selector_texts(&doc, &[".article__abstract > p:not(.eyebrow)"]).join(" ");
            non_empty_text(text)
                .into_iter()
                .chain(selector_texts(
                    &doc,
                    &[".proceedings-detail hr + .row > .col-md-12"],
                ))
                .collect()
        }
        Some(AbstractSource::Ndss) => extract_ndss_abstract(&doc).into_iter().collect(),
        Some(AbstractSource::Iclr | AbstractSource::Neurips) => {
            extract_neurips_abstract(&doc).into_iter().collect()
        }
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
    candidates.extend(abstract_after_heading(&doc));
    candidates
}

/// Text after the first `<h2>`-`<h4>` "Abstract" heading, up to the next heading or list among its siblings.
fn abstract_after_heading(doc: &Html) -> Option<String> {
    let selector = Selector::parse("h2, h3, h4").ok()?;
    let heading = doc.select(&selector).find(|heading| {
        collapse_ws(&heading.text().collect::<String>())
            .trim_end_matches(':')
            .eq_ignore_ascii_case("abstract")
    })?;
    let mut text = String::new();
    for node in heading.next_siblings() {
        if let Some(element) = ElementRef::wrap(node) {
            match element.value().name() {
                "h1" | "h2" | "h3" | "h4" | "ul" | "ol" => break,
                "script" | "style" | "noscript" => continue,
                _ => {}
            }
            text.extend(element.text().flat_map(|t| [t, " "]));
        } else if let Some(t) = node.value().as_text() {
            text.push_str(t);
        }
    }
    non_empty_text(text)
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

const BOILERPLATE_PREFIXES: &[&str] = &[
    "Promoting openness in scientific communication and the peer-review process",
    "Electronic proceedings of ",
    "Paper presented at ",
    "This repository contains ",
    "Author(s):",
    "HAL is a multi-disciplinary open access archive",
    "Article Share on",
    "Permission is granted for noncommercial",
    "Presented on ",
    "This chapter contains sections titled",
    "This material is presented to ensure timely dissemination",
    "Rights to individual papers remain",
    "(Article begins on next page)",
    "Provides notice of upcoming conference events",
];

fn is_boilerplate_abstract(text: &str) -> bool {
    let body = text.trim_end_matches('.');
    let (head, year) = body.rsplit_once(' ').unwrap_or(("", body));
    let year_final = year.len() == 4 && year.chars().all(|c| c.is_ascii_digit());
    BOILERPLATE_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
        || text.contains("ResearchGate, letting you")
        // a short citation: "Authors. Title. Venue. 2019."
        || (text.len() < 300 && year_final && head.ends_with('.'))
        // a proceedings title has no sentence end at all
        || (text.len() < 300
            && !text.contains(['.', '!', '?'])
            && (text.contains("Proceedings") || text.contains("Conference")))
        || (text.len() < 300 && text.starts_with("Warning:"))
        || (text.len() < 300 && text.contains(" to me ") && text.contains("I am "))
        || text.contains("Get Citation Alerts")
        || text
            .strip_prefix('\'')
            .and_then(|text| text.strip_suffix('\''))
            .and_then(|text| text.split_once("' published in '"))
            .is_some_and(|(title, publication)| !title.is_empty() && !publication.is_empty())
        || ((text.contains("Proceedings of ") || text.contains("Findings of "))
            && (year_final || text.contains(". In: ")))
}

fn is_url_only(text: &str) -> bool {
    let lower = text.trim().trim_end_matches('.').to_ascii_lowercase();
    !lower.contains(char::is_whitespace)
        && (lower.starts_with("http:") || lower.starts_with("https:") || lower.starts_with("doi:"))
}

fn trim_standard_suffix(text: &str) -> &str {
    let lower = text.to_ascii_lowercase();
    let text = [
        "ccs concepts",
        "acm reference format:",
        "© 20",
        "©20",
        "1 introduction",
        "permission to make digital or hard copies",
        "personal use of this material is permitted",
        "© copyright",
    ]
    .iter()
    // "since the 2011 introduction" is not a "1 Introduction" section heading
    .filter_map(|marker| {
        lower
            .match_indices(marker)
            .map(|(index, _)| index)
            .find(|&index| !lower[..index].ends_with(|c: char| c.is_ascii_digit()))
    })
    .min()
    .map_or(text, |index| &text[..index])
    .trim_end();
    // "... results. Copyright © 2015 AAAI" is cut at "© 20" above; this also repairs stored
    // abstracts that were cut there
    text.strip_suffix(" Copyright")
        .filter(|rest| rest.ends_with(['.', '!', '?', ')']))
        .unwrap_or(text)
}

/// PDF and IEEE extraction residue: ligatures ("eﬃcient"), soft hyphens ("sys(cid:173) tem",
/// "Kuhn(cid:173) Tucker", "buf\u{ad}fers"), and IEEE's quotes in "the modelpsilas output" and
/// "a dasiacascadepsila".
fn repair_extraction_marks(text: &str) -> String {
    let text = text
        .replace('ﬀ', "ff")
        .replace('ﬁ', "fi")
        .replace('ﬂ', "fl")
        .replace('ﬃ', "ffi")
        .replace('ﬄ', "ffl")
        .replace("(cid:173)", "\u{ad}")
        .replace(" \u{ad}", " -")
        .replace("\u{ad} ", "\u{ad}");
    let text = replace_marker(&text, "\u{ad}", |before, after| {
        let mut end = before.chars().rev();
        let in_word = end.next().is_some_and(char::is_lowercase)
            && end.next().is_some_and(char::is_alphabetic);
        if in_word && after.starts_with(char::is_lowercase) {
            ""
        } else {
            "-"
        }
    });
    let text = replace_marker(&text, "psila", |_, after| {
        let glued = after
            .strip_prefix('s')
            .unwrap_or(after)
            .starts_with(char::is_alphabetic);
        if glued {
            "psila"
        } else {
            "'"
        }
    });
    replace_marker(&text, "dasia", |before, _| {
        if before.ends_with(char::is_alphabetic) {
            "dasia"
        } else {
            "'"
        }
    })
}

/// Replace each `marker` with what `by` picks from the text before and after it.
fn replace_marker(text: &str, marker: &str, by: impl Fn(&str, &str) -> &'static str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut parts = text.split(marker).peekable();
    while let Some(part) = parts.next() {
        out.push_str(part);
        if let Some(after) = parts.peek() {
            out.push_str(by(&out, after));
        }
    }
    out
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
    let text = repair_extraction_marks(&text);
    let text = strip_footnote_tail(fix_sentence_spacing(trim_standard_suffix(&text)));
    let text = text.as_str();
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

/// "reliable.Specifically" -> "reliable. Specifically", but "U.S.Army" stays as it is.
fn fix_sentence_spacing(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    let mut word_start = 0;
    for (i, &c) in chars.iter().enumerate() {
        if c.is_whitespace() {
            word_start = i + 1;
        }
        out.push(c);
        if matches!(c, '.' | '!' | '?')
            && sentence_end_word(&chars[word_start..i])
            && chars.get(i + 1).is_some_and(char::is_ascii_uppercase)
            && (chars.get(i + 2).is_some_and(char::is_ascii_lowercase)
                || (matches!(chars[i + 1], 'A' | 'I') && chars.get(i + 2) == Some(&' ')))
        {
            out.push(' ');
            word_start = i + 1;
        }
    }
    out
}

fn sentence_end_word(word: &[char]) -> bool {
    matches!(word.last(), Some(')' | ']' | '%' | '"' | '”'))
        || (word.iter().all(|&c| c.is_ascii_lowercase() || c == '-')
            && word.iter().filter(|c| c.is_ascii_lowercase()).count() >= 2
            && word != ['w', 'w', 'w'])
}

/// Drop a footnote mark or section number after the last sentence: "results. 1." -> "results.",
/// but not "in Fig. 1.".
fn strip_footnote_tail(text: String) -> String {
    let core = text.strip_suffix('.').unwrap_or(&text);
    let Some(end) = core.rfind(['.', '!', '?']) else {
        return text;
    };
    let tail = &core[end + 1..];
    let ends_with_period = core.len() < text.len();
    let digits = tail.trim_start().replacen(' ', "", 1);
    let footnote = (1..=2).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit());
    // a "1." or "I." heading follows a space ("§.3." and "A.I." stay); footnote digits need not
    let mark = if ends_with_period {
        tail.starts_with(' ') && (tail == " I" || footnote)
    } else {
        footnote
    };
    let word = core[..end].rsplit(' ').next().unwrap_or("");
    // "Fig", "Eq", "No", "Sec" and "Ref" are abbreviations, not sentence ends
    let abbreviation = word.len() <= 4
        && word.starts_with(|c: char| c.is_ascii_uppercase())
        && word[1..].bytes().all(|b| b.is_ascii_lowercase());
    if mark && word.ends_with(|c: char| !c.is_ascii_digit() && c != '.') && !abbreviation {
        return core[..=end].to_string();
    }
    text
}

fn non_empty_text(text: impl AsRef<str>) -> Option<String> {
    let decoded = decode_html_entities(text.as_ref());
    let decoded = decode_html_entities(&decoded);
    let text = strip_abstract_heading(&collapse_ws(&decoded)).to_string();
    text.chars().any(char::is_alphabetic).then_some(text)
}

fn strip_abstract_heading(text: &str) -> &str {
    let text = text
        .strip_prefix("Summary form only given.")
        .map_or(text, str::trim_start);
    let Some(rest) = text
        .get(.."Abstract".len())
        .filter(|prefix| prefix.eq_ignore_ascii_case("Abstract"))
        .and_then(|_| text.get("Abstract".len()..))
    else {
        return text;
    };
    if let Some(rest) = [":", ".", " -", " –", " —", "—", "–"]
        .iter()
        .find_map(|separator| rest.strip_prefix(separator))
    {
        return rest.trim_start();
    }
    // a bare "ABSTRACT" is a heading; a bare "Abstract" only before a sentence opener, so
    // "Abstract We present" loses it but "Abstract Interpretation is" keeps it
    let shouted = text.starts_with("ABSTRACT");
    rest.strip_prefix(' ')
        .filter(|rest| {
            let mut words = rest.split_whitespace();
            words.next().is_some_and(|word| {
                word.starts_with(char::is_uppercase)
                    && (shouted || ["A", "An", "In", "Our", "The", "This", "We"].contains(&word))
            }) && !words
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

pub(crate) fn decode_html_entities(s: &str) -> String {
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
    openreview_cache: HashMap<String, String>,
    /// Forum ids whose OpenReview batch failed transiently.
    openreview_unanswered: HashSet<String>,
    doi_cache: HashMap<String, Option<String>>,
    openreview_login_tokens: [OnceLock<String>; 2],
}

pub enum EnrichResult {
    Found(String),
    Missing(String),
}

/// A failed API request. Rate limits, server and network errors are
/// `transient`: the paper is retried in a later run instead of remembered as a miss.
#[derive(Debug)]
struct FetchError {
    reason: String,
    transient: bool,
}

impl FetchError {
    fn new(reason: impl Into<String>, transient: bool) -> Self {
        let reason = reason.into();
        Self { reason, transient }
    }
}

/// The response, or `None` after noting a transient failure from `source` in `transient`.
fn ok_or_note(
    source: &str,
    result: std::result::Result<Value, FetchError>,
    transient: &mut Option<String>,
) -> Option<Value> {
    result
        .map_err(|e| {
            if e.transient {
                transient.get_or_insert_with(|| format!("{source} {}", e.reason));
            }
        })
        .ok()
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

fn openreview_auth_target(url: &str) -> Option<(usize, &'static str)> {
    if url.starts_with("https://api2.openreview.net/") {
        return Some((OPENREVIEW_API_V2, "https://api2.openreview.net/login"));
    }
    url.starts_with("https://api.openreview.net/")
        .then_some((OPENREVIEW_API_V1, "https://api.openreview.net/login"))
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
            openreview_unanswered: HashSet::new(),
            doi_cache: HashMap::new(),
            openreview_login_tokens: [OnceLock::new(), OnceLock::new()],
        }
    }

    /// `Err` means a source did not answer; retry the paper later. `Missing`
    /// means every source answered without an abstract.
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
            let result = match cached_doi_abstract(doi_cache, &paper)
                .or_else(|| cached_openreview_abstract(openreview_cache, &paper))
            {
                Some(abs) => Ok(EnrichResult::Found(abs)),
                None => enricher.enrich_after_prefetch(&paper).await,
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
            let Some(id) = paper.url.as_deref().and_then(openreview_forum_id) else {
                continue;
            };
            if !self.openreview_cache.contains_key(&id) && seen.insert(id.clone()) {
                needed.push(id);
            }
        }
        for ids in needed.chunks(OPENREVIEW_BATCH_SIZE) {
            let mut missing = ids.to_vec();
            let mut transient = false;
            for base in [
                "https://api2.openreview.net/notes",
                "https://api.openreview.net/notes",
            ] {
                match self.openreview_abstracts_from(base, &missing).await {
                    Ok(abstracts) => self.openreview_cache.extend(abstracts),
                    Err(e) => {
                        transient |= e.transient;
                        eprintln!(
                            "warning: OpenReview batch lookup failed for {base}: {}",
                            terminal_safe(&e.reason)
                        );
                    }
                }
                missing.retain(|id| !self.openreview_cache.contains_key(id));
                if missing.is_empty() {
                    break;
                }
            }
            if transient {
                self.openreview_unanswered.extend(missing);
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
                    eprintln!(
                        "warning: Semantic Scholar DOI batch lookup failed: {}",
                        terminal_safe(&e.reason)
                    );
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
                        eprintln!(
                            "warning: OpenAlex DOI batch lookup failed: {}",
                            terminal_safe(&e.reason)
                        );
                        retry_individually.extend(missing.iter().cloned());
                    }
                }
            }
            store_doi_batch_results(&mut self.doi_cache, chunk, hits, &retry_individually);
        }
    }

    async fn enrich_after_prefetch(&self, paper: &Paper) -> Result<EnrichResult> {
        let mut transient = paper
            .url
            .as_deref()
            .and_then(openreview_forum_id)
            .filter(|id| self.openreview_unanswered.contains(id))
            .map(|_| "OpenReview batch failed".to_string());
        if let Some(abs) = self.api_by_doi(paper, &mut transient).await {
            return Ok(EnrichResult::Found(abs));
        }
        match (self.fallback_enrich(paper, &mut transient).await, transient) {
            (EnrichResult::Missing(_), Some(reason)) => Err(crate::Error::Other(reason)),
            (result, _) => Ok(result),
        }
    }

    async fn fallback_enrich(&self, paper: &Paper, transient: &mut Option<String>) -> EnrichResult {
        let source = paper_source(paper);
        let mut static_miss = None;
        if source.is_some_and(|source| source != AbstractSource::Openreview) && paper.url.is_some()
        {
            match self.static_scrape(paper, source, transient).await {
                EnrichResult::Found(abs) => return EnrichResult::Found(abs),
                EnrichResult::Missing(reason) => {
                    static_miss = Some(reason);
                }
            }
        }

        if let Some(abs) = self.api_by_title(paper, transient).await {
            return EnrichResult::Found(abs);
        }
        if let Some(reason) = static_miss {
            return EnrichResult::Missing(reason);
        }
        if paper.url.is_some() {
            return self.static_scrape(paper, source, transient).await;
        }
        EnrichResult::Missing("API metadata lookup missed and paper has no URL".into())
    }

    async fn api_by_doi(&self, paper: &Paper, transient: &mut Option<String>) -> Option<String> {
        let doi = paper.doi.as_deref().and_then(normalized_doi)?;
        if !self.doi_cache.contains_key(&doi) {
            if let Some(abs) = ok_or_note(
                "OpenAlex",
                self.fetch_openalex_json(&format!("https://api.openalex.org/works/doi:{doi}"), &[])
                    .await,
                transient,
            )
            .filter(|json| json_doi_title_matches(json, &paper.title))
            .and_then(|json| abstract_from_openalex(&json))
            .and_then(|abs| usable_abstract_for_title(abs, &paper.title))
            {
                return Some(abs);
            }
        }

        let cr = self
            .client
            .get(format!("https://api.crossref.org/works/{doi}"));
        ok_or_note("Crossref", self.fetch_json(cr).await, transient)
            .and_then(|json| json.get("message").cloned())
            .filter(|message| crossref_title_matches(message, &paper.title))
            .and_then(|message| abstract_from_crossref(&message))
            .and_then(|abs| usable_abstract_for_title(abs, &paper.title))
    }

    async fn openalex_doi_abstracts(
        &self,
        dois: &[String],
        expected_titles: &HashMap<String, String>,
    ) -> std::result::Result<HashMap<String, String>, FetchError> {
        let filter = format!("doi:{}", dois.join("|"));
        let json = self
            .fetch_openalex_json(
                "https://api.openalex.org/works",
                &[
                    ("filter", filter.as_str()),
                    ("select", "doi,title,abstract_inverted_index"),
                    // above the batch size: one DOI can match several works; OpenAlex caps it at 100
                    ("per-page", "100"),
                ],
            )
            .await?;
        Ok(abstracts_from_openalex_works(&json, expected_titles))
    }

    async fn semantic_scholar_doi_abstracts(
        &self,
        dois: &[String],
        expected_titles: &HashMap<String, String>,
    ) -> std::result::Result<HashMap<String, String>, FetchError> {
        let ids = dois
            .iter()
            .map(|doi| format!("DOI:{doi}"))
            .collect::<Vec<_>>();
        let req = self
            .client
            .post("https://api.semanticscholar.org/graph/v1/paper/batch")
            .query(&[("fields", "externalIds,title,abstract")])
            .json(&serde_json::json!({ "ids": ids }));
        let json = self.fetch_semantic_scholar_json(req).await?;
        Ok(abstracts_from_semantic_scholar_batch(
            &json,
            expected_titles,
        ))
    }

    /// Free OpenAlex endpoints: autocomplete for candidates, then each work by id.
    async fn api_by_title(&self, paper: &Paper, transient: &mut Option<String>) -> Option<String> {
        let hits = ok_or_note(
            "OpenAlex",
            self.fetch_openalex_json(
                "https://api.openalex.org/autocomplete/works",
                &[("q", paper.title.as_str())],
            )
            .await,
            transient,
        )?;
        for id in openalex_title_hits(&hits, &paper.title) {
            if let Some(abs) = ok_or_note(
                "OpenAlex",
                self.fetch_openalex_json(
                    &format!("https://api.openalex.org/works/{id}"),
                    &[(
                        "select",
                        "title,publication_year,authorships,type,abstract_inverted_index",
                    )],
                )
                .await,
                transient,
            )
            .filter(|work| openalex_identity_matches(work, paper))
            .and_then(|work| abstract_from_openalex(&work))
            .and_then(|abs| usable_abstract_for_title(abs, &paper.title))
            {
                return Some(abs);
            }
        }
        None
    }

    async fn openreview_abstracts_from(
        &self,
        base: &str,
        ids: &[String],
    ) -> std::result::Result<HashMap<String, String>, FetchError> {
        let req = self.openreview_get(base).await.query(&[
            ("ids", ids.join(",")),
            ("limit", ids.len().to_string()),
            ("select", "id,forum,content.abstract".to_string()),
        ]);
        let json = self.fetch_json(req).await?;
        let mut abstracts = abstracts_from_openreview_notes(&json);
        Ok(ids
            .iter()
            .filter_map(|id| abstracts.remove_entry(id))
            .collect())
    }

    async fn openreview_get(&self, url: &str) -> reqwest::RequestBuilder {
        let req = self.client.get(url);
        let Some((api, login_url)) = openreview_auth_target(url) else {
            return req;
        };
        if let Some(token) = self.openreview_login_token(api, login_url).await {
            return req.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        req
    }

    async fn openreview_login_token(&self, api: usize, login_url: &str) -> Option<String> {
        let token_cache = &self.openreview_login_tokens[api];
        if let Some(token) = token_cache.get() {
            return Some(token.clone());
        }
        let token = self.openreview_login(login_url).await;
        if let Some(token) = &token {
            let _ = token_cache.set(token.clone());
        }
        token
    }

    async fn openreview_login(&self, login_url: &str) -> Option<String> {
        let username = self.secrets.openreview_username.as_deref()?;
        let password = self.secrets.openreview_password.as_deref()?;
        let req = self.client.post(login_url).json(&serde_json::json!({
            "id": username,
            "password": password,
            "expiresIn": OPENREVIEW_LOGIN_EXPIRES_IN,
        }));
        let json = self.fetch_json(req).await.ok()?;
        json.get("token")
            .and_then(|token| token.as_str())
            .map(str::to_string)
    }

    async fn fetch_openalex_json(
        &self,
        url: &str,
        query: &[(&str, &str)],
    ) -> std::result::Result<Value, FetchError> {
        let plain_req = || self.client.get(url).query(query);
        let req = match &self.secrets.openalex_api_key {
            Some(key) => plain_req().query(&[("api_key", key.as_str())]),
            None => plain_req(),
        };
        match self.fetch_json(req).await {
            Err(e)
                if self.secrets.openalex_api_key.is_some()
                    && should_retry_without_api_key(&e.reason) =>
            {
                self.fetch_json(plain_req()).await
            }
            other => other,
        }
    }

    async fn fetch_semantic_scholar_json(
        &self,
        plain_req: reqwest::RequestBuilder,
    ) -> std::result::Result<Value, FetchError> {
        let Some(key) = &self.secrets.semantic_scholar_key else {
            return self.fetch_json(plain_req).await;
        };
        let fallback = plain_req.try_clone();
        let req = plain_req.header("x-api-key", key);
        match self.fetch_json(req).await {
            Err(e) if should_retry_without_api_key(&e.reason) => match fallback {
                Some(req) => self.fetch_json(req).await,
                None => Err(e),
            },
            other => other,
        }
    }

    /// Fetch JSON, waiting once when a 429 asks for at most MAX_RATE_LIMIT_SLEEP.
    async fn fetch_json(
        &self,
        req: reqwest::RequestBuilder,
    ) -> std::result::Result<Value, FetchError> {
        let rate_limit_req = req.try_clone();
        let mut resp = self.send(req).await.map_err(|r| FetchError::new(r, true))?;
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            if let (Some(req), Some(delay)) = (rate_limit_req, rate_limit_delay(resp.headers())) {
                tokio::time::sleep(delay).await;
                resp = self.send(req).await.map_err(|r| FetchError::new(r, true))?;
            }
        }
        let status = resp.status();
        if !status.is_success() {
            return Err(FetchError::new(
                http_status_reason("HTTP", status),
                crate::error::is_transient_status(status),
            ));
        }
        let bytes = read_body_limited(resp, MAX_JSON_BODY_BYTES).await?;
        serde_json::from_slice::<Value>(&bytes)
            .map_err(|_| FetchError::new("invalid JSON response", false))
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
        transient: &mut Option<String>,
    ) -> EnrichResult {
        let mut miss = |reason: String, is_transient: bool| {
            if is_transient {
                transient.get_or_insert(reason.clone());
            }
            EnrichResult::Missing(reason)
        };
        let mut next = paper.url.clone().expect("paper URL checked by caller");
        for _ in 0..=MAX_STATIC_REDIRECTS {
            let url = match allowed_static_url(&next).await {
                Ok(url) => url,
                Err(reason) => return miss(reason.into(), reason == STATIC_DNS_FAILED),
            };
            let resp = match self.send(self.client.get(url.clone())).await {
                Ok(resp) => resp,
                Err(reason) => return miss(format!("static scrape {reason}"), true),
            };
            if resp.status().is_redirection() {
                let Some(next_url) = redirect_url(&url, &resp) else {
                    return EnrichResult::Missing("redirect without valid location".into());
                };
                next = next_url.into();
                continue;
            }
            let status = resp.status();
            if !status.is_success() {
                return miss(
                    http_status_reason("static scrape HTTP", status),
                    crate::error::is_transient_status(status),
                );
            }
            let html = match read_body_limited(resp, MAX_HTML_BODY_BYTES).await {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(e) => return miss(format!("static scrape {}", e.reason), e.transient),
            };
            if !static_page_title_matches(&html, paper) {
                return EnrichResult::Missing("static page title did not match paper".into());
            }
            let source = source_from_static_url(&url).or(source);
            return match abstract_candidates(&html, source)
                .into_iter()
                .find_map(|abs| usable_abstract_for_title(abs, &paper.title))
            {
                Some(abs) => EnrichResult::Found(abs),
                None => EnrichResult::Missing("static page had no extractable abstract".into()),
            };
        }

        EnrichResult::Missing("too many redirects".into())
    }
}

fn source_from_static_url(url: &Url) -> Option<AbstractSource> {
    let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    match host.as_str() {
        "aclanthology.org" | "www.aclanthology.org" => Some(AbstractSource::Acl),
        "ijcai.org" | "www.ijcai.org" => Some(AbstractSource::Ijcai),
        "openaccess.thecvf.com" => Some(AbstractSource::Cvf),
        "proceedings.iclr.cc" => Some(AbstractSource::Iclr),
        "openreview.net" | "www.openreview.net" => Some(AbstractSource::Openreview),
        "proceedings.mlr.press" => Some(AbstractSource::Pmlr),
        "neurips.cc"
        | "datasets-benchmarks-proceedings.neurips.cc"
        | "www.neurips.cc"
        | "proceedings.neurips.cc"
        | "nips.cc"
        | "www.nips.cc"
        | "papers.nips.cc" => Some(AbstractSource::Neurips),
        "idp.springer.com" | "link.springer.com" => Some(AbstractSource::Springer),
        "ndss-symposium.org" | "www.ndss-symposium.org" => Some(AbstractSource::Ndss),
        "usenix.org" | "www.usenix.org" => Some(AbstractSource::Usenix),
        _ => None,
    }
}

fn is_doi_host(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    matches!(
        host.trim_end_matches('.').to_ascii_lowercase().as_str(),
        "doi.org" | "dx.doi.org"
    )
}

fn is_allowed_static_host(url: &Url) -> bool {
    if is_doi_host(url) || source_from_static_url(url).is_some() {
        return true;
    }
    // These publishers use generic metadata rather than a source-specific parser.
    let Some(host) = url.host_str() else {
        return false;
    };
    matches!(
        host.trim_end_matches('.').to_ascii_lowercase().as_str(),
        "aaai.org" | "www.aaai.org" | "ojs.aaai.org" | "iclr.cc" | "www.iclr.cc"
    )
}

fn static_fetch_url(raw: &str) -> Option<Url> {
    let mut url = parse_static_url(raw)?;
    if !is_allowed_static_host(&url) {
        return None;
    }
    match (url.scheme(), url.port_or_known_default()) {
        ("https", Some(443)) => Some(url),
        ("http", Some(80)) => {
            url.set_scheme("https").ok()?;
            url.set_port(None).ok()?;
            Some(url)
        }
        _ => None,
    }
}

fn source_from_paper_url(raw: &str) -> Option<AbstractSource> {
    let url = parse_static_url(raw)?;
    source_from_doi_url(&url).or_else(|| source_from_static_url(&url))
}

fn source_from_doi_url(url: &Url) -> Option<AbstractSource> {
    if !is_doi_host(url) {
        return None;
    }
    let doi = url.path().trim_start_matches('/').to_ascii_lowercase();
    match doi.as_str() {
        doi if doi.starts_with("10.18653/") => Some(AbstractSource::Acl),
        doi if doi.starts_with("10.24963/") => Some(AbstractSource::Ijcai),
        doi if doi.starts_with("10.1007/") => Some(AbstractSource::Springer),
        doi if doi.starts_with("10.14722/") => Some(AbstractSource::Ndss),
        _ => None,
    }
}

fn paper_source(paper: &Paper) -> Option<AbstractSource> {
    paper.url.as_deref().and_then(source_from_paper_url)
}

fn cached_openreview_abstract(cache: &HashMap<String, String>, paper: &Paper) -> Option<String> {
    let forum_id = openreview_forum_id(paper.url.as_deref()?)?;
    cache
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
    reason.starts_with("HTTP 401") || reason.starts_with("HTTP 403")
}

/// How long a 429 asks us to wait, or `None` when that is too long to be worth it
/// (OpenAlex sends the time until its daily budget resets).
fn rate_limit_delay(headers: &header::HeaderMap) -> Option<Duration> {
    let secs = header_seconds(headers, header::RETRY_AFTER)
        .or_else(|| header_seconds(headers, "ratelimit-reset"))
        .unwrap_or(60);
    (secs <= MAX_RATE_LIMIT_SLEEP.as_secs()).then(|| Duration::from_secs(secs.max(1)))
}

fn header_seconds(headers: &header::HeaderMap, name: impl header::AsHeaderName) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

async fn allowed_static_url(raw: &str) -> std::result::Result<Url, &'static str> {
    let url = static_fetch_url(raw).ok_or("URL rejected by static scraper")?;
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
                .map_err(|_| STATIC_DNS_FAILED)?
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
            if let Some(ipv4) = ip.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(ipv4));
            }
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

/// A body over `limit` is final; a read error (such as the request timeout) is transient.
async fn read_body_limited(
    mut resp: reqwest::Response,
    limit: usize,
) -> std::result::Result<Vec<u8>, FetchError> {
    let too_large = || FetchError::new("response too large", false);
    if resp.content_length().is_some_and(|len| len > limit as u64) {
        return Err(too_large());
    }

    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|_| FetchError::new("response read failed", true))?
    {
        if body.len() + chunk.len() > limit {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
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
    fn openreview_auth_routes_both_api_versions() {
        assert_eq!(
            openreview_auth_target("https://api2.openreview.net/notes"),
            Some((OPENREVIEW_API_V2, "https://api2.openreview.net/login"))
        );
        assert_eq!(
            openreview_auth_target("https://api.openreview.net/notes"),
            Some((OPENREVIEW_API_V1, "https://api.openreview.net/login"))
        );
        assert_eq!(
            openreview_auth_target("https://api.openreview.net.evil/notes"),
            None
        );
    }

    #[tokio::test]
    async fn openreview_requests_keep_tokens_scoped_to_each_api() {
        let enricher = Enricher::new(Secrets::default());
        enricher.openreview_login_tokens[OPENREVIEW_API_V1]
            .set("api1-token".into())
            .unwrap();
        enricher.openreview_login_tokens[OPENREVIEW_API_V2]
            .set("api2-token".into())
            .unwrap();
        for (url, expected) in [
            (
                "https://api.openreview.net/notes",
                Some("Bearer api1-token"),
            ),
            (
                "https://api2.openreview.net/notes",
                Some("Bearer api2-token"),
            ),
            ("https://api.openreview.net.example/notes", None),
            ("https://api2.openreview.net.example/notes", None),
            ("https://example.com/api2.openreview.net/notes", None),
            ("http://api.openreview.net/notes", None),
            ("http://api2.openreview.net/notes", None),
        ] {
            let request = enricher.openreview_get(url).await.build().unwrap();
            assert_eq!(
                request
                    .headers()
                    .get(header::AUTHORIZATION)
                    .map(|h| h.to_str().unwrap()),
                expected,
                "{url}"
            );
        }
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
    fn api_key_fallback_is_limited_to_auth_statuses() {
        assert!(should_retry_without_api_key("HTTP 403 (blocked)"));
        assert!(!should_retry_without_api_key("HTTP 429 (rate limited)"));
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
    fn rate_limit_delay_skips_waits_beyond_the_cap() {
        let mut headers = header::HeaderMap::new();
        assert_eq!(rate_limit_delay(&headers), Some(Duration::from_secs(60)));
        headers.insert("ratelimit-reset", header::HeaderValue::from_static("0"));
        assert_eq!(rate_limit_delay(&headers), Some(Duration::from_secs(1)));
        headers.insert(
            header::RETRY_AFTER,
            header::HeaderValue::from_static("21063"),
        );
        assert_eq!(rate_limit_delay(&headers), None);
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
            "Author(s): Alice Example, Bob Example Download: Paper (PDF) Date: 2014 Associated Event: NDSS Symposium 2014",
            "HAL is a multi-disciplinary open access archive for the deposit and dissemination of scientific research documents.",
            "Join ResearchGate to discover and stay up-to-date with the latest research from leading experts in Security and many other scientific topics. ResearchGate, letting you access and read them immediately.",
            "Alice Example and Bob Example. A paper title about kernels. Annual Meeting of the Association. 2019.",
            "EMNLP-CoNLL 2012 - 2012 Joint Conference on Empirical Methods in Natural Language Processing and Computational Natural Language Learning, Proceedings of the Conference",
            "This chapter contains sections titled: Preliminaries The inference rule Global consistency Local consistency Conclusions References",
            "Rights to individual papers remain with the author or the author's employer. Permission is granted for noncommercial reproduction of the work.",
            "This material is presented to ensure timely dissemination of scholarly and technical work. Copyright and all rights therein are retained by authors.",
            "Personal use of this material is permitted. However, permission to reprint/republish this material for advertising must be obtained from the IEEE.",
            "(Article begins on next page) The Harvard community has made this article openly available. Please share how this access benefits you. Your story matters.",
            "Provides notice of upcoming conference events of interest to practitioners and researchers.",
            "Article A paper title Share on CCS '07: Proceedings of the 14th ACM conference Get Citation AlertsNew Citation Alert added! Save to BinderSave to BinderCreate a New Binder",
            "Contents List of Tables iv 1 Introduction 1 1.1 The Problem . . . . . . . . . . 1 1.2 Common Attacks . . . . . . . . . . 2 1.3 A Solution . . . . . 4",
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
        assert_eq!(
            abstract_text(format!(
                "{abs} 1 Introduction Recently, the quality of research"
            ))
            .as_deref(),
            Some(abs.as_str())
        );
        assert_eq!(
            abstract_text(format!(
                "{abs} Permission to make digital or hard copies of all or part"
            ))
            .as_deref(),
            Some(abs.as_str())
        );
        assert!(
            abstract_text(format!("{abs} Copy to Clipboard Download Related Material")).is_none()
        );
        // a cut-off first sentence is still real text
        assert!(abstract_text("We present a simple, but effective method to incorporate syntactic dependency information directly into transformer-based language models (e").is_some());
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
    fn repairs_stored_abstract_typography() {
        let abs = fixture_abstract("Clean abstract.");
        for (raw, want) in [
            (format!("Abstract—{abs}"), abs.clone()),
            (format!("Summary form only given. {abs}"), abs.clone()),
            (format!("{abs} 1"), abs.clone()),
            (format!("{abs}12"), abs.clone()),
            (
                format!("Results are reliable.Specifically, we win. {abs}"),
                format!("Results are reliable. Specifically, we win. {abs}"),
            ),
            (format!("{abs} 1."), abs.clone()),
            (format!("{abs} I."), abs.clone()),
            (format!("{abs} 1 1"), abs.clone()),
            (format!("{abs} Gains in BLEU. 1"), format!("{abs} Gains in BLEU.")),
            (format!("{abs} Copyright"), abs.clone()),
            (
                format!("Eﬃcient Ma(cid:173) chines of Kuhn(cid:173) Tucker, Q(cid:173) learning, buf\u{ad}fers, ports \u{ad} the. {abs}"),
                format!("Efficient Machines of Kuhn-Tucker, Q-learning, buffers, ports - the. {abs}"),
            ),
            (
                format!("The modelpsilas dasiacascadepsila and Apsila, not ipsilateral. {abs}"),
                format!("The model's 'cascade' and A', not ipsilateral. {abs}"),
            ),
            (
                format!("We pre-train.Then (LLMs).In [1].We 5%.We 7 %.We \"wiser\".We use it.A {abs}"),
                format!("We pre-train. Then (LLMs). In [1]. We 5%. We 7 %. We \"wiser\". We use it. A {abs}"),
            ),
        ] {
            let once = abstract_text(&raw);
            assert_eq!(once.as_deref(), Some(want.as_str()), "{raw}");
            assert_eq!(abstract_text(once.unwrap()).as_deref(), Some(want.as_str()));
        }
        for kept in [
            format!("We use e.g.Using TensorFlow.We and U.S.Army data. {abs}"),
            format!("Data is at www.WelQrate.org. {abs}"),
            format!("{abs} We use Python 3.12"),
            format!("{abs} Use responsible A.I."),
            format!("{abs} See Ref. 12"),
            format!("{abs} Up 99%.... 1"),
            format!("{abs} On Fair Use and Copyright"),
            format!("{abs} We study the EU copyright 2019 reform."),
            format!("{abs} See Fig. 1."),
            format!("{abs} A mIoU of 54. 7."),
            format!("{abs} Since the 2011 introduction of SDN, controllers grew."),
        ] {
            assert_eq!(abstract_text(&kept).as_deref(), Some(kept.as_str()));
        }
    }

    #[test]
    fn html_abstract_heading_reads_siblings_until_next_heading_or_list() {
        let first = fixture_abstract("First paragraph.");
        let html = format!(
            "<html><body><h1>Paper</h1><h3>Abstract:</h3><p>{first}</p><script>track(1);</script>\
             <style>p {{ color: red }}</style><noscript>Enable JavaScript</noscript><p>Second paragraph.</p>\
             <ul><li>View the full text</li></ul><p>Footer</p></body></html>"
        );
        assert_eq!(
            abstract_candidates(&html, None).last().map(String::as_str),
            Some(format!("{first} Second paragraph.").as_str())
        );
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
    fn title_matching_ignores_spacing() {
        assert!(titles_match("BIS i: A Study", "BISi - a study."));
        assert!(titles_match("3-D Scene Graphs", "3D scene graphs."));
        assert!(!titles_match("A Study of Graphs", "A Study of Graph"));
        assert!(doi_titles_match(
            "Face Value? Perceptions of Faces",
            "Face Value?"
        ));
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
    fn openalex_title_hits_keep_first_three_matching_titles() {
        let title = "TD-MPC2: Scalable, Robust World Models for Continuous Control.";
        let hit = |id: &str, name: &str| json!({"id": format!("https://openalex.org/{id}"), "display_name": name});
        let results = json!({"results": [
            hit("W1", "TD-MPC2: Scalable, Robust World Models for Continuous Control"),
            hit("W2", "TD-MPC: Temporal Difference Learning for Model Predictive Control"),
            hit("W3", "TD-MPC2 - Scalable, Robust World Models for Continuous Control"),
            hit("W4", "TD-MPC2: scalable, robust world models for continuous control"),
            hit("W5", "TD-MPC2: Scalable, Robust World Models for Continuous Control"),
        ]});
        assert_eq!(openalex_title_hits(&results, title), ["W1", "W3", "W4"]);
    }

    #[test]
    fn openalex_title_route_requires_identity_match() {
        let td_mpc = paper("TD-MPC2: Scalable, Robust World Models for Continuous Control.");
        let work = |author: &str, year: i64, kind: &str| {
            json!({
                "title": "TD-MPC2: Scalable, Robust World Models for Continuous Control",
                "publication_year": year,
                "type": kind,
                "authorships": [
                    {"author": {"display_name": "Other Person"}},
                    {"author": {"display_name": author}},
                ],
            })
        };
        for (author, year, kind, matches) in [
            ("X. Wang", 2024, "article", true),
            ("Nicklas Hansen", 2023, "article", true),
            ("Nick Hanson", 2024, "article", false),
            ("Wrong Author", 2024, "article", false),
            ("Nicklas Hansen", 2021, "article", false),
            ("Nicklas Hansen", 2024, "dissertation", false),
            ("Nicklas Hansen", 2024, "editorial", false),
        ] {
            let work = work(author, year, kind);
            assert_eq!(openalex_identity_matches(&work, &td_mpc), matches, "{work}");
        }
        let work = work("Nicklas Hansen", 2024, "article");
        assert!(!openalex_identity_matches(&work, &paper("Different Title")));
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

    #[tokio::test]
    async fn openreview_batch_requests_exact_ids_across_tracks_and_api_versions() {
        use std::io::{BufRead, BufReader, Write};

        let ids = [
            "conference",
            "workshop",
            "later-api1",
            "modern-api2",
            "missing",
        ]
        .map(str::to_string);
        let abstract_for = |id| fixture_abstract(&format!("Exact {id} abstract."));
        let body = json!({"notes": [
            {"id": "workshop", "content": {"abstract": abstract_for("workshop")}},
            {"id": "modern-api2", "content": {"abstract": {"value": abstract_for("modern-api2")}}},
            {"id": "unrequested", "content": {"abstract": "Unrelated abstract."}},
            {"id": "later-api1", "content": {"abstract": abstract_for("later-api1")}},
            {"id": "conference", "content": {"abstract": abstract_for("conference")}}
        ]})
        .to_string();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/notes", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in [("200 OK", body.as_str()), ("503 Service Unavailable", "")] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    request.push_str(&line);
                }
                write!(
                    socket,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                requests.push(request);
            }
            requests
        });

        let enricher = Enricher::new(Secrets::default());
        let abstracts = enricher
            .openreview_abstracts_from(&base, &ids)
            .await
            .unwrap();
        // An outage keeps the batch's papers retryable instead of remembered as misses.
        let outage = enricher.openreview_abstracts_from(&base, &ids).await;
        assert!(outage.is_err_and(|e| e.transient));
        let requests = server.join().unwrap();
        let target = requests[0]
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        let url = Url::parse(&base).unwrap().join(target).unwrap();
        assert_eq!(
            url.query_pairs().into_owned().collect::<HashMap<_, _>>(),
            HashMap::from([
                ("ids".into(), ids.join(",")),
                ("limit".into(), "5".into()),
                ("select".into(), "id,forum,content.abstract".into()),
            ])
        );
        assert_eq!(abstracts.len(), 4);
        for id in &ids[..4] {
            assert_eq!(abstracts.get(id), Some(&abstract_for(id)));
        }
        assert!(!abstracts.contains_key("missing"));
        assert!(!abstracts.contains_key("unrequested"));
    }

    #[test]
    fn cached_openreview_abstract_uses_forum_id() {
        let abs = fixture_abstract("Cached abstract.");
        let mut cache = HashMap::new();
        cache.insert("Oxh5CstDJU".to_string(), abs.clone());
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
        enricher
            .openreview_cache
            .insert("Oxh5CstDJU".to_string(), abs.clone());

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
            None
        );
        assert_eq!(
            source_from_paper_url("https://doi.org/10.18653/V1/D19-1593"),
            Some(AbstractSource::Acl)
        );
        assert_eq!(
            source_from_paper_url("https://doi.org/10.1109/RAID67961.2025.00012"),
            None
        );
        assert_eq!(
            source_from_paper_url("https://doi.org/10.24963/ijcai.2024/1"),
            Some(AbstractSource::Ijcai)
        );
    }

    #[tokio::test]
    async fn cached_doi_miss_still_tries_crossref() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut enricher = Enricher::new(Secrets::default());
        enricher.client = reqwest::Client::builder()
            .proxy(
                reqwest::Proxy::all(format!("http://{}", listener.local_addr().unwrap())).unwrap(),
            )
            .build()
            .unwrap();
        let mut paper = paper("Expected Title");
        paper.doi = Some("10.24963/IJCAI.2026/476".into());
        paper.url = None;
        enricher
            .doi_cache
            .insert(normalized_doi(paper.doi.as_deref().unwrap()).unwrap(), None);

        let mut requests = Vec::new();
        let proxy = async {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(&mut socket);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                requests.push(line.trim_end().to_owned());
                loop {
                    line.clear();
                    assert!(reader.read_line(&mut line).await.unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                socket.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            }
        };
        let results = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                results = enricher.enrich_many(vec![paper], 1) => results,
                _ = proxy => unreachable!("proxy runs until enrichment finishes"),
            }
        })
        .await
        .expect("local proxy and enrichment must finish");

        // DOI route (Crossref), then title route (OpenAlex autocomplete), each sent twice by send().
        let crossref = "CONNECT api.crossref.org:443 HTTP/1.1";
        let openalex = "CONNECT api.openalex.org:443 HTTP/1.1";
        assert_eq!(requests, [crossref, crossref, openalex, openalex]);
        assert!(
            results[0].1.is_err(),
            "an unreachable Crossref keeps the paper retryable"
        );
    }

    #[tokio::test]
    async fn static_scrape_outage_is_transient() {
        // Nothing listens on this proxy, so every request fails like a network outage.
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let mut enricher = Enricher::new(Secrets::default());
        enricher.client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://{closed}")).unwrap())
            .build()
            .unwrap();
        let mut paper = paper("Expected Title");
        paper.url = Some("https://aclanthology.org/D19-1593/".into());
        let mut transient = None;
        let result = enricher.static_scrape(&paper, None, &mut transient).await;
        assert!(matches!(result, EnrichResult::Missing(_)));
        assert!(transient.is_some(), "an outage keeps the paper retryable");
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
    fn html_ijcai_article_uses_full_abstract_without_heading() {
        let html = r#"<html><head>
            <meta name="description" content="Truncated abstract...">
        </head><body>
            <p>Navigation text</p>
            <div class="article__abstract">
                <p class="eyebrow">Abstract</p>
                <p>We study <em>temporal graphs</em> and introduce an adaptive sampling
                method.</p><p>Experiments on several benchmarks show improved prediction
                accuracy while reducing the computational cost.</p>
            </div>
            <p>Keywords: graphs</p>
        </body></html>"#;
        assert_eq!(
            extract_abstract_html(html, Some(AbstractSource::Ijcai)).as_deref(),
            Some("We study temporal graphs and introduce an adaptive sampling method. Experiments on several benchmarks show improved prediction accuracy while reducing the computational cost.")
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
    fn html_iclr_proceedings_uses_full_primary_abstract() {
        let mut paper = paper("A proceedings paper");
        paper.url = Some(
            "https://proceedings.iclr.cc/paper_files/paper/2026/hash/example-Abstract-Conference.html"
                .into(),
        );
        let url = static_fetch_url(paper.url.as_deref().unwrap()).unwrap();
        let html = r#"<html><head>
            <meta name="citation_title" content="A proceedings paper">
            <meta name="description" content="Truncated abstract...">
        </head><body>
            <p>Navigation text</p>
            <section class="paper-section">
                <h2 class="section-label">Abstract</h2>
                <p class="paper-abstract">We study <em>private streams</em> and give an
                efficient algorithm with improved accuracy. Our analysis establishes
                bounds on both the error and the required space.</p>
            </section>
        </body></html>"#;
        assert!(static_page_title_matches(html, &paper));
        assert!(paper_source(&paper).is_some());
        let abstract_text = abstract_candidates(html, source_from_static_url(&url))
            .into_iter()
            .find_map(|abs| usable_abstract_for_title(abs, &paper.title));
        assert_eq!(
            abstract_text.as_deref(),
            Some("We study private streams and give an efficient algorithm with improved accuracy. Our analysis establishes bounds on both the error and the required space.")
        );
        assert!(static_fetch_url("https://proceedings.iclr.cc.attacker.example/paper").is_none());
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
        let theory = "Abstract Interpretation is a theory of sound approximation.";
        assert_eq!(non_empty_text(theory).as_deref(), Some(theory));
        let once = non_empty_text(format!("Abstract: {theory}")).unwrap();
        assert_eq!(once, theory);
        assert_eq!(non_empty_text(&once).as_deref(), Some(theory));
    }

    #[test]
    fn html_no_abstract() {
        assert!(extract_abstract_html("<html></html>", None).is_none());
    }

    #[test]
    fn ipv4_mapped_ipv6_uses_ipv4_address_policy() {
        assert!(!is_public_ip("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!is_public_ip("::ffff:169.254.169.254".parse().unwrap()));
        assert!(is_public_ip("::ffff:8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn static_scrape_normalizes_only_allowed_publishers() {
        for raw in [
            "https://doi.org/10.9999/example",
            "https://datasets-benchmarks-proceedings.neurips.cc/paper/1",
            "https://aaai.org/paper/1",
            "https://www.aaai.org/paper/1",
            "https://ojs.aaai.org/paper/1",
            "https://iclr.cc/paper/1",
            "https://www.iclr.cc/paper/1",
            "https://idp.springer.com/authorize",
        ] {
            assert_eq!(static_fetch_url(raw).unwrap().as_str(), raw);
        }

        assert_eq!(
            static_fetch_url("http://papers.nips.cc/paper/1")
                .unwrap()
                .as_str(),
            "https://papers.nips.cc/paper/1"
        );
        assert_eq!(
            static_fetch_url("http://proceedings.mlr.press:80/paper/1")
                .unwrap()
                .as_str(),
            "https://proceedings.mlr.press/paper/1"
        );

        for raw in [
            "http://www.usenix.org:8080/conference/example",
            "https://www.usenix.org:8443/conference/example",
            "https://example.com/paper",
            "https://dl.acm.org/doi/10.1145/example",
            "https://ieeexplore.ieee.org/document/1",
            "https://doi.ieeecomputersociety.org/10.1109/example",
            "https://attacker.neurips.cc/paper",
            "https://aaai.org.attacker.example/paper",
            "https://attacker.iclr.cc/paper",
        ] {
            assert!(static_fetch_url(raw).is_none(), "accepted {raw}");
        }
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
