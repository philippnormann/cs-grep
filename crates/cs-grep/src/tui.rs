//! Interactive search TUI: type a query, browse results, read details.

use std::{
    collections::HashMap,
    io,
    process::{Command, Stdio},
};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap};

use crate::{build_search, SortMode};
use cs_grep_core::config::Config;
use cs_grep_core::db::{Database, Search};
use cs_grep_core::{Error as CoreError, Paper, Result as CoreResult};
use url::Url;

const BORDER: Color = Color::Rgb(51, 65, 85);
const HEADER: Color = Color::Rgb(125, 211, 252);
const LINK: Color = Color::Rgb(94, 234, 212);
const VENUE: Color = Color::Rgb(245, 158, 11);
const DIM: Color = Color::Rgb(100, 116, 139);
const MUTED: Color = Color::Rgb(148, 163, 184);
const TEXT: Color = Color::Rgb(226, 232, 240);
const SELECTED_BG: Color = Color::Rgb(30, 41, 59);
const MIN_WINDOW_SIZE: usize = 128;
const MAX_WINDOW_SIZE: usize = 512;
const VENUE_COL_WIDTH: usize = 10;
const MIN_RANK_COL_WIDTH: usize = 4;
const SORT_MODES: [SortMode; 4] = [
    SortMode::Year,
    SortMode::Rank,
    SortMode::Relevance,
    SortMode::Venue,
];

struct App {
    db: Database,
    config: Config,
    rank_display: RankDisplay,
    input: String,
    cursor: usize,
    sort: SortMode,
    results: Vec<Paper>,
    window_start: usize,
    total: usize,
    selected: usize,
    status: String,
}

struct RankDisplay {
    ranks: HashMap<String, String>,
    width: usize,
}

impl RankDisplay {
    fn from_config(config: &Config) -> Self {
        let ranks = config
            .venues
            .iter()
            .filter_map(|venue| {
                let rank = venue.rank.as_deref().filter(|rank| !rank.is_empty())?;
                Some((venue.id.clone(), rank.to_string()))
            })
            .collect::<HashMap<_, _>>();
        let width = ranks
            .values()
            .map(|rank| rank.chars().count() + 2)
            .max()
            .unwrap_or(0)
            .max(MIN_RANK_COL_WIDTH);
        Self { ranks, width }
    }

    fn rank(&self, venue: &str) -> Option<&str> {
        self.ranks.get(venue).map(String::as_str)
    }

    fn cell(&self, venue: &str) -> String {
        match self.ranks.get(venue) {
            Some(rank) => format!("{:<width$}", format!("[{rank}]"), width = self.width),
            None => format!("{:<width$}", "", width = self.width),
        }
    }
}

impl App {
    fn new(db: Database, config: Config) -> Self {
        let rank_display = RankDisplay::from_config(&config);
        let mut app = Self {
            db,
            config,
            rank_display,
            input: String::new(),
            cursor: 0,
            sort: SortMode::Year,
            results: Vec::new(),
            window_start: 0,
            total: 0,
            selected: 0,
            status: String::new(),
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        let page_size = current_page_size();
        let search = match self.base_search() {
            Ok(search) => search,
            Err(e) => {
                self.set_error(query_error_status(&e));
                return;
            }
        };
        let total = match self.db.search_count(&search) {
            Ok(total) => total,
            Err(e) => {
                self.set_error(format!("db error: {e}"));
                return;
            }
        };
        self.total = total;
        self.selected = 0;
        self.load_window_for_search(&search, page_size);
    }

    fn base_search(&self) -> CoreResult<Search> {
        build_search(&self.input, &self.config, self.sort, None, None)
    }

    fn load_window(&mut self, page_size: usize) {
        let search = match self.base_search() {
            Ok(search) => search,
            Err(e) => {
                self.set_error(query_error_status(&e));
                return;
            }
        };
        self.load_window_for_search(&search, page_size);
    }

    fn load_window_for_search(&mut self, search: &Search, page_size: usize) {
        let window_size = window_size(page_size);
        let window_start = result_window_start(self.selected, window_size, self.total);
        match self.fetch_window(search, window_start, window_size) {
            Ok(mut rows) => {
                let mut window_start = window_start;
                if rows.is_empty() && window_start > 0 {
                    self.total = window_start;
                    self.selected = self.selected.min(window_start.saturating_sub(1));
                    window_start = result_window_start(self.selected, window_size, self.total);
                    match self.fetch_window(search, window_start, window_size) {
                        Ok(retry_rows) => {
                            rows = retry_rows;
                            self.finish_window_load(window_start, rows);
                        }
                        Err(e) => self.set_error(format!("db error: {e}")),
                    }
                } else {
                    self.finish_window_load(window_start, rows);
                }
            }
            Err(e) => self.set_error(format!("db error: {e}")),
        }
    }

    fn fetch_window(
        &self,
        search: &Search,
        window_start: usize,
        window_size: usize,
    ) -> CoreResult<Vec<Paper>> {
        let mut search = search.clone();
        search.limit = Some(window_size);
        search.offset = Some(window_start);
        self.db.search(&search)
    }

    fn finish_window_load(&mut self, window_start: usize, rows: Vec<Paper>) {
        self.results = rows;
        self.window_start = window_start;
        self.status = result_status(self.total);
    }

    fn ensure_visible_loaded(&mut self, page_size: usize) {
        let page_size = bounded_page_size(page_size);
        let visible_start = visible_result_start(self.selected, page_size, self.total);
        let visible_stop = visible_end(visible_start, page_size, self.total);
        let preload = preload_size(page_size);
        let wanted_start = visible_start.saturating_sub(preload);
        let wanted_end = visible_end(visible_stop, preload, self.total);
        let loaded_start = self.window_start;
        let loaded_end = self.window_start + self.results.len();
        if wanted_start < loaded_start || wanted_end > loaded_end {
            self.load_window(page_size);
        }
    }

    fn set_error(&mut self, status: String) {
        self.results.clear();
        self.window_start = 0;
        self.total = 0;
        self.selected = 0;
        self.status = status;
    }

    fn has_no_results(&self) -> bool {
        self.total == 0
    }

    fn selected_paper(&self) -> Option<&Paper> {
        let index = self.selected.checked_sub(self.window_start)?;
        self.results.get(index)
    }

    fn cycle_sort(&mut self, direction: SortDirection) {
        self.sort = next_sort(self.sort, direction);
        self.refresh();
    }

    fn open_selected_url(&mut self) {
        let Some(url) = self.selected_url() else {
            self.status = "no URL for selected paper".to_string();
            return;
        };
        match open_url(&url) {
            Ok(()) => self.status = "opened paper URL".to_string(),
            Err(e) => self.status = format!("open failed: {e}"),
        }
    }

    fn selected_url(&self) -> Option<String> {
        self.selected_paper().and_then(|paper| {
            paper
                .url
                .as_deref()
                .filter(|url| !url.is_empty())
                .map(str::to_string)
                .or_else(|| doi_url(paper))
        })
    }

    fn move_cursor(&mut self, delta: isize) {
        self.cursor = offset_index(self.cursor, delta).min(input_len(&self.input));
    }

    fn move_cursor_to_start(&mut self) {
        self.cursor = 0;
    }

    fn move_cursor_to_end(&mut self) {
        self.cursor = input_len(&self.input);
    }

    fn insert_char(&mut self, c: char) {
        let index = byte_index(&self.input, self.cursor);
        self.input.insert(index, c);
        self.cursor += 1;
        self.refresh();
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = byte_index(&self.input, self.cursor - 1);
        let end = byte_index(&self.input, self.cursor);
        self.input.replace_range(start..end, "");
        self.cursor -= 1;
        self.refresh();
    }

    fn delete_char(&mut self) {
        if self.cursor >= input_len(&self.input) {
            return;
        }
        let start = byte_index(&self.input, self.cursor);
        let end = byte_index(&self.input, self.cursor + 1);
        self.input.replace_range(start..end, "");
        self.refresh();
    }

    fn move_selection(&mut self, delta: isize) {
        self.set_selected(offset_index(self.selected, delta));
    }

    fn jump(&mut self, to: usize) {
        self.set_selected(to);
    }

    fn set_selected(&mut self, target: usize) {
        if self.has_no_results() {
            return;
        }
        self.selected = target.min(self.total.saturating_sub(1));
        self.ensure_visible_loaded(current_page_size());
    }

    fn jump_to_end(&mut self) {
        if self.total > 0 {
            self.selected = self.total - 1;
            self.load_window(current_page_size());
        }
    }
}

#[derive(Clone, Copy)]
enum SortDirection {
    Forward,
    Backward,
}

pub fn run(db: Database, config: Config) -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        anyhow::bail!("--tui requires an interactive terminal");
    }
    let mut terminal = ratatui::init();
    let app = App::new(db, config);
    let res = event_loop(&mut terminal, app);
    ratatui::restore();
    res
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, mut app: App) -> Result<()> {
    loop {
        app.ensure_visible_loaded(current_page_size());
        terminal.draw(|f| draw(f, &app))?;
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let (_, height) = terminal::size()?;
            let page_size = results_page_size(height);
            match (key.code, key.modifiers) {
                (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => break,
                (KeyCode::Down, _) => app.move_selection(1),
                (KeyCode::Up, _) => app.move_selection(-1),
                (KeyCode::PageDown, _) => app.move_selection(page_size as isize),
                (KeyCode::PageUp, _) => app.move_selection(-(page_size as isize)),
                (KeyCode::Tab, _) => app.cycle_sort(SortDirection::Forward),
                (KeyCode::BackTab, _) => app.cycle_sort(SortDirection::Backward),
                (KeyCode::Enter, _) => app.open_selected_url(),
                (KeyCode::Home, KeyModifiers::CONTROL) => app.jump(0),
                (KeyCode::End, KeyModifiers::CONTROL) => app.jump_to_end(),
                (KeyCode::Home, _) | (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
                    app.move_cursor_to_start();
                }
                (KeyCode::End, _) | (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                    app.move_cursor_to_end();
                }
                (KeyCode::Left, _) => app.move_cursor(-1),
                (KeyCode::Right, _) => app.move_cursor(1),
                (KeyCode::Backspace, _) => app.backspace(),
                (KeyCode::Delete, _) => app.delete_char(),
                (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                    app.insert_char(c);
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn draw(f: &mut Frame, app: &App) {
    let detail_height = detail_panel_height(f.area().height);
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(7),
        Constraint::Length(detail_height),
        Constraint::Length(1),
    ])
    .split(f.area());

    let input_width = chunks[0].width.saturating_sub(2) as usize;
    let input_view = input_view(&app.input, input_width, app.cursor);
    let input_line = input_line(&input_view);
    let input = Paragraph::new(input_line)
        .style(Style::default().fg(TEXT))
        .block(panel(" query "));
    f.render_widget(input, chunks[0]);
    render_input_cursor(f, chunks[0], input_view.cursor_offset);

    let page_size = bounded_page_size(chunks[1].height.saturating_sub(2).max(1) as usize);
    let start = visible_result_start(app.selected, page_size, app.total);
    let local_start = start
        .saturating_sub(app.window_start)
        .min(app.results.len());
    let local_end = (local_start + page_size).min(app.results.len());
    let items: Vec<ListItem> = app.results[local_start..local_end]
        .iter()
        .map(|paper| result_item(&app.rank_display, paper))
        .collect();
    let mut state = ListState::default();
    if !items.is_empty() && app.selected >= start {
        state.select(Some((app.selected - start).min(items.len() - 1)));
    }
    let title = results_title(app);
    let list = List::new(items)
        .block(panel(title))
        .style(Style::default().fg(TEXT))
        .highlight_style(
            Style::default()
                .fg(TEXT)
                .bg(SELECTED_BG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸ ");
    f.render_stateful_widget(list, chunks[1], &mut state);

    f.render_widget(detail(app), chunks[2]);
    f.render_widget(footer(), chunks[3]);
}

fn detail_panel_height(total_height: u16) -> u16 {
    let target = (total_height / 3).clamp(14, 20);
    let max_height = total_height
        .saturating_sub(3)
        .saturating_sub(7)
        .saturating_sub(1)
        .max(6);
    target.min(max_height)
}

fn results_page_size(total_height: u16) -> usize {
    total_height
        .saturating_sub(3)
        .saturating_sub(detail_panel_height(total_height))
        .saturating_sub(1)
        .saturating_sub(2)
        .max(1) as usize
}

fn visible_result_start(selected: usize, page_size: usize, total: usize) -> usize {
    bounded_start(selected, page_size, total)
}

fn visible_end(start: usize, size: usize, total: usize) -> usize {
    start.saturating_add(size).min(total)
}

fn current_page_size() -> usize {
    terminal::size().map_or(20, |(_, height)| {
        bounded_page_size(results_page_size(height))
    })
}

fn bounded_page_size(page_size: usize) -> usize {
    page_size.clamp(1, MAX_WINDOW_SIZE)
}

fn window_size(page_size: usize) -> usize {
    let page_size = bounded_page_size(page_size);
    page_size
        .saturating_mul(3)
        .clamp(MIN_WINDOW_SIZE, MAX_WINDOW_SIZE)
}

fn preload_size(page_size: usize) -> usize {
    let page_size = bounded_page_size(page_size);
    window_size(page_size).saturating_sub(page_size) / 2
}

fn result_window_start(selected: usize, window_size: usize, total: usize) -> usize {
    bounded_start(selected, window_size, total)
}

fn bounded_start(selected: usize, size: usize, total: usize) -> usize {
    if total == 0 || total <= size {
        return 0;
    }
    selected.saturating_sub(size / 2).min(total - size)
}

fn offset_index(index: usize, delta: isize) -> usize {
    if delta.is_negative() {
        index.saturating_sub(delta.unsigned_abs())
    } else {
        index.saturating_add(delta.unsigned_abs())
    }
}

fn panel(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(BORDER))
        .title(Line::from(Span::styled(
            title.into(),
            Style::default().fg(HEADER).add_modifier(Modifier::BOLD),
        )))
}

struct InputView {
    text: String,
    cursor_offset: u16,
    is_placeholder: bool,
}

fn input_view(input: &str, width: usize, cursor: usize) -> InputView {
    let max_width = width.max(1);
    let len = input_len(input);
    let cursor = cursor.min(len);
    if input.is_empty() {
        return InputView {
            text: "all papers".to_string(),
            cursor_offset: 0,
            is_placeholder: true,
        };
    }
    let start = if cursor >= max_width {
        cursor + 1 - max_width
    } else {
        0
    };
    let text = input.chars().skip(start).take(max_width).collect();
    let cursor_offset = cursor
        .saturating_sub(start)
        .min(max_width.saturating_sub(1)) as u16;
    InputView {
        text,
        cursor_offset,
        is_placeholder: false,
    }
}

fn input_line(view: &InputView) -> Line<'static> {
    let color = if view.is_placeholder { DIM } else { TEXT };
    Line::from(Span::styled(view.text.clone(), Style::default().fg(color)))
}

fn render_input_cursor(f: &mut Frame, area: Rect, cursor_offset: u16) {
    let width = area.width.saturating_sub(2);
    let offset = cursor_offset.min(width.saturating_sub(1));
    f.set_cursor_position(Position::new(area.x + 1 + offset, area.y + 1));
}

fn input_len(input: &str) -> usize {
    input.chars().count()
}

fn byte_index(input: &str, char_index: usize) -> usize {
    input
        .char_indices()
        .nth(char_index)
        .map_or(input.len(), |(index, _)| index)
}

fn results_title(app: &App) -> String {
    if app.has_no_results() {
        return format!(" results · {} ", app.status);
    }
    let position = app.selected + 1;
    format!(
        " results · {} · sort {} · {position}/{} ",
        app.status,
        sort_label(app.sort),
        app.total
    )
}

fn result_status(total: usize) -> String {
    format!("{total} papers")
}

fn next_sort(sort: SortMode, direction: SortDirection) -> SortMode {
    let index = SORT_MODES
        .iter()
        .position(|mode| *mode == sort)
        .unwrap_or(0);
    let next = match direction {
        SortDirection::Forward => (index + 1) % SORT_MODES.len(),
        SortDirection::Backward => (index + SORT_MODES.len() - 1) % SORT_MODES.len(),
    };
    SORT_MODES[next]
}

fn sort_label(sort: SortMode) -> &'static str {
    match sort {
        SortMode::Year => "year",
        SortMode::Rank => "rank",
        SortMode::Relevance => "relevance",
        SortMode::Venue => "venue",
    }
}

fn result_item(rank_display: &RankDisplay, p: &Paper) -> ListItem<'static> {
    let mut spans = vec![
        Span::styled(
            format!("{:<VENUE_COL_WIDTH$}", p.venue),
            Style::default().fg(VENUE).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(format!("{:>4}", p.year), Style::default().fg(MUTED)),
        Span::raw(" "),
        Span::styled(rank_display.cell(&p.venue), Style::default().fg(DIM)),
        Span::raw("  "),
    ];
    spans.push(Span::styled(p.title.clone(), Style::default().fg(TEXT)));
    ListItem::new(Line::from(spans))
}

fn detail(app: &App) -> Paragraph<'static> {
    let Some(p) = app.selected_paper() else {
        return Paragraph::new(Line::from(Span::styled(
            "No paper selected",
            Style::default().fg(DIM),
        )))
        .block(panel(" paper "));
    };
    let mut lines = vec![
        Line::from(Span::styled(
            p.title.clone(),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(metadata_spans(&app.rank_display, p)),
    ];
    lines.push(Line::from(""));
    lines.push(label_line("authors", &p.authors, MUTED));
    if let Some(doi) = p.doi.as_deref().filter(|doi| !doi.is_empty()) {
        lines.push(label_line("doi", doi, LINK));
    }
    if let Some(url) = &p.url {
        lines.push(label_line("url", url, LINK));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "abstract",
        Style::default().fg(DIM).add_modifier(Modifier::BOLD),
    )));
    if let Some(abs) = p.abstract_text.as_deref().filter(|abs| !abs.is_empty()) {
        lines.push(Line::from(Span::styled(
            abs.to_string(),
            Style::default().fg(MUTED),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "No abstract available",
            Style::default().fg(DIM),
        )));
    }
    Paragraph::new(lines)
        .style(Style::default().fg(TEXT))
        .block(panel(" paper "))
        .wrap(Wrap { trim: true })
}

fn footer() -> Paragraph<'static> {
    Paragraph::new(Line::from(vec![
        Span::styled(
            "Tab",
            Style::default().fg(HEADER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" sort  ", Style::default().fg(DIM)),
        Span::styled(
            "Enter",
            Style::default().fg(HEADER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" open  ", Style::default().fg(DIM)),
        Span::styled(
            "↑↓",
            Style::default().fg(HEADER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" move  ", Style::default().fg(DIM)),
        Span::styled("\"phrase\"", Style::default().fg(LINK)),
        Span::styled(" WHERE tag:security", Style::default().fg(DIM)),
    ]))
}

fn query_error_status(error: &CoreError) -> String {
    match error {
        CoreError::Query(message) => format!("query: {message}"),
        _ => error.to_string(),
    }
}

fn metadata_spans(rank_display: &RankDisplay, p: &Paper) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    push_meta(
        &mut spans,
        p.venue.clone(),
        Style::default().fg(VENUE).add_modifier(Modifier::BOLD),
    );
    push_meta(&mut spans, p.year.to_string(), Style::default().fg(MUTED));
    if let Some(rank) = rank_display.rank(&p.venue) {
        push_meta(
            &mut spans,
            format!("rank {rank}"),
            Style::default().fg(MUTED),
        );
    }
    spans
}

fn push_meta(spans: &mut Vec<Span<'static>>, value: String, style: Style) {
    if !spans.is_empty() {
        spans.push(Span::styled("  ·  ", Style::default().fg(DIM)));
    }
    spans.push(Span::styled(value, style));
}

fn label_line(label: &str, value: &str, value_color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label:<8}"),
            Style::default().fg(DIM).add_modifier(Modifier::BOLD),
        ),
        Span::styled(value.to_string(), Style::default().fg(value_color)),
    ])
}

fn doi_url(paper: &Paper) -> Option<String> {
    paper
        .doi
        .as_deref()
        .filter(|doi| !doi.is_empty())
        .map(|doi| format!("https://doi.org/{doi}"))
}

fn open_url(url: &str) -> io::Result<()> {
    let url = browser_url(url).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "only http(s) URLs can be opened",
        )
    })?;
    let mut command = opener_command(url.as_str());
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

fn browser_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    matches!(url.scheme(), "http" | "https").then_some(url)
}

#[cfg(target_os = "macos")]
fn opener_command(url: &str) -> Command {
    let mut command = Command::new("open");
    command.arg(url);
    command
}

#[cfg(target_os = "windows")]
fn opener_command(url: &str) -> Command {
    let mut command = Command::new("rundll32.exe");
    command.args(["url.dll,FileProtocolHandler", url]);
    command
}

#[cfg(all(unix, not(target_os = "macos")))]
fn opener_command(url: &str) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(url);
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use cs_grep_core::config::{Defaults, Venue};

    fn paper(index: usize) -> Paper {
        Paper {
            dblp_key: format!("k{index}"),
            venue: "NDSS".to_string(),
            year: 2024,
            title: format!("Paper {index}"),
            authors: "Alice Example".to_string(),
            doi: None,
            url: None,
            abstract_text: None,
        }
    }

    fn config_with_ranks(ranks: &[(&str, &str)]) -> Config {
        Config {
            bundles: Vec::new(),
            defaults: Defaults::default(),
            venues: ranks
                .iter()
                .map(|(id, rank)| Venue {
                    id: id.to_string(),
                    name: String::new(),
                    dblp_stream: format!("conf/{}", id.to_ascii_lowercase()),
                    aliases: Vec::new(),
                    rank: (!rank.is_empty()).then(|| rank.to_string()),
                    tags: Vec::new(),
                })
                .collect(),
            tag_labels: Default::default(),
        }
    }

    #[test]
    fn window_size_has_fixed_upper_bound() {
        assert_eq!(window_size(1), MIN_WINDOW_SIZE);
        assert_eq!(window_size(10_000), MAX_WINDOW_SIZE);
    }

    #[test]
    fn window_start_stays_bounded_near_selection() {
        let size = window_size(20);
        assert_eq!(size, MIN_WINDOW_SIZE);
        assert_eq!(result_window_start(0, size, 10_000), 0);
        assert_eq!(result_window_start(5_000, size, 10_000), 5_000 - size / 2);
        assert_eq!(result_window_start(9_999, size, 10_000), 10_000 - size);
    }

    #[test]
    fn preload_fits_inside_window_bound() {
        let page_size = 40;
        assert!(page_size + preload_size(page_size) * 2 <= MAX_WINDOW_SIZE);
    }

    #[test]
    fn browser_url_accepts_only_http_urls() {
        assert!(browser_url("https://example.com/paper").is_some());
        assert!(browser_url("http://example.com/paper").is_some());
        assert!(browser_url("file:///tmp/paper").is_none());
        assert!(browser_url("mailto:paper@example.com").is_none());
    }

    #[test]
    fn sort_cycle_includes_rank() {
        assert_eq!(
            next_sort(SortMode::Year, SortDirection::Forward),
            SortMode::Rank
        );
        assert_eq!(
            next_sort(SortMode::Rank, SortDirection::Forward),
            SortMode::Relevance
        );
        assert_eq!(
            next_sort(SortMode::Relevance, SortDirection::Backward),
            SortMode::Rank
        );
        assert_eq!(
            next_sort(SortMode::Rank, SortDirection::Backward),
            SortMode::Year
        );
    }

    #[test]
    fn rank_display_uses_configured_width() {
        let cfg = config_with_ranks(&[("NDSS", "A*"), ("RAID", "A")]);
        let display = RankDisplay::from_config(&cfg);
        assert_eq!(display.cell("NDSS"), "[A*]");
        assert_eq!(display.cell("RAID"), "[A] ");
        assert_eq!(display.cell("UNKNOWN"), "    ");
        assert_eq!(display.rank("NDSS"), Some("A*"));
    }

    #[test]
    fn rank_display_expands_for_longer_configured_ranks() {
        let cfg = config_with_ranks(&[("NDSS", "A-plus"), ("RAID", "A")]);
        let display = RankDisplay::from_config(&cfg);
        assert_eq!(display.cell("NDSS"), "[A-plus]");
        assert_eq!(display.cell("RAID"), "[A]     ");
    }

    #[test]
    fn refresh_counts_all_results_but_keeps_window_bounded() {
        let mut db = Database::open_in_memory().unwrap();
        let papers = (0..1_000).map(paper).collect::<Vec<_>>();
        db.upsert_papers(&papers).unwrap();

        let app = App::new(db, Config::defaults().unwrap());

        assert_eq!(app.total, 1_000);
        assert!(app.results.len() <= MAX_WINDOW_SIZE);
        assert_eq!(app.status, "1000 papers");
    }
}
