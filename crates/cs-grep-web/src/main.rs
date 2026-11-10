use std::collections::{BTreeMap, HashMap};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::{
    extract::{rejection::QueryRejection, Query, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use clap::Parser;
use cs_grep_core::config::{Config, Venue};
use cs_grep_core::db::{Database, Search, Sort};
use cs_grep_core::query::{self, FilterExpr};
use cs_grep_core::{output, Paper};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tracing::{info, warn};

macro_rules! asset {
    ($name:literal) => {
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/", $name))
    };
}

const INDEX_HTML: &str = asset!("index.html");
/// Files served under `/static/`, as (name, content type, contents).
const STATIC_ASSETS: &[(&str, &str, &str)] = &[
    ("styles.css", "text/css", asset!("styles.css")),
    ("filters.css", "text/css", asset!("filters.css")),
    ("app.js", "application/javascript", asset!("app.js")),
    ("dom.js", "application/javascript", asset!("dom.js")),
    ("filters.js", "application/javascript", asset!("filters.js")),
    (
        "venue-selection.js",
        "application/javascript",
        asset!("venue-selection.js"),
    ),
];
const MAX_LIMIT: usize = 500;
const MAX_QUERY_BYTES: usize = 4096;
const MAX_DB_JOBS: usize = 8;
const QUERY_TIME_LIMIT: Duration = Duration::from_secs(5);
const QUEUE_TIMEOUT: Duration = Duration::from_secs(2);

/// Access log middleware - logs method, path, status, latency
async fn access_log(req: axum::http::Request<axum::body::Body>, next: Next) -> impl IntoResponse {
    let start = Instant::now();
    let method = req.method().clone();
    // Log the path only so search text stays out of the logs.
    let path = req.uri().path().to_owned();
    let response = next.run(req).await;
    let latency = start.elapsed().as_millis();
    info!(
        "{} {} {} {}ms",
        method,
        path,
        response.status().as_u16(),
        latency
    );
    response
}

/// Application state shared across all requests
#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    db_path: Arc<PathBuf>,
    ranks: Arc<HashMap<String, String>>,
    jobs: Arc<Semaphore>,
}

impl AppState {
    fn new(config: Config, db_path: PathBuf) -> Self {
        let mut ranks = HashMap::new();
        for venue in &config.venues {
            if let Some(rank) = venue.rank.as_deref().filter(|r| !r.is_empty()) {
                ranks.insert(venue.id.clone(), rank.to_string());
            }
        }
        Self {
            config: Arc::new(config),
            db_path: Arc::new(db_path),
            ranks: Arc::new(ranks),
            jobs: Arc::new(Semaphore::new(MAX_DB_JOBS)),
        }
    }

    async fn with_db<T: Send + 'static>(
        &self,
        job: impl FnOnce(&Database) -> cs_grep_core::Result<T> + Send + 'static,
    ) -> Result<T, ApiError> {
        // Waiters are served in arrival order, so a busy server stays fair.
        let permit = tokio::time::timeout(QUEUE_TIMEOUT, self.jobs.clone().acquire_owned())
            .await
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| {
                ApiError(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "server busy, retry shortly".into(),
                )
            })?;
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            // Keep admission occupied even if the client cancels the request.
            let _permit = permit;
            // One connection per job so a slow query doesn't block the others.
            let db = Database::open_read_only(&db_path).map_err(internal_error)?;
            db.set_time_limit(QUERY_TIME_LIMIT)
                .map_err(internal_error)?;
            job(&db).map_err(database_error)
        })
        .await
        .map_err(internal_error)?
    }
}

#[derive(Debug)]
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.0, Json(ErrorResponse { error: self.1 })).into_response();
        if self.0 == StatusCode::SERVICE_UNAVAILABLE {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        response
    }
}

impl From<QueryRejection> for ApiError {
    fn from(error: QueryRejection) -> Self {
        Self(StatusCode::BAD_REQUEST, error.body_text())
    }
}

fn bad_request(message: impl ToString) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.to_string())
}

fn internal_error(error: impl std::fmt::Display) -> ApiError {
    warn!("request failed: {error}");
    ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal server error".into(),
    )
}

fn database_error(error: cs_grep_core::Error) -> ApiError {
    if let cs_grep_core::Error::Db(ref db_error) = error {
        if db_error.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted) {
            return bad_request("query took too long");
        }
        let message = db_error.to_string();
        if message == "fts5: parser stack overflow"
            || message.starts_with("fts5 expression tree is too large (")
            || message.starts_with("Expression tree is too large (")
        {
            return bad_request("query is too complex");
        }
    }
    internal_error(error)
}

#[derive(Parser, Debug)]
#[command(name = "cs-grep-web", version)]
struct Args {
    /// Host to listen on
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Port to listen on
    #[arg(short, long, default_value_t = 5002)]
    port: u16,
    /// Override database path
    #[arg(long)]
    db: Option<PathBuf>,
    /// Override user config path
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Deserialize)]
struct SearchParams {
    #[serde(default)]
    q: String,
    sort: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
    /// Comma-separated venue ids. Omitted means all venues; empty means none.
    venues: Option<String>,
}

fn default_limit() -> usize {
    120
}

#[derive(Deserialize)]
struct BibtexParams {
    key: String,
}

#[derive(Serialize)]
struct SearchResponse {
    papers: Vec<Paper>,
    next_offset: Option<usize>,
    /// All matching papers; counted on the first page only, so scrolling stays fast.
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<usize>,
    ranks: HashMap<String, String>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .without_time()
        .with_ansi(std::io::stdout().is_terminal())
        .init();

    let args = Args::parse();

    // Resolve paths
    let paths = cs_grep_core::config::Paths::resolve().context("failed to resolve paths")?;
    let db_path = args.db.unwrap_or_else(|| paths.db_path());
    let config_path = args.config.unwrap_or_else(|| paths.user_config_path());
    let config =
        Config::load_with_bundles(Some(&config_path), None).context("loading venue config")?;
    Database::open_read_only(&db_path).with_context(|| {
        format!(
            "no database at {}; run `cs-grep init` then `cs-grep update`",
            db_path.display()
        )
    })?;
    let app = router(AppState::new(config, db_path));

    let listener = tokio::net::TcpListener::bind((args.host.as_str(), args.port))
        .await
        .with_context(|| format!("failed to bind to {}:{}", args.host, args.port))?;
    info!(
        "cs-grep web UI listening on http://{}",
        listener.local_addr()?
    );
    axum::serve(listener, app).await?;
    Ok(())
}

fn router(state: AppState) -> Router {
    let mut router = Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route("/api/search", get(api_search))
        .route("/api/bibtex", get(api_bibtex))
        .route("/api/venues", get(api_venues));
    for &(name, content_type, contents) in STATIC_ASSETS {
        router = router.route(
            &format!("/static/{name}"),
            get(move || async move { ([(header::CONTENT_TYPE, content_type)], contents) }),
        );
    }
    router
        .fallback(|| async { ApiError(StatusCode::NOT_FOUND, "not found".into()) })
        .method_not_allowed_fallback(|| async {
            ApiError(StatusCode::METHOD_NOT_ALLOWED, "method not allowed".into())
        })
        .layer(middleware::from_fn(access_log))
        .with_state(state)
}

async fn api_search(
    State(state): State<AppState>,
    params: Result<Query<SearchParams>, QueryRejection>,
) -> Result<Json<SearchResponse>, ApiError> {
    let Query(params) = params?;
    if params.q.len() > MAX_QUERY_BYTES {
        return Err(bad_request(format!(
            "q must be at most {MAX_QUERY_BYTES} bytes"
        )));
    }
    if !(1..=MAX_LIMIT).contains(&params.limit) {
        return Err(bad_request(format!(
            "limit must be between 1 and {MAX_LIMIT}"
        )));
    }
    if params.offset > (i64::MAX as usize).saturating_sub(params.limit + 1) {
        return Err(bad_request("offset is too large"));
    }
    let sort = match params.sort.as_deref().unwrap_or("year") {
        "year" => Sort::Year,
        "relevance" => Sort::Relevance,
        "venue" => Sort::Venue,
        "rank" => Sort::Rank(state.config.rank_sort_order()),
        _ => return Err(bad_request("sort must be year, relevance, venue, or rank")),
    };
    let parsed = query::parse(&params.q, &state.config).map_err(bad_request)?;
    let mut filter = parsed.filter;
    if let Some(venues) = &params.venues {
        let selectors: Vec<String> = venues
            .split(',')
            .filter(|id| !id.is_empty())
            .map(String::from)
            .collect();
        let selected = FilterExpr::Venue(
            state
                .config
                .resolve_venues(&selectors)
                .map_err(bad_request)?,
        );
        filter = Some(match filter {
            Some(typed) => FilterExpr::And(vec![typed, selected]),
            None => selected,
        });
    }
    let search = Search {
        fts: parsed.fts,
        filter,
        sort,
        limit: Some(params.limit + 1),
        offset: Some(params.offset),
    };
    let first_page = params.offset == 0;
    let (mut papers, total) = state
        .with_db(move |db| {
            let total = first_page.then(|| db.search_count(&search)).transpose()?;
            Ok((db.search(&search)?, total))
        })
        .await?;
    let next_offset = (papers.len() > params.limit).then_some(params.offset + params.limit);
    papers.truncate(params.limit);
    Ok(Json(SearchResponse {
        papers,
        next_offset,
        total,
        ranks: (*state.ranks).clone(),
    }))
}

#[derive(Serialize)]
struct VenuesResponse {
    venues: Vec<Venue>,
    tags: BTreeMap<String, String>,
}

async fn api_venues(State(state): State<AppState>) -> Json<VenuesResponse> {
    Json(VenuesResponse {
        venues: state.config.venues.clone(),
        tags: state.config.tag_labels.clone(),
    })
}

async fn api_bibtex(
    State(state): State<AppState>,
    params: Result<Query<BibtexParams>, QueryRejection>,
) -> Result<String, ApiError> {
    let Query(params) = params?;
    let paper = state
        .with_db(move |db| db.get_by_key(&params.key))
        .await?
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "paper not found".into()))?;
    output::render(&[paper], output::Format::Bibtex, None).map_err(internal_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::ServiceExt;

    fn paper(key: &str, venue: &str, year: i32, title: &str) -> Paper {
        Paper {
            dblp_key: key.into(),
            venue: venue.into(),
            year,
            title: title.into(),
            authors: "Alice Smith".into(),
            doi: Some("10.1/x".into()),
            url: Some("https://example.com".into()),
            abstract_text: Some("Research abstract".into()),
        }
    }

    fn test_state(extra: &[Paper]) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("papers.db");
        let mut db = Database::open(&path).unwrap();
        db.upsert_papers(&[
            paper("k1", "NDSS", 2020, "Fuzzing the Linux kernel"),
            paper("k2", "CCS", 2021, "Side channel attacks"),
            paper("k3", "SP", 2019, "Kernel exploitation"),
        ])
        .unwrap();
        db.upsert_papers(extra).unwrap();
        (dir, AppState::new(Config::defaults().unwrap(), path))
    }

    fn test_app() -> (tempfile::TempDir, Router) {
        let (dir, state) = test_state(&[]);
        (dir, router(state))
    }

    async fn request(app: Router, uri: &str) -> Response {
        app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn json(response: Response) -> serde_json::Value {
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn search_filters_and_paginates_without_an_extra_empty_page() {
        let (_dir, app) = test_app();
        let first = json(request(app.clone(), "/api/search?limit=2").await).await;
        assert_eq!(first["papers"].as_array().unwrap().len(), 2);
        assert_eq!(first["papers"][0]["dblp_key"], "k2");
        assert_eq!(first["papers"][1]["dblp_key"], "k1");
        assert_eq!(first["next_offset"], 2);
        assert_eq!(first["total"], 3);
        assert_eq!(first["ranks"]["NDSS"], "A*");
        let last = json(request(app.clone(), "/api/search?limit=1&offset=2").await).await;
        assert_eq!(last["papers"][0]["dblp_key"], "k3");
        assert_eq!(last["next_offset"], serde_json::Value::Null);
        assert!(last.get("total").is_none(), "later pages skip the count");
        let empty = json(request(app.clone(), "/api/search?offset=3").await).await;
        assert!(empty["papers"].as_array().unwrap().is_empty());
        assert_eq!(empty["next_offset"], serde_json::Value::Null);
        for sort in ["year", "relevance", "venue", "rank"] {
            let result = json(
                request(
                    app.clone(),
                    // `_` checks that unknown parameters are ignored.
                    &format!("/api/search?q=fuzzing&sort={sort}&_=1"),
                )
                .await,
            )
            .await;
            assert_eq!(result["papers"].as_array().unwrap().len(), 1);
            assert_eq!(result["papers"][0]["dblp_key"], "k1");
        }
    }

    #[tokio::test]
    async fn venue_selection_narrows_typed_filters() {
        let (_dir, app) = test_app();
        let keys = |result: serde_json::Value| -> Vec<String> {
            let papers = result["papers"].as_array().unwrap();
            papers
                .iter()
                .map(|p| p["dblp_key"].as_str().unwrap().into())
                .collect()
        };
        for (params, expected) in [
            ("venues=NDSS,ccs,NDSS", vec!["k2", "k1"]),
            ("venues=", vec![]),
            ("venues=SP&q=*+WHERE+venue:SP+OR+venue:NDSS", vec!["k3"]),
        ] {
            let result = json(request(app.clone(), &format!("/api/search?{params}")).await).await;
            assert_eq!(keys(result), expected, "{params}");
        }
        let response = request(app.clone(), "/api/search?venues=NDSS,nope").await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let catalog = json(request(app, "/api/venues").await).await;
        let venues = catalog["venues"].as_array().unwrap();
        assert!(venues.iter().any(|v| v["id"] == "NDSS"));
        assert_eq!(catalog["tags"]["ml"], "Machine Learning");
    }

    #[tokio::test]
    async fn default_and_maximum_pages_are_bounded() {
        let extra: Vec<_> = (0..510)
            .map(|i| paper(&format!("extra{i}"), "NDSS", 2025, "Extra paper"))
            .collect();
        let (_dir, state) = test_state(&extra);
        let app = router(state);
        for (uri, limit) in [("/api/search", 120), ("/api/search?limit=500", 500)] {
            let result = json(request(app.clone(), uri).await).await;
            assert_eq!(result["papers"].as_array().unwrap().len(), limit);
            assert_eq!(result["next_offset"], limit);
        }
    }

    #[tokio::test]
    async fn invalid_parameters_and_queries_return_json_400() {
        let (_dir, app) = test_app();
        for params in [
            "limit=0",
            "limit=501",
            "limit=-1",
            "limit=nonsense",
            "limit=",
            "limit=18446744073709551615",
            "offset=-1",
            "offset=nope",
            "offset=9223372036854775807",
            "limit=1&limit=2",
            "sort=unknown",
            "sort=",
            "q=%22unclosed",
            "q=%00",
            "q=year:notanumber",
        ] {
            let response = request(app.clone(), &format!("/api/search?{params}")).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{params}");
            assert!(json(response).await["error"].is_string());
        }
        let response = request(
            app.clone(),
            &format!("/api/search?q={}", "x".repeat(MAX_QUERY_BYTES + 1)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(json(response).await["error"].is_string());
        let response = request(
            app,
            &format!("/api/search?q={}term{}", "(".repeat(1000), ")".repeat(1000)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(json(response).await["error"]
            .as_str()
            .unwrap()
            .contains("nesting"));
    }

    #[tokio::test]
    async fn sqlite_query_limits_return_json_400() {
        let (_dir, app) = test_app();
        for query in [
            format!("x{}", "+NOT+y".repeat(600)),
            format!("{}fuzzing", "title:+".repeat(63)),
            format!("*+WHERE{}", "+doi:a".repeat(600)),
        ] {
            let response = request(app.clone(), &format!("/api/search?q={query}")).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert_eq!(json(response).await["error"], "query is too complex");
            assert_eq!(
                request(app.clone(), "/api/search?q=fuzzing&limit=1")
                    .await
                    .status(),
                StatusCode::OK
            );
        }
    }

    #[tokio::test]
    async fn bibtex_returns_text_or_json_errors() {
        let (_dir, app) = test_app();
        let response = request(app.clone(), "/api/bibtex?key=k1").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        let text = String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(text.contains("Fuzzing the Linux kernel"));
        for (uri, status) in [
            ("/api/bibtex?key=missing", StatusCode::NOT_FOUND),
            ("/api/bibtex?key=", StatusCode::NOT_FOUND),
            ("/api/bibtex", StatusCode::BAD_REQUEST),
        ] {
            let response = request(app.clone(), uri).await;
            assert_eq!(response.status(), status);
            assert!(json(response).await["error"].is_string());
        }
    }

    #[tokio::test]
    async fn production_routes_serve_assets_and_json_errors() {
        let (_dir, app) = test_app();
        let assets = STATIC_ASSETS
            .iter()
            .map(|(name, content_type, _)| (format!("/static/{name}"), *content_type));
        for (uri, content_type) in
            std::iter::once(("/".to_string(), "text/html; charset=utf-8")).chain(assets)
        {
            let response = request(app.clone(), &uri).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert_eq!(response.headers()[header::CONTENT_TYPE], content_type);
        }
        for uri in ["/api/unknown", "/unknown"] {
            let response = request(app.clone(), uri).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            assert!(json(response).await["error"].is_string());
        }
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/search")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(json(response).await["error"].is_string());
    }

    #[tokio::test(start_paused = true)]
    async fn overload_rejects_database_work_but_keeps_assets_available() {
        let (_dir, state) = test_state(&[]);
        let _permits = state
            .jobs
            .clone()
            .try_acquire_many_owned(MAX_DB_JOBS as u32)
            .unwrap();
        let app = router(state);
        for uri in ["/api/search", "/api/bibtex?key=k1"] {
            let response = request(app.clone(), uri).await;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(response.headers()[header::RETRY_AFTER], "1");
            assert!(json(response).await["error"].is_string());
        }
        assert_eq!(
            request(app, "/static/app.js").await.status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn cancelled_requests_keep_their_blocking_job_permit() {
        let (_dir, state) = test_state(&[]);
        let worker = state.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            worker
                .with_db(move |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .await
        });
        entered_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(state.jobs.available_permits(), MAX_DB_JOBS - 1);
        // The blocked job has its own connection, so other searches still run.
        let response = request(router(state), "/api/search?q=fuzzing").await;
        assert_eq!(response.status(), StatusCode::OK);
        release_tx.send(()).unwrap();
    }

    #[tokio::test]
    async fn interrupted_queries_return_400() {
        let extra: Vec<_> = (0..500)
            .map(|i| paper(&format!("extra{i}"), "NDSS", 2025, "Extra paper"))
            .collect();
        let (_dir, state) = test_state(&extra);
        let error = state
            .with_db(|db| {
                db.set_time_limit(Duration::ZERO)?;
                db.search(&Search::default())
            })
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(error.1, "query took too long");
    }

    #[tokio::test]
    async fn internal_database_errors_are_not_reported_as_bad_queries() {
        let (_dir, state) = test_state(&[]);
        let error = state
            .with_db(|db| {
                db.search(&Search {
                    fts: Some("\"".into()),
                    ..Default::default()
                })
            })
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            json(error.into_response()).await,
            serde_json::json!({"error": "internal server error"})
        );
    }
}
