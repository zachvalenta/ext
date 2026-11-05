use std::{
    collections::HashSet,
    env,
    ffi::OsStr,
    fs,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process,
    process::Command,
    time::Duration,
};

use clap::{ArgGroup, Parser, ValueEnum};
use crossterm::{
    event::{self, Event as TermEvent, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use pulldown_cmark::{Event, HeadingLevel, Parser as MdParser, Tag, TagEnd};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Terminal,
};
use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Parser)]
#[command(about = "Extract heading-based outline trees from Markdown files.")]
#[command(group(ArgGroup::new("output_mode").args(["tree", "outline", "json", "yaml", "tui"]).multiple(false)))]
struct Args {
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    #[arg(long, value_enum)]
    format: Option<OutputFormat>,

    #[arg(long)]
    tree: bool,

    #[arg(long)]
    outline: bool,

    #[arg(long)]
    json: bool,

    #[arg(long)]
    yaml: bool,

    #[arg(long)]
    tui: bool,

    #[arg(long)]
    output: Option<PathBuf>,

    #[arg(long)]
    config: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = StyleMode::Auto)]
    style: StyleMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormat {
    Outline,
    Tree,
    Json,
    Yaml,
    Tui,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum StyleMode {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Debug, Deserialize)]
struct Config {
    #[serde(default)]
    ignore_heading_paths: Vec<Vec<String>>,
    #[serde(default = "default_ignore_mode")]
    ignore_mode: IgnoreMode,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum IgnoreMode {
    Subtree,
    Node,
}

fn default_ignore_mode() -> IgnoreMode {
    IgnoreMode::Subtree
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ignore_heading_paths: Vec::new(),
            ignore_mode: IgnoreMode::Subtree,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct Node {
    text: String,
    level: u8,
    children: Vec<Node>,
}

#[derive(Clone, Debug, Serialize)]
struct FileRecord {
    path: String,
    headings: Vec<Node>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = Args::parse();
    let config = load_config(args.config.as_deref())?;
    let files = find_markdown_files(&args.paths);

    if files.is_empty() {
        return Err("No Markdown files found.".to_string());
    }

    let records = file_records(&files, &config)?;
    let format = selected_format(&args);

    if format == OutputFormat::Tui {
        if args.output.is_some() {
            return Err("--output cannot be used with TUI output".to_string());
        }
        return run_tui(&records);
    }

    let use_style = should_style(args.style, args.output.is_some());
    let output = render_output(&records, format, use_style)?;

    if let Some(output_path) = args.output {
        fs::write(output_path, output).map_err(|err| format!("Failed to write output: {err}"))?;
    } else {
        println!("{output}");
    }

    Ok(())
}

fn selected_format(args: &Args) -> OutputFormat {
    if args.tree {
        return OutputFormat::Tree;
    }
    if args.outline {
        return OutputFormat::Outline;
    }
    if args.json {
        return OutputFormat::Json;
    }
    if args.yaml {
        return OutputFormat::Yaml;
    }
    if args.tui {
        return OutputFormat::Tui;
    }
    if let Some(format) = args.format {
        return format;
    }
    if args.output.is_none() && io::stdout().is_terminal() {
        OutputFormat::Tui
    } else {
        OutputFormat::Tree
    }
}

fn load_config(config_path: Option<&Path>) -> Result<Config, String> {
    let candidates = if let Some(path) = config_path {
        vec![expand_home(path)]
    } else {
        let mut paths = Vec::new();
        let cwd = env::current_dir().map_err(|err| format!("Failed to read cwd: {err}"))?;
        paths.push(cwd.join(".ext.json"));
        for parent in cwd.ancestors().skip(1) {
            paths.push(parent.join(".ext.json"));
        }
        paths.push(home_dir().join(".config/ext/config.json"));
        paths
    };

    for path in candidates {
        if path.exists() {
            let text = fs::read_to_string(&path)
                .map_err(|err| format!("Failed to read config {}: {err}", path.display()))?;
            return serde_json::from_str(&text)
                .map_err(|err| format!("Failed to parse config {}: {err}", path.display()));
        }
    }

    Ok(Config::default())
}

fn expand_home(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        return home_dir();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    path.to_path_buf()
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn find_markdown_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();

    for raw_path in paths {
        let path = expand_home(raw_path);
        if !path.exists() {
            eprintln!("File not found: {}", path.display());
            continue;
        }

        if path.is_file() {
            if is_markdown(&path) {
                files.push(path);
            } else {
                eprintln!("Skipping non-Markdown file: {}", path.display());
            }
            continue;
        }

        collect_markdown_files(&path, &mut files);
    }

    files.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
    files.dedup();
    files
}

fn collect_markdown_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        eprintln!("Could not read directory: {}", dir.display());
        return;
    };

    let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_markdown_files(&path, files);
        } else if path.is_file() && is_markdown(&path) {
            files.push(path);
        }
    }
}

fn is_markdown(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("md" | "markdown" | "mdown")
    )
}

fn file_records(files: &[PathBuf], config: &Config) -> Result<Vec<FileRecord>, String> {
    files
        .iter()
        .map(|path| {
            let md_text = fs::read_to_string(path)
                .map_err(|err| format!("Failed to read {}: {err}", path.display()))?;
            Ok(FileRecord {
                path: path.to_string_lossy().to_string(),
                headings: extract_heading_tree(&md_text, config),
            })
        })
        .collect()
}

fn extract_heading_tree(md_text: &str, config: &Config) -> Vec<Node> {
    let headings = flat_headings(md_text);
    let tree = nest_headings(&headings);
    let ignored = config
        .ignore_heading_paths
        .iter()
        .cloned()
        .map(|path| path.into_iter().collect::<Vec<_>>())
        .collect::<HashSet<_>>();
    filter_tree(&tree, &ignored, config.ignore_mode, &[])
}

fn flat_headings(md_text: &str) -> Vec<Node> {
    let mut headings = Vec::new();
    let mut current: Option<(u8, String)> = None;

    for event in MdParser::new(md_text) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                current = Some((heading_level(level), String::new()));
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, text)) = current.take() {
                    headings.push(Node {
                        text: text.trim().to_string(),
                        level,
                        children: Vec::new(),
                    });
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((_, ref mut heading_text)) = current {
                    heading_text.push_str(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some((_, ref mut heading_text)) = current {
                    heading_text.push(' ');
                }
            }
            _ => {}
        }
    }

    headings
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn nest_headings(flat_headings: &[Node]) -> Vec<Node> {
    let mut roots: Vec<Node> = Vec::new();
    let mut stack: Vec<Vec<usize>> = Vec::new();

    for heading in flat_headings {
        while let Some(path) = stack.last() {
            if node_at(&roots, path).level < heading.level {
                break;
            }
            stack.pop();
        }

        let node = Node {
            text: heading.text.clone(),
            level: heading.level,
            children: Vec::new(),
        };

        let new_path = if let Some(parent_path) = stack.last() {
            let parent = node_at_mut(&mut roots, parent_path);
            parent.children.push(node);
            let mut path = parent_path.clone();
            path.push(parent.children.len() - 1);
            path
        } else {
            roots.push(node);
            vec![roots.len() - 1]
        };

        stack.push(new_path);
    }

    roots
}

fn node_at<'a>(roots: &'a [Node], path: &[usize]) -> &'a Node {
    let mut node = &roots[path[0]];
    for child_index in path.iter().skip(1) {
        node = &node.children[*child_index];
    }
    node
}

fn node_at_mut<'a>(roots: &'a mut [Node], path: &[usize]) -> &'a mut Node {
    let mut node = &mut roots[path[0]];
    for child_index in path.iter().skip(1) {
        node = &mut node.children[*child_index];
    }
    node
}

fn filter_tree(
    nodes: &[Node],
    ignored_paths: &HashSet<Vec<String>>,
    ignore_mode: IgnoreMode,
    ancestors: &[String],
) -> Vec<Node> {
    let mut filtered = Vec::new();

    for node in nodes {
        let mut path = ancestors.to_vec();
        path.push(node.text.clone());
        let ignored = ignored_paths.contains(&path);

        if ignored && ignore_mode == IgnoreMode::Subtree {
            continue;
        }

        let children = filter_tree(&node.children, ignored_paths, ignore_mode, &path);

        if ignored && ignore_mode == IgnoreMode::Node {
            filtered.extend(children);
        } else {
            filtered.push(Node {
                text: node.text.clone(),
                level: node.level,
                children,
            });
        }
    }

    filtered
}

fn run_tui(records: &[FileRecord]) -> Result<(), String> {
    enable_raw_mode().map_err(|err| format!("Failed to enable raw mode: {err}"))?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)
        .map_err(|err| format!("Failed to enter alternate screen: {err}"))?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal =
        Terminal::new(backend).map_err(|err| format!("Failed to start TUI: {err}"))?;
    let cards = tui_cards(records);
    let mut selected = 0usize;
    let mut scroll: u16 = 0;
    let mut positions = Vec::new();
    let mut search = SearchState::default();
    let mut result = Ok(());

    loop {
        let draw_result = terminal.draw(|frame| {
            let area = frame.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(1)])
                .split(area);
            let body = chunks[0];
            let footer_area = chunks[1];
            positions = layout_tui_cards(&cards, body, scroll);

            if let Some(position) = positions.iter().find(|position| position.index == selected) {
                if let Some(match_y) = search.current_match_y(&cards, selected) {
                    let target_y = position.full_y.saturating_add(match_y);
                    if target_y < scroll {
                        scroll = target_y;
                        positions = layout_tui_cards(&cards, body, scroll);
                    } else {
                        let visible_bottom = scroll.saturating_add(body.height);
                        if target_y >= visible_bottom {
                            scroll = target_y.saturating_sub(body.height.saturating_sub(1));
                            positions = layout_tui_cards(&cards, body, scroll);
                        }
                    }
                } else if position.full_y < scroll {
                    scroll = position.full_y;
                    positions = layout_tui_cards(&cards, body, scroll);
                } else {
                    let bottom = position.full_y.saturating_add(position.rect.height);
                    let visible_bottom = scroll.saturating_add(body.height);
                    if bottom > visible_bottom {
                        scroll = bottom.saturating_sub(body.height);
                        positions = layout_tui_cards(&cards, body, scroll);
                    }
                }
            }

            for position in &positions {
                if !position.visible || position.rect.height == 0 || position.rect.width == 0 {
                    continue;
                }
                let card = &cards[position.index];
                let is_selected = position.index == selected;
                let border = if is_selected {
                    cat_mauve()
                } else {
                    cat_surface2()
                };
                let title_style = Style::default()
                    .fg(cat_rosewater())
                    .add_modifier(Modifier::BOLD);
                let query = search.active_query();
                let mut match_index = query
                    .map(|query| count_matches_before(&cards, position.index, query))
                    .unwrap_or(0);
                let title = highlighted_title(
                    &card.title,
                    title_style,
                    query,
                    search.current_match(),
                    &mut match_index,
                );
                let lines =
                    highlighted_lines(&card.lines, query, search.current_match(), &mut match_index);
                let block = Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border));
                let paragraph = Paragraph::new(lines)
                    .block(block)
                    .scroll((position.clipped_top, 0));
                frame.render_widget(paragraph, position.rect);
            }

            let footer = Paragraph::new(search.footer_line(&cards));
            frame.render_widget(footer, footer_area);
        });

        if let Err(err) = draw_result {
            result = Err(format!("Failed to draw TUI: {err}"));
            break;
        }

        if event::poll(Duration::from_millis(100))
            .map_err(|err| format!("Failed to poll terminal events: {err}"))?
        {
            match event::read().map_err(|err| format!("Failed to read terminal event: {err}"))? {
                TermEvent::Key(key) => match search.mode {
                    SearchMode::Input => match key.code {
                        KeyCode::Enter => {
                            if let Some(index) = search.commit(&cards, selected) {
                                selected = index;
                            }
                        }
                        KeyCode::Esc => search.cancel(),
                        KeyCode::Backspace => search.backspace(),
                        KeyCode::Char(ch) => search.push(ch),
                        _ => {}
                    },
                    SearchMode::Normal => match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Esc => {
                            if search.has_query() {
                                search.clear();
                            } else {
                                break;
                            }
                        }
                        KeyCode::Char('/') => search.start(),
                        KeyCode::Char('n') => {
                            if let Some(index) =
                                search.navigate(&cards, selected, SearchDirection::Next)
                            {
                                selected = index;
                            }
                        }
                        KeyCode::Char('p') => {
                            if let Some(index) =
                                search.navigate(&cards, selected, SearchDirection::Previous)
                            {
                                selected = index;
                            }
                        }
                        KeyCode::Char('h') | KeyCode::Left => selected = selected.saturating_sub(1),
                        KeyCode::Char('l') | KeyCode::Right => {
                            selected = (selected + 1).min(cards.len().saturating_sub(1));
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            selected = adjacent_card(selected, &positions, DirectionHint::Down)
                                .unwrap_or_else(|| {
                                    (selected + 1).min(cards.len().saturating_sub(1))
                                });
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            selected = adjacent_card(selected, &positions, DirectionHint::Up)
                                .unwrap_or_else(|| selected.saturating_sub(1));
                        }
                        KeyCode::PageDown => scroll = scroll.saturating_add(10),
                        KeyCode::PageUp => scroll = scroll.saturating_sub(10),
                        KeyCode::Home => selected = 0,
                        KeyCode::End => selected = cards.len().saturating_sub(1),
                        _ => {}
                    },
                },
                TermEvent::Resize(_, _) => {}
                _ => {}
            }
        }
    }

    disable_raw_mode().map_err(|err| format!("Failed to disable raw mode: {err}"))?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .map_err(|err| format!("Failed to leave alternate screen: {err}"))?;
    terminal
        .show_cursor()
        .map_err(|err| format!("Failed to show cursor: {err}"))?;

    result
}

#[derive(Clone)]
struct TuiCard {
    title: String,
    width: u16,
    height: u16,
    lines: Vec<Line<'static>>,
}

#[derive(Clone)]
struct TuiCardPosition {
    index: usize,
    rect: Rect,
    full_y: u16,
    clipped_top: u16,
    visible: bool,
}

#[derive(Clone, Copy)]
enum DirectionHint {
    Up,
    Down,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SearchMode {
    Normal,
    Input,
}

#[derive(Clone, Copy)]
enum SearchDirection {
    Next,
    Previous,
}

#[derive(Clone, Copy)]
struct SearchMatch {
    card_index: usize,
    line_y: u16,
}

struct SearchState {
    mode: SearchMode,
    query: String,
    input: String,
    cursor: Option<usize>,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            mode: SearchMode::Normal,
            query: String::new(),
            input: String::new(),
            cursor: None,
        }
    }
}

impl SearchState {
    fn start(&mut self) {
        self.mode = SearchMode::Input;
        self.input.clear();
    }

    fn commit(&mut self, cards: &[TuiCard], selected: usize) -> Option<usize> {
        self.query = self.input.trim().to_string();
        self.mode = SearchMode::Normal;
        self.cursor = first_match_at_or_after_card(selected, cards, &self.query);
        self.cursor.and_then(|cursor| {
            search_matches(cards, &self.query)
                .get(cursor)
                .map(|search_match| search_match.card_index)
        })
    }

    fn cancel(&mut self) {
        self.input.clear();
        self.mode = SearchMode::Normal;
    }

    fn clear(&mut self) {
        self.query.clear();
        self.input.clear();
        self.cursor = None;
        self.mode = SearchMode::Normal;
    }

    fn has_query(&self) -> bool {
        !self.query.is_empty()
    }

    fn push(&mut self, ch: char) {
        self.input.push(ch);
    }

    fn backspace(&mut self) {
        self.input.pop();
    }

    fn active_query(&self) -> Option<&str> {
        let text = match self.mode {
            SearchMode::Normal => &self.query,
            SearchMode::Input => &self.input,
        };
        let text = text.trim();
        (!text.is_empty()).then_some(text)
    }

    fn current_match(&self) -> Option<usize> {
        (self.mode == SearchMode::Normal)
            .then_some(self.cursor)
            .flatten()
    }

    fn navigate(
        &mut self,
        cards: &[TuiCard],
        selected: usize,
        direction: SearchDirection,
    ) -> Option<usize> {
        let matches = search_matches(cards, &self.query);
        if matches.is_empty() {
            self.cursor = None;
            return None;
        }

        let cursor = match self.cursor {
            Some(cursor) if cursor < matches.len() => match direction {
                SearchDirection::Next => (cursor + 1) % matches.len(),
                SearchDirection::Previous => (cursor + matches.len() - 1) % matches.len(),
            },
            _ => first_match_at_or_after_card(selected, cards, &self.query).unwrap_or(0),
        };
        self.cursor = Some(cursor);
        matches
            .get(cursor)
            .map(|search_match| search_match.card_index)
    }

    fn current_match_y(&self, cards: &[TuiCard], selected: usize) -> Option<u16> {
        let cursor = self.current_match()?;
        let search_match = *search_matches(cards, &self.query).get(cursor)?;
        (search_match.card_index == selected).then_some(search_match.line_y)
    }

    fn footer_line(&self, cards: &[TuiCard]) -> Line<'static> {
        match self.mode {
            SearchMode::Input => {
                let total = search_matches(cards, &self.input).len();
                Line::from(vec![
                    Span::styled(
                        " NORMAL ",
                        Style::default()
                            .fg(cat_mauve())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" h/j/k/l cards  "),
                    Span::styled(" / ", Style::default().fg(cat_mauve())),
                    Span::styled(self.input.clone(), Style::default().fg(cat_yellow())),
                    Span::styled(format!(" ({total})  "), Style::default().fg(cat_subtext0())),
                    Span::styled(" Enter ", Style::default().fg(cat_mauve())),
                    Span::raw("accept  "),
                    Span::styled(" Esc ", Style::default().fg(cat_mauve())),
                    Span::raw("cancel"),
                ])
            }
            SearchMode::Normal => {
                let mut spans = vec![
                    Span::styled(
                        " NORMAL ",
                        Style::default()
                            .fg(cat_mauve())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" h/j/k/l cards  "),
                    Span::styled(" / ", Style::default().fg(cat_mauve())),
                    Span::raw("search"),
                ];
                if !self.query.is_empty() {
                    let total = search_matches(cards, &self.query).len();
                    let current = self.cursor.map(|cursor| cursor + 1).unwrap_or(0).min(total);
                    spans.push(Span::styled(" /", Style::default().fg(cat_subtext0())));
                    spans.push(Span::styled(
                        self.query.clone(),
                        Style::default().fg(cat_yellow()),
                    ));
                    spans.push(Span::styled(
                        format!(" ({current}/{total})"),
                        Style::default().fg(cat_subtext0()),
                    ));
                }
                spans.extend([
                    Span::raw("  "),
                    Span::styled(" n/p ", Style::default().fg(cat_mauve())),
                    Span::raw("matches  "),
                    Span::styled(" PgUp/PgDn ", Style::default().fg(cat_mauve())),
                    Span::raw("scroll  "),
                    Span::styled(" q ", Style::default().fg(cat_mauve())),
                    Span::raw("quit"),
                ]);
                Line::from(spans)
            }
        }
    }
}

fn tui_cards(records: &[FileRecord]) -> Vec<TuiCard> {
    records
        .iter()
        .map(|record| {
            let mut lines = Vec::new();
            let mut plain_lines = Vec::new();
            tui_node_lines(&record.headings, "", &mut lines, &mut plain_lines);
            let title = file_name(record).to_uppercase();
            let content_width = plain_lines
                .iter()
                .map(|line| display_width(line))
                .chain([display_width(&title)])
                .max()
                .unwrap_or(0);
            let width = ((content_width + 4).max(28).min(64)) as u16;
            let height = (lines.len() + 2).min(u16::MAX as usize) as u16;
            TuiCard {
                title,
                width,
                height,
                lines,
            }
        })
        .collect()
}

fn tui_node_lines(
    nodes: &[Node],
    prefix: &str,
    lines: &mut Vec<Line<'static>>,
    plain_lines: &mut Vec<String>,
) {
    for (i, node) in nodes.iter().enumerate() {
        let is_last = i == nodes.len() - 1;
        let branch = if is_last { "└── " } else { "├── " };
        let child_prefix = format!("{}{}", prefix, if is_last { "    " } else { "│   " });
        let plain = format!("{prefix}{branch}{}", node.text);
        plain_lines.push(plain);
        lines.push(Line::from(vec![
            Span::styled(prefix.to_string(), Style::default().fg(cat_surface2())),
            Span::styled(branch, Style::default().fg(cat_surface2())),
            Span::styled(
                node.text.clone(),
                Style::default().fg(color_for_level(node.level)),
            ),
        ]));
        tui_node_lines(&node.children, &child_prefix, lines, plain_lines);
    }
}

fn layout_tui_cards(cards: &[TuiCard], area: Rect, scroll: u16) -> Vec<TuiCardPosition> {
    if cards.is_empty() || area.width == 0 || area.height == 0 {
        return Vec::new();
    }

    let gap = 1u16;
    let columns = tui_columns(cards, area.width as usize, gap as usize);
    let column_widths = columns
        .iter()
        .map(|column| {
            column
                .iter()
                .map(|&index| cards[index].width)
                .max()
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    let mut positions = Vec::new();
    let mut x = area.x;

    for (column_index, column) in columns.iter().enumerate() {
        let mut y = 0u16;
        let width =
            column_widths[column_index].min(area.width.saturating_sub(x.saturating_sub(area.x)));

        for &card_index in column {
            let card = &cards[card_index];
            let height = card.height;
            let card_bottom = y.saturating_add(height);
            let visible = card_bottom > scroll && y < scroll.saturating_add(area.height);
            let clipped_top = scroll.saturating_sub(y).min(height);
            let visible_y = y.saturating_sub(scroll);
            let clipped_height = height
                .saturating_sub(clipped_top)
                .min(area.height.saturating_sub(visible_y));

            positions.push(TuiCardPosition {
                index: card_index,
                rect: Rect::new(x, area.y + visible_y, width, clipped_height),
                full_y: y,
                clipped_top,
                visible,
            });

            y = y.saturating_add(height).saturating_add(1);
        }

        x = x.saturating_add(width).saturating_add(gap);
    }

    positions
}

fn tui_columns(cards: &[TuiCard], viewport_width: usize, gap: usize) -> Vec<Vec<usize>> {
    let max_columns = max_styled_columns(viewport_width).min(cards.len());

    for column_count in (1..=max_columns).rev() {
        let columns = distribute_tui_cards(cards, column_count);
        let width = columns
            .iter()
            .map(|column| {
                column
                    .iter()
                    .map(|&index| cards[index].width as usize)
                    .max()
                    .unwrap_or(0)
            })
            .sum::<usize>()
            + gap * columns.len().saturating_sub(1);
        if width <= viewport_width || column_count == 1 {
            return columns;
        }
    }

    distribute_tui_cards(cards, 1)
}

fn distribute_tui_cards(cards: &[TuiCard], column_count: usize) -> Vec<Vec<usize>> {
    let mut columns = vec![Vec::new(); column_count];
    let mut heights = vec![0u16; column_count];

    for (index, card) in cards.iter().enumerate() {
        let column_index = heights
            .iter()
            .enumerate()
            .min_by_key(|(_, height)| *height)
            .map(|(index, _)| index)
            .unwrap_or(0);
        if heights[column_index] > 0 {
            heights[column_index] = heights[column_index].saturating_add(1);
        }
        heights[column_index] = heights[column_index].saturating_add(card.height);
        columns[column_index].push(index);
    }

    columns
}

fn adjacent_card(
    selected: usize,
    positions: &[TuiCardPosition],
    direction: DirectionHint,
) -> Option<usize> {
    let current = positions
        .iter()
        .find(|position| position.index == selected)?;
    let current_center = current.rect.x.saturating_add(current.rect.width / 2);

    positions
        .iter()
        .filter(|position| match direction {
            DirectionHint::Down => position.full_y > current.full_y,
            DirectionHint::Up => position.full_y < current.full_y,
        })
        .min_by_key(|position| {
            let center = position.rect.x.saturating_add(position.rect.width / 2);
            let horizontal_distance = center.abs_diff(current_center);
            let vertical_distance = position.full_y.abs_diff(current.full_y);
            horizontal_distance as u32 * 4 + vertical_distance as u32
        })
        .map(|position| position.index)
}

fn search_matches(cards: &[TuiCard], query: &str) -> Vec<SearchMatch> {
    cards
        .iter()
        .enumerate()
        .flat_map(|(card_index, card)| card_search_matches(card_index, card, query))
        .collect()
}

fn card_search_matches(card_index: usize, card: &TuiCard, query: &str) -> Vec<SearchMatch> {
    let mut matches = Vec::new();
    matches.extend(
        std::iter::repeat(SearchMatch {
            card_index,
            line_y: 0,
        })
        .take(match_ranges(&card_title_label(&card.title), query).len()),
    );

    for (line_index, line) in card.lines.iter().enumerate() {
        let count = line_match_count(line, query);
        matches.extend(
            std::iter::repeat(SearchMatch {
                card_index,
                line_y: line_index.saturating_add(1).min(u16::MAX as usize) as u16,
            })
            .take(count),
        );
    }

    matches
}

fn first_match_at_or_after_card(selected: usize, cards: &[TuiCard], query: &str) -> Option<usize> {
    let matches = search_matches(cards, query);
    matches
        .iter()
        .position(|search_match| search_match.card_index >= selected)
        .or_else(|| (!matches.is_empty()).then_some(0))
}

fn count_matches_before(cards: &[TuiCard], card_index: usize, query: &str) -> usize {
    cards
        .iter()
        .take(card_index)
        .map(|card| card_match_count(card, query))
        .sum()
}

fn card_match_count(card: &TuiCard, query: &str) -> usize {
    match_ranges(&card_title_label(&card.title), query).len()
        + card
            .lines
            .iter()
            .map(|line| line_match_count(line, query))
            .sum::<usize>()
}

fn line_match_count(line: &Line<'static>, query: &str) -> usize {
    line.spans
        .iter()
        .map(|span| match_ranges(span.content.as_ref(), query).len())
        .sum()
}

fn card_title_label(title: &str) -> String {
    format!(" {title} ")
}

fn highlighted_title(
    title: &str,
    style: Style,
    query: Option<&str>,
    current_match: Option<usize>,
    match_index: &mut usize,
) -> Line<'static> {
    Line::from(highlight_text(
        &card_title_label(title),
        style,
        query,
        current_match,
        match_index,
    ))
}

fn highlighted_lines(
    lines: &[Line<'static>],
    query: Option<&str>,
    current_match: Option<usize>,
    match_index: &mut usize,
) -> Vec<Line<'static>> {
    lines
        .iter()
        .map(|line| {
            let spans = line
                .spans
                .iter()
                .flat_map(|span| {
                    highlight_text(
                        span.content.as_ref(),
                        span.style,
                        query,
                        current_match,
                        match_index,
                    )
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect()
}

fn highlight_text(
    text: &str,
    style: Style,
    query: Option<&str>,
    current_match: Option<usize>,
    match_index: &mut usize,
) -> Vec<Span<'static>> {
    let Some(query) = query.map(str::trim).filter(|query| !query.is_empty()) else {
        return vec![Span::styled(text.to_string(), style)];
    };
    let ranges = match_ranges(text, query);
    if ranges.is_empty() {
        return vec![Span::styled(text.to_string(), style)];
    }

    let mut spans = Vec::new();
    let mut cursor = 0usize;
    for (start, end) in ranges {
        if cursor < start {
            spans.push(Span::styled(text[cursor..start].to_string(), style));
        }
        let highlight_style = if current_match == Some(*match_index) {
            current_search_match_style()
        } else {
            search_match_style()
        };
        spans.push(Span::styled(text[start..end].to_string(), highlight_style));
        *match_index += 1;
        cursor = end;
    }
    if cursor < text.len() {
        spans.push(Span::styled(text[cursor..].to_string(), style));
    }
    spans
}

fn match_ranges(text: &str, query: &str) -> Vec<(usize, usize)> {
    let haystack = text.to_ascii_lowercase();
    let needle = query.to_ascii_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let mut offset = 0usize;
    while let Some(start) = haystack[offset..].find(&needle) {
        let start = offset + start;
        let end = start + needle.len();
        ranges.push((start, end));
        offset = end;
    }
    ranges
}

fn search_match_style() -> Style {
    Style::default().fg(Color::Black).bg(cat_light_yellow())
}

fn current_search_match_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(cat_dark_yellow())
        .add_modifier(Modifier::BOLD)
}

fn render_output(
    records: &[FileRecord],
    format: OutputFormat,
    use_style: bool,
) -> Result<String, String> {
    match format {
        OutputFormat::Json => serde_json::to_string_pretty(records)
            .map_err(|err| format!("Failed to render JSON: {err}")),
        OutputFormat::Yaml => render_yaml(records),
        OutputFormat::Tree => Ok(render_tree(records, use_style)),
        OutputFormat::Outline => Ok(render_outline(records)),
        OutputFormat::Tui => Err("TUI output cannot be rendered as text".to_string()),
    }
}

fn render_yaml(records: &[FileRecord]) -> Result<String, String> {
    let mut lines = Vec::new();

    for record in records {
        lines.push(format!("- path: {}", yaml_string(&record.path)?));
        lines.push("  children:".to_string());
        render_yaml_nodes(&record.headings, 4, &mut lines)?;
    }

    Ok(lines.join("\n"))
}

fn render_yaml_nodes(nodes: &[Node], indent: usize, lines: &mut Vec<String>) -> Result<(), String> {
    for node in nodes {
        lines.push(format!(
            "{}- header: {}",
            " ".repeat(indent),
            yaml_string(&node.text)?
        ));
        if !node.children.is_empty() {
            lines.push(format!("{}  children:", " ".repeat(indent)));
            render_yaml_nodes(&node.children, indent + 4, lines)?;
        }
    }

    Ok(())
}

fn yaml_string(text: &str) -> Result<String, String> {
    // JSON double-quoted strings are valid YAML scalars. This keeps headings with
    // YAML-significant characters (for example `:`, `#`, or `---`) unambiguous.
    serde_json::to_string(text).map_err(|err| format!("Failed to render YAML: {err}"))
}

fn render_outline(records: &[FileRecord]) -> String {
    let mut lines = Vec::new();
    let multi_file = records.len() > 1;

    for record in records {
        if multi_file {
            lines.push(record.path.clone());
        }
        render_outline_nodes(&record.headings, 0, &mut lines);
        if multi_file {
            lines.push(String::new());
        }
    }

    lines.join("\n").trim_end().to_string()
}

fn render_outline_nodes(nodes: &[Node], depth: usize, lines: &mut Vec<String>) {
    for node in nodes {
        lines.push(format!("{}{}", "\t".repeat(depth), node.text));
        render_outline_nodes(&node.children, depth + 1, lines);
    }
}

fn render_tree(records: &[FileRecord], use_style: bool) -> String {
    if use_style {
        return render_styled_tree(records);
    }

    let mut lines = Vec::new();

    for (file_i, record) in records.iter().enumerate() {
        if file_i > 0 {
            lines.push(String::new());
        }

        let name = file_name(record);
        lines.push(name);
        render_plain_tree_nodes(&record.headings, "", &mut lines);
    }

    lines.join("\n")
}

fn file_name(record: &FileRecord) -> String {
    Path::new(&record.path)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or(&record.path)
        .to_string()
}

fn render_plain_tree_nodes(nodes: &[Node], prefix: &str, lines: &mut Vec<String>) {
    for (i, node) in nodes.iter().enumerate() {
        let is_last = i == nodes.len() - 1;
        let branch = if is_last { "└── " } else { "├── " };
        let child_prefix = format!("{}{}", prefix, if is_last { "    " } else { "│   " });
        lines.push(format!("{prefix}{branch}{}", node.text));
        render_plain_tree_nodes(&node.children, &child_prefix, lines);
    }
}

fn render_styled_tree(records: &[FileRecord]) -> String {
    let viewport_width = terminal_width().unwrap_or(100).saturating_sub(2) as usize;
    let gap = 3;
    let blocks = records.iter().map(styled_block).collect::<Vec<_>>();

    if blocks.is_empty() {
        return String::new();
    }

    let columns = masonry_columns(&blocks, viewport_width, gap);
    let column_widths = columns
        .iter()
        .map(|column| column.iter().map(|block| block.width).max().unwrap_or(0))
        .collect::<Vec<_>>();
    let column_lines = columns
        .iter()
        .zip(column_widths.iter())
        .map(|(column, width)| render_masonry_column(column, *width))
        .collect::<Vec<_>>();
    let height = column_lines
        .iter()
        .map(|lines| lines.len())
        .max()
        .unwrap_or(0);
    let mut output = Vec::new();

    for line_index in 0..height {
        let mut line = String::new();
        for (column_index, lines) in column_lines.iter().enumerate() {
            if column_index > 0 {
                line.push_str(&" ".repeat(gap));
            }

            if let Some(column_line) = lines.get(line_index) {
                line.push_str(column_line);
            } else if later_columns_have_content(&column_lines, column_index, line_index) {
                line.push_str(&" ".repeat(column_widths[column_index]));
            }
        }
        output.push(line.trim_end().to_string());
    }

    output.join("\n")
}

fn masonry_columns<'a>(
    blocks: &'a [StyledBlock],
    viewport_width: usize,
    gap: usize,
) -> Vec<Vec<&'a StyledBlock>> {
    let max_columns = max_styled_columns(viewport_width).min(blocks.len());

    for column_count in (1..=max_columns).rev() {
        let columns = distribute_blocks(blocks, column_count);
        let total_width = masonry_width(&columns, gap);
        if total_width <= viewport_width || column_count == 1 {
            return columns;
        }
    }

    distribute_blocks(blocks, 1)
}

fn distribute_blocks<'a>(
    blocks: &'a [StyledBlock],
    column_count: usize,
) -> Vec<Vec<&'a StyledBlock>> {
    let mut columns = vec![Vec::new(); column_count];
    let mut heights = vec![0; column_count];

    for block in blocks {
        let column_index = heights
            .iter()
            .enumerate()
            .min_by_key(|(_, height)| *height)
            .map(|(index, _)| index)
            .unwrap_or(0);

        if heights[column_index] > 0 {
            heights[column_index] += 1;
        }
        heights[column_index] += block.lines.len();
        columns[column_index].push(block);
    }

    columns
}

fn masonry_width(columns: &[Vec<&StyledBlock>], gap: usize) -> usize {
    columns
        .iter()
        .map(|column| column.iter().map(|block| block.width).max().unwrap_or(0))
        .sum::<usize>()
        + gap * columns.len().saturating_sub(1)
}

fn render_masonry_column(blocks: &[&StyledBlock], width: usize) -> Vec<String> {
    let mut lines = Vec::new();

    for (block_index, block) in blocks.iter().enumerate() {
        if block_index > 0 {
            lines.push(" ".repeat(width));
        }

        for line in &block.lines {
            lines.push(format!(
                "{}{}",
                line,
                " ".repeat(width.saturating_sub(block.width))
            ));
        }
    }

    lines
}

fn later_columns_have_content(
    column_lines: &[Vec<String>],
    column_index: usize,
    line_index: usize,
) -> bool {
    column_lines
        .iter()
        .skip(column_index + 1)
        .any(|lines| line_index < lines.len())
}

fn max_styled_columns(viewport_width: usize) -> usize {
    match viewport_width {
        0..=89 => 1,
        90..=159 => 2,
        _ => 3,
    }
}

struct StyledBlock {
    width: usize,
    lines: Vec<String>,
}

fn styled_block(record: &FileRecord) -> StyledBlock {
    let name = file_name(record);
    let mut content = Vec::new();
    styled_tree_content(&record.headings, "  ", true, &mut content);

    let min_width = 28;
    let max_content_width = content
        .iter()
        .map(|(plain, _)| display_width(plain))
        .chain([display_width(&file_label_plain(&name))])
        .max()
        .unwrap_or(0);
    let width = (max_content_width + 3).max(min_width);
    let inner_width = width.saturating_sub(2);

    let mut lines = Vec::new();
    lines.push(styled_block_top(&name, inner_width));
    for (plain, styled) in content {
        lines.push(styled_block_line(&plain, &styled, inner_width));
    }
    lines.push(styled_block_bottom(inner_width));

    StyledBlock { width, lines }
}

fn styled_tree_content(
    nodes: &[Node],
    prefix: &str,
    root: bool,
    lines: &mut Vec<(String, String)>,
) {
    for (i, node) in nodes.iter().enumerate() {
        let is_last = i == nodes.len() - 1;
        let branch = if is_last { "└─" } else { "├─" };
        let child_prefix = format!("{}{}", prefix, if is_last { "   " } else { "│  " });
        let level_color = color_for_level(node.level);
        let plain = format!("{prefix}{branch} {}", node.text);
        let styled = format!(
            "{}{} {}",
            paint(prefix, cat_surface2()),
            paint(branch, cat_surface2()),
            paint(&node.text, level_color)
        );

        lines.push((plain, styled));
        styled_tree_content(&node.children, &child_prefix, false, lines);
    }

    if root && nodes.is_empty() {
        let plain = "  ∅ no headings".to_string();
        let styled = paint(&plain, cat_subtext0());
        lines.push((plain, styled));
    }
}

fn styled_block_top(name: &str, inner_width: usize) -> String {
    let label = file_label_plain(name);
    let filler = "─".repeat(inner_width.saturating_sub(display_width(&label)));
    format!(
        "{}{}{}{}",
        paint("╭", cat_surface2()),
        styled_file_label(name),
        paint(&filler, cat_surface2()),
        paint("╮", cat_surface2())
    )
}

fn file_label_plain(name: &str) -> String {
    format!(" {} ", name.to_uppercase())
}

fn styled_file_label(name: &str) -> String {
    format!(" {} ", bold(&paint(&name.to_uppercase(), cat_rosewater())))
}

fn styled_block_line(plain: &str, styled: &str, inner_width: usize) -> String {
    let padding = " ".repeat(inner_width.saturating_sub(display_width(plain)));
    format!(
        "{}{}{}{}",
        paint("│", cat_surface2()),
        styled,
        padding,
        paint("│", cat_surface2())
    )
}

fn styled_block_bottom(inner_width: usize) -> String {
    format!(
        "{}{}{}",
        paint("╰", cat_surface2()),
        paint(&"─".repeat(inner_width), cat_surface2()),
        paint("╯", cat_surface2())
    )
}

fn color_for_level(level: u8) -> Color {
    match level {
        1 => cat_mauve(),
        2 => cat_blue(),
        3 => cat_green(),
        4 => cat_peach(),
        5 => cat_yellow(),
        _ => cat_subtext0(),
    }
}

fn cat_rosewater() -> Color {
    Color::Rgb(245, 224, 220)
}

fn cat_mauve() -> Color {
    Color::Rgb(203, 166, 247)
}

fn cat_blue() -> Color {
    Color::Rgb(137, 180, 250)
}

fn cat_green() -> Color {
    Color::Rgb(166, 227, 161)
}

fn cat_peach() -> Color {
    Color::Rgb(250, 179, 135)
}

fn cat_yellow() -> Color {
    Color::Rgb(249, 226, 175)
}

fn cat_light_yellow() -> Color {
    Color::Rgb(255, 241, 199)
}

fn cat_dark_yellow() -> Color {
    Color::Rgb(203, 151, 75)
}

fn cat_subtext0() -> Color {
    Color::Rgb(166, 173, 200)
}

fn cat_surface2() -> Color {
    Color::Rgb(88, 91, 112)
}

fn terminal_width() -> Option<u16> {
    Command::new("sh")
        .args(["-c", "stty size < /dev/tty"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| {
            text.split_whitespace()
                .nth(1)
                .and_then(|width| width.parse::<u16>().ok())
        })
        .filter(|&width| width > 0)
        .or_else(|| {
            env::var("COLUMNS")
                .ok()
                .and_then(|width| width.parse::<u16>().ok())
                .filter(|&width| width > 0)
        })
}

fn display_width(text: &str) -> usize {
    // Terminals disagree on some emoji ZWJ / VS16 / keycap clusters. unicode-width
    // gives us the baseline, then we add a small correction for the keycap emoji
    // sequences that show up often in these notes, e.g. `1️⃣`, `2️⃣`.
    UnicodeWidthStr::width(text) + keycap_width_correction(text)
}

fn keycap_width_correction(text: &str) -> usize {
    let chars = text.chars().collect::<Vec<_>>();
    chars
        .windows(3)
        .filter(|window| {
            window[0].is_ascii_digit() && window[1] == '\u{fe0f}' && window[2] == '\u{20e3}'
        })
        .count()
}

fn should_style(style: StyleMode, writing_to_file: bool) -> bool {
    match style {
        StyleMode::Always => true,
        StyleMode::Never => false,
        StyleMode::Auto => !writing_to_file && io::stdout().is_terminal(),
    }
}

fn bold(text: &str) -> String {
    format!("\x1b[1m{text}\x1b[22m")
}

fn paint(text: &str, color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("\x1b[38;2;{r};{g};{b}m{text}\x1b[0m"),
        Color::Indexed(index) => format!("\x1b[38;5;{index}m{text}\x1b[0m"),
        Color::Reset => text.to_string(),
        _ => {
            let code = match color {
                Color::Black => 30,
                Color::Red => 31,
                Color::Green => 32,
                Color::Yellow => 33,
                Color::Blue => 34,
                Color::Magenta => 35,
                Color::Cyan => 36,
                Color::Gray | Color::White => 37,
                Color::DarkGray => 90,
                Color::LightRed => 91,
                Color::LightGreen => 92,
                Color::LightYellow => 93,
                Color::LightBlue => 94,
                Color::LightMagenta => 95,
                Color::LightCyan => 96,
                Color::Rgb(_, _, _) | Color::Indexed(_) | Color::Reset => unreachable!(),
            };
            format!("\x1b[{code}m{text}\x1b[0m")
        }
    }
}
