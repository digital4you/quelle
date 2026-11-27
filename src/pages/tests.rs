use super::*;

#[test]
fn hard_http_failures_have_distinct_non_browser_outcomes() {
    for (status, expected) in [
        (404, "not_found"),
        (410, "not_found"),
        (401, "authentication_required"),
        (429, "rate_limited"),
        (503, "upstream_error"),
        (403, "blocked"),
    ] {
        assert_eq!(status_error(status).unwrap().code(), expected);
    }
    assert!(status_error(200).is_none());
}

#[test]
fn error_documents_are_not_evidence_but_error_documentation_is() {
    for html in [
        "<title>404 Not Found</title><h1>404 Not Found</h1>",
        "<h1>401</h1><p>Unauthorized</p>",
        "<h1>This page doesn’t exist</h1><p>404 DEPLOYMENT_NOT_FOUND</p>",
        "<h1>503</h1><p>Service Unavailable</p>",
    ] {
        assert!(
            extract(
                Url::parse("https://example.org/").unwrap(),
                html.as_bytes(),
                Some("text/html"),
                None,
                true
            )
            .is_err(),
            "{html}"
        );
    }
    assert!(read("<main><h1>404 Not Found</h1><p>The HTTP 404 status code indicates that the requested resource cannot be found. This article explains how to configure a custom error response and diagnose incorrect routes.</p><pre>HTTP/1.1 404 Not Found</pre></main>").content.contains("diagnose"));
}

#[test]
fn long_navigation_shell_needs_rendering_but_short_articles_do_not() {
    let html = format!(
        "<script src='/app.js'></script><div id='root'><nav>{}</nav><main></main></div>",
        "<a href='/menu'>Products and services</a>".repeat(100)
    );
    assert!(matches!(
        extract(
            Url::parse("https://example.org/").unwrap(),
            html.as_bytes(),
            Some("text/html"),
            None,
            false
        ),
        Err(PageError::NeedsJs)
    ));
    assert!(read("<script src='/analytics.js'></script><main><h1>Hours</h1><p>Open Monday to Friday, 09:00–17:00.</p></main>").content.contains("09:00"));
}

#[test]
fn content_preserves_destination_links_and_table_relationships() {
    let text = read("<main><h1>Rates</h1><a href='/register'>Register</a><table><tr><th>Type</th><th>2023</th><th>2024</th></tr><tr><td>Standard</td><td>7.7%</td><td>8.1%</td></tr></table><pre>let x = 1;\n  println!(\"{x}\");</pre></main>").content;
    assert!(text.contains("https://example.org/register"), "{text}");
    assert!(text.contains("| Type | 2023 | 2024 |"), "{text}");
    assert!(text.contains("| Standard | 7.7% | 8.1% |"), "{text}");
    assert!(text.contains("\n  println!"), "{text}");
}

#[test]
fn legacy_encoding_and_case_insensitive_charset_are_preserved() {
    let bytes = b"<p>fran\xe7ais caf\xe9</p>";
    assert!(decode_bytes(bytes, Some("text/html; Charset=windows-1252")).contains("français café"));
    assert!(decode_bytes(bytes, Some("text/html")).contains("français café"));
}

fn read(html: &str) -> Source {
    extract(
        Url::parse("https://example.org/").unwrap(),
        html.as_bytes(),
        Some("text/html"),
        None,
        false,
    )
    .unwrap()
}

#[test]
fn full_text_keeps_order_short_values_and_repeated_rows() {
    let long = "élan ".repeat(2000);
    let html = format!(
        "<h1>A</h1><section>First<br>Second</section><table><tr><td>3</td><td>7</td></tr><tr><td>3</td><td>7</td></tr></table><p>{long}</p><footer>End</footer>"
    );
    let text = read(&html).content;
    assert!(text.starts_with("# A\n\nFirst\nSecond"), "{text}");
    assert_eq!(text.matches('3').count(), 2);
    assert_eq!(text.matches("élan").count(), 2000);
    assert!(text.ends_with("End"), "{text}");
}

#[test]
fn links_expose_public_contact_targets_and_exclude_hidden_markup() {
    let html = r#"<script>Invisible</script><p hidden>Secret</p>
        <a href="mailto:office%40example.org?subject=test">Email</a>
        <a onclick="window.location.href = 'tel:' + '+41 44 123 45 67'">Call</a>
        <a href="javascript:location.href='mailto:' + 'info' + '@example.org'">Write</a>
        <a onclick="location.href='mailto:' + dynamic">Ask</a>"#;
    let text = read(html).content;
    for term in [
        "Email",
        "office@example.org",
        "+41 44 123 45 67",
        "info@example.org",
        "Ask",
    ] {
        assert!(text.contains(term), "{text}");
    }
    for term in ["Invisible", "Secret", "subject", "javascript:", "dynamic"] {
        assert!(!text.contains(term), "{text}");
    }
}

#[test]
fn ordinary_discussion_of_javascript_is_content() {
    let text = read("<main><h1>JavaScript</h1><p>JavaScript supports functions.</p></main>");
    assert!(text.content.contains("JavaScript supports functions."));
}

#[test]
fn page_limit_respects_utf8() {
    let source = read(&format!("<p>{}</p>", "é".repeat(FULL_TEXT_BYTES)));
    assert_eq!(source.content.len(), FULL_TEXT_BYTES);
    assert_eq!(source.metadata.truncated, Some(true));
}

#[test]
fn negotiated_missing_markdown_retries_html_without_retrying_real_errors() {
    let original = Url::parse("https://example.org/docs/start").unwrap();
    let markdown = Url::parse("https://example.org/raw/docs/start.md").unwrap();
    assert!(retry_html_negotiation(
        &original,
        &markdown,
        404,
        Some("text/markdown; charset=utf-8")
    ));
    assert!(retry_html_negotiation(
        &original,
        &original,
        406,
        Some("text/html")
    ));
    assert!(!retry_html_negotiation(
        &markdown,
        &markdown,
        404,
        Some("text/markdown")
    ));
    assert!(!retry_html_negotiation(
        &original,
        &original,
        404,
        Some("text/html")
    ));
    for status in [200, 401, 403, 429, 503] {
        assert!(!retry_html_negotiation(
            &original,
            &markdown,
            status,
            Some("text/markdown")
        ));
    }
}

#[test]
fn evidence_links_keep_sections_without_relaxing_fetch_target_checks() {
    let html = r##"<main><h1>Law</h1><p>See <a href="/law#a773">Article 773</a> and
        <a href="#rates">Table</a>. <a href="http://127.0.0.1/#x">Private</a>
        <a href="javascript:alert(1)">Script</a></p></main>"##;
    let source = read(html);
    assert!(source.content.contains("https://example.org/law#a773"));
    assert!(source.content.contains("https://example.org/#rates"));
    assert!(!source.content.contains("127.0.0.1"));
    assert!(!source.content.contains("javascript:"));
    let base = Url::parse("https://example.org/").unwrap();
    let doc = Html::parse_document(html);
    let md = markdown::html_to_markdown(&doc, &base, &ExtractOptions::default(), 10000);
    assert!(md.contains("[Table](https://example.org/#rates)"), "{md}");
    let links = markdown::extract_links(&doc, &base, 20);
    assert_eq!(links.len(), 2);
    assert!(links.iter().any(|u| u.fragment() == Some("a773")));
    assert!(links.iter().any(|u| u.fragment() == Some("rates")));
    assert!(public_url(links[0].clone()).unwrap().fragment().is_none());
}

#[test]
fn table_cells_keep_blocks_captions_spans_and_escaped_pipes() {
    let text = read(r#"<h1>Rates</h1><table><caption>CHF, annual totals</caption>
        <tr><th rowspan="2">Type</th><th colspan="2">Period</th></tr>
        <tr><th>2023</th><th>2024</th></tr>
        <tr><td><p>A | B</p></td><td><div>7.7%</div></td><td><p>8.1%</p><a href="/notes#rate">Note</a></td></tr>
        </table><pre>fn main() {
    work();
}</pre>"#).content;
    assert!(text.contains("CHF, annual totals"), "{text}");
    assert!(text.contains("| Type | Period | Period |"), "{text}");
    assert!(text.contains("| Type | 2023 | 2024 |"), "{text}");
    assert!(text.contains("| A \\| B | 7.7% | 8.1%"), "{text}");
    assert!(text.contains("https://example.org/notes#rate"), "{text}");
    assert!(text.contains("\n    work();\n"), "{text}");
}

#[test]
fn main_content_removes_site_header_and_consent_but_keeps_evidence_and_contacts() {
    let prose =
        "The dated policy decision preserves its rate, publication date and economic explanation. "
            .repeat(8);
    let html = format!(
        r#"<header><p>Skip Links Navigation</p><h1>Site logo</h1><a href="/menu">Site menu</a></header>
        <main><header><h1>Policy decision</h1></header><p>{prose}</p><p>24 September 2026, 0.0%</p></main>
        <footer><a href="mailto:press@example.org">Press contact</a></footer>
        <div id="cookie-banner"><p>Accept advertising cookies</p></div>"#
    );
    let text = read(&html).content;
    for unwanted in [
        "Skip Links Navigation",
        "Site menu",
        "Site logo",
        "Accept advertising",
    ] {
        assert!(!text.contains(unwanted), "{text}");
    }
    for wanted in [
        "Policy decision",
        "24 September 2026",
        "0.0%",
        "press@example.org",
    ] {
        assert!(text.contains(wanted), "{text}");
    }
    // A standalone article header still carries the title; an unrelated tiny
    // main region must not replace the rest of the evidence.
    assert!(
        read("<header><h1>Article title</h1></header><p>Actual article.</p>")
            .content
            .contains("Article title")
    );
    let text = read(&format!(
        "<main><p>Sidebar</p></main><section><p>{prose}</p></section>"
    ))
    .content;
    assert!(text.contains("economic explanation"));
}

#[test]
fn loading_application_is_not_evidence_even_with_long_support_notices() {
    let support = "Use a current browser to access all application features. ".repeat(100);
    for main in [
        "",
        "<div role='status'>Laden der Anwendung...</div>",
        "<div aria-busy='true'>Loading application</div>",
    ] {
        let html = format!(
            "<script src='/app.js'></script><app-root><main>{main}</main><footer><p>{support}</p></footer></app-root>"
        );
        for rendered in [false, true] {
            assert!(
                matches!(
                    extract(
                        Url::parse("https://example.org/").unwrap(),
                        html.as_bytes(),
                        Some("text/html"),
                        None,
                        rendered
                    ),
                    Err(PageError::NeedsJs)
                ),
                "{main}, rendered={rendered}"
            );
        }
    }
    let html = "<script src='/app.js'></script><main><div hidden role='status'>Loading application</div><h1>Application loading guide</h1><p>This article explains loading application data and how to show a useful progress indicator while a request is pending. It is documentation, not an unfinished application.</p></main>";
    assert!(read(html).content.contains("progress indicator"));
}

#[test]
fn documentation_examples_do_not_require_client_rendering() {
    let prose = "Server-rendered documentation explains hydration and payload transfer with complete examples. ".repeat(12);
    let html = format!(
        r#"<script src='/analytics.js'></script><main><h1>Data fetching</h1><p>{prose}</p>
        <pre><code><span>{{{{</span><span> data </span><span>}}}}</span>
    {{{{ error }}}}
    {{{{ status }}}}</code></pre>
        <p>Inline examples: <code>{{{{ name }}}}</code>, <samp>{{{{ result }}}}</samp>, <kbd>{{{{ key }}}}</kbd>.</p>
        <pre><code><span ng-bind='one'>one</span><span ng-bind='two'>two</span><span ng-bind='three'>three</span><span ng-bind='four'>four</span><span ng-bind='five'>five</span></code></pre>
        </main>"#
    );
    let source = read(&html);
    assert!(source.content.contains("payload transfer"));
    assert!(source.content.contains("{{ data }}"));
    assert!(source.content.contains("\n    {{ error }}"));
}

#[test]
fn actual_template_bindings_still_require_rendering_beside_code_examples() {
    let prose =
        "A loading application may already contain substantial explanatory text. ".repeat(20);
    for pending in [
        "<p><span>{{</span><b>first</b><span>}}</span> {{ second }} {{ third }}</p>",
        "<div ng-app><div ng-controller><p ng-bind='one'></p><p ng-bind='two'></p><p ng-bind='three'></p></div></div>",
    ] {
        let html = format!(
            "<script src='/app.js'></script><main><p>{prose}</p><pre><code>{{{{ literal }}}}</code></pre>{pending}</main>"
        );
        assert!(matches!(
            extract(
                Url::parse("https://example.org/").unwrap(),
                html.as_bytes(),
                Some("text/html"),
                None,
                false
            ),
            Err(PageError::NeedsJs)
        ));
    }
}

#[test]
fn collapsed_accordions_and_tabpanels_preserve_content_while_keeping_hidden_traps_excluded() {
    let prose =
        "Official Swiss federal documentation explains the legal basis and publications in detail. "
            .repeat(6);
    let html = format!(
        r#"<body>
        <nav aria-hidden="true"><a href="/hidden-nav">Hidden nav item</a></nav>
        <main>
            <h1>Languages and VAT</h1>
            <p>{prose}</p>
            <p hidden>Secret hidden trap</p>
            <div style="display:none">Hidden promotional overlay</div>
            <div hidden role="status">Loading application</div>
            <ul class="accordion">
                <li class="accordion__item">
                    <button id="btn-laws" aria-expanded="false" aria-controls="drawer-laws">Lois</button>
                    <div id="drawer-laws" class="accordion__drawer" role="region" aria-labelledby="btn-laws">
                        <div class="accordion__content" style="visibility:hidden;">
                            <span aria-hidden="true">ICON_DECORATION</span>
                            <p>Loi fédérale sur les langues nationales (<a href="/rs-441">RS 441.1</a>)</p>
                        </div>
                    </div>
                </li>
                <li class="accordion__item">
                    <button id="btn-hours" aria-expanded="false" aria-controls="drawer-hours">Öffnungszeiten</button>
                    <div id="drawer-hours" class="accordion__drawer" role="region" aria-labelledby="btn-hours" aria-hidden="true">
                        <p>Montag bis Freitag 09.00–12.00 Uhr</p>
                    </div>
                </li>
            </ul>
            <div role="tabpanel" hidden>
                <p>Archived October 2026 release notes</p>
            </div>
        </main>
        </body>"#
    );

    let base = Url::parse("https://example.org/").unwrap();
    let doc = Html::parse_document(&html);
    let content = read(&html).content;
    let md = markdown::html_to_markdown(&doc, &base, &ExtractOptions::default(), 64 * 1024);
    let plain = clean::content_text(&doc, &base, false);

    for (label, output) in [("content", &content), ("markdown", &md), ("plain", &plain)] {
        for wanted in [
            "RS 441.1",
            "https://example.org/rs-441",
            "Montag bis Freitag 09.00–12.00 Uhr",
            "Archived October 2026 release notes",
        ] {
            assert!(
                output.contains(wanted),
                "{label} missing disclosure content {wanted:?}: {output}"
            );
        }
        for unwanted in [
            "Secret hidden trap",
            "Hidden promotional overlay",
            "Hidden nav item",
            "ICON_DECORATION",
            "Loading application",
        ] {
            assert!(
                !output.contains(unwanted),
                "{label} leaked hidden content {unwanted:?}: {output}"
            );
        }
    }
}

#[test]
fn content_emits_structured_markdown_with_clean_links_contacts_and_typography() {
    let html = r#"<main>
        <h1>Jahres&#173;bericht 2025</h1>
        <h2>Key&#8203; Takeaways</h2>
        <p>Compare <code>useFetch</code> and <strong>WAL mode</strong>.</p>
        <ul><li>First bullet</li><li>Second bullet</li></ul>
        <ol><li>Step one</li><li>Step two</li></ol>
        <pre><code class="language-rust">fn main() {
    let x = 1;
}</code></pre>
        <p><a href="/home"><div>Home</div></a></p>
        <p><a href="/learn">Learn SQLite</a><a href="/play">SQLite playground</a></p>
        <p><a href="tel:+41584624197">+41 58 462 41 97</a></p>
        <p><a href="tel:0041582211279">+41 58 221 12 79</a></p>
        <p><a href="mailto:info@example.org">info@example.org</a></p>
    </main>"#;
    let base = Url::parse("https://example.org/").unwrap();
    let doc = Html::parse_document(html);

    let content = read(html).content;
    assert!(content.contains("# Jahresbericht 2025"), "{content}");
    assert!(content.contains("## Key Takeaways"), "{content}");
    assert!(content.contains("`useFetch`"), "{content}");
    assert!(content.contains("**WAL mode**"), "{content}");
    assert!(
        content.contains("- First bullet\n- Second bullet"),
        "{content}"
    );
    assert!(content.contains("1. Step one\n2. Step two"), "{content}");
    assert!(
        content.contains("```rust\nfn main() {\n    let x = 1;\n}\n```"),
        "{content}"
    );
    assert!(
        content.contains("[Home](https://example.org/home)"),
        "{content}"
    );
    assert!(
        content.contains("[Learn SQLite](https://example.org/learn) [SQLite playground](https://example.org/play)"),
        "{content}"
    );
    assert!(
        !content.contains('\u{00ad}'),
        "soft hyphen leaked: {content}"
    );
    assert!(!content.contains('\u{200b}'), "zwsp leaked: {content}");

    // Plain text format (/api/scrape formats:["text"]) must also avoid glued/newline links & duplicate phones.
    let plain = clean::content_text(&doc, &base, false);
    assert!(plain.contains("Jahresbericht 2025"), "{plain}");
    assert!(!plain.contains('\u{00ad}'), "plain soft hyphen: {plain}");
    assert!(!plain.contains('\u{200b}'), "plain zwsp: {plain}");
    assert!(
        !plain.contains("Home\n (https://example.org/home)"),
        "orphan newline before url: {plain}"
    );
    assert!(
        !plain.contains("(https://example.org/learn)SQLite"),
        "glued links in plain text: {plain}"
    );
    assert!(
        !plain.contains("+41 58 462 41 97 (+41584624197)"),
        "duplicated phone number: {plain}"
    );
    assert!(
        !plain.contains("+41 58 221 12 79 (0041582211279)"),
        "duplicated 00-prefixed phone number: {plain}"
    );
    assert!(
        !plain.contains("info@example.org (info@example.org)"),
        "duplicated email: {plain}"
    );
}

#[test]
fn sidebars_site_menus_and_multilingual_skip_links_are_stripped_safely() {
    let html = r##"<body>
        <a href="#footer">Direkt zum Footer springen</a>
        <a href="#main-content">Überspringe den Hauptinhalt</a>
        <a href="#footer">Skip to footer</a>
        <div id="sidebar" class="sidebar">
            <a href="/ch1">1. Introduction</a>
            <a href="/ch2">2. Installation</a>
        </div>
        <div class="menu mainmenu"><a href="/about">About</a><a href="/download">Download</a></div>
        <div id="pgSideNav"><a href="/manuals">Quick Links</a></div>
        <div id="content">
            <h1>References and Borrowing</h1>
            <p>A reference lets you borrow a value without taking ownership of it.</p>
        </div>
    </body>"##;
    let content = read(html).content;
    for unwanted in [
        "Direkt zum Footer",
        "Überspringe den Hauptinhalt",
        "Skip to footer",
        "1. Introduction",
        "2. Installation",
        "Download",
        "Quick Links",
    ] {
        assert!(
            !content.contains(unwanted),
            "chrome leaked {unwanted:?}: {content}"
        );
    }
    assert!(content.contains("# References and Borrowing"), "{content}");
    assert!(content.contains("borrow a value"), "{content}");

    // Safety guard: never strip a container that holds the page heading or <main>.
    let guarded = r#"<div class="sidebar"><h1>Actual Article</h1><p>Important evidence inside misnamed wrapper.</p></div>"#;
    let kept = read(guarded).content;
    assert!(kept.contains("Actual Article"), "{kept}");
    assert!(kept.contains("Important evidence"), "{kept}");
}

#[test]
fn code_blocks_preserve_indentation_nested_fences_and_ignore_inner_link_markup() {
    let html = r#"<main>
        <h1>Code Example</h1>
        <pre><code class="language-markdown">```rust
    fn <a href="/doc/main">main</a>() {}
```</code></pre>
    </main>"#;
    let content = read(html).content;
    assert!(content.contains("````markdown\n"), "{content}");
    assert!(content.contains("\n    fn main() {}\n"), "{content}");
    assert!(
        !content.contains("[main]("),
        "link markdown injected inside pre: {content}"
    );
}

#[test]
fn same_host_origin_check_distinguishes_same_host_redirects_from_cross_host_hops() {
    let a = Url::parse("https://nuxt.com/docs/getting-started/data-fetching").unwrap();
    let b = Url::parse("https://nuxt.com/raw/docs/getting-started/data-fetching.md").unwrap();
    let c = Url::parse("http://nuxt.com/docs/getting-started/data-fetching").unwrap();
    let d = Url::parse("https://docs.nuxt.com/getting-started").unwrap();
    assert!(same_origin(&a, &b));
    assert!(!same_origin(&a, &c));
    assert!(!same_origin(&a, &d));
}

#[test]
fn linked_images_separate_from_adjacent_label_text_and_pdf_strips_soft_hyphens() {
    let html = r#"<main>
        <p><a href="/home"><img src="/logo.png" alt="Acme">Acme Home</a></p>
        <p><a href="/dot"><img src="/icon.png" alt="Icon">.</a></p>
    </main>"#;
    let content = read(html).content;
    assert!(
        content.contains(
            "[![Acme](https://example.org/logo.png) Acme Home](https://example.org/home)"
        ),
        "image and text glued inside link: {content}"
    );
    assert!(
        content.contains("[![Icon](https://example.org/icon.png).](https://example.org/dot)"),
        "punctuation after image should not gain extra space: {content}"
    );

    let cleaned = clean::strip_invisible_format_chars("Jahres\u{00ad}bericht\u{200b} 2025\u{feff}");
    assert_eq!(cleaned, "Jahresbericht 2025");
}

#[test]
fn render_concurrency_defaults_to_four_and_respects_bounds() {
    assert_eq!(clamp_render_concurrency(0), 1);
    assert_eq!(clamp_render_concurrency(4), 4);
    assert_eq!(clamp_render_concurrency(99), 16);
    assert_eq!(DEFAULT_RENDER_CONCURRENCY, 4);
}

#[test]
fn static_product_catalog_with_many_short_articles_does_not_require_js() {
    let cards: String = (1..=20)
        .map(|i| {
            format!(
                r#"<article class="product_pod"><h3><a href="/book-{i}">Book Title Number {i}</a></h3><p class="price_color">£{i}.99</p><p class="instock">In stock</p></article>"#
            )
        })
        .collect();
    let html = format!(
        r#"<html><body><script src="/static/jquery.min.js"></script>
        <div class="page"><h1>All products</h1><section>{cards}</section></div></body></html>"#
    );
    let source = read(&html);
    assert!(
        source.content.contains("Book Title Number 1"),
        "{}",
        source.content
    );
    assert!(
        source.content.contains("Book Title Number 20"),
        "{}",
        source.content
    );
    assert!(source.content.contains("£20.99"), "{}", source.content);
}

#[test]
fn nested_layout_tables_do_not_escape_literal_pipes() {
    let html = r#"<table><tr><td><table><tr>
        <td><b><a href="/news">Hacker News</a></b> <a href="/newest">new</a> | <a href="/past">past</a></td>
    </tr></table></td></tr></table>"#;
    let content = read(html).content;
    assert!(
        content.contains("[new](https://example.org/newest) | [past](https://example.org/past)"),
        "{content}"
    );
    assert!(
        !content.contains(r"\|"),
        "escaped pipe leaked in layout table: {content}"
    );
}
