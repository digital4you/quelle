//! Zero-LLM deterministic structured extraction from HTML:
//! heading outline with exact `#id` anchors, real data tables, and JSON-LD blocks.

use std::{collections::HashSet, sync::LazyLock};

use ego_tree::{NodeId, iter::Edge};
use scraper::{ElementRef, Html, Selector};
use serde::Serialize;
use url::Url;

use super::{clean, markdown::ExtractOptions};

const MAX_OUTLINE_ENTRIES: usize = 300;
const MAX_TABLES: usize = 64;
const MAX_TABLE_ROWS: usize = 500;
const MAX_JSON_LD_BLOCKS: usize = 16;
const MAX_JSON_LD_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StructuredData {
    pub outline: Vec<HeadingEntry>,
    pub tables: Vec<StructuredTable>,
    pub json_ld: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HeadingEntry {
    pub level: u8,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub url: Url,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StructuredTable {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Extracts heading `(level, normalized_text, optional_id)` tuples for section-anchored
/// Markdown chunking without building full tables or JSON-LD payloads.
pub fn extract_heading_anchors(
    document: &Html,
    options: &ExtractOptions,
) -> Vec<(u8, String, Option<String>)> {
    let root = select_content_root(document, options);
    let excluded = collect_ids(document, &options.exclude_tags);
    let disclosures = clean::disclosure_ids(document.root_element());
    extract_outline_from(
        root,
        &Url::parse("https://placeholder.invalid").unwrap(),
        options.only_main_content,
        &excluded,
        &disclosures,
    )
    .into_iter()
    .map(|h| (h.level, h.text, h.id))
    .collect()
}

/// Extracts document outline, real data tables (excluding layout tables), and
/// JSON-LD payloads in a single deterministic pass over the parsed HTML DOM.
pub fn extract_structured(
    document: &Html,
    base_url: &Url,
    options: &ExtractOptions,
) -> StructuredData {
    let root = select_content_root(document, options);
    let excluded = collect_ids(document, &options.exclude_tags);
    let disclosures = clean::disclosure_ids(document.root_element());
    let strip_chrome = options.only_main_content;

    let outline = extract_outline_from(root, base_url, strip_chrome, &excluded, &disclosures);
    let tables = extract_tables_from(root, strip_chrome, &excluded, &disclosures);
    let json_ld = extract_json_ld(document);

    StructuredData {
        outline,
        tables,
        json_ld,
    }
}

fn select_content_root<'a>(document: &'a Html, options: &ExtractOptions) -> ElementRef<'a> {
    let root = document.root_element();
    if !options.only_main_content {
        return root;
    }
    static MAIN_REGIONS: LazyLock<Selector> = LazyLock::new(|| {
        Selector::parse("main, [role=main], article").expect("main region selector")
    });
    let disclosures = clean::disclosure_ids(root);
    let excluded = collect_ids(document, &options.exclude_tags);
    let full_len = visible_text_len(root);
    document
        .select(&MAIN_REGIONS)
        .filter(|el| {
            !excluded.contains(&el.id())
                && !el.ancestors().filter_map(ElementRef::wrap).any(|p| {
                    matches!(p.value().name(), "noscript" | "template")
                        || clean::is_hidden_node(p, &disclosures)
                })
        })
        .take(32)
        .max_by_key(|el| visible_text_len(*el))
        .filter(|el| {
            let rlen = visible_text_len(*el);
            rlen >= 120 && rlen * 4 >= full_len
        })
        .unwrap_or(root)
}

fn visible_text_len(el: ElementRef<'_>) -> usize {
    el.descendants()
        .filter_map(|n| {
            let text = n.value().as_text()?;
            let hidden = n
                .parent()
                .and_then(|p| p.value().as_element())
                .is_some_and(|e| matches!(e.name(), "script" | "style" | "noscript" | "template"));
            (!hidden).then_some(text.trim().len())
        })
        .sum()
}

fn collect_ids(document: &Html, selectors: &[String]) -> HashSet<NodeId> {
    let mut ids = HashSet::new();
    for sel_str in selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            for el in document.select(&sel) {
                ids.insert(el.id());
            }
        }
    }
    ids
}

fn should_skip(
    el: ElementRef<'_>,
    strip_chrome: bool,
    excluded: &HashSet<NodeId>,
    disclosures: &HashSet<NodeId>,
) -> bool {
    let name = el.value().name();
    excluded.contains(&el.id())
        || matches!(
            name,
            "head" | "script" | "style" | "noscript" | "template" | "svg" | "iframe" | "dialog"
        )
        || clean::is_hidden_node(el, disclosures)
        || el.attr("aria-modal") == Some("true")
        || (strip_chrome && super::markdown::is_markdown_chrome(el))
}

fn resolve_heading_id(el: ElementRef<'_>) -> Option<String> {
    // 1. Heading's own `id`
    if let Some(id) = el.attr("id").map(str::trim).filter(|s| !s.is_empty()) {
        return Some(id.to_owned());
    }
    // 2. Child anchor or span `<a id="...">` / `<a name="...">` / `<a href="#...">`
    for desc in el.descendants().filter_map(ElementRef::wrap) {
        if let Some(id) = desc
            .attr("id")
            .or_else(|| desc.attr("name"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(id.to_owned());
        }
        if desc.value().name() == "a"
            && let Some(href) = desc.attr("href").map(str::trim)
            && let Some(frag) = href.strip_prefix('#').filter(|s| !s.is_empty())
        {
            return Some(frag.to_owned());
        }
    }
    // 3. Immediate enclosing `<section id="...">`, `<article id="...">`, or `<div id="...">`
    //    when this heading is the opening heading of that container.
    if let Some(parent) = el.parent().and_then(ElementRef::wrap)
        && matches!(
            parent.value().name(),
            "section" | "article" | "div" | "header"
        )
        && let Some(id) = parent.attr("id").map(str::trim).filter(|s| !s.is_empty())
    {
        return Some(id.to_owned());
    }
    None
}

fn clean_element_text(el: ElementRef<'_>) -> String {
    let mut parts = Vec::new();
    for node in el.descendants() {
        if let Some(text) = node.value().as_text() {
            let hidden = node.ancestors().filter_map(ElementRef::wrap).any(|p| {
                matches!(
                    p.value().name(),
                    "script" | "style" | "noscript" | "template"
                )
            });
            if !hidden {
                let cleaned = clean::strip_invisible_format_chars(text);
                for word in cleaned.split_whitespace() {
                    parts.push(word.to_owned());
                }
            }
        }
    }
    parts.join(" ")
}

fn extract_outline_from(
    root: ElementRef<'_>,
    base_url: &Url,
    strip_chrome: bool,
    excluded: &HashSet<NodeId>,
    disclosures: &HashSet<NodeId>,
) -> Vec<HeadingEntry> {
    let mut outline = Vec::new();
    let mut skip: Option<NodeId> = None;

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
        let Edge::Open(_) = edge else {
            continue;
        };
        let Some(el) = ElementRef::wrap(node) else {
            continue;
        };
        if should_skip(el, strip_chrome, excluded, disclosures) {
            skip = Some(node.id());
            continue;
        }
        let name = el.value().name();
        if matches!(name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
            let level = name.as_bytes()[1] - b'0';
            let text = clean_element_text(el);
            if text.is_empty() {
                continue;
            }
            let id = resolve_heading_id(el);
            let mut url = base_url.clone();
            url.set_fragment(id.as_deref());
            outline.push(HeadingEntry {
                level,
                text,
                id,
                url,
            });
            if outline.len() >= MAX_OUTLINE_ENTRIES {
                break;
            }
        }
    }

    outline
}

struct ParsedTable {
    caption: Option<String>,
    has_th: bool,
    has_nested_table: bool,
    header_rows: usize,
    rows: Vec<Vec<String>>,
}

fn parse_table_element(table: ElementRef<'_>) -> ParsedTable {
    let mut caption = None;
    let mut has_th = false;
    let mut has_nested_table = false;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut header_rows = 0usize;
    let mut spans: Vec<Option<(usize, String)>> = Vec::new();
    let mut in_thead = false;
    let mut skip_nested: Option<NodeId> = None;

    for edge in table.traverse() {
        let node = match edge {
            Edge::Open(n) | Edge::Close(n) => n,
        };
        if node.id() == table.id() {
            continue;
        }
        if let Some(id) = skip_nested {
            if id == node.id() && matches!(edge, Edge::Close(_)) {
                skip_nested = None;
            }
            continue;
        }
        let Some(el) = ElementRef::wrap(node) else {
            continue;
        };
        let name = el.value().name();
        match edge {
            Edge::Open(_) => match name {
                "table" => {
                    has_nested_table = true;
                    skip_nested = Some(node.id());
                }
                "caption" if caption.is_none() => {
                    let c = clean_element_text(el);
                    if !c.is_empty() {
                        caption = Some(c);
                    }
                    skip_nested = Some(node.id());
                }
                "thead" => in_thead = true,
                "tr" if rows.len() < MAX_TABLE_ROWS => {
                    let mut current_row: Vec<String> = Vec::new();
                    let mut row_all_th = true;
                    let mut cell_count = 0usize;

                    let carry = |row: &mut Vec<String>, spans: &mut [Option<(usize, String)>]| {
                        let col = row.len();
                        if let Some(Some((rem, val))) = spans.get_mut(col) {
                            row.push(val.clone());
                            *rem -= 1;
                            if *rem == 0 {
                                spans[col] = None;
                            }
                            true
                        } else {
                            false
                        }
                    };

                    for child in el.children().filter_map(ElementRef::wrap) {
                        let cname = child.value().name();
                        if cname != "td" && cname != "th" {
                            continue;
                        }
                        cell_count += 1;
                        if cname == "th" {
                            has_th = true;
                        } else {
                            row_all_th = false;
                        }
                        while carry(&mut current_row, &mut spans) {}
                        let text = clean_element_text(child);
                        let colspan = child
                            .attr("colspan")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(1)
                            .clamp(1, 20);
                        let rowspan = child
                            .attr("rowspan")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(1)
                            .clamp(1, 50);
                        for _ in 0..colspan {
                            let col = current_row.len();
                            current_row.push(text.clone());
                            if rowspan > 1 {
                                spans.resize(spans.len().max(col + 1), None);
                                spans[col] = Some((rowspan - 1, text.clone()));
                            }
                        }
                    }
                    while current_row.len() < spans.len() {
                        if !carry(&mut current_row, &mut spans) {
                            current_row.push(String::new());
                        }
                    }
                    if cell_count > 0 && current_row.iter().any(|c| !c.trim().is_empty()) {
                        if rows.is_empty() && (in_thead || row_all_th) {
                            header_rows = 1;
                        }
                        rows.push(current_row);
                    }
                    skip_nested = Some(node.id());
                }
                _ => {}
            },
            Edge::Close(_) => {
                if name == "thead" {
                    in_thead = false;
                }
            }
        }
    }

    ParsedTable {
        caption,
        has_th,
        has_nested_table,
        header_rows,
        rows,
    }
}

fn is_real_data_table(t: &ParsedTable) -> bool {
    if t.has_nested_table || t.rows.len() < 2 {
        return false;
    }
    let max_cols = t.rows.iter().map(Vec::len).max().unwrap_or(0);
    if !(2..=20).contains(&max_cols) {
        return false;
    }
    if t.has_th {
        return true;
    }
    // Without `<th>`, exclude Hacker-News-style numbered link lists (e.g. `1. | Story`)
    // and require >= 3 rows with consistent columns and at least 2/3 filled cells,
    // plus at least one numeric/tabular signal or >= 3 columns.
    if max_cols == 2 {
        let numbered_list = t
            .rows
            .iter()
            .filter(|r| {
                r.first().is_some_and(|c| {
                    c.trim_end_matches('.')
                        .chars()
                        .all(|ch| ch.is_ascii_digit())
                })
            })
            .count();
        if numbered_list * 2 >= t.rows.len() {
            return false;
        }
    }
    let consistent = t.rows.iter().filter(|r| r.len() == max_cols).count();
    let filled: usize = t
        .rows
        .iter()
        .flat_map(|r| r.iter())
        .filter(|c| !c.trim().is_empty())
        .count();
    let total = t.rows.len() * max_cols;
    t.rows.len() >= 3 && consistent * 2 >= t.rows.len() && filled * 3 >= total * 2
}

fn extract_tables_from(
    root: ElementRef<'_>,
    strip_chrome: bool,
    excluded: &HashSet<NodeId>,
    disclosures: &HashSet<NodeId>,
) -> Vec<StructuredTable> {
    let mut tables = Vec::new();
    let mut skip: Option<NodeId> = None;

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
        let Edge::Open(_) = edge else {
            continue;
        };
        let Some(el) = ElementRef::wrap(node) else {
            continue;
        };
        if should_skip(el, strip_chrome, excluded, disclosures) {
            skip = Some(node.id());
            continue;
        }
        if el.value().name() == "table" {
            let parsed = parse_table_element(el);
            skip = Some(node.id());
            if !is_real_data_table(&parsed) {
                continue;
            }
            let max_cols = parsed.rows.iter().map(Vec::len).max().unwrap_or(0);
            let mut normalized_rows: Vec<Vec<String>> = parsed
                .rows
                .into_iter()
                .map(|mut r| {
                    r.resize(max_cols, String::new());
                    r
                })
                .collect();
            let headers = if parsed.header_rows > 0 && !normalized_rows.is_empty() {
                normalized_rows.remove(0)
            } else {
                Vec::new()
            };
            tables.push(StructuredTable {
                caption: parsed.caption,
                headers,
                rows: normalized_rows,
            });
            if tables.len() >= MAX_TABLES {
                break;
            }
        }
    }

    tables
}

fn extract_json_ld(document: &Html) -> Vec<serde_json::Value> {
    static JSON_LD: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse(r#"script[type="application/ld+json" i]"#).unwrap());
    let mut blocks = Vec::new();
    let mut total_bytes = 0usize;
    for script in document.select(&JSON_LD).take(MAX_JSON_LD_BLOCKS) {
        let raw: String = script.text().collect();
        let trimmed = raw.trim();
        if trimmed.is_empty() || total_bytes + trimmed.len() > MAX_JSON_LD_BYTES {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            total_bytes += trimmed.len();
            blocks.push(val);
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_outline_data_tables_and_json_ld_while_skipping_layout_tables() {
        let html = r#"<!doctype html>
        <html>
        <head>
            <script type="application/ld+json">
            {"@context":"https://schema.org","@type":"Article","headline":"ECB Rates","datePublished":"2026-03-19"}
            </script>
        </head>
        <body>
            <nav><h2 id="nav-h">Site Navigation</h2></nav>
            <main>
                <h1 id="main-title">Key ECB Interest Rates</h1>
                <section id="deposit-facility">
                    <h2>Deposit Facility</h2>
                    <table>
                        <caption>Eurosystem Rates</caption>
                        <thead><tr><th>Effective Date</th><th>Deposit</th><th>MRO</th></tr></thead>
                        <tbody>
                            <tr><td>2026-03-19</td><td>2.50%</td><td>2.65%</td></tr>
                            <tr><td>2026-02-05</td><td>2.75%</td><td>2.90%</td></tr>
                        </tbody>
                    </table>
                </section>
                <table class="layout-list">
                    <tr><td>1.</td><td><a href="https://example.org/a">Story One</a></td></tr>
                    <tr><td>2.</td><td><a href="https://example.org/b">Story Two</a></td></tr>
                </table>
            </main>
        </body>
        </html>"#;

        let doc = Html::parse_document(html);
        let base = Url::parse("https://www.ecb.europa.eu/rates").unwrap();
        let data = extract_structured(&doc, &base, &ExtractOptions::default());

        // Outline skips <nav> and resolves both heading id and enclosing <section id>
        assert_eq!(data.outline.len(), 2);
        assert_eq!(data.outline[0].level, 1);
        assert_eq!(data.outline[0].text, "Key ECB Interest Rates");
        assert_eq!(
            data.outline[0].url.as_str(),
            "https://www.ecb.europa.eu/rates#main-title"
        );
        assert_eq!(data.outline[1].level, 2);
        assert_eq!(data.outline[1].text, "Deposit Facility");
        assert_eq!(
            data.outline[1].url.as_str(),
            "https://www.ecb.europa.eu/rates#deposit-facility"
        );

        // Only the real data table is extracted; the Hacker-News-style layout table is skipped.
        assert_eq!(data.tables.len(), 1);
        assert_eq!(data.tables[0].caption.as_deref(), Some("Eurosystem Rates"));
        assert_eq!(
            data.tables[0].headers,
            vec!["Effective Date", "Deposit", "MRO"]
        );
        assert_eq!(
            data.tables[0].rows,
            vec![
                vec!["2026-03-19", "2.50%", "2.65%"],
                vec!["2026-02-05", "2.75%", "2.90%"],
            ]
        );

        // JSON-LD extracted deterministically
        assert_eq!(data.json_ld.len(), 1);
        assert_eq!(data.json_ld[0]["headline"], "ECB Rates");
    }
}
