mod tui;

use std::{
    collections::BTreeMap,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use cs_grep_core::abstracts::{EnrichResult, Enricher};
use cs_grep_core::config::{Config, Paths, Secrets};
use cs_grep_core::db::{Database, Search, Sort};
use cs_grep_core::dblp::Dblp;
use cs_grep_core::output::{self, Column, Format};
use cs_grep_core::query;
use cs_grep_core::Paper;

#[derive(Parser)]
#[command(
    name = "cs-grep",
    about = "Search computer science research literature",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Search expression: text [WHERE metadata filters].
    #[arg(value_name = "QUERY")]
    query: Vec<String>,

    /// Result ordering (default: relevance).
    #[arg(long, value_enum)]
    sort: Option<SortMode>,

    /// Output format (default: table).
    #[arg(long)]
    format: Option<Format>,

    /// Limit number of results.
    #[arg(long)]
    limit: Option<usize>,

    /// Columns for table/csv output (comma-separated).
    #[arg(long, value_delimiter = ',')]
    fields: Vec<Column>,

    /// Launch the interactive TUI.
    #[arg(long)]
    tui: bool,

    /// Override database path.
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    /// Override user config.yaml path.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
}

impl Cli {
    fn has_search_args(&self) -> bool {
        !self.query.is_empty()
            || self.sort.is_some()
            || self.format.is_some()
            || self.limit.is_some()
            || !self.fields.is_empty()
            || self.tui
    }
}

#[derive(Subcommand)]
enum Command {
    /// Create the data/config directories and an empty database.
    Init,
    /// Fetch paper metadata from dblp (incremental, idempotent).
    Update(UpdateArgs),
    /// Fill missing abstracts on the existing database (no dblp re-fetch).
    Enrich(EnrichArgs),
}

/// Default number of concurrent abstract fetches.
const DEFAULT_JOBS: usize = 8;
const ENRICH_BATCH_SIZE: usize = 500;
const ENRICH_PROGRESS_INTERVAL: usize = 500;

#[derive(clap::Args)]
struct UpdateArgs {
    /// Only ingest from these venues (id or alias).
    #[arg(long, value_delimiter = ',')]
    venue: Vec<String>,
    /// Use these bundled venue sets for this update (overrides config bundles).
    #[arg(long, value_delimiter = ',')]
    bundle: Vec<String>,
    /// Minimum year (overrides config default).
    #[arg(long)]
    since: Option<i32>,
}

#[derive(clap::Args)]
struct EnrichArgs {
    /// Only enrich these venues (id or alias); default is all.
    #[arg(long, value_delimiter = ',')]
    venue: Vec<String>,
    /// Use these bundled venue sets for this enrich run (overrides config bundles).
    #[arg(long, value_delimiter = ',')]
    bundle: Vec<String>,
    /// Only enrich papers from this year onward.
    #[arg(long)]
    since: Option<i32>,
    /// Concurrent abstract fetches.
    #[arg(long, default_value_t = DEFAULT_JOBS)]
    jobs: usize,
    /// Stop after this many papers (useful for sampling / validation).
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SortMode {
    Relevance,
    Year,
    Venue,
    Rank,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let error = format!("{error:#}");
            eprintln!("error: {}", output::terminal_safe(&error));
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    reject_search_args_for_subcommands(&cli)?;
    let paths = Paths::resolve()?;
    let bundle_override = match &cli.command {
        Some(Command::Update(args)) if !args.bundle.is_empty() => Some(args.bundle.as_slice()),
        Some(Command::Enrich(args)) if !args.bundle.is_empty() => Some(args.bundle.as_slice()),
        _ => None,
    };
    let config = load_config_with_bundles(&cli, &paths, bundle_override)?;

    match &cli.command {
        Some(Command::Init) => cmd_init(&cli, &paths),
        Some(Command::Update(args)) => cmd_update(args, &cli, &paths, &config).await,
        Some(Command::Enrich(args)) => cmd_enrich(args, &cli, &paths, &config).await,
        None if cli.tui => {
            let db = open_db(&cli, &paths)?;
            tui::run(db, config)
        }
        None => cmd_search(&cli, &paths, &config),
    }
}

fn reject_search_args_for_subcommands(cli: &Cli) -> Result<()> {
    if cli.command.is_some() && cli.has_search_args() {
        anyhow::bail!(
            "search query/options cannot be used with subcommands; put command-specific options after the subcommand"
        );
    }
    Ok(())
}

fn log_header(title: &str) {
    eprintln!("{title}");
}

fn log_field(label: &str, value: impl std::fmt::Display) {
    let value = value.to_string();
    eprintln!("  {label:<10} {}", output::terminal_safe(&value));
}

fn log_blank() {
    eprintln!();
}

fn load_config_with_bundles(
    cli: &Cli,
    paths: &Paths,
    bundle_override: Option<&[String]>,
) -> Result<Config> {
    let user_path = config_path(cli, paths);
    Config::load_with_bundles(Some(&user_path), bundle_override).context("loading venue config")
}

fn config_path(cli: &Cli, paths: &Paths) -> PathBuf {
    cli.config
        .clone()
        .unwrap_or_else(|| paths.user_config_path())
}

fn db_path(cli: &Cli, paths: &Paths) -> PathBuf {
    cli.db.clone().unwrap_or_else(|| paths.db_path())
}

fn open_db(cli: &Cli, paths: &Paths) -> Result<Database> {
    let path = db_path(cli, paths);
    Database::open_existing(&path).with_context(|| {
        format!(
            "no database at {}; run `cs-grep init` then `cs-grep update`",
            path.display()
        )
    })
}

fn cmd_init(cli: &Cli, paths: &Paths) -> Result<()> {
    paths.ensure_dirs()?;
    let path = db_path(cli, paths);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Database::open(&path).context("creating database")?;
    let config_path = config_path(cli, paths);
    write_default_config(&config_path)?;
    log_header("cs-grep initialized");
    log_field("database", path.display());
    log_field("config", config_path.display());
    log_blank();
    log_field("next", "`cs-grep update`");
    Ok(())
}

fn write_default_config(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, Config::default_user_config_yaml()?)?;
    Ok(())
}

fn cmd_search(cli: &Cli, paths: &Paths, config: &Config) -> Result<()> {
    let db = open_db(cli, paths)?;

    let raw = cli.query.join(" ");
    let search = build_search(
        &raw,
        config,
        cli.sort.unwrap_or(SortMode::Relevance),
        cli.limit,
        None,
    )?;
    let papers = db.search(&search)?;

    let columns = (!cli.fields.is_empty()).then_some(cli.fields.as_slice());
    let format = cli.format.unwrap_or(Format::Table);
    let out = if std::io::stdout().is_terminal() {
        output::render_terminal(&papers, format, columns)
    } else {
        output::render(&papers, format, columns)
    }
    .map_err(|e| anyhow::anyhow!(e))?;
    if !out.is_empty() {
        print!("{out}");
        if !out.ends_with('\n') {
            println!();
        }
    }
    if matches!(format, Format::Table) {
        eprintln!("results    {}", papers.len());
    }
    Ok(())
}

pub(crate) fn build_search(
    raw_query: &str,
    config: &Config,
    sort: SortMode,
    limit: Option<usize>,
    offset: Option<usize>,
) -> cs_grep_core::Result<Search> {
    let parsed = query::parse(raw_query, config)?;
    let sort = match sort {
        SortMode::Relevance => Sort::Relevance,
        SortMode::Year => Sort::Year,
        SortMode::Venue => Sort::Venue,
        SortMode::Rank => Sort::Rank(config.rank_sort_order()),
    };

    Ok(Search {
        fts: parsed.fts,
        filter: parsed.filter,
        sort,
        limit,
        offset,
    })
}

async fn cmd_update(args: &UpdateArgs, cli: &Cli, paths: &Paths, config: &Config) -> Result<()> {
    paths.ensure_dirs()?;
    let path = db_path(cli, paths);
    let mut db = Database::open(&path).context("opening database")?;

    let venue_ids = if args.venue.is_empty() {
        config.all_venue_ids()
    } else {
        config.resolve_venues(&args.venue)?
    };
    let min_year = args.since.unwrap_or(config.defaults.min_year);

    log_header("cs-grep update");
    log_field("bundles", config.bundles.join(", "));
    log_field("venues", venue_ids.len());
    log_field("since", min_year);
    log_blank();

    let dblp = Dblp::default();
    let mut total = 0usize;
    let mut failed = Vec::new();
    for id in &venue_ids {
        let venue = config.venue(id).expect("resolved venue");
        let safe_id = output::terminal_safe(id);
        eprint!("  {safe_id:<12} ");
        let _ = std::io::stderr().flush();
        match dblp.fetch_venue(venue, min_year).await {
            Ok(papers) => {
                let n = db.upsert_papers(&papers)?;
                total += papers.len();
                eprintln!("fetched {:>5} papers, {:>5} upserted", papers.len(), n);
            }
            Err(e) => {
                let error = e.to_string();
                eprintln!("failed   {}", output::terminal_safe(&error));
                failed.push(id.clone());
            }
        }
    }
    log_blank();
    log_header("summary");
    log_field("fetched", format_args!("{total} papers"));
    log_field("failed", failed.len());
    log_field("database", format_args!("{} papers", db.count()?));

    if !failed.is_empty() {
        anyhow::bail!("failed to fetch venue(s): {}", failed.join(", "));
    }

    Ok(())
}

async fn cmd_enrich(args: &EnrichArgs, cli: &Cli, paths: &Paths, config: &Config) -> Result<()> {
    let mut db = open_db(cli, paths)?;
    let venue_ids = if args.venue.is_empty() {
        if args.bundle.is_empty() {
            Vec::new()
        } else {
            config.all_venue_ids()
        }
    } else {
        config.resolve_venues(&args.venue)?
    };
    let years = match args.since {
        Some(year) => vec![query::YearRange::new(Some(year), None)?],
        None => Vec::new(),
    };
    enrich_abstracts(&mut db, &venue_ids, &years, args.jobs, args.limit).await
}

/// Fill missing abstracts, running up to `jobs` fetches concurrently.
/// `venue_ids` empty means all venues; `limit` caps how many are attempted.
async fn enrich_abstracts(
    db: &mut Database,
    venue_ids: &[String],
    years: &[query::YearRange],
    jobs: usize,
    limit: Option<usize>,
) -> Result<()> {
    let mut enricher = Enricher::new(Secrets::load());
    let pending = db.count_missing_abstracts(venue_ids, years)?;
    let total = limit.map_or(pending, |limit| limit.min(pending));
    let jobs = jobs.max(1);
    log_header("abstract enrichment");
    log_field("pending", format_args!("{pending} abstracts"));
    if !years.is_empty() {
        log_field("years", format_args!("{}", format_year_ranges(years)));
    }
    if total != pending {
        log_field("selected", format_args!("{total} abstracts"));
    }
    log_field("jobs", jobs);
    log_blank();

    let mut filled = 0usize;
    let mut processed = 0usize;
    let mut misses: BTreeMap<String, usize> = BTreeMap::new();
    let mut after_id = 0;
    while processed < total {
        let remaining = total - processed;
        let batch = db.papers_missing_abstract_batch(
            venue_ids,
            years,
            after_id,
            remaining.min(ENRICH_BATCH_SIZE),
        )?;
        let Some(next_after_id) = batch.last().map(|paper| paper.id) else {
            break;
        };
        after_id = next_after_id;

        let papers = batch
            .into_iter()
            .map(|missing| missing.paper)
            .collect::<Vec<_>>();

        let batch_len = papers.len();
        log_field(
            "batch",
            format_args!(
                "{}-{} / {} ({})",
                processed + 1,
                processed + batch_len,
                total,
                batch_venue_summary(&papers)
            ),
        );
        let results = enricher.enrich_many(papers, jobs).await;

        let mut abstract_updates = Vec::new();
        for (paper, res) in results {
            processed += 1;
            let dblp_key = paper.dblp_key;
            match res {
                Ok(EnrichResult::Found(abs)) => {
                    abstract_updates.push((dblp_key, abs));
                    filled += 1;
                }
                Ok(EnrichResult::Missing(reason)) => {
                    *misses.entry(reason).or_default() += 1;
                }
                Err(e) => {
                    let error = e.to_string();
                    eprintln!(
                        "warning: abstract fetch failed for {}: {}",
                        output::terminal_safe(&dblp_key),
                        output::terminal_safe(&error)
                    );
                }
            }
            if processed.is_multiple_of(ENRICH_PROGRESS_INTERVAL) {
                log_field(
                    "progress",
                    format_args!("{processed}/{total} processed, {filled} filled"),
                );
            }
        }
        log_field(
            "batch done",
            format_args!(
                "{} filled, {} missed",
                abstract_updates.len(),
                batch_len - abstract_updates.len()
            ),
        );
        db.set_abstracts(&abstract_updates)?;
    }
    log_blank();
    log_header("summary");
    log_field("filled", format_args!("{filled}/{processed} abstracts"));
    if !misses.is_empty() {
        log_field("missed", processed - filled);
        for (reason, count) in misses {
            eprintln!("  {count:>10} {}", output::terminal_safe(&reason));
        }
    }
    Ok(())
}

fn format_year_ranges(years: &[query::YearRange]) -> String {
    years
        .iter()
        .map(|year| match year.bounds() {
            (Some(min), Some(max)) if min == max => min.to_string(),
            (Some(min), Some(max)) => format!("{min}-{max}"),
            (Some(min), None) => format!("{min}-"),
            (None, Some(max)) => format!("-{max}"),
            (None, None) => String::new(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn batch_venue_summary(inputs: &[Paper]) -> String {
    let mut counts = BTreeMap::new();
    for paper in inputs {
        *counts.entry(paper.venue.as_str()).or_insert(0usize) += 1;
    }
    counts
        .into_iter()
        .map(|(venue, count)| format!("{venue}:{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_test_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("cs-grep-{name}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn build_search_maps_query_and_options() {
        let config = Config::defaults().unwrap();
        let search = build_search(
            "malware WHERE tag:ml AND year:2020-",
            &config,
            SortMode::Year,
            Some(10),
            Some(5),
        )
        .unwrap();

        assert!(search.fts.is_some());
        assert!(matches!(search.filter, Some(query::FilterExpr::And(_))));
        assert!(matches!(search.sort, Sort::Year));
        assert_eq!(search.limit, Some(10));
        assert_eq!(search.offset, Some(5));
    }

    #[test]
    fn enrich_options_are_parsed() {
        let cli = Cli::try_parse_from([
            "cs-grep",
            "enrich",
            "--bundle",
            "se,ml",
            "--venue",
            "acl,emnlp",
            "--since",
            "2025",
            "--jobs",
            "4",
            "--limit",
            "10",
        ])
        .unwrap();
        let Some(Command::Enrich(args)) = cli.command else {
            panic!("expected enrich command");
        };
        assert_eq!(args.bundle, vec!["se".to_string(), "ml".to_string()]);
        assert_eq!(args.venue, vec!["acl".to_string(), "emnlp".to_string()]);
        assert_eq!(args.since, Some(2025));
        assert_eq!(args.jobs, 4);
        assert_eq!(args.limit, Some(10));
    }

    #[test]
    fn search_output_options_are_parsed_and_rejected_with_commands() {
        let cli =
            Cli::try_parse_from(["cs-grep", "--format", "json", "--fields", "year,title"]).unwrap();
        assert_eq!(cli.format, Some(Format::Json));
        assert_eq!(cli.fields, vec![Column::Year, Column::Title]);

        let cli = Cli::try_parse_from(["cs-grep", "--format", "json", "update"]).unwrap();
        assert!(reject_search_args_for_subcommands(&cli).is_err());

        let cli = Cli::try_parse_from(["cs-grep", "--db", "papers.db", "update"]).unwrap();
        assert!(reject_search_args_for_subcommands(&cli).is_ok());
    }

    #[test]
    fn bundle_override_limits_venue_resolution() {
        let cli =
            Cli::try_parse_from(["cs-grep", "update", "--bundle", "se,ml", "--venue", "ndss"])
                .unwrap();
        let paths = Paths {
            data_dir: temp_test_path("data"),
            config_dir: temp_test_path("config"),
        };
        let Some(Command::Update(args)) = &cli.command else {
            panic!("expected update command");
        };
        let config = load_config_with_bundles(&cli, &paths, Some(&args.bundle)).unwrap();
        assert!(config.venue("ICSE").is_some());
        assert!(config.venue("ICML").is_some());
        assert!(config.resolve_venues(&args.venue).is_err());
    }

    #[test]
    fn write_default_config_creates_but_does_not_overwrite() {
        let path = temp_test_path("config.yaml");
        let _ = std::fs::remove_file(&path);
        write_default_config(&path).unwrap();
        let default = std::fs::read_to_string(&path).unwrap();
        assert!(default.contains("security"));
        assert!(default.contains("min_year: 2000"));

        std::fs::write(&path, "bundles: []\n").unwrap();
        write_default_config(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "bundles: []\n");
        let _ = std::fs::remove_file(path);
    }
}
