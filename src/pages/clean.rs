use std::{
    collections::{HashMap, HashSet},
    sync::LazyLock,
};

use ego_tree::{NodeId, iter::Edge};
use regex::Regex;
use scraper::{ElementRef, Html, Selector};

pub(super) fn content_text(document: &Html, base: &url::Url, keep_chrome: bool) -> String {
    let root = document.root_element();
    let full = page_text_with_links(root, keep_chrome, Some(base));
    if keep_chrome {
        return full;
    }
    static MAIN: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse("main, article, [role=main]").unwrap());
    let best = document
        .select(&MAIN)
        .filter(|el| {
            !el.ancestors().filter_map(ElementRef::wrap).any(|p| {
                matches!(p.value().name(), "noscript" | "template")
                    || p.attr("hidden").is_some()
                    || p.attr("aria-hidden") == Some("true")
            })
        })
        .take(32)
        .map(|el| page_text_with_links(el, false, Some(base)))
        .max_by_key(String::len);
    let Some(mut main) = best.filter(|s| s.trim().len() >= 250 && s.len() * 4 >= full.len()) else {
        return full;
    };
    // Contact information outside the article can be evidence too. Keep its
    // public targets without importing the entire site footer or consent UI.
    let helpers = contact_helpers(root);
    let disclosures = disclosure_ids(root);
    static CONTACT: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a, button").unwrap());
    for el in root.select(&CONTACT) {
        if std::iter::once(el)
            .chain(el.ancestors().filter_map(ElementRef::wrap))
            .any(|p| {
                is_hidden_node(p, &disclosures)
                    || matches!(p.value().name(), "noscript" | "template")
                    || super::markdown::is_markdown_chrome(p)
            })
        {
            continue;
        }
        if let Some(target) = contact(el, &helpers)
            && !same_contact(&main, &target)
        {
            let label = super::normalize(el.text(), 150);
            main.push('\n');
            if label.is_empty() || same_contact(&label, &target) {
                main.push_str(&target);
            } else {
                main.push_str(&label);
                main.push_str(" (");
                main.push_str(&target);
                main.push(')');
            }
            main.push('\n');
        }
    }
    main
}

/// Collect visible body text once, including short labels, mixed inline/block
/// content, contact links and the footer. Preserve line breaks in code samples.
/// Without `keep_chrome`, site navigation, search forms, the site banner and
/// skip links are left out.
pub(super) fn page_text(root: ElementRef<'_>, keep_chrome: bool) -> String {
    page_text_with_links(root, keep_chrome, None)
}

pub(super) fn page_text_with_links(
    root: ElementRef<'_>,
    keep_chrome: bool,
    base: Option<&url::Url>,
) -> String {
    let helpers = contact_helpers(root);
    let disclosures = disclosure_ids(root);
    let mut text = String::new();
    let mut skip = None;
    let mut pre = 0;
    let mut space = false;
    let mut link_start = 0;
    for edge in root.traverse() {
        let node = match edge {
            Edge::Open(node) | Edge::Close(node) => node,
        };
        if let Some(id) = skip {
            if id == node.id() && matches!(edge, Edge::Close(_)) {
                skip = None;
            }
            continue;
        }
        if let Some(el) = ElementRef::wrap(node) {
            let name = el.value().name();
            if matches!(edge, Edge::Open(_)) {
                if matches!(
                    name,
                    "head" | "script" | "style" | "noscript" | "template" | "svg"
                ) || is_hidden_node(el, &disclosures)
                    || (!keep_chrome && super::markdown::is_markdown_chrome(el))
                {
                    skip = Some(node.id());
                    continue;
                }
                if name == "table"
                    && let Some(base) = base
                {
                    if !text.is_empty() && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(&super::markdown::table_text(el, base));
                    text.push('\n');
                    space = false;
                    skip = Some(node.id());
                    continue;
                }
                if name == "pre" {
                    pre += 1;
                }
                if matches!(name, "a" | "button") {
                    link_start = text.len();
                }
            } else {
                if matches!(name, "a" | "button")
                    && let Some(contact) = contact(el, &helpers)
                    && !same_contact(&text[link_start..], &contact)
                {
                    trim_trailing_link_ws(&mut text, link_start);
                    if text.len() > link_start {
                        text.push_str(" (");
                        text.push_str(&contact);
                        text.push(')');
                    } else {
                        text.push_str(&contact);
                    }
                    space = true;
                }
                if name == "pre" {
                    pre -= 1;
                }
                if name == "a"
                    && let Some(base) = base
                    && let Some(href) = el.attr("href").filter(|h| !h.trim().is_empty())
                    && let Ok(url) = base.join(href).and_then(|u| {
                        super::evidence_url(u).map_err(|_| url::ParseError::EmptyHost)
                    })
                {
                    trim_trailing_link_ws(&mut text, link_start);
                    if text.len() > link_start && !text[link_start..].contains(url.as_str()) {
                        text.push_str(" (");
                        text.push_str(url.as_str());
                        text.push(')');
                        space = true;
                    }
                }
            }
            if matches!(name, "td" | "th") {
                space = true;
            } else if !inline(name) && !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
                space = false;
            }
        } else if let Edge::Open(_) = edge
            && let Some(value) = node.value().as_text()
        {
            if pre > 0 {
                text.push_str(value);
            } else {
                for c in value.chars() {
                    if is_invisible_format_char(c) {
                        continue;
                    }
                    if c.is_whitespace() {
                        space = true;
                    } else {
                        let suppress = matches!(
                            c,
                            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '’' | '\''
                        );
                        if space && !suppress && !text.is_empty() && !text.ends_with('\n') {
                            text.push(' ');
                        }
                        space = false;
                        text.push(c);
                    }
                }
            }
        }
        if text.len() > super::FULL_TEXT_BYTES && text.trim_end().len() > super::FULL_TEXT_BYTES {
            break;
        }
    }
    text
}

fn trim_trailing_link_ws(text: &mut String, link_start: usize) {
    while text.len() > link_start && text.ends_with([' ', '\n', '\r', '\t']) {
        text.pop();
    }
}

pub(super) fn is_invisible_format_char(c: char) -> bool {
    matches!(c, '\u{00ad}' | '\u{200b}' | '\u{feff}')
}

pub(super) fn strip_invisible_format_chars(text: &str) -> String {
    text.chars()
        .filter(|&c| !is_invisible_format_char(c))
        .collect()
}

/// Returns true when `haystack` already expresses the same email or phone number
/// as `target` (ignoring case for emails and spacing/formatting/`00` vs `+` for
/// phone numbers), preventing redundant `+41 58 462 41 97 (+41584624197)` output.
pub(super) fn same_contact(haystack: &str, target: &str) -> bool {
    let target_trimmed = target.trim();
    if target_trimmed.is_empty() {
        return true;
    }
    let hay_lower = haystack.to_ascii_lowercase();
    let target_lower = target_trimmed.to_ascii_lowercase();
    if hay_lower.contains(&target_lower) {
        return true;
    }
    if target_trimmed.contains('@') {
        return false;
    }
    let norm_digits = |s: &str| -> String {
        let digits: String = s.chars().filter(char::is_ascii_digit).collect();
        digits.strip_prefix("00").unwrap_or(&digits).to_owned()
    };
    let target_digits = norm_digits(target_trimmed);
    if target_digits.len() < 6 {
        return false;
    }
    let hay_digits = norm_digits(haystack);
    hay_digits.contains(&target_digits)
}

/// Identifies collapsed content disclosure containers (ARIA accordions and tab
/// panels) whose `hidden`, `aria-hidden="true"`, or `visibility:hidden` /
/// `display:none` styling hides unread article sections rather than decorative
/// icons, dialogs, loading indicators or navigation chrome.
pub(super) fn disclosure_ids(root: ElementRef<'_>) -> HashSet<NodeId> {
    static TOGGLE: LazyLock<Selector> = LazyLock::new(|| {
        Selector::parse(
            "button[aria-controls], [role=button][aria-controls], [role=tab][aria-controls], [aria-expanded][aria-controls]",
        )
        .unwrap()
    });
    static CANDIDATES: LazyLock<Selector> = LazyLock::new(|| {
        Selector::parse("[role=tabpanel], [role=region][aria-labelledby], [id], [class]").unwrap()
    });

    let mut controlled_ids: HashSet<&str> = HashSet::new();
    for toggle in root.select(&TOGGLE) {
        let in_chrome = std::iter::once(toggle)
            .chain(toggle.ancestors().filter_map(ElementRef::wrap))
            .any(|p| {
                matches!(p.value().name(), "nav" | "dialog" | "aside")
                    || matches!(
                        p.attr("role"),
                        Some(
                            "navigation" | "search" | "dialog" | "alertdialog" | "menu" | "menubar"
                        )
                    )
                    || super::markdown::is_markdown_chrome(p)
            });
        if in_chrome {
            continue;
        }
        if let Some(controls) = toggle.attr("aria-controls") {
            for id in controls.split_whitespace() {
                let lower = id.to_ascii_lowercase();
                if !lower.contains("search")
                    && !lower.contains("menu")
                    && !lower.contains("nav")
                    && !lower.contains("cookie")
                    && !lower.contains("modal")
                {
                    controlled_ids.insert(id);
                }
            }
        }
    }

    let mut ids = HashSet::new();
    for el in root.select(&CANDIDATES) {
        if matches!(
            el.value().name(),
            "head"
                | "script"
                | "style"
                | "noscript"
                | "template"
                | "svg"
                | "iframe"
                | "dialog"
                | "nav"
                | "aside"
                | "header"
                | "footer"
        ) || matches!(
            el.attr("role"),
            Some(
                "status"
                    | "alert"
                    | "dialog"
                    | "alertdialog"
                    | "menu"
                    | "menubar"
                    | "tooltip"
                    | "navigation"
                    | "search"
                    | "banner"
                    | "table"
            )
        ) || el.attr("aria-busy") == Some("true")
            || el.attr("aria-modal") == Some("true")
            || el.attr("data-component").is_some()
            || el.attr("identifier") == Some("table")
            || super::markdown::is_markdown_chrome(el)
        {
            continue;
        }
        let is_tabpanel = el.attr("role") == Some("tabpanel");
        let is_labeled_region =
            el.attr("role") == Some("region") && el.attr("aria-labelledby").is_some();
        let is_controlled = el.attr("id").is_some_and(|id| controlled_ids.contains(id));
        let has_disclosure_class = el.attr("class").is_some_and(|cls| {
            cls.split_whitespace().any(|tok| {
                let lower = tok.to_ascii_lowercase();
                matches!(
                    lower.as_str(),
                    "accordion__drawer"
                        | "accordion__content"
                        | "accordion-body"
                        | "accordion-content"
                        | "accordion-collapse"
                        | "accordion-panel"
                        | "tab-pane"
                        | "tabpanel"
                        | "collapsible-content"
                )
            })
        });
        if is_tabpanel || is_labeled_region || is_controlled || has_disclosure_class {
            ids.insert(el.id());
        }
    }
    ids
}

pub(super) fn is_hidden_node(el: ElementRef<'_>, disclosures: &HashSet<NodeId>) -> bool {
    if disclosures.contains(&el.id()) {
        return false;
    }
    el.attr("hidden").is_some()
        || el.attr("aria-hidden") == Some("true")
        || el.attr("style").is_some_and(hidden_style)
}

/// Page chrome: navigation, search, the site banner, language switchers and
/// skip links. Headers inside the main content or holding the page heading
/// stay, and so do footers, which carry addresses and contact details.
pub(super) fn is_chrome(el: ElementRef<'_>) -> bool {
    chrome(el)
}

fn chrome(el: ElementRef<'_>) -> bool {
    static SKIP_LINK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?i)^(?:skip|jump)(?: directly)? to (?:the )?(?:main )?(?:content|navigation|search|footer)",
            r"|^(?:direkt )?zu[mr]? (?:haupt)?(?:inhalt|navigation|suche|footer|fusszeile|fußzeile)(?: springen)?",
            r"|^überspringe(?:n)? (?:den |die )?(?:haupt)?(?:inhalt|navigation|suche)",
            r"|^(?:aller|passer|accéder) (?:directement )?(?:au|à la) (?:contenu|navigation|recherche|pied de page)",
            r"|^(?:vai|salta|passa) (?:direttamente )?(?:al|alla) (?:contenuto|navigazione|ricerca|piè di pagina)",
        ))
        .expect("skip link pattern")
    });
    static HEADING: LazyLock<Selector> = LazyLock::new(|| Selector::parse("h1").unwrap());
    if matches!(el.attr("role"), Some("navigation" | "search" | "banner")) {
        return true;
    }
    match el.value().name() {
        "nav" => true,
        "header" => {
            !el.ancestors().filter_map(ElementRef::wrap).any(|parent| {
                matches!(parent.value().name(), "main" | "article")
                    || parent.attr("role") == Some("main")
            }) && el.select(&HEADING).next().is_none()
        }
        // Language switchers mark their links with hreflang.
        "a" if el.attr("hreflang").is_some() => true,
        "a" => {
            el.attr("href").is_some_and(|href| href.starts_with('#')) && {
                let label = el.text().collect::<String>();
                label.len() < 80 && SKIP_LINK.is_match(label.trim())
            }
        }
        _ => false,
    }
}

pub(super) fn hidden_style(style: &str) -> bool {
    style
        .split(';')
        .filter_map(|rule| rule.split_once(':'))
        .any(|(prop, value)| {
            let value = value.split('!').next().unwrap_or("").trim();
            (prop.trim().eq_ignore_ascii_case("display") && value.eq_ignore_ascii_case("none"))
                || (prop.trim().eq_ignore_ascii_case("visibility")
                    && value.eq_ignore_ascii_case("hidden"))
        })
}

pub(super) fn contact_passages(root: ElementRef<'_>, title: &str) -> Vec<String> {
    static CONTACTS: LazyLock<Selector> = LazyLock::new(|| {
        Selector::parse(r#"a[href^="mailto:" i], a[href^="tel:" i], a[href^="javascript:" i], a[onclick], button[onclick]"#)
            .expect("contact links")
    });
    let mut links = root.select(&CONTACTS).peekable();
    if links.peek().is_none() {
        return Vec::new();
    }
    let helpers = contact_helpers(root);
    links
        .filter(|el| {
            !std::iter::once(*el)
                .chain(el.ancestors().filter_map(ElementRef::wrap))
                .any(|parent| {
                    parent.attr("hidden").is_some()
                        || parent.attr("aria-hidden") == Some("true")
                        || parent.attr("style").is_some_and(hidden_style)
                        || matches!(parent.value().name(), "template" | "noscript")
                })
        })
        .filter_map(|el| {
            let value = contact(el, &helpers)?;
            let label = super::normalize(super::visible_text(el), 150);
            let kind = if value.contains('@') { "Email" } else { "Tel" };
            Some(if label.is_empty() || label == value {
                format!("{title} — {kind}: {value}")
            } else {
                format!("{title} — {label} ({kind}: {value})")
            })
        })
        .take(super::MAX_BLOCKS)
        .collect()
}

pub(super) fn inline(name: &str) -> bool {
    !matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "br"
            | "caption"
            | "dd"
            | "details"
            | "dialog"
            | "div"
            | "dl"
            | "dt"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hr"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "summary"
            | "table"
            | "tbody"
            | "thead"
            | "tr"
            | "ul"
    )
}

pub(super) struct Helper {
    params: Vec<String>,
    expression: String,
}

// Recognize a single navigation statement and string concatenation. Never
// evaluate JS, invoke handlers or follow mailto/tel links during extraction.
fn navigation_expression(js: &str) -> Option<&str> {
    static NAV: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
        r"^(?:(?:(?:window|document)\.)?location(?:\.href)?\s*=\s*(?P<set>.+?)|(?:(?:window\.)?open|(?:window\.)?location\.(?:assign|replace))\(\s*(?P<open>.+?)\s*\))\s*;?$"
    ).expect("contact navigation")
    });
    let m = NAV.captures(js.trim())?;
    m.name("set").or_else(|| m.name("open")).map(|s| s.as_str())
}

pub(super) fn contact_helpers(root: ElementRef<'_>) -> HashMap<String, Helper> {
    static SCRIPTS: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse("script:not([src])").unwrap());
    static FUNCTION: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"function\s+([A-Za-z_$][\w$]*)\s*\(([^()]*)\)\s*\{\s*([^{}]{1,512})\s*\}")
            .expect("simple contact function")
    });
    let mut helpers = HashMap::new();
    for script in root.select(&SCRIPTS).flat_map(|s| s.text()) {
        if !script.contains("mailto:") && !script.contains("tel:") {
            continue;
        }
        for m in FUNCTION.captures_iter(script) {
            if let Some(expr) = navigation_expression(&m[3])
                && (expr.contains("mailto:") || expr.contains("tel:"))
            {
                let params = m[2]
                    .split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_owned)
                    .collect();
                helpers.insert(
                    m[1].to_owned(),
                    Helper {
                        params,
                        expression: expr.to_owned(),
                    },
                );
                if helpers.len() == 128 {
                    return helpers;
                }
            }
        }
    }
    helpers
}

fn literal(input: &str) -> Option<(&str, &str)> {
    let quote = input.chars().next().filter(|c| matches!(c, '\'' | '"'))?;
    let (value, rest) = input[1..].split_once(quote)?;
    (!value.contains('\\')).then_some((value, rest.trim_start()))
}

fn concatenate(mut expr: &str, vars: &HashMap<&str, &str>) -> Option<String> {
    let mut value = String::new();
    loop {
        let (part, rest) = if let Some(pair) = literal(expr) {
            pair
        } else {
            let end = expr
                .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '$')))
                .unwrap_or(expr.len());
            (*vars.get(&expr[..end])?, expr[end..].trim_start())
        };
        value.push_str(part);
        if rest.is_empty() {
            return Some(value);
        }
        expr = rest.strip_prefix('+')?.trim_start();
    }
}

fn from_handler(js: &str, helpers: &HashMap<String, Helper>) -> Option<String> {
    let mut vars = HashMap::new();
    if let Some(expr) = navigation_expression(js) {
        return concatenate(expr, &vars);
    }
    let js = js.trim().trim_end_matches(';').trim().strip_suffix(')')?;
    let (name, args) = js.split_once('(')?;
    let helper = helpers.get(name.trim().strip_prefix("return ").unwrap_or(name.trim()))?;
    let mut args = args.trim();
    for (index, param) in helper.params.iter().enumerate() {
        if index > 0 {
            args = args.strip_prefix(',')?.trim_start();
        }
        let (value, rest) = literal(args)?;
        vars.insert(param.as_str(), value);
        args = rest;
    }
    if !args.is_empty() {
        return None;
    }
    concatenate(&helper.expression, &vars)
}

pub(super) fn contact(el: ElementRef<'_>, helpers: &HashMap<String, Helper>) -> Option<String> {
    [el.attr("href"), el.attr("onclick")]
        .into_iter()
        .flatten()
        .find_map(|attr| {
            let attr = attr.trim();
            if attr.len() > 2048 {
                return None;
            }
            let handler;
            let uri = if attr
                .get(..7)
                .is_some_and(|s| s.eq_ignore_ascii_case("mailto:"))
                || attr
                    .get(..4)
                    .is_some_and(|s| s.eq_ignore_ascii_case("tel:"))
            {
                attr
            } else {
                handler = from_handler(attr.strip_prefix("javascript:").unwrap_or(attr), helpers)?;
                &handler
            };
            let (scheme, address) = uri.split_once(':')?;
            if !scheme.eq_ignore_ascii_case("mailto") && !scheme.eq_ignore_ascii_case("tel") {
                return None;
            }
            let value = super::percent_decode(address.split(['?', '#']).next()?)
                .trim()
                .to_owned();
            (!value.is_empty() && !value.chars().any(char::is_control)).then_some(value)
        })
}
