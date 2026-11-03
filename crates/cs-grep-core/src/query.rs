//! Query language for full-text search and metadata filters.
//!
//! A query is a full-text expression followed by an optional metadata filter:
//! `text [WHERE filter]`. Both expressions support AND, OR, NOT, parentheses,
//! and implicit AND. `*` is the match-all text expression.

use crate::config::Config;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YearRange {
    min: Option<i32>,
    max: Option<i32>,
}

impl YearRange {
    pub fn new(min: Option<i32>, max: Option<i32>) -> Result<Self> {
        if min.is_none() && max.is_none() {
            return Err(Error::Query(
                "year range must have at least one bound".into(),
            ));
        }
        if let (Some(min), Some(max)) = (min, max) {
            if min > max {
                return Err(Error::Query(format!(
                    "year range minimum {min} is after maximum {max}"
                )));
            }
        }
        Ok(Self { min, max })
    }

    pub fn single(year: i32) -> Self {
        Self {
            min: Some(year),
            max: Some(year),
        }
    }

    pub fn bounds(&self) -> (Option<i32>, Option<i32>) {
        (self.min, self.max)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterExpr {
    Venue(Vec<String>),
    Year(YearRange),
    Doi(String),
    And(Vec<FilterExpr>),
    Or(Vec<FilterExpr>),
    Not(Box<FilterExpr>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedQuery {
    pub fts: Option<String>,
    pub filter: Option<FilterExpr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    LParen,
    RParen,
    And,
    Or,
    Not,
    Where,
    Field(FieldKind),
    Term(String),
    Phrase(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FieldKind {
    Fts(&'static str),
    Metadata(MetadataField),
}

impl FieldKind {
    fn label(&self) -> &'static str {
        match self {
            Self::Fts("authors") => "author",
            Self::Fts(field) => field,
            Self::Metadata(field) => field.label(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataField {
    Venue,
    Year,
    Rank,
    Tag,
    Doi,
}

impl MetadataField {
    fn label(self) -> &'static str {
        match self {
            Self::Venue => "venue",
            Self::Year => "year",
            Self::Rank => "rank",
            Self::Tag => "tag",
            Self::Doi => "doi",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Term(String),
    Phrase(String),
    Field(FieldKind, Box<Node>),
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
}

fn map_field(name: &str) -> Option<FieldKind> {
    let field = match name.to_ascii_lowercase().as_str() {
        "title" => FieldKind::Fts("title"),
        "abstract" => FieldKind::Fts("abstract"),
        "author" => FieldKind::Fts("authors"),
        "venue" => FieldKind::Metadata(MetadataField::Venue),
        "year" => FieldKind::Metadata(MetadataField::Year),
        "rank" => FieldKind::Metadata(MetadataField::Rank),
        "tag" => FieldKind::Metadata(MetadataField::Tag),
        "doi" => FieldKind::Metadata(MetadataField::Doi),
        _ => return None,
    };
    Some(field)
}

fn is_word_delim(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | ')' | '"')
}

fn tokenize(input: &str) -> Result<Vec<Tok>> {
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '(' => {
                out.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                out.push(Tok::RParen);
                i += 1;
            }
            '"' => {
                i += 1;
                let start = i;
                while i < chars.len() && chars[i] != '"' {
                    i += 1;
                }
                if i >= chars.len() {
                    return Err(Error::Query("unterminated quoted phrase".into()));
                }
                out.push(Tok::Phrase(chars[start..i].iter().collect()));
                i += 1;
            }
            '&' | '|' => {
                return Err(Error::Query(format!(
                    "unsupported operator `{c}`; use `AND` or `OR`"
                )));
            }
            '-' if i + 1 < chars.len() && !is_word_delim(chars[i + 1]) => {
                return Err(Error::Query("unsupported negation `-`; use `NOT`".into()));
            }
            _ => {
                let start = i;
                while i < chars.len() && !is_word_delim(chars[i]) {
                    i += 1;
                }
                push_word(&mut out, chars[start..i].iter().collect())?;
            }
        }
    }
    Ok(out)
}

fn push_word(out: &mut Vec<Tok>, word: String) -> Result<()> {
    match word.to_ascii_uppercase().as_str() {
        "AND" => {
            out.push(Tok::And);
            return Ok(());
        }
        "OR" => {
            out.push(Tok::Or);
            return Ok(());
        }
        "NOT" => {
            out.push(Tok::Not);
            return Ok(());
        }
        "WHERE" => {
            out.push(Tok::Where);
            return Ok(());
        }
        _ => {}
    }
    if let Some((name, rest)) = word.split_once(':') {
        if let Some(field) = map_field(name) {
            out.push(Tok::Field(field));
            if !rest.is_empty() {
                out.push(Tok::Term(rest.to_string()));
            }
            return Ok(());
        }
        if !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            return Err(Error::Query(format!(
                "unknown field `{name}`; expected title, author, abstract, venue, year, rank, tag, or doi"
            )));
        }
    }
    out.push(Tok::Term(word));
    Ok(())
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    depth: usize,
}

impl Parser {
    fn parse(mut self) -> Result<Node> {
        let node = self.parse_or()?;
        if self.pos != self.toks.len() {
            return Err(Error::Query(format!(
                "unexpected token: {:?}",
                self.toks[self.pos]
            )));
        }
        Ok(node)
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn advance(&mut self) -> Option<Tok> {
        let token = self.toks.get(self.pos).cloned();
        self.pos += 1;
        token
    }

    fn parse_or(&mut self) -> Result<Node> {
        let mut nodes = vec![self.parse_and()?];
        while matches!(self.peek(), Some(Tok::Or)) {
            self.advance();
            nodes.push(self.parse_and()?);
        }
        Ok(collapse(nodes, true))
    }

    fn parse_and(&mut self) -> Result<Node> {
        let mut nodes = vec![self.parse_unary()?];
        loop {
            match self.peek() {
                None | Some(Tok::RParen | Tok::Or) => break,
                Some(Tok::And) => {
                    self.advance();
                    nodes.push(self.parse_unary()?);
                }
                _ => nodes.push(self.parse_unary()?),
            }
        }
        Ok(collapse(nodes, false))
    }

    fn parse_unary(&mut self) -> Result<Node> {
        if self.depth == 64 {
            return Err(Error::Query("query nesting exceeds 64 levels".into()));
        }
        self.depth += 1;
        let result = if matches!(self.peek(), Some(Tok::Not)) {
            self.advance();
            self.parse_unary().map(|node| Node::Not(Box::new(node)))
        } else {
            self.parse_primary()
        };
        self.depth -= 1;
        result
    }

    fn parse_primary(&mut self) -> Result<Node> {
        match self.advance() {
            Some(Tok::LParen) => {
                let inner = self.parse_or()?;
                match self.advance() {
                    Some(Tok::RParen) => Ok(inner),
                    _ => Err(Error::Query("expected ')'".into())),
                }
            }
            Some(Tok::Field(field)) => {
                let operand = self.parse_unary()?;
                Ok(Node::Field(field, Box::new(operand)))
            }
            Some(Tok::Term(term)) => Ok(Node::Term(term)),
            Some(Tok::Phrase(phrase)) => Ok(Node::Phrase(phrase)),
            other => Err(Error::Query(format!("unexpected token: {other:?}"))),
        }
    }
}

fn collapse(mut nodes: Vec<Node>, or: bool) -> Node {
    if nodes.len() == 1 {
        nodes.pop().unwrap()
    } else if or {
        Node::Or(nodes)
    } else {
        Node::And(nodes)
    }
}

fn escape(s: &str) -> String {
    s.replace('"', "\"\"")
}

fn quote_term(term: &str) -> String {
    if let Some(prefix) = term.strip_suffix('*') {
        if !prefix.is_empty() {
            return format!("\"{}\"*", escape(prefix));
        }
    }
    format!("\"{}\"", escape(term))
}

fn render_text(node: &Node) -> Result<String> {
    match node {
        Node::Term(term) => Ok(quote_term(term)),
        Node::Phrase(phrase) => Ok(format!("\"{}\"", escape(phrase))),
        Node::Field(FieldKind::Fts(column), inner) => {
            Ok(format!("{column} : ({})", render_text(inner)?))
        }
        Node::Field(FieldKind::Metadata(field), _) => Err(Error::Query(format!(
            "`{}:` is a metadata filter; move it after `WHERE`",
            field.label()
        ))),
        Node::Or(children) => {
            let parts = children
                .iter()
                .map(render_text)
                .collect::<Result<Vec<_>>>()?;
            Ok(format!("({})", parts.join(" OR ")))
        }
        Node::Not(_) => Err(negation_error()),
        Node::And(children) => {
            let mut positive = Vec::new();
            let mut negative = Vec::new();
            for child in children {
                match child {
                    Node::Not(inner) => negative.push(render_text(inner)?),
                    _ => positive.push(render_text(child)?),
                }
            }
            if positive.is_empty() {
                return Err(negation_error());
            }
            let mut rendered = if positive.len() == 1 {
                positive.remove(0)
            } else {
                format!("({})", positive.join(" AND "))
            };
            for excluded in negative {
                rendered = format!("{rendered} NOT {excluded}");
            }
            Ok(rendered)
        }
    }
}

fn render_text_root(node: &Node) -> Result<Option<String>> {
    if matches!(node, Node::Term(term) if term == "*") {
        return Ok(None);
    }
    if contains_match_all(node) {
        return Err(Error::Query(
            "`*` is only valid as the complete text expression".into(),
        ));
    }
    Ok(Some(render_text(node)?))
}

fn contains_match_all(node: &Node) -> bool {
    match node {
        Node::Term(term) => term == "*",
        Node::Field(_, inner) | Node::Not(inner) => contains_match_all(inner),
        Node::And(children) | Node::Or(children) => children.iter().any(contains_match_all),
        Node::Phrase(_) => false,
    }
}

fn render_filter(node: Node, config: &Config) -> Result<FilterExpr> {
    match node {
        Node::Field(FieldKind::Metadata(field), inner) => filter_atom(field, *inner, config),
        Node::And(children) => Ok(FilterExpr::And(
            children
                .into_iter()
                .map(|child| render_filter(child, config))
                .collect::<Result<_>>()?,
        )),
        Node::Or(children) => Ok(FilterExpr::Or(
            children
                .into_iter()
                .map(|child| render_filter(child, config))
                .collect::<Result<_>>()?,
        )),
        Node::Not(inner) => Ok(FilterExpr::Not(Box::new(render_filter(*inner, config)?))),
        Node::Field(field @ FieldKind::Fts(_), _) => Err(Error::Query(format!(
            "`{}:` is a full-text field; move it before `WHERE`",
            field.label()
        ))),
        Node::Term(term) | Node::Phrase(term) => Err(Error::Query(format!(
            "full-text term `{term}` is not allowed after `WHERE`"
        ))),
    }
}

fn filter_atom(field: MetadataField, node: Node, config: &Config) -> Result<FilterExpr> {
    let value = match node {
        Node::Term(value) | Node::Phrase(value) => value,
        _ => {
            return Err(Error::Query(format!(
                "`{}:` expects one term or quoted value",
                field.label()
            )));
        }
    };
    if value.contains(',') {
        return Err(Error::Query(
            "comma-separated filter values are not supported; use `OR`".into(),
        ));
    }
    match field {
        MetadataField::Venue => {
            let venue = config.venue(&value).ok_or_else(|| {
                Error::Query(format!("unknown venue `{value}` in the active catalog"))
            })?;
            Ok(FilterExpr::Venue(vec![venue.id.clone()]))
        }
        MetadataField::Rank => {
            let venues = config.venues_by_rank(std::slice::from_ref(&value));
            if venues.is_empty() {
                return Err(Error::Query(format!(
                    "unknown rank `{value}` in the active catalog"
                )));
            }
            Ok(FilterExpr::Venue(venues))
        }
        MetadataField::Tag => {
            let venues = config.venues_by_tag(std::slice::from_ref(&value));
            if venues.is_empty() {
                return Err(Error::Query(format!(
                    "unknown tag `{value}` in the active catalog"
                )));
            }
            Ok(FilterExpr::Venue(venues))
        }
        MetadataField::Year => Ok(FilterExpr::Year(parse_year_range(&value)?)),
        MetadataField::Doi => Ok(FilterExpr::Doi(value)),
    }
}

pub fn parse(input: &str, config: &Config) -> Result<ParsedQuery> {
    if input.contains('\0') {
        return Err(Error::Query("query contains a null byte".into()));
    }
    let mut tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Ok(ParsedQuery::default());
    }

    let where_positions = tokens
        .iter()
        .enumerate()
        .filter_map(|(index, token)| matches!(token, Tok::Where).then_some(index))
        .collect::<Vec<_>>();
    if where_positions.len() > 1 {
        return Err(Error::Query("query may contain only one `WHERE`".into()));
    }

    let filter_tokens = where_positions.first().map(|&index| {
        let filter = tokens.split_off(index + 1);
        tokens.pop();
        filter
    });
    if tokens.is_empty() {
        return Err(Error::Query(
            "missing text expression before `WHERE`; use `* WHERE ...`".into(),
        ));
    }
    if filter_tokens.as_ref().is_some_and(Vec::is_empty) {
        return Err(Error::Query(
            "missing metadata expression after `WHERE`".into(),
        ));
    }

    let text = Parser {
        toks: tokens,
        pos: 0,
        depth: 0,
    }
    .parse()?;
    let fts = render_text_root(&text)?;
    let filter = filter_tokens
        .map(|tokens| {
            Parser {
                toks: tokens,
                pos: 0,
                depth: 0,
            }
            .parse()
        })
        .transpose()?
        .map(|node| render_filter(node, config))
        .transpose()?;
    Ok(ParsedQuery { fts, filter })
}

pub fn parse_year_range(s: &str) -> Result<YearRange> {
    let s = s.trim();
    let bad = || {
        Error::Query(format!(
            "invalid year `{s}`; use 2020, 2018-2024, 2020-, or -2019"
        ))
    };
    let year = |value: &str| -> Result<i32> { value.trim().parse().map_err(|_| bad()) };
    match s.split_once('-') {
        None => Ok(YearRange::single(year(s)?)),
        Some((lower, upper)) => {
            let min = (!lower.trim().is_empty())
                .then(|| year(lower))
                .transpose()?;
            let max = (!upper.trim().is_empty())
                .then(|| year(upper))
                .transpose()?;
            YearRange::new(min, max).map_err(|_| bad())
        }
    }
}

fn negation_error() -> Error {
    Error::Query("a negated text term requires a positive term, e.g. `kernel NOT windows`".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::defaults().unwrap()
    }

    fn text(input: &str) -> String {
        parse(input, &config()).unwrap().fts.unwrap()
    }

    #[test]
    fn text_search_syntax() {
        assert_eq!(text("fuzzing"), "\"fuzzing\"");
        assert_eq!(text("fuzzing kernel"), "(\"fuzzing\" AND \"kernel\")");
        assert_eq!(text("a OR b"), "(\"a\" OR \"b\")");
        assert_eq!(text("(a OR b) c"), "((\"a\" OR \"b\") AND \"c\")");
        assert_eq!(text("\"side channel\""), "\"side channel\"");
        assert_eq!(text("fuzz*"), "\"fuzz\"*");
        assert_eq!(text("title:fuzzing"), "title : (\"fuzzing\")");
        assert_eq!(text("author:\"jane doe\""), "authors : (\"jane doe\")");
        assert_eq!(text("kernel NOT windows"), "\"kernel\" NOT \"windows\"");
    }

    #[test]
    fn match_all_and_where_boundary() {
        let parsed = parse("* WHERE tag:security", &config()).unwrap();
        assert!(parsed.fts.is_none());
        assert!(matches!(parsed.filter, Some(FilterExpr::Venue(_))));

        assert!(parse("* AND kernel", &config()).is_err());
        assert!(parse("WHERE tag:ml", &config()).is_err());
        assert!(parse("* WHERE", &config()).is_err());
        assert!(parse("* WHERE tag:ml WHERE year:2020", &config()).is_err());
        assert!(parse("kernel tag:ml", &config()).is_err());
        assert!(parse("kernel WHERE title:fuzzing", &config()).is_err());
        assert!(parse("kernel WHERE malware", &config()).is_err());
    }

    #[test]
    fn metadata_boolean_tree_is_preserved() {
        let parsed = parse(
            "* WHERE (tag:ml AND tag:security) OR NOT year:2020-",
            &config(),
        )
        .unwrap();
        let Some(FilterExpr::Or(parts)) = parsed.filter else {
            panic!("expected OR filter");
        };
        assert!(matches!(parts[0], FilterExpr::And(_)));
        assert!(matches!(parts[1], FilterExpr::Not(_)));
    }

    #[test]
    fn filter_values_are_resolved_and_validated() {
        let parsed = parse(
            "* WHERE venue:oakland AND rank:a* AND tag:security AND doi:10.1145",
            &config(),
        )
        .unwrap();
        let Some(FilterExpr::And(parts)) = parsed.filter else {
            panic!("expected AND filter");
        };
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], FilterExpr::Venue(vec!["SP".into()]));
        assert!(matches!(
            &parts[1],
            FilterExpr::Venue(venues) if venues.iter().any(|venue| venue == "NDSS")
        ));
        assert!(matches!(
            &parts[2],
            FilterExpr::Venue(venues) if venues.iter().any(|venue| venue == "CCS")
        ));
        assert!(parse("* WHERE venue:nope", &config()).is_err());
        assert!(parse("* WHERE rank:nope", &config()).is_err());
        assert!(parse("* WHERE tag:nope", &config()).is_err());
        assert!(parse("* WHERE tag:ml,security", &config()).is_err());
    }

    #[test]
    fn year_ranges() {
        assert_eq!(parse_year_range("2020").unwrap(), YearRange::single(2020));
        assert_eq!(
            parse_year_range("2018-2024").unwrap(),
            YearRange::new(Some(2018), Some(2024)).unwrap()
        );
        assert_eq!(
            parse_year_range("2020-").unwrap(),
            YearRange::new(Some(2020), None).unwrap()
        );
        assert_eq!(
            parse_year_range("-2019").unwrap(),
            YearRange::new(None, Some(2019)).unwrap()
        );
        assert!(parse_year_range("-").is_err());
        assert!(parse_year_range("abc").is_err());
        assert!(parse_year_range("2024-2018").is_err());
    }

    #[test]
    fn syntax_errors_are_strict() {
        assert!(parse("\0", &config()).is_err());
        assert!(parse("NOT windows", &config()).is_err());
        assert!(parse("(a OR b", &config()).is_err());
        assert!(parse("\"unterminated", &config()).is_err());
        assert!(parse("foo:bar", &config()).is_err());
    }

    #[test]
    fn excessive_nesting_returns_an_error_without_overflowing_the_stack() {
        for input in [
            format!("{}term{}", "(".repeat(1000), ")".repeat(1000)),
            format!("{}term", "NOT ".repeat(1000)),
        ] {
            let error = parse(&input, &config()).unwrap_err();
            assert!(error
                .to_string()
                .contains("query nesting exceeds 64 levels"));
        }
        assert!(parse(
            &format!("{}term{}", "(".repeat(63), ")".repeat(63)),
            &config()
        )
        .is_ok());
    }
}
