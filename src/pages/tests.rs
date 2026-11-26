use super::*;

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
    assert!(text.starts_with("A\nFirst\nSecond"));
    assert_eq!(text.matches('3').count(), 2);
    assert_eq!(text.matches("élan").count(), 2000);
    assert!(text.ends_with("End"));
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
