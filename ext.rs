use std::{
    collections::HashSet,
    env,
    ffi::OsStr,
    fs,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process,
    process::Command,
};

use clap::{Parser, ValueEnum};
use pulldown_cmark::{Event, HeadingLevel, Parser as MdParser, Tag, TagEnd};
use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Parser)]
#[command(about = "Extract heading-based outline trees from Markdown files.")]
struct Args {
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    #[arg(long, value_enum, default_value_t = OutputFormat::Tree)]
    format: OutputFormat,

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
    let use_style = should_style(args.style, args.output.is_some());
    let output = render_output(&records, args.format, use_style)?;

    if let Some(output_path) = args.output {
        fs::write(output_path, output).map_err(|err| format!("Failed to write output: {err}"))?;
    } else {
        println!("{output}");
    }

    Ok(())
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

fn render_output(
    records: &[FileRecord],
    format: OutputFormat,
    use_style: bool,
) -> Result<String, String> {
    match format {
        OutputFormat::Json => serde_json::to_string_pretty(records)
            .map_err(|err| format!("Failed to render JSON: {err}")),
        OutputFormat::Tree => Ok(render_tree(records, use_style)),
        OutputFormat::Outline => Ok(render_outline(records)),
    }
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
    let viewport_width = terminal_width().unwrap_or(100) as usize;
    let gap = 2;
    let blocks = records.iter().map(styled_block).collect::<Vec<_>>();
    let mut output = Vec::new();
    let mut index = 0;

    while index < blocks.len() {
        let mut row_width = 0;
        let mut row = Vec::new();

        while index < blocks.len() {
            let block = &blocks[index];
            let next_width = if row.is_empty() {
                block.width
            } else {
                row_width + gap + block.width
            };

            if !row.is_empty() && next_width > viewport_width {
                break;
            }

            row_width = next_width;
            row.push(block);
            index += 1;
        }

        if !output.is_empty() {
            output.push(String::new());
        }

        let row_height = row.iter().map(|block| block.lines.len()).max().unwrap_or(0);
        for line_index in 0..row_height {
            let mut line = String::new();
            for (block_index, block) in row.iter().enumerate() {
                if block_index > 0 {
                    line.push_str(&" ".repeat(gap));
                }
                if let Some(block_line) = block.lines.get(line_index) {
                    line.push_str(block_line);
                } else {
                    line.push_str(&" ".repeat(block.width));
                }
            }
            output.push(line);
        }
    }

    output.join("\n")
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
        .chain([display_width(&format!(" {name} "))])
        .max()
        .unwrap_or(0);
    let width = (max_content_width + 2).max(min_width);
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
            paint(prefix, Color::DarkGray),
            paint(branch, Color::DarkGray),
            paint(&node.text, level_color)
        );

        lines.push((plain, styled));
        styled_tree_content(&node.children, &child_prefix, false, lines);
    }

    if root && nodes.is_empty() {
        let plain = "  ∅ no headings".to_string();
        let styled = paint(&plain, Color::DarkGray);
        lines.push((plain, styled));
    }
}

fn styled_block_top(name: &str, inner_width: usize) -> String {
    let label = format!(" {name} ");
    let filler = "─".repeat(inner_width.saturating_sub(display_width(&label)));
    format!(
        "{}{}{}{}",
        paint("╭", Color::DarkGray),
        paint(&label, Color::Yellow),
        paint(&filler, Color::DarkGray),
        paint("╮", Color::DarkGray)
    )
}

fn styled_block_line(plain: &str, styled: &str, inner_width: usize) -> String {
    let padding = " ".repeat(inner_width.saturating_sub(display_width(plain)));
    format!(
        "{}{}{}{}",
        paint("│", Color::DarkGray),
        styled,
        padding,
        paint("│", Color::DarkGray)
    )
}

fn styled_block_bottom(inner_width: usize) -> String {
    format!(
        "{}{}{}",
        paint("╰", Color::DarkGray),
        paint(&"─".repeat(inner_width), Color::DarkGray),
        paint("╯", Color::DarkGray)
    )
}

fn color_for_level(level: u8) -> Color {
    match level {
        1 => Color::Magenta,
        2 => Color::Cyan,
        3 => Color::Green,
        4 => Color::Yellow,
        5 => Color::LightBlue,
        _ => Color::Gray,
    }
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
    UnicodeWidthStr::width(text)
}

fn should_style(style: StyleMode, writing_to_file: bool) -> bool {
    match style {
        StyleMode::Always => true,
        StyleMode::Never => false,
        StyleMode::Auto => !writing_to_file && io::stdout().is_terminal(),
    }
}

fn paint(text: &str, color: Color) -> String {
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
        Color::Rgb(_, _, _) | Color::Indexed(_) | Color::Reset => 39,
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}
