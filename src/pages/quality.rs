//! Conservative failure and incomplete-rendering checks shared by both APIs.
use super::*;

pub(super) fn text_error(text: &str) -> Option<PageError> {
    let text = text.trim().to_lowercase();
    if text.len() > 300 {
        return None;
    }
    if (text.len() < 100
        && (text.starts_with("404 not found") || text.starts_with("404: page not found")))
        || text.contains("404 deployment_not_found")
        || text.contains("404 deployment\\_not\\_found")
    {
        return Some(PageError::NotFound);
    }
    if text.starts_with("401") && text.contains("unauthorized") {
        return Some(PageError::Unauthorized);
    }
    if text.starts_with("503") && text.contains("service unavailable") {
        return Some(PageError::Upstream);
    }
    None
}

pub(super) fn check(document: &Html, rendered: bool) -> Result<(), PageError> {
    static TITLE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("title, h1").unwrap());
    static MAIN: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse("main, article, [role=main]").unwrap());
    static SCRIPT: LazyLock<Selector> = LazyLock::new(|| {
        Selector::parse("script[src], script[type=module], app-root, #__next, #__nuxt").unwrap()
    });
    static PROSE: LazyLock<Selector> = LazyLock::new(|| Selector::parse("p, pre, td, dd").unwrap());
    static LINKS: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse("a, nav, footer, [role=navigation]").unwrap());
    static TABLES: LazyLock<Selector> = LazyLock::new(|| {
        Selector::parse("[identifier=table], [data-component=table], [role=table]").unwrap()
    });
    static FEEDS: LazyLock<Selector> = LazyLock::new(|| {
        Selector::parse(
            "[identifier=teaser-nnsb-container], [data-component=news], [data-component=feed]",
        )
        .unwrap()
    });
    static FEED_ITEMS: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse("a, article, li, p, td").unwrap());
    let plain = clean::page_text(document.root_element(), false);
    if let Some(error) = text_error(&plain) {
        return Err(error);
    }
    let meaningful_main = document.select(&MAIN).any(|m| {
        m.select(&PROSE)
            .any(|p| clean::page_text(p, false).chars().count() >= 120)
    });
    if !meaningful_main {
        for title in document.select(&TITLE) {
            let title = normalize(title.text(), 180).to_lowercase();
            if title.starts_with("404")
                || title.starts_with("410 gone")
                || matches!(
                    title.as_str(),
                    "page not found" | "this page doesn’t exist" | "this page doesn't exist"
                )
            {
                return Err(PageError::NotFound);
            }
            if title == "401" || title == "unauthorized" || title == "authentication required" {
                return Err(PageError::Unauthorized);
            }
            if title == "503" || title == "service unavailable" || title == "502 bad gateway" {
                return Err(PageError::Upstream);
            }
        }
    }
    if document.select(&SCRIPT).next().is_some() {
        // Hydrated tables can be missing even when the surrounding article is
        // long. Do not turn the surrounding prose into apparently complete evidence.
        if document
            .select(&TABLES)
            .filter(|table| !hidden(*table))
            .any(|table| clean::page_text(table, false).trim().is_empty())
        {
            return Err(PageError::NeedsJs);
        }
        // SSR pages can also contain empty client-hydrated news/feed widgets
        // (e.g. Nuxt containers carrying only a section heading until hydrated).
        // Escalate once from static HTML, without failing an already-rendered page.
        if !rendered
            && document
                .select(&FEEDS)
                .filter(|feed| !hidden(*feed))
                .any(|feed| feed.select(&FEED_ITEMS).next().is_none())
        {
            return Err(PageError::NeedsJs);
        }
        let mut main_size: Option<usize> = None;
        let mut total_main_size = 0usize;
        for m in document.select(&MAIN).filter(|m| !hidden(*m)) {
            let len = clean::page_text(m, false).chars().count();
            main_size = Some(main_size.map_or(len, |best| best.max(len)));
            total_main_size = total_main_size.saturating_add(len);
        }
        static LOADING: LazyLock<Selector> = LazyLock::new(|| {
            Selector::parse(
                "main, [role=main], [role=status], [aria-busy=true], .loading, .loader, .spinner",
            )
            .unwrap()
        });
        let loading = document.select(&LOADING).filter(|m| !hidden(*m)).any(|m| {
            let text = clean::page_text(m, false);
            let label = text
                .trim()
                .trim_end_matches(['.', '…'])
                .trim()
                .to_lowercase();
            matches!(
                label.as_str(),
                "loading"
                    | "loading application"
                    | "loading document"
                    | "laden der anwendung"
                    | "dokument wird geladen"
                    | "chargement de l’application"
                    | "chargement de l'application"
                    | "caricamento dell'applicazione"
            )
        });
        let prose_size = document
            .select(&PROSE)
            .map(|p| clean::page_text(p, false).chars().count())
            .max()
            .unwrap_or(0);
        let link_size = document
            .select(&LINKS)
            .map(|p| p.text().map(str::len).sum::<usize>())
            .sum::<usize>();
        if main_size == Some(0)
            || (loading && main_size.is_none_or(|n| n < 300))
            || (prose_size < 80
                && plain.len() < 5000
                && link_size > plain.len() / 2
                && total_main_size < 250
                && main_size.is_none_or(|n| n < 80))
        {
            return Err(PageError::NeedsJs);
        }
    }
    Ok(())
}

fn hidden(el: scraper::ElementRef<'_>) -> bool {
    std::iter::once(el)
        .chain(el.ancestors().filter_map(scraper::ElementRef::wrap))
        .any(|p| {
            matches!(p.value().name(), "noscript" | "template")
                || p.attr("hidden").is_some()
                || p.attr("aria-hidden") == Some("true")
                || p.attr("style").is_some_and(clean::hidden_style)
        })
}
