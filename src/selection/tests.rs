use super::*;

fn candidate(url: &str, title: &str) -> Candidate {
    Candidate {
        url: Url::parse(url).unwrap(),
        title: title.into(),
        description: title.into(),
    }
}

#[test]
fn canonical_keys_strip_tracking_but_keep_semantic_parameters() {
    assert_eq!(
        url_key(&Url::parse("https://example.org/a?utm_source=x&id=1#top").unwrap()),
        "https://example.org/a?id=1"
    );
    assert_ne!(
        url_key(&Url::parse("https://example.org/a?version=2024").unwrap()),
        url_key(&Url::parse("https://example.org/a?version=2025").unwrap())
    );
}

#[test]
fn requested_versions_and_primary_domains_beat_generic_popularity() {
    let hits = vec![
        candidate(
            "https://postgresql.org/docs/current/transaction-iso.html",
            "Transaction isolation",
        ),
        candidate(
            "https://postgresql.org/docs/13/transaction-iso.html",
            "Transaction isolation",
        ),
        candidate(
            "https://vendor.example/blog",
            "PostgreSQL 13 transaction isolation",
        ),
    ];
    let ranked = order(
        "PostgreSQL 13 transaction isolation",
        Language::En,
        hits,
        &["postgresql.org".into()],
    );
    assert!(ranked[0].url.path().contains("/13/"));
}

#[test]
fn diversity_does_not_delete_other_primary_documents() {
    let hits = vec![
        candidate("https://docs.example.org/a", "Introduction"),
        candidate("https://docs.example.org/a?utm_source=x", "Introduction"),
        candidate("https://docs.example.org/b", "Evidence"),
        candidate("https://other.example.org/c", "Evidence"),
    ];
    let ranked = order("Evidence", Language::En, hits, &[]);
    assert_eq!(ranked.len(), 3);
    assert!(ranked.iter().any(|h| h.url.path() == "/b"));
}

#[test]
fn identical_content_is_duplicate_even_through_aliases() {
    let mut seen = EvidenceSet::default();
    let a = Url::parse("https://one.example/a").unwrap();
    let b = Url::parse("https://two.example/b").unwrap();
    let text = "A substantive document with evidence and several facts. ".repeat(30);
    assert!(seen.insert(&a, &text));
    assert!(!seen.insert(&b, &text));
    assert!(seen.insert(&b, "A distinct document addressing a different question."));
}

#[test]
fn preferred_domains_are_boundary_checked_and_followups_are_targeted() {
    let hosts = vec!["admin.ch".into()];
    assert!(matches_domain(
        &Url::parse("https://kmu.admin.ch/a").unwrap(),
        &hosts
    ));
    assert!(!matches_domain(
        &Url::parse("https://admin.ch.evil.org/a").unwrap(),
        &hosts
    ));
    let follow = followup("GmbH capital", &hosts).unwrap();
    assert!(follow.contains("site:admin.ch"));
    assert!(followup("GmbH capital", &[]).is_none());
}

#[test]
fn bm25f_and_trigram_cosine_prioritize_title_and_rare_compound_matches() {
    let hits = vec![
        Candidate {
            url: Url::parse("https://generic.example.org/overview").unwrap(),
            title: "General Swiss Guide".into(),
            description: "Overview of general rules in Switzerland for many topics and taxes."
                .repeat(6),
        },
        Candidate {
            url: Url::parse("https://tax.example.org/mwst-steuersatz").unwrap(),
            title: "Mehrwertsteuersatz Schweiz 2024".into(),
            description: "Aktuelle MWST Normalsatz und Sondersatz Regelungen.".into(),
        },
    ];
    let ranked = order("Schweiz MWST Steuersätze", Language::De, hits, &[]);
    assert_eq!(
        ranked[0].url.as_str(),
        "https://tax.example.org/mwst-steuersatz",
        "BM25F + char-trigram cosine should rank compound-word & title match ahead of noisy long snippet"
    );
    assert!(char_trigram_cosine("Steuersätze", "Mehrwertsteuersatz") > 0.35);
}
