//! robots.txt parser (Allow/Disallow/Crawl-delay/Sitemap) with per-host caching,
//! plus bounded recursive sitemap.xml discovery.

use std::{
    collections::{HashMap, HashSet},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use regex::Regex;
use tokio::time::Instant as TokioInstant;
use url::Url;

use crate::{pages, politeness};

const ROBOTS_CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const ROBOTS_MAX_BYTES: usize = 512 * 1024;
const SITEMAP_MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_SITEMAP_FILES: usize = 12;

#[derive(Clone, Debug)]
struct Rule {
    allow: bool,
    pattern: String,
}

#[derive(Clone, Debug, Default)]
pub struct RobotsPolicy {
    rules: Vec<Rule>,
    pub crawl_delay: Option<Duration>,
    pub sitemaps: Vec<Url>,
}

impl RobotsPolicy {
    pub fn is_allowed(&self, url: &Url) -> bool {
        let mut target = url.path().to_owned();
        if let Some(q) = url.query() {
            target.push('?');
            target.push_str(q);
        }
        // RFC 9309: longest matching rule wins; Allow wins ties.
        let mut best: Option<(usize, bool)> = None;
        for rule in &self.rules {
            if rule.pattern.is_empty() {
                continue;
            }
            if matches_robots_pattern(&target, &rule.pattern) {
                let len = rule.pattern.len();
                match best {
                    None => best = Some((len, rule.allow)),
                    Some((best_len, best_allow))
                        if len > best_len || (len == best_len && rule.allow && !best_allow) =>
                    {
                        best = Some((len, rule.allow));
                    }
                    _ => {}
                }
            }
        }
        best.is_none_or(|(_, allow)| allow)
    }

    pub fn parse(text: &str, base: &Url) -> Self {
        let mut sitemaps = Vec::new();
        let mut wildcard_rules = Vec::new();
        let mut specific_rules = Vec::new();
        let mut wildcard_delay: Option<Duration> = None;
        let mut specific_delay: Option<Duration> = None;

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Group {
            None,
            Wildcard,
            Specific,
            Other,
        }

        let mut group = Group::None;
        let mut saw_rule_in_group = false;

        for raw_line in text.lines() {
            let line = raw_line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((field, value)) = line.split_once(':') else {
                continue;
            };
            let field = field.trim().to_ascii_lowercase();
            let value = value.trim();

            if field == "sitemap" {
                if let Ok(u) = base.join(value)
                    && let Ok(clean) = pages::public_url(u)
                    && !sitemaps.contains(&clean)
                {
                    sitemaps.push(clean);
                }
                continue;
            }

            if field == "user-agent" {
                if saw_rule_in_group {
                    group = Group::None;
                    saw_rule_in_group = false;
                }
                let ua = value.to_ascii_lowercase();
                if ua == "quelle" || ua.starts_with("quelle/") {
                    group = Group::Specific;
                } else if ua == "*" && group != Group::Specific {
                    group = Group::Wildcard;
                } else if group == Group::None {
                    group = Group::Other;
                }
                continue;
            }

            saw_rule_in_group = true;
            let (rules, delay_slot) = match group {
                Group::Specific => (&mut specific_rules, &mut specific_delay),
                Group::Wildcard => (&mut wildcard_rules, &mut wildcard_delay),
                _ => continue,
            };

            match field.as_str() {
                "disallow" if !value.is_empty() => rules.push(Rule {
                    allow: false,
                    pattern: value.to_owned(),
                }),
                "allow" if !value.is_empty() => rules.push(Rule {
                    allow: true,
                    pattern: value.to_owned(),
                }),
                "crawl-delay" => {
                    if let Ok(secs) = value.parse::<f64>()
                        && secs.is_finite()
                        && secs > 0.0
                    {
                        *delay_slot = Some(Duration::from_secs_f64(secs.min(10.0)));
                    }
                }
                _ => {}
            }
        }

        let (rules, crawl_delay) = if !specific_rules.is_empty() || specific_delay.is_some() {
            (specific_rules, specific_delay.or(wildcard_delay))
        } else {
            (wildcard_rules, wildcard_delay)
        };

        Self {
            rules,
            crawl_delay,
            sitemaps,
        }
    }
}

fn matches_robots_pattern(path: &str, pattern: &str) -> bool {
    let end_anchor = pattern.ends_with('$');
    let pat = pattern.strip_suffix('$').unwrap_or(pattern);
    if !pat.contains('*') {
        return if end_anchor {
            path == pat
        } else {
            path.starts_with(pat)
        };
    }
    let parts: Vec<&str> = pat.split('*').collect();
    let mut pos = 0usize;
    for (idx, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if idx == 0 {
            if !path.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if let Some(found) = path[pos..].find(part) {
            pos += found + part.len();
        } else {
            return false;
        }
    }
    if end_anchor {
        if let Some(last) = parts.last() {
            if last.is_empty() {
                true
            } else {
                path.ends_with(last)
            }
        } else {
            true
        }
    } else {
        true
    }
}

static ROBOTS_CACHE: OnceLock<Mutex<HashMap<String, (Instant, RobotsPolicy)>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<String, (Instant, RobotsPolicy)>> {
    ROBOTS_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Fetches and caches `/robots.txt` for the given URL's origin. Missing or
/// unreadable robots.txt defaults to an allow-all policy.
pub async fn fetch_robots(url: &Url, deadline: TokioInstant) -> RobotsPolicy {
    let origin = format!(
        "{}://{}",
        url.scheme(),
        url.host_str().unwrap_or_default().to_ascii_lowercase()
    );
    if let Ok(guard) = cache().lock()
        && let Some((stored, policy)) = guard.get(&origin)
        && stored.elapsed() <= ROBOTS_CACHE_TTL
    {
        return policy.clone();
    }

    let Ok(robots_url) = Url::parse(&format!("{origin}/robots.txt")) else {
        return RobotsPolicy::default();
    };
    let fetch_deadline = deadline.min(TokioInstant::now() + Duration::from_secs(6));
    let policy =
        match pages::download_raw(robots_url.clone(), ROBOTS_MAX_BYTES, fetch_deadline).await {
            Ok((_, _, _, bytes)) => {
                let text = String::from_utf8_lossy(&bytes);
                RobotsPolicy::parse(&text, &robots_url)
            }
            Err(_) => RobotsPolicy::default(),
        };

    if let Some(delay) = policy.crawl_delay
        && let Some(host) = url.host_str()
    {
        politeness::global().set_crawl_delay(host, delay);
    }

    if let Ok(mut guard) = cache().lock() {
        guard.retain(|_, (t, _)| t.elapsed() <= ROBOTS_CACHE_TTL);
        if guard.len() >= 512 {
            guard.clear();
        }
        guard.insert(origin, (Instant::now(), policy.clone()));
    }
    policy
}

/// Extracts URLs from a site's sitemaps (from robots.txt `Sitemap:` directives
/// or fallback `/sitemap.xml`), recursively following sitemap indexes up to
/// `MAX_SITEMAP_FILES`.
pub async fn discover_sitemap_urls(
    seed: &Url,
    robots: &RobotsPolicy,
    limit: usize,
    deadline: TokioInstant,
) -> Vec<Url> {
    static LOC: OnceLock<Regex> = OnceLock::new();
    let loc_re = LOC.get_or_init(|| {
        Regex::new(r"(?is)<loc>\s*(?:<!\[CDATA\[)?(.*?)(?:\]\]>)?\s*</loc>").expect("loc regex")
    });

    let mut queue: Vec<Url> = if !robots.sitemaps.is_empty() {
        robots.sitemaps.clone()
    } else if let Ok(default_sm) = Url::parse(&format!(
        "{}://{}/sitemap.xml",
        seed.scheme(),
        seed.host_str().unwrap_or_default()
    )) {
        vec![default_sm]
    } else {
        vec![]
    };

    let mut visited_sitemaps = HashSet::new();
    let mut urls = Vec::new();
    let mut seen_urls = HashSet::new();

    while let Some(sm_url) = queue.pop() {
        if visited_sitemaps.len() >= MAX_SITEMAP_FILES || urls.len() >= limit {
            break;
        }
        if TokioInstant::now() >= deadline {
            break;
        }
        if !visited_sitemaps.insert(sm_url.clone()) {
            continue;
        }
        let step_deadline = deadline.min(TokioInstant::now() + Duration::from_secs(8));
        let Ok((_, _, _, bytes)) =
            pages::download_raw(sm_url.clone(), SITEMAP_MAX_BYTES, step_deadline).await
        else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let is_index = text.to_ascii_lowercase().contains("<sitemapindex");

        for cap in loc_re.captures_iter(&text) {
            let raw = cap[1]
                .trim()
                .replace("&amp;", "&")
                .replace("&lt;", "<")
                .replace("&gt;", ">");
            let Ok(parsed) = Url::parse(&raw) else {
                continue;
            };
            let Ok(clean) = pages::public_url(parsed) else {
                continue;
            };
            if is_index {
                if !visited_sitemaps.contains(&clean) && queue.len() < MAX_SITEMAP_FILES * 2 {
                    queue.push(clean);
                }
            } else if seen_urls.insert(clean.clone()) {
                urls.push(clean);
                if urls.len() >= limit {
                    break;
                }
            }
        }
    }

    urls
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_robots_allow_disallow_crawl_delay_and_sitemaps() {
        let txt = r#"
            User-agent: *
            Disallow: /private/
            Allow: /private/public-page
            Disallow: /*.json$
            Crawl-delay: 1.5
            Sitemap: https://example.org/sitemap.xml
        "#;
        let base = Url::parse("https://example.org/robots.txt").unwrap();
        let policy = RobotsPolicy::parse(txt, &base);
        assert_eq!(policy.crawl_delay, Some(Duration::from_millis(1500)));
        assert_eq!(policy.sitemaps.len(), 1);
        assert!(policy.is_allowed(&Url::parse("https://example.org/docs").unwrap()));
        assert!(!policy.is_allowed(&Url::parse("https://example.org/private/secret").unwrap()));
        assert!(policy.is_allowed(&Url::parse("https://example.org/private/public-page").unwrap()));
        assert!(!policy.is_allowed(&Url::parse("https://example.org/api/data.json").unwrap()));
    }
}
