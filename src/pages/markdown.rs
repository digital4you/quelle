//! HTML -> LLM-ready Markdown converter, link extractor, and rich page metadata.
//!
//! Produces clean Markdown preserving document structure (headings, lists,
//! links, code blocks, blockquotes, real data tables, bold/italic) while:
//! - Preferring `<main>` / `<article>` / `[role="main"]` when `only_main_content` is true
//! - Stripping site chrome, dialogs/menus, and empty list items
//! - Keeping proper spacing between adjacent inline elements and links
//! - Flattening HTML layout tables (like Hacker News) while keeping real data tables
//! - Supporting CSS selector filtering (`include_tags` / `exclude_tags`)

use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
};

use ego_tree::{NodeId, iter::Edge};
use scraper::{ElementRef, Html, Selector};
use serde::Serialize;
use url::Url;

use super::{clean, evidence_url};

/// Rich metadata extracted from HTML `<meta>` / `<link>` tags and response info.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub og_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub og_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub og_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

/// Options controlling HTML -> Markdown / text extraction.
#[derive(Clone, Debug)]
pub struct ExtractOptions {
    pub only_main_content: bool,
    pub include_tags: Vec<String>,
    pub exclude_tags: Vec<String>,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            only_main_content: true,
            include_tags: Vec::new(),
            exclude_tags: Vec::new(),
        }
    }
}

pub fn extract_meta(document: &Html, base_url: &Url) -> PageMeta {
    static HTML_TAG: LazyLock<Selector> = LazyLock::new(|| Selector::parse("html").unwrap());
    static META: LazyLock<Selector> = LazyLock::new(|| Selector::parse("meta").unwrap());
    static CANONICAL: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse(r#"link[rel="canonical" i]"#).unwrap());

    let mut meta = PageMeta::default();
    if let Some(html) = document.select(&HTML_TAG).next()
        && let Some(lang) = html.attr("lang").map(str::trim).filter(|s| !s.is_empty())
    {
        meta.language = Some(lang.to_owned());
    }
    if let Some(link) = document.select(&CANONICAL).next()
        && let Some(href) = link.attr("href").map(str::trim).filter(|s| !s.is_empty())
        && let Ok(resolved) = base_url.join(href)
    {
        meta.canonical_url = Some(resolved.into());
    }
    for tag in document.select(&META) {
        let Some(content) = tag.attr("content").map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        if let Some(name) = tag.attr("name") {
            if name.eq_ignore_ascii_case("description") && meta.description.is_none() {
                meta.description = Some(content.to_owned());
            } else if name.eq_ignore_ascii_case("author") && meta.author.is_none() {
                meta.author = Some(content.to_owned());
            } else if (name.eq_ignore_ascii_case("pubdate")
                || name.eq_ignore_ascii_case("date")
                || name.eq_ignore_ascii_case("dc.date.issued"))
                && meta.published_time.is_none()
            {
                meta.published_time = Some(content.to_owned());
            }
        }
        if let Some(prop) = tag.attr("property") {
            if prop.eq_ignore_ascii_case("og:title") && meta.og_title.is_none() {
                meta.og_title = Some(content.to_owned());
            } else if prop.eq_ignore_ascii_case("og:description") && meta.og_description.is_none() {
                meta.og_description = Some(content.to_owned());
            } else if prop.eq_ignore_ascii_case("og:image") && meta.og_image.is_none() {
                if let Ok(resolved) = base_url.join(content) {
                    meta.og_image = Some(resolved.into());
                } else {
                    meta.og_image = Some(content.to_owned());
                }
            } else if prop.eq_ignore_ascii_case("article:published_time")
                && meta.published_time.is_none()
            {
                meta.published_time = Some(content.to_owned());
            } else if (prop.eq_ignore_ascii_case("article:modified_time")
                || prop.eq_ignore_ascii_case("og:updated_time"))
                && meta.modified_time.is_none()
            {
                meta.modified_time = Some(content.to_owned());
            } else if prop.eq_ignore_ascii_case("article:author") && meta.author.is_none() {
                meta.author = Some(content.to_owned());
            }
        }
    }
    extract_json_ld_meta(document, &mut meta);
    meta
}

fn extract_json_ld_meta(document: &Html, meta: &mut PageMeta) {
    static JSON_LD: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse(r#"script[type="application/ld+json" i]"#).unwrap());
    for script in document.select(&JSON_LD).take(8) {
        let raw: String = script.text().collect();
        let Ok(val) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
            continue;
        };
        visit_json_ld(&val, meta);
        if meta.author.is_some() && meta.published_time.is_some() && meta.modified_time.is_some() {
            break;
        }
    }
}

fn visit_json_ld(val: &serde_json::Value, meta: &mut PageMeta) {
    match val {
        serde_json::Value::Array(items) => {
            for item in items {
                visit_json_ld(item, meta);
            }
        }
        serde_json::Value::Object(map) => {
            if meta.published_time.is_none()
                && let Some(s) = map.get("datePublished").and_then(|v| v.as_str())
                && !s.trim().is_empty()
            {
                meta.published_time = Some(s.trim().to_owned());
            }
            if meta.modified_time.is_none()
                && let Some(s) = map.get("dateModified").and_then(|v| v.as_str())
                && !s.trim().is_empty()
            {
                meta.modified_time = Some(s.trim().to_owned());
            }
            if meta.author.is_none()
                && let Some(author) = map.get("author")
            {
                let name = match author {
                    serde_json::Value::String(s) => Some(s.as_str()),
                    serde_json::Value::Object(obj) => obj.get("name").and_then(|v| v.as_str()),
                    serde_json::Value::Array(arr) => arr.first().and_then(|first| match first {
                        serde_json::Value::String(s) => Some(s.as_str()),
                        serde_json::Value::Object(obj) => obj.get("name").and_then(|v| v.as_str()),
                        _ => None,
                    }),
                    _ => None,
                };
                if let Some(n) = name.map(str::trim).filter(|s| !s.is_empty()) {
                    meta.author = Some(n.to_owned());
                }
            }
            if let Some(graph) = map.get("@graph") {
                visit_json_ld(graph, meta);
            }
        }
        _ => {}
    }
}

/// Extracts deduplicated, resolved public HTTP(S) links from an HTML document.
pub fn extract_links(document: &Html, base_url: &Url, max_links: usize) -> Vec<Url> {
    static ANCHOR: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[href]").unwrap());
    let mut seen = HashSet::new();
    let mut links = Vec::new();
    for el in document.select(&ANCHOR) {
        let Some(raw) = el.attr("href").map(str::trim) else {
            continue;
        };
        if raw.is_empty()
            || raw.starts_with("javascript:")
            || raw.starts_with("mailto:")
            || raw.starts_with("tel:")
            || raw.starts_with("data:")
        {
            continue;
        }
        let Ok(joined) = base_url.join(raw) else {
            continue;
        };
        let Ok(clean_url) = evidence_url(joined) else {
            continue;
        };
        if seen.insert(clean_url.clone()) {
            links.push(clean_url);
            if links.len() >= max_links {
                break;
            }
        }
    }
    links
}

/// Converts an HTML document into clean, LLM-ready Markdown.
pub fn html_to_markdown(
    document: &Html,
    base_url: &Url,
    options: &ExtractOptions,
    max_bytes: usize,
) -> String {
    let excluded = collect_matching_ids(document, &options.exclude_tags);

    let root = document.root_element();
    let disclosures = clean::disclosure_ids(root);
    let helpers = clean::contact_helpers(root);

    // If `include_tags` is specified, render only matching subtrees in order.
    if !options.include_tags.is_empty() {
        let mut out = String::new();
        let mut rendered = HashSet::new();
        for sel_str in &options.include_tags {
            let Ok(selector) = Selector::parse(sel_str) else {
                continue;
            };
            for el in document.select(&selector) {
                if rendered.insert(el.id()) && !excluded.contains(&el.id()) {
                    let chunk = render_element(
                        el,
                        base_url,
                        false,
                        &excluded,
                        &disclosures,
                        &helpers,
                        max_bytes,
                    );
                    if !chunk.is_empty() {
                        if !out.is_empty() {
                            out.push_str("\n\n");
                        }
                        out.push_str(&chunk);
                    }
                    if out.len() >= max_bytes {
                        break;
                    }
                }
            }
            if out.len() >= max_bytes {
                break;
            }
        }
        return tidy_markdown(&out, max_bytes);
    }

    // When `only_main_content` is enabled, prefer a substantial `<main>` /
    // `<article>` / `[role="main"]` region so UI modals, theme pickers, and
    // sidebars outside `<main>` are dropped cleanly.
    if options.only_main_content {
        static MAIN_REGIONS: LazyLock<Selector> = LazyLock::new(|| {
            Selector::parse("main, [role=main], article").expect("main region selector")
        });
        let full = render_element(
            root,
            base_url,
            true,
            &excluded,
            &disclosures,
            &helpers,
            max_bytes,
        );
        let full_len = full.trim().len();
        let best_region = document
            .select(&MAIN_REGIONS)
            .filter(|el| {
                !excluded.contains(&el.id())
                    && !el.ancestors().filter_map(ElementRef::wrap).any(|p| {
                        matches!(p.value().name(), "noscript" | "template")
                            || clean::is_hidden_node(p, &disclosures)
                    })
            })
            .take(32)
            .map(|el| {
                render_element(
                    el,
                    base_url,
                    true,
                    &excluded,
                    &disclosures,
                    &helpers,
                    max_bytes,
                )
            })
            .max_by_key(|s| s.trim().len());
        if let Some(mut region) = best_region {
            let rlen = region.trim().len();
            if rlen >= 250 && rlen * 4 >= full_len {
                // Supplement public contact targets outside the main article
                // (e.g. press/footer contacts) without importing site chrome.
                static CONTACT: LazyLock<Selector> =
                    LazyLock::new(|| Selector::parse("a, button").unwrap());
                for el in root.select(&CONTACT) {
                    if std::iter::once(el)
                        .chain(el.ancestors().filter_map(ElementRef::wrap))
                        .any(|p| {
                            excluded.contains(&p.id())
                                || clean::is_hidden_node(p, &disclosures)
                                || matches!(p.value().name(), "noscript" | "template")
                                || is_markdown_chrome(p)
                        })
                    {
                        continue;
                    }
                    if let Some(target) = clean::contact(el, &helpers)
                        && !clean::same_contact(&region, &target)
                    {
                        let label = super::normalize(el.text(), 150);
                        let scheme = if target.contains('@') {
                            "mailto"
                        } else {
                            "tel"
                        };
                        ensure_blank_line(&mut region);
                        if label.is_empty() || clean::same_contact(&label, &target) {
                            region.push_str(&format!("[{target}]({scheme}:{target})\n"));
                        } else {
                            region.push_str(&format!("[{label}]({scheme}:{target})\n"));
                        }
                    }
                }
                return tidy_markdown(&region, max_bytes);
            }
        }
        let md = if full.trim().is_empty() {
            render_element(
                root,
                base_url,
                false,
                &excluded,
                &disclosures,
                &helpers,
                max_bytes,
            )
        } else {
            full
        };
        return tidy_markdown(&md, max_bytes);
    }

    let md = render_element(
        root,
        base_url,
        false,
        &excluded,
        &disclosures,
        &helpers,
        max_bytes,
    );
    tidy_markdown(&md, max_bytes)
}

fn collect_matching_ids(document: &Html, selectors: &[String]) -> HashSet<NodeId> {
    let mut ids = HashSet::new();
    for sel_str in selectors {
        if let Ok(selector) = Selector::parse(sel_str) {
            for el in document.select(&selector) {
                ids.insert(el.id());
            }
        }
    }
    ids
}

#[derive(Clone, Copy)]
enum ListKind {
    Unordered,
    Ordered(usize),
}

struct ListItemFrame {
    line_start: usize,
    content_start: usize,
    ordered: bool,
}

struct LinkFrame {
    href: String,
    start_len: usize,
    contact_target: Option<String>,
}

struct PreFrame {
    body_start: usize,
    lang: Option<String>,
}

struct TableFrame {
    start_len: usize,
    has_th: bool,
    has_nested_table: bool,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    in_row: bool,
    cell_start: Option<usize>,
    cell_span: (usize, usize),
    spans: Vec<Option<(usize, String)>>,
    caption: String,
    caption_start: Option<usize>,
}

impl TableFrame {
    fn new(start_len: usize) -> Self {
        Self {
            start_len,
            has_th: false,
            has_nested_table: false,
            rows: Vec::new(),
            current_row: Vec::new(),
            in_row: false,
            cell_start: None,
            cell_span: (1, 1),
            spans: Vec::new(),
            caption: String::new(),
            caption_start: None,
        }
    }

    fn carry_span(&mut self) -> bool {
        let col = self.current_row.len();
        if let Some(Some((remaining, value))) = self.spans.get_mut(col) {
            self.current_row.push(value.clone());
            *remaining -= 1;
            if *remaining == 0 {
                self.spans[col] = None;
            }
            true
        } else {
            false
        }
    }

    fn push_cell(&mut self, value: String) {
        while self.carry_span() {}
        let (cols, rows) = self.cell_span;
        for _ in 0..cols {
            let col = self.current_row.len();
            self.current_row.push(value.clone());
            if rows > 1 {
                self.spans.resize(self.spans.len().max(col + 1), None);
                self.spans[col] = Some((rows - 1, value.clone()));
            }
        }
    }

    /// Distinguishes real data tables from HTML layout tables (e.g. Hacker News).
    fn is_data_table(&self) -> bool {
        if self.has_nested_table || self.rows.is_empty() {
            return false;
        }
        let non_empty_rows: Vec<&Vec<String>> = self
            .rows
            .iter()
            .filter(|r| r.iter().any(|c| !c.trim().is_empty()))
            .collect();
        if non_empty_rows.is_empty() {
            return false;
        }
        let max_cols = non_empty_rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if !(2..=12).contains(&max_cols) {
            return false;
        }
        if self.has_th {
            return true;
        }
        // Without `<th>`, require >= 2 rows with mostly consistent column counts
        // and no mostly-empty spacer columns.
        if non_empty_rows.len() < 2 {
            return false;
        }
        let consistent = non_empty_rows
            .iter()
            .filter(|r| r.len() == max_cols)
            .count();
        let filled_cells: usize = non_empty_rows
            .iter()
            .flat_map(|r| r.iter())
            .filter(|c| !c.trim().is_empty())
            .count();
        let total_cells = non_empty_rows.len() * max_cols;
        consistent * 2 >= non_empty_rows.len() && filled_cells * 3 >= total_cells * 2
    }

    fn format_markdown(&self) -> String {
        let mut buf = String::new();
        if !self.caption.is_empty() {
            buf.push_str(&self.caption);
            buf.push_str("\n\n");
        }
        if self.is_data_table() {
            let cols = self.rows.iter().map(|r| r.len()).max().unwrap_or(0);
            let mut emitted_header = false;
            for row in &self.rows {
                if !row.iter().any(|c| !c.trim().is_empty()) {
                    continue;
                }
                buf.push('|');
                for idx in 0..cols {
                    let cell = row.get(idx).map(String::as_str).unwrap_or("");
                    buf.push(' ');
                    buf.push_str(&cell.replace('|', "\\|"));
                    buf.push_str(" |");
                }
                buf.push('\n');
                if !emitted_header {
                    buf.push('|');
                    for _ in 0..cols {
                        buf.push_str(" --- |");
                    }
                    buf.push('\n');
                    emitted_header = true;
                }
            }
        } else {
            // Layout table: flatten each non-empty row into a clean line.
            for row in &self.rows {
                let cells: Vec<&str> = row
                    .iter()
                    .map(|c| c.trim())
                    .filter(|c| !c.is_empty())
                    .collect();
                if !cells.is_empty() {
                    buf.push_str(&cells.join(" "));
                    buf.push('\n');
                }
            }
        }
        buf
    }
}

pub(super) fn table_text(root: ElementRef<'_>, base: &Url) -> String {
    let disclosures = clean::disclosure_ids(root);
    let helpers = clean::contact_helpers(root);
    render_element(
        root,
        base,
        true,
        &HashSet::new(),
        &disclosures,
        &helpers,
        super::FULL_TEXT_BYTES,
    )
}

fn render_element(
    root: ElementRef<'_>,
    base_url: &Url,
    strip_chrome: bool,
    excluded: &HashSet<NodeId>,
    disclosures: &HashSet<NodeId>,
    helpers: &HashMap<String, clean::Helper>,
    max_bytes: usize,
) -> String {
    let mut out = String::new();
    let mut skip: Option<NodeId> = None;
    let mut pre_depth: usize = 0;
    let mut pre_frames: Vec<PreFrame> = Vec::new();
    let mut code_depth: usize = 0;
    let mut blockquote_depth: usize = 0;
    let mut lists: Vec<ListKind> = Vec::new();
    let mut list_items: Vec<ListItemFrame> = Vec::new();
    let mut links: Vec<LinkFrame> = Vec::new();
    let mut tables: Vec<TableFrame> = Vec::new();
    let mut pending_space = false;

    let mut heading_open: Option<usize> = None;
    let mut bold_open: HashMap<NodeId, usize> = HashMap::new();
    let mut italic_open: HashMap<NodeId, usize> = HashMap::new();

    for edge in root.traverse() {
        let node = match edge {
            Edge::Open(n) | Edge::Close(n) => n,
        };
        if let Some(id) = skip {
            if id == node.id() && matches!(edge, Edge::Close(_)) {
                skip = None;
            }
            continue;
        }
        let in_table_row = tables.last().is_some_and(|t| t.in_row);
        if let Some(el) = ElementRef::wrap(node) {
            let name = el.value().name();
            if matches!(edge, Edge::Open(_)) {
                if excluded.contains(&node.id())
                    || matches!(
                        name,
                        "head"
                            | "script"
                            | "style"
                            | "noscript"
                            | "template"
                            | "svg"
                            | "iframe"
                            | "dialog"
                    )
                    || clean::is_hidden_node(el, disclosures)
                    || el.attr("aria-modal") == Some("true")
                    || (strip_chrome && is_markdown_chrome(el))
                {
                    skip = Some(node.id());
                    continue;
                }
                if pre_depth > 0 {
                    if name == "pre" {
                        pre_depth += 1;
                    } else if is_pre_ui_chrome(el) {
                        skip = Some(node.id());
                    } else if name == "br" {
                        out.push('\n');
                    }
                    continue;
                }
                match name {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        if links.is_empty() {
                            let level = (name.as_bytes()[1] - b'0') as usize;
                            ensure_blank_line(&mut out);
                            prefix_blockquote(&mut out, blockquote_depth);
                            for _ in 0..level {
                                out.push('#');
                            }
                            out.push(' ');
                            heading_open = Some(out.len());
                            pending_space = false;
                        } else {
                            pending_space = true;
                        }
                    }
                    "p" => {
                        if !in_table_row && links.is_empty() {
                            ensure_blank_line(&mut out);
                            prefix_blockquote(&mut out, blockquote_depth);
                            pending_space = false;
                        } else {
                            pending_space = true;
                        }
                    }
                    "br" => {
                        if in_table_row || !links.is_empty() {
                            pending_space = true;
                        } else {
                            out.push('\n');
                            prefix_blockquote(&mut out, blockquote_depth);
                            pending_space = false;
                        }
                    }
                    "hr" => {
                        ensure_blank_line(&mut out);
                        out.push_str("---\n\n");
                        pending_space = false;
                    }
                    "blockquote" => {
                        ensure_blank_line(&mut out);
                        blockquote_depth += 1;
                        pending_space = false;
                    }
                    "pre" => {
                        ensure_blank_line(&mut out);
                        let lang = code_language(el);
                        pre_frames.push(PreFrame {
                            body_start: out.len(),
                            lang,
                        });
                        pre_depth += 1;
                        pending_space = false;
                    }
                    "dt" => {
                        ensure_blank_line(&mut out);
                        out.push_str("**");
                        bold_open.insert(node.id(), out.len());
                        pending_space = false;
                    }
                    "dd" => {
                        ensure_newline(&mut out);
                        out.push_str(": ");
                        pending_space = false;
                    }
                    "code" => {
                        if code_depth == 0 {
                            flush_space(&mut out, &mut pending_space, '`');
                            out.push('`');
                        }
                        code_depth += 1;
                    }
                    "strong" | "b" => {
                        flush_space(&mut out, &mut pending_space, '*');
                        out.push_str("**");
                        bold_open.insert(node.id(), out.len());
                    }
                    "em" | "i" => {
                        flush_space(&mut out, &mut pending_space, '*');
                        out.push('*');
                        italic_open.insert(node.id(), out.len());
                    }
                    "ul" => {
                        if lists.is_empty() {
                            ensure_blank_line(&mut out);
                        } else {
                            ensure_newline(&mut out);
                        }
                        lists.push(ListKind::Unordered);
                        pending_space = false;
                    }
                    "ol" => {
                        if lists.is_empty() {
                            ensure_blank_line(&mut out);
                        } else {
                            ensure_newline(&mut out);
                        }
                        lists.push(ListKind::Ordered(1));
                        pending_space = false;
                    }
                    "li" => {
                        ensure_newline(&mut out);
                        let line_start = out.len();
                        prefix_blockquote(&mut out, blockquote_depth);
                        let indent = lists.len().saturating_sub(1);
                        for _ in 0..indent {
                            out.push_str("  ");
                        }
                        let ordered = match lists.last_mut() {
                            Some(ListKind::Ordered(n)) => {
                                out.push_str(&format!("{n}. "));
                                *n += 1;
                                true
                            }
                            _ => {
                                out.push_str("- ");
                                false
                            }
                        };
                        list_items.push(ListItemFrame {
                            line_start,
                            content_start: out.len(),
                            ordered,
                        });
                        pending_space = false;
                    }
                    "a" | "button" => {
                        if let Some(target) = clean::contact(el, helpers) {
                            let scheme = if target.contains('@') {
                                "mailto"
                            } else {
                                "tel"
                            };
                            if out.ends_with(')') || (out.ends_with("**") && bold_open.is_empty()) {
                                pending_space = true;
                            }
                            flush_space(&mut out, &mut pending_space, '[');
                            out.push('[');
                            links.push(LinkFrame {
                                href: format!("{scheme}:{target}"),
                                start_len: out.len(),
                                contact_target: Some(target),
                            });
                        } else if name == "a"
                            && let Some(href) = el.attr("href").map(str::trim)
                            && !href.is_empty()
                            && !href.starts_with("javascript:")
                            && let Some(url) = base_url
                                .join(href)
                                .ok()
                                .and_then(|u| evidence_url(u).ok())
                                .map(String::from)
                        {
                            // Separate adjacent links like `[A](u1)[B](u2)` -> `[A](u1) [B](u2)`
                            if out.ends_with(')') || (out.ends_with("**") && bold_open.is_empty()) {
                                pending_space = true;
                            }
                            flush_space(&mut out, &mut pending_space, '[');
                            out.push('[');
                            links.push(LinkFrame {
                                href: url,
                                start_len: out.len(),
                                contact_target: None,
                            });
                        }
                    }
                    "img" => {
                        let alt = el.attr("alt").map(str::trim).unwrap_or("");
                        if !alt.is_empty()
                            && let Some(src) = el.attr("src").map(str::trim)
                            && !src.starts_with("data:")
                            && let Ok(resolved) = base_url.join(src)
                        {
                            flush_space(&mut out, &mut pending_space, '!');
                            out.push_str(&format!("![{alt}]({resolved})"));
                            pending_space = true;
                        }
                    }
                    "table" => {
                        if let Some(parent) = tables.last_mut() {
                            parent.has_nested_table = true;
                        } else {
                            ensure_blank_line(&mut out);
                        }
                        tables.push(TableFrame::new(out.len()));
                        pending_space = false;
                    }
                    "tr" => {
                        if let Some(t) = tables.last_mut() {
                            t.in_row = true;
                            t.current_row.clear();
                            pending_space = false;
                        }
                    }
                    "caption" => {
                        if let Some(t) = tables.last_mut() {
                            t.caption_start = Some(out.len());
                        }
                    }
                    "td" | "th" => {
                        if let Some(t) = tables.last_mut()
                            && t.in_row
                        {
                            if name == "th" {
                                t.has_th = true;
                            }
                            t.cell_start = Some(out.len());
                            let span = |name| {
                                el.attr(name)
                                    .and_then(|s| s.parse::<usize>().ok())
                                    .unwrap_or(1)
                                    .clamp(1, 32)
                            };
                            t.cell_span = (span("colspan"), span("rowspan"));
                            pending_space = false;
                        }
                    }
                    _ => {
                        if !clean::inline(name) {
                            if in_table_row || !links.is_empty() {
                                pending_space = true;
                            } else {
                                ensure_newline(&mut out);
                                pending_space = false;
                            }
                        }
                    }
                }
            } else {
                // Close tag
                if pre_depth > 0 {
                    if name == "pre" {
                        pre_depth = pre_depth.saturating_sub(1);
                        if pre_depth == 0
                            && let Some(frame) = pre_frames.pop()
                        {
                            let body = out[frame.body_start..].to_owned();
                            out.truncate(frame.body_start);
                            let fence_len = max_backtick_run(&body).max(2) + 1;
                            for _ in 0..fence_len {
                                out.push('`');
                            }
                            if let Some(lang) = frame.lang {
                                out.push_str(&lang);
                            }
                            out.push('\n');
                            out.push_str(&body);
                            if !out.ends_with('\n') {
                                out.push('\n');
                            }
                            for _ in 0..fence_len {
                                out.push('`');
                            }
                            out.push_str("\n\n");
                            pending_space = false;
                        }
                    } else if is_pre_line_container(el)
                        && let Some(frame) = pre_frames.last()
                        && out.len() > frame.body_start
                        && !out.ends_with('\n')
                    {
                        out.push('\n');
                    }
                    continue;
                }
                match name {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        if links.is_empty() {
                            if let Some(start) = heading_open.take() {
                                if out[start..].trim().is_empty() {
                                    let line_start = out[..start].rfind('\n').map_or(0, |i| i + 1);
                                    out.truncate(line_start);
                                } else {
                                    out.push_str("\n\n");
                                }
                            }
                            pending_space = false;
                        } else {
                            pending_space = true;
                        }
                    }
                    "p" => {
                        if !in_table_row && links.is_empty() {
                            ensure_blank_line(&mut out);
                        }
                        pending_space = in_table_row || !links.is_empty();
                    }
                    "blockquote" => {
                        blockquote_depth = blockquote_depth.saturating_sub(1);
                        ensure_blank_line(&mut out);
                        pending_space = false;
                    }
                    "code" => {
                        code_depth = code_depth.saturating_sub(1);
                        if code_depth == 0 {
                            out.push('`');
                        }
                    }
                    "strong" | "b" | "dt" => {
                        if let Some(start) = bold_open.remove(&node.id()) {
                            let trailing =
                                pending_space || out[start..].ends_with(char::is_whitespace);
                            let inner = out[start..].trim().to_owned();
                            if inner.is_empty() {
                                out.truncate(start.saturating_sub(2));
                            } else {
                                out.truncate(start);
                                out.push_str(&inner);
                                out.push_str("**");
                                if trailing {
                                    pending_space = true;
                                }
                            }
                        }
                        if name == "dt" {
                            ensure_newline(&mut out);
                            pending_space = false;
                        }
                    }
                    "em" | "i" => {
                        if let Some(start) = italic_open.remove(&node.id()) {
                            let trailing =
                                pending_space || out[start..].ends_with(char::is_whitespace);
                            let inner = out[start..].trim().to_owned();
                            if inner.is_empty() {
                                out.truncate(start.saturating_sub(1));
                            } else {
                                out.truncate(start);
                                out.push_str(&inner);
                                out.push('*');
                                if trailing {
                                    pending_space = true;
                                }
                            }
                        }
                    }
                    "span" | "small" | "label" | "kbd" => {
                        // Separate adjacent inline containers like `<span>quote</span><span>by Author</span>`
                        if !out.is_empty() && !out.ends_with([' ', '\n', '[', '(', '*', '`']) {
                            pending_space = true;
                        }
                    }
                    "ul" | "ol" => {
                        lists.pop();
                        if lists.is_empty() {
                            ensure_blank_line(&mut out);
                        }
                        pending_space = false;
                    }
                    "li" => {
                        if let Some(frame) = list_items.pop() {
                            if out[frame.content_start..].trim().is_empty() {
                                out.truncate(frame.line_start);
                                if frame.ordered
                                    && let Some(ListKind::Ordered(n)) = lists.last_mut()
                                {
                                    *n = n.saturating_sub(1);
                                }
                            } else {
                                ensure_newline(&mut out);
                            }
                        } else {
                            ensure_newline(&mut out);
                        }
                        pending_space = false;
                    }
                    "a" | "button" => {
                        if let Some(frame) = links.last()
                            && (name == "a" || frame.contact_target.is_some())
                        {
                            let frame = links.pop().unwrap();
                            let label = out[frame.start_len..]
                                .split_whitespace()
                                .collect::<Vec<_>>()
                                .join(" ");
                            if label.is_empty() {
                                out.truncate(frame.start_len.saturating_sub(1));
                                if let Some(target) = frame.contact_target {
                                    out.push_str(&target);
                                }
                            } else {
                                out.truncate(frame.start_len);
                                out.push_str(&label);
                                out.push_str("](");
                                out.push_str(&frame.href);
                                out.push(')');
                            }
                        } else if name == "button"
                            && !out.is_empty()
                            && !out.ends_with([' ', '\n', '[', '(', '*', '`'])
                        {
                            pending_space = true;
                        }
                    }
                    "td" | "th" => {
                        if let Some(t) = tables.last_mut()
                            && t.in_row
                            && let Some(start) = t.cell_start.take()
                        {
                            let cell = out[start..]
                                .split_whitespace()
                                .collect::<Vec<_>>()
                                .join(" ");
                            out.truncate(start);
                            t.push_cell(cell);
                        }
                    }
                    "caption" => {
                        if let Some(t) = tables.last_mut()
                            && let Some(start) = t.caption_start.take()
                        {
                            t.caption = out[start..].trim().to_owned();
                            out.truncate(start);
                        }
                    }
                    "tr" => {
                        if let Some(t) = tables.last_mut() {
                            while t.current_row.len() < t.spans.len() {
                                if !t.carry_span() {
                                    t.current_row.push(String::new());
                                }
                            }
                            if t.in_row && !t.current_row.is_empty() {
                                let row = std::mem::take(&mut t.current_row);
                                t.rows.push(row);
                            }
                            t.in_row = false;
                            pending_space = false;
                        }
                    }
                    "table" => {
                        if let Some(t) = tables.pop() {
                            let formatted = t.format_markdown();
                            if tables.is_empty() {
                                out.truncate(t.start_len);
                                out.push_str(&formatted);
                                ensure_blank_line(&mut out);
                            } else {
                                // Nested table inside an outer layout table cell: append flattened text
                                out.truncate(t.start_len);
                                out.push_str(formatted.trim());
                            }
                        }
                        pending_space = false;
                    }
                    _ => {
                        if !clean::inline(name) {
                            if in_table_row || !links.is_empty() {
                                pending_space = true;
                            } else {
                                ensure_newline(&mut out);
                                pending_space = false;
                            }
                        }
                    }
                }
            }
        } else if let Edge::Open(_) = edge
            && let Some(text) = node.value().as_text()
        {
            if pre_depth > 0 {
                out.push_str(text);
            } else {
                for c in text.chars() {
                    if code_depth == 0 && clean::is_invisible_format_char(c) {
                        continue;
                    }
                    if c.is_whitespace() {
                        pending_space = true;
                    } else {
                        flush_space(&mut out, &mut pending_space, c);
                        out.push(c);
                    }
                }
            }
        }

        if out.len() > max_bytes + 4096 {
            break;
        }
    }

    out
}

fn max_backtick_run(text: &str) -> usize {
    let mut best = 0;
    let mut current = 0;
    for b in text.bytes() {
        if b == b'`' {
            current += 1;
            best = best.max(current);
        } else {
            current = 0;
        }
    }
    best
}

fn is_pre_ui_chrome(el: ElementRef<'_>) -> bool {
    let name = el.value().name();
    if name == "button" {
        return true;
    }
    if let Some(cls) = el.attr("class") {
        let lower = cls.to_ascii_lowercase();
        if lower.split_whitespace().any(|tok| {
            matches!(
                tok,
                "language-id"
                    | "playground-link"
                    | "copy-button"
                    | "code-copy"
                    | "twoslash-popup-info"
            )
        }) {
            return true;
        }
    }
    false
}

fn is_pre_line_container(el: ElementRef<'_>) -> bool {
    matches!(el.value().name(), "div" | "p" | "li")
        && el.attr("class").is_some_and(|cls| {
            cls.split_whitespace().any(|tok| {
                tok.eq_ignore_ascii_case("line") || tok.eq_ignore_ascii_case("code-line")
            })
        })
}

fn clean_lang_token(raw: &str) -> Option<String> {
    let clean: String = raw
        .trim()
        .chars()
        .take(24)
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '#'))
        .collect();
    (!clean.is_empty()).then_some(clean)
}

fn code_language(pre: ElementRef<'_>) -> Option<String> {
    let extract = |classes: &str| -> Option<String> {
        classes.split_whitespace().find_map(|cls| {
            let lang = cls
                .strip_prefix("language-")
                .or_else(|| cls.strip_prefix("lang-"))?;
            clean_lang_token(lang)
        })
    };
    for attr in ["data-language", "data-lang"] {
        if let Some(val) = pre.attr(attr)
            && let Some(lang) = clean_lang_token(val)
        {
            return Some(lang);
        }
    }
    if let Some(cls) = pre.attr("class")
        && let Some(lang) = extract(cls)
    {
        return Some(lang);
    }
    for child in pre.children().filter_map(ElementRef::wrap) {
        if child.value().name() == "code"
            && let Some(cls) = child.attr("class")
            && let Some(lang) = extract(cls)
        {
            return Some(lang);
        }
        if child.attr("class").is_some_and(|c| {
            c.split_whitespace()
                .any(|t| t.eq_ignore_ascii_case("language-id"))
        }) {
            let label: String = child.text().collect();
            if let Some(lang) = clean_lang_token(&label) {
                return Some(lang);
            }
        }
    }
    None
}

/// Cleans server-negotiated Markdown/MDX documents (e.g. `react.dev`, Nuxt)
/// outside fenced code blocks and inline code spans: strips standalone MDX
/// layout wrapper tags and `{/* ... */}` comments, and resolves root-relative
/// Markdown links against `base_url`.
pub(super) fn clean_negotiated_markdown(raw: &str, base_url: &Url) -> String {
    static STANDALONE_MDX: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r#"^</?(?:Intro|InlineToc|Note|Pitfall|DeepDive|Sandpack|Recipes|CodeStep|Recap|Challenges|Solution|YouWillLearn|Wip|Canary|Deprecated|Illustration|Diagram|TerminalBlock)(?:\s[^>]*)?/?>$"#,
        )
        .expect("standalone mdx tag pattern")
    });
    static MDX_COMMENT: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\{/\*.*?\*/\}").expect("mdx comment pattern"));
    static REL_LINK: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\]\((/[^)\s]+)\)").expect("relative markdown link"));

    let mut out = String::with_capacity(raw.len());
    let mut fence_len = 0usize;
    let mut blank_run = 0usize;

    for line in raw.lines() {
        let trimmed = line.trim_start();
        let leading_ticks = trimmed.bytes().take_while(|&b| b == b'`').count();
        let was_in_code = fence_len > 0;
        if fence_len == 0 {
            if leading_ticks >= 3 {
                fence_len = leading_ticks;
            }
        } else if leading_ticks >= fence_len && trimmed[leading_ticks..].trim().is_empty() {
            fence_len = 0;
        }
        if was_in_code || fence_len > 0 || leading_ticks >= 3 {
            blank_run = 0;
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(line);
            continue;
        }
        let line_trim = line.trim();
        if STANDALONE_MDX.is_match(line_trim) {
            continue;
        }
        let processed = transform_outside_inline_code(line, |seg| {
            let no_comments = MDX_COMMENT.replace_all(seg, "");
            REL_LINK
                .replace_all(&no_comments, |caps: &regex::Captures<'_>| {
                    let rel = &caps[1];
                    if rel.starts_with("//") {
                        return caps[0].to_owned();
                    }
                    match base_url.join(rel).ok().and_then(|u| evidence_url(u).ok()) {
                        Some(abs) => format!("]({abs})"),
                        None => caps[0].to_owned(),
                    }
                })
                .into_owned()
        });
        let cleaned_line = processed.trim_end();
        if cleaned_line.is_empty() {
            blank_run += 1;
            if blank_run > 1 || out.is_empty() {
                continue;
            }
        } else {
            blank_run = 0;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(cleaned_line);
    }
    out.trim().to_owned()
}

fn transform_outside_inline_code(line: &str, f: impl Fn(&str) -> String) -> String {
    if !line.contains('`') {
        return f(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    let mut in_code = false;
    while let Some(pos) = rest.find('`') {
        let (before, after) = rest.split_at(pos);
        if in_code {
            out.push_str(before);
        } else {
            out.push_str(&f(before));
        }
        out.push('`');
        in_code = !in_code;
        rest = &after[1..];
    }
    if in_code {
        out.push_str(rest);
    } else {
        out.push_str(&f(rest));
    }
    out
}

pub(super) fn is_markdown_chrome(el: ElementRef<'_>) -> bool {
    if clean::is_chrome(el) {
        return true;
    }
    if matches!(
        el.attr("role"),
        Some("dialog" | "alertdialog" | "menu" | "menubar" | "toolbar")
    ) {
        return true;
    }
    if matches!(el.value().name(), "aside") {
        return true;
    }
    let protected = || {
        static PROTECTED: LazyLock<Selector> =
            LazyLock::new(|| Selector::parse("h1, main, article, [role=main]").unwrap());
        matches!(el.value().name(), "html" | "body" | "main" | "article")
            || el.attr("role") == Some("main")
            || el.select(&PROTECTED).next().is_some()
    };
    // Conservative EU/CH cookie-consent banner & CMP overlay stripping
    if let Some(id) = el.attr("id") {
        let lower = id.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "onetrust-consent-sdk"
                | "onetrust-banner-sdk"
                | "cybotcookiebotdialog"
                | "didomi-host"
                | "usercentrics-root"
                | "tarteaucitronroot"
                | "borlabscookiebox"
                | "cookie-notice"
                | "cookie-banner"
                | "cookie-consent"
                | "cc-main"
        ) || lower.contains("cookie-banner")
            || lower.contains("cookie-consent")
            || lower.contains("consent-banner")
        {
            return true;
        }
        if matches!(
            lower.as_str(),
            "sidebar"
                | "pgsidenav"
                | "pgnavbar"
                | "submenu"
                | "searchmenu"
                | "site-navigation"
                | "main-navigation"
                | "skip-links"
        ) && !protected()
        {
            return true;
        }
    }
    if let Some(cls) = el.attr("class") {
        let lower = cls.to_ascii_lowercase();
        if lower.split_whitespace().any(|token| {
            matches!(
                token,
                "cookie-banner"
                    | "cookie-consent"
                    | "cookie-notice"
                    | "consent-banner"
                    | "cc-window"
                    | "didomi-popup-container"
                    | "onetrust-pc-dark-filter"
            )
        }) {
            return true;
        }
        if lower.split_whitespace().any(|token| {
            matches!(
                token,
                "sidebar"
                    | "mainmenu"
                    | "submenu"
                    | "searchmenu"
                    | "navbar-collapse"
                    | "navheader"
                    | "navfooter"
                    | "menu-dropdowns"
                    | "headernavigationandutilitycontainer"
                    | "skip-link"
                    | "skip-links"
            )
        }) && !protected()
        {
            return true;
        }
    }
    false
}

fn flush_space(out: &mut String, pending: &mut bool, next_char: char) {
    if *pending {
        let suppress = matches!(
            next_char,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '’' | '\''
        );
        if !suppress
            && !out.is_empty()
            && !out.ends_with('\n')
            && !out.ends_with(' ')
            && !out.ends_with('[')
            && !out.ends_with('(')
        {
            out.push(' ');
        }
        *pending = false;
    }
}

fn ensure_newline(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn ensure_blank_line(out: &mut String) {
    if out.is_empty() {
        return;
    }
    if out.ends_with("\n\n") {
        return;
    }
    if out.ends_with('\n') {
        out.push('\n');
    } else {
        out.push_str("\n\n");
    }
}

fn prefix_blockquote(out: &mut String, depth: usize) {
    if depth > 0 && (out.is_empty() || out.ends_with('\n')) {
        for _ in 0..depth {
            out.push_str("> ");
        }
    }
}

fn tidy_markdown(raw: &str, max_bytes: usize) -> String {
    let mut cleaned = String::with_capacity(raw.len().min(max_bytes));
    let mut blank_run = 0usize;
    let mut fence_len = 0usize;
    for line in raw.lines() {
        let trimmed = line.trim_start();
        let leading_ticks = trimmed.bytes().take_while(|&b| b == b'`').count();
        if fence_len == 0 {
            if leading_ticks >= 3 {
                fence_len = leading_ticks;
            }
        } else if leading_ticks >= fence_len && trimmed[leading_ticks..].trim().is_empty() {
            fence_len = 0;
        }
        let in_code = fence_len > 0 || leading_ticks >= 3;
        let line = if in_code { line } else { line.trim_end() };
        if !in_code && line.is_empty() {
            blank_run += 1;
            if blank_run > 1 || cleaned.is_empty() {
                continue;
            }
        } else {
            blank_run = 0;
        }
        if !cleaned.is_empty() {
            cleaned.push('\n');
        }
        cleaned.push_str(line);
        if cleaned.len() >= max_bytes {
            let end = cleaned.floor_char_boundary(max_bytes);
            cleaned.truncate(end);
            break;
        }
    }
    cleaned.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_markdown_preserves_structure_and_strips_chrome() {
        let html = r#"
            <html lang="en">
            <head>
                <title>Test Page</title>
                <meta name="description" content="A concise summary">
                <meta property="og:title" content="OG Test">
                <link rel="canonical" href="/canonical">
            </head>
            <body>
                <nav><a href="/home">Home</a></nav>
                <main>
                    <h1>Main Heading</h1>
                    <p>Hello <strong>world</strong> with a <a href="/docs">link</a>.</p>
                    <ul><li>First</li><li>Second</li></ul>
                    <pre><code>let x = 1;</code></pre>
                    <table>
                        <tr><th>Name</th><th>Value</th></tr>
                        <tr><td>Alpha</td><td>42</td></tr>
                    </table>
                </main>
            </body>
            </html>
        "#;
        let base = Url::parse("https://example.org/page").unwrap();
        let doc = Html::parse_document(html);
        let md = html_to_markdown(&doc, &base, &ExtractOptions::default(), 64 * 1024);

        assert!(!md.contains("Home"), "nav chrome should be stripped: {md}");
        assert!(md.contains("# Main Heading"), "missing heading: {md}");
        assert!(
            md.contains("Hello **world** with a [link](https://example.org/docs)."),
            "missing formatted paragraph: {md}"
        );
        assert!(md.contains("- First\n- Second"), "missing list: {md}");
        assert!(
            md.contains("```\nlet x = 1;\n```"),
            "missing code block: {md}"
        );
        assert!(
            md.contains("| Name | Value |\n| --- | --- |\n| Alpha | 42 |"),
            "missing table: {md}"
        );

        let meta = extract_meta(&doc, &base);
        assert_eq!(meta.language.as_deref(), Some("en"));
        assert_eq!(meta.description.as_deref(), Some("A concise summary"));
        assert_eq!(
            meta.canonical_url.as_deref(),
            Some("https://example.org/canonical")
        );

        let links = extract_links(&doc, &base, 20);
        assert_eq!(links.len(), 2);
    }

    #[test]
    fn include_and_exclude_tags_filter_markdown() {
        let html = r#"
            <body>
                <article>
                    <h2>Keep Me</h2>
                    <div class="ad">Buy Now</div>
                    <p>Body text</p>
                </article>
                <section><h2>Other</h2></section>
            </body>
        "#;
        let base = Url::parse("https://example.org/").unwrap();
        let doc = Html::parse_document(html);
        let opts = ExtractOptions {
            only_main_content: true,
            include_tags: vec!["article".to_owned()],
            exclude_tags: vec![".ad".to_owned()],
        };
        let md = html_to_markdown(&doc, &base, &opts, 64 * 1024);
        assert!(md.contains("Keep Me"));
        assert!(md.contains("Body text"));
        assert!(!md.contains("Buy Now"));
        assert!(!md.contains("Other"));
    }

    #[test]
    fn strips_empty_bullets_separates_inline_and_flattens_layout_tables() {
        let html = r#"
            <body>
                <div role="dialog"><h2>Keyboard shortcuts</h2><p>Press ? for help</p></div>
                <main>
                    <h1>Article Title</h1>
                    <p>This is a substantial main article paragraph with plenty of real content so that the main region heuristic selects it over outer UI overlays and keyboard help dialogs without any problem whatsoever. Adding another sentence ensures we cross the character threshold cleanly.</p>
                    <ul>
                        <li><a hreflang="fr" href="/fr">Français</a></li>
                        <li>Real item</li>
                    </ul>
                    <p><span>“Quote text.”</span><span>by Author</span></p>
                    <p><a href="/a">Link A</a><a href="/b">Link B</a></p>
                    <table>
                        <tr><td><table><tr><td>1.</td><td><a href="/story">Story Title</a></td></tr></table></td></tr>
                    </table>
                </main>
            </body>
        "#;
        let base = Url::parse("https://example.org/").unwrap();
        let doc = Html::parse_document(html);
        let md = html_to_markdown(&doc, &base, &ExtractOptions::default(), 64 * 1024);

        assert!(!md.contains("Keyboard shortcuts"), "dialog leaked: {md}");
        assert!(!md.contains("\n-\n"), "empty bullet remained: {md}");
        assert!(md.contains("- Real item"), "real item missing: {md}");
        assert!(
            md.contains("“Quote text.” by Author"),
            "missing span space: {md}"
        );
        assert!(
            md.contains("[Link A](https://example.org/a) [Link B](https://example.org/b)"),
            "adjacent links glued: {md}"
        );
        assert!(
            !md.contains("| --- |"),
            "layout table should be flattened: {md}"
        );
        assert!(
            md.contains("1. [Story Title](https://example.org/story)"),
            "flattened row missing: {md}"
        );
    }

    #[test]
    fn extracts_json_ld_meta_code_langs_definitions_and_strips_cookie_banners() {
        let html = r#"
            <html>
            <head>
                <script type="application/ld+json">
                {
                    "@context": "https://schema.org",
                    "@type": "Article",
                    "author": {"@type": "Person", "name": "Ada Lovelace"},
                    "datePublished": "2026-11-01T10:00:00Z",
                    "dateModified": "2026-11-15T12:30:00Z"
                }
                </script>
            </head>
            <body>
                <div id="onetrust-consent-sdk"><p>We use cookies to personalize ads.</p></div>
                <main>
                    <h1>Rust Guide</h1>
                    <p>This is a detailed guide with plenty of prose so the main region heuristic selects it over outer overlays and cookie banners without hesitation. Adding another clear sentence guarantees we exceed the minimum character threshold cleanly.</p>
                    <pre><code class="language-rust">fn main() {}</code></pre>
                    <dl>
                        <dt>Ownership</dt>
                        <dd>Each value has a single owner.</dd>
                    </dl>
                </main>
            </body>
            </html>
        "#;
        let base = Url::parse("https://example.org/guide").unwrap();
        let doc = Html::parse_document(html);
        let md = html_to_markdown(&doc, &base, &ExtractOptions::default(), 64 * 1024);

        assert!(
            !md.contains("personalize ads"),
            "cookie banner leaked: {md}"
        );
        assert!(
            md.contains("```rust\nfn main() {}\n```"),
            "missing code lang: {md}"
        );
        assert!(
            md.contains("**Ownership**\n: Each value has a single owner."),
            "missing dl/dt/dd: {md}"
        );

        let meta = extract_meta(&doc, &base);
        assert_eq!(meta.author.as_deref(), Some("Ada Lovelace"));
        assert_eq!(meta.published_time.as_deref(), Some("2026-11-01T10:00:00Z"));
        assert_eq!(meta.modified_time.as_deref(), Some("2026-11-15T12:30:00Z"));
    }
}
