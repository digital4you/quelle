use std::{collections::HashMap, sync::LazyLock};

use ego_tree::iter::Edge;
use regex::Regex;
use scraper::{ElementRef, Selector};

/// Collect visible body text once, including short labels, mixed inline/block
/// content, contact links and the footer. Preserve line breaks in code samples.
/// Without `keep_chrome`, site navigation, search forms, the site banner and
/// skip links are left out.
pub(super) fn page_text(root: ElementRef<'_>, keep_chrome: bool) -> String {
    let helpers = contact_helpers(root);
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
                ) || el.attr("hidden").is_some()
                    || el.attr("aria-hidden") == Some("true")
                    || el.attr("style").is_some_and(hidden_style)
                    || (!keep_chrome && chrome(el))
                {
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
                    && !text[link_start..].contains(&contact)
                {
                    if text.len() > link_start {
                        text.push_str(" (");
                        text.push_str(&contact);
                        text.push(')');
                    } else {
                        text.push_str(&contact);
                    }
                }
                if name == "pre" {
                    pre -= 1;
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
                    if c.is_whitespace() {
                        space = true;
                    } else {
                        if space && !text.is_empty() && !text.ends_with('\n') {
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

/// Page chrome: navigation, search, the site banner, language switchers and
/// skip links. Headers inside the main content or holding the page heading
/// stay, and so do footers, which carry addresses and contact details.
pub(super) fn is_chrome(el: ElementRef<'_>) -> bool {
    chrome(el)
}

fn chrome(el: ElementRef<'_>) -> bool {
    static SKIP_LINK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?i)^(?:skip|jump)(?: directly)? to (?:the )?(?:main )?(?:content|navigation|search)",
            r"|^(?:direkt )?zu[mr]? (?:haupt)?(?:inhalt|navigation|suche)",
            r"|^(?:aller|passer|accéder) (?:directement )?(?:au|à la) (?:contenu|navigation|recherche)",
            r"|^(?:vai|salta|passa) (?:direttamente )?(?:al|alla) (?:contenuto|navigazione|ricerca)",
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

struct Helper {
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

fn contact_helpers(root: ElementRef<'_>) -> HashMap<String, Helper> {
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

fn contact(el: ElementRef<'_>, helpers: &HashMap<String, Helper>) -> Option<String> {
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
