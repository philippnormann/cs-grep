# cs-grep

Fast, local search across computer science research literature.

![cs-grep TUI](assets/tui.png)

`cs-grep` builds a local SQLite/FTS5 index from DBLP and gives you a clean CLI
and TUI for searching papers with an expressive query language across title,
authors, abstract, venue, year, rank, tag, and DOI.

## Why

- Search across configurable computer science venue catalogs.
- Keep the corpus local and query it quickly with SQLite/FTS5.
- Search in the CLI, TUI, or browser, and export CSV, JSON, or BibTeX for scripts.

## Install

Requires Rust 1.95 or newer.

```sh
cargo install --git https://github.com/philippnormann/cs-grep cs-grep
```

Or from a local checkout:

```sh
cargo install --path crates/cs-grep
```

Cargo installs to `~/.cargo/bin` on macOS/Linux and
`%USERPROFILE%\.cargo\bin` on Windows. Make sure that directory is on `PATH`.

Or use [`nix`](https://nixos.org/):

```sh
nix run "github:philippnormann/cs-grep" -- <arguments> # run once
nix shell "github:philippnormann/cs-grep"              # add to PATH
```

## Use

```sh
cs-grep init
cs-grep update --since 2018
cs-grep --tui
```

In the TUI, use `Tab` to cycle sort modes, arrow keys to move, and `Enter` to
open the selected paper URL.

Sort CLI results with `--sort relevance`, `--sort year`, `--sort venue`, or
`--sort rank`.

Search from the shell:

```sh
cs-grep 'title:fuzzing WHERE venue:ndss AND year:2020-'
cs-grep '("side channel" OR cache) WHERE venue:CCS OR venue:SP'
cs-grep '* WHERE doi:10.1145' --fields venue,year,title,doi
```

More examples:

```sh
# Recent malware-detection papers in A/A* venues
cs-grep 'malware detection WHERE year:2022- AND (rank:A OR rank:A*)' --sort year

# Export matching papers as BibTeX
cs-grep 'kernel fuzz* WHERE venue:USENIX-SEC' --format bibtex > papers.bib

# Script-friendly JSON output
cs-grep 'large language model WHERE year:2023-' --format json

# Limit output for quick triage
cs-grep '(ransomware OR botnet) WHERE year:2020-' --limit 20

# Search a custom database path
cs-grep --db ./papers.db 'symbolic execution WHERE venue:ccs'
```

## Query Language

Queries have a full-text expression followed by optional metadata filters:

```sh
cs-grep '(malware OR botnet) WHERE year:2020- AND NOT venue:CCS'
```

- Both sides support `AND`, `OR`, `NOT`, parentheses, and implicit `AND`.
- Text supports phrases, trailing prefixes such as `fuzz*`, and the fields
  `title:`, `author:`, and `abstract:`.
- `WHERE` supports `venue:`, `year:`, `rank:`, `tag:`, and `doi:`.
- `*` matches all papers when only metadata filters are needed.
- Year values can be `2020`, `2018-2024`, `2020-`, or `-2019`.

Text `NOT` requires a positive text term; metadata filters can be negated
directly.

```sh
cs-grep '* WHERE tag:ml AND tag:security'
cs-grep '* WHERE tag:ml OR tag:security'
```

## Bundles and Tags

Bundles choose which venue catalogs `update` and `enrich` process. Tags filter
papers by venue family during search. Bundled venues use the broad tags
`security`, `ml`, `nlp`, `cv`, `se`, and `hci`; custom venues can define
additional tags. The default bundles are `security`, `ml`, and `se`; `hci` is
available separately.

```sh
cs-grep update --bundle security,ml
cs-grep update --bundle hci
cs-grep enrich --bundle ml
```

```sh
# ML venue papers
cs-grep 'mechanistic interpretability WHERE tag:ml AND year:2023-'

# Papers from ML-security venues
cs-grep '(prompt injection OR jailbreak) WHERE tag:ml AND tag:security'
```

## Venues

The bundled venue catalogs are listed in
[`crates/cs-grep-core/venues/`](crates/cs-grep-core/venues/).

After `cs-grep init`, you can extend or override the catalog with a user
`config.yaml`:

- macOS: `~/Library/Application Support/cs-grep/config.yaml`
- Linux: `~/.config/cs-grep/config.yaml`
- Windows: `%APPDATA%\cs-grep\config.yaml`

You can also pass a specific file with `--config path/to/config.yaml`.

`bundles` selects bundled venue sets for subfields. User venues are always
merged after selected bundles by `id`: reuse an existing `id` to override a
bundled venue, or add a new `id` to extend the catalog. `tags` gives tags the
display names shown by the web UI's topic filters; a tag without one shows its
id.

```yaml
bundles: [security, ml, se]

defaults:
  min_year: 2018

tags:
  malware: Malware Analysis

venues:
  - id: DIMVA
    name: Conference on Detection of Intrusions and Malware & Vulnerability Assessment
    dblp_stream: conf/dimva
    aliases: [dimva]
    rank: B
    tags: [systems, network, malware]
```

Then ingest and search it:

```sh
cs-grep update --venue DIMVA
cs-grep 'malware WHERE venue:DIMVA'
```

`dblp_stream` is the DBLP stream id used by the RDF endpoint, such as
`conf/dimva`. A paper listed in several configured venues is stored under the
last one.

## Abstracts

Abstract enrichment is optional, cached, and best-effort. cs-grep uses paper
DOIs, titles and DBLP paper URLs to find abstracts.

```sh
cs-grep enrich --jobs 8
```

Papers that no source has an abstract for are skipped until a recheck is due:
after 7 days for this year's papers, 30 days for last year's and 180 days for
older ones, or sooner when DBLP changes their title, DOI or URL. To retry all of
them now, for example after adding an API key, run
`sqlite3 <database> 'DELETE FROM abstract_misses'`.

After an upgrade that changes the abstract rules, the next `enrich` re-checks
stored abstracts once and repairs or clears those that no longer pass, so the
cleared ones are fetched again. Back up the database first if you want to keep
the old text.

No API keys are required, but keys can improve rate limits and coverage.

| Variable | Used for | Get a key |
|---|---|---|
| `OPENALEX_API_KEY` | OpenAlex lookups | [openalex.org/settings/api](https://openalex.org/settings/api) |
| `SEMANTIC_SCHOLAR_S2_KEY` | Semantic Scholar lookups | [semanticscholar.org/product/api](https://www.semanticscholar.org/product/api) |
| `OPENREVIEW_USERNAME` / `OPENREVIEW_PASSWORD` | OpenReview lookups | [OpenReview API docs](https://docs.openreview.net/getting-started/using-the-api/installing-and-instantiating-the-python-client) |

Set them in the shell:

```sh
export OPENALEX_API_KEY=...
export SEMANTIC_SCHOLAR_S2_KEY=...
export OPENREVIEW_USERNAME=...
export OPENREVIEW_PASSWORD=...
```

Or place them in a local `.env` file:

```sh
OPENALEX_API_KEY=
SEMANTIC_SCHOLAR_S2_KEY=
OPENREVIEW_USERNAME=
OPENREVIEW_PASSWORD=
```

`.env` is loaded automatically when present.

## Web UI

`cs-grep-web` serves a search page for your cs-grep database. It's useful for
hosting one instance for your group, so everyone can search the same database
without installing anything.

```sh
cargo install --git https://github.com/philippnormann/cs-grep cs-grep-web
cs-grep-web  # http://localhost:5002
```

With Nix, run `nix run "github:philippnormann/cs-grep#csGrepWeb"`. Pass
`--host 0.0.0.0` to allow remote access, and see `cs-grep-web --help` for other
options. Restart the server after changing venues.

The Filters panel narrows results by topic, venue rank, or single venues.
Topics combine, and active filters are kept in the page URL, so a filtered view
can be bookmarked or shared.

### Docker

```sh
docker compose build
docker compose run --rm web cs-grep --db /data/papers.db --config /data/config.yaml init
docker compose run --rm web cs-grep --db /data/papers.db --config /data/config.yaml update --since 2018
docker compose run --rm web cs-grep --db /data/papers.db --config /data/config.yaml enrich  # optional, adds abstracts
docker compose up -d  # http://localhost:5002
```

The database and config live in a Docker volume. With Podman, use
`podman compose`.

To serve a database you already have (Linux paths shown):

```sh
docker run --rm --init --user "$(id -u):$(id -g)" -p 127.0.0.1:5002:5002 \
  -v ~/.local/share/cs-grep:/data -v ~/.config/cs-grep:/config:ro cs-grep:latest \
  cs-grep-web --host 0.0.0.0 --db /data/papers.db --config /config/config.yaml
```

To share it with your group, put it behind a reverse proxy with TLS and
authentication, and run `update` (and optionally `enrich`) from cron. After
changing venues, copy the config in with
`docker compose cp config.yaml web:/data/config.yaml`, then run `update` and
`docker compose restart`.

### HTTP API

#### `GET /api/search`

| Parameter | Description |
| --- | --- |
| `q` | Query in the [query language](#query-language). Empty matches all papers. |
| `sort` | `year` (default), `relevance`, `venue`, or `rank` |
| `limit` | Results per page, 1-500 (default 120) |
| `offset` | Results to skip (default 0) |
| `venues` | Comma-separated venue ids or aliases to search in, ANDed with `q`. Omitted searches all venues; empty searches none. |

```sh
curl 'http://localhost:5002/api/search?q=fuzzing+WHERE+venue:NDSS+AND+year:2024&limit=1'
```

```json
{
  "papers": [
    {
      "dblp_key": "https://dblp.org/rec/conf/ndss/BorrelloFDBQG24",
      "venue": "NDSS",
      "year": 2024,
      "title": "Predictive Context-sensitive Fuzzing.",
      "authors": "Pietro Borrello, Andrea Fioraldi, Daniele Cono D'Elia, ...",
      "doi": null,
      "url": "https://www.ndss-symposium.org/ndss-paper/predictive-context-sensitive-fuzzing/",
      "abstract": "Coverage-guided fuzzers expose bugs by progressively mutating testcases ..."
    }
  ],
  "next_offset": 1,
  "ranks": { "NDSS": "A*", "CCS": "A*" }
}
```

For the next page, pass `next_offset` as `offset`. It is `null` on the last
page. `ranks` maps each venue to its rank.

#### `GET /api/venues`

Returns the venue catalog as `{"venues": [...], "tags": {"ml": "Machine Learning", ...}}`.
Each venue has `id`, `name`, `dblp_stream`, `aliases`, `rank`, and `tags`.

#### `GET /api/bibtex?key=<dblp_key>`

Returns the paper's BibTeX entry as plain text.

#### Errors

Errors come back as JSON with a 4xx or 5xx status:

```json
{ "error": "limit must be between 1 and 500" }
```

## Acknowledgements

Inspired by [top4grep](https://github.com/Kyle-Kyle/top4grep).

## License

Released under the [MIT License](LICENSE).
