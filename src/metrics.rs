//! Auth-protected production observability endpoints:
//! - `GET /metrics`: Prometheus text exposition format (0.0.4)
//! - `GET /api/status`: Structured JSON snapshot of engines, caches, hosts, CDP, and slots

use std::{fmt::Write, sync::Arc};

use axum::{
    Json,
    body::Body,
    extract::State,
    http::{Request, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use subtle::ConstantTimeEq;

use crate::{App, discovery::EngineMetricSnapshot, pages, politeness, problem};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheTierStats {
    pub memory_hits: u64,
    pub disk_hits: u64,
    pub misses: u64,
    pub entries: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    pub tracked: usize,
    pub open_circuits: usize,
    pub slow_hosts: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotStatus {
    pub api_available: usize,
    pub page_available: usize,
    pub crawl_available: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CdpStatus {
    pub pool_enabled: bool,
    pub subresource_ssrf_blocks: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CrawlStatusSummary {
    pub running: usize,
    pub stored: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusSnapshot {
    pub uptime_seconds: u64,
    pub discovery_mode: &'static str,
    pub slots: SlotStatus,
    pub discovery_cache: CacheTierStats,
    pub page_cache: CacheTierStats,
    pub hosts: HostStatus,
    pub cdp: CdpStatus,
    pub crawls: CrawlStatusSummary,
    pub engines: Vec<EngineMetricSnapshot>,
}

fn authorized(app: &App, req: &Request<Body>) -> bool {
    let expected = app.authorization.as_bytes();
    req.headers()
        .get(header::AUTHORIZATION)
        .is_some_and(|value| bool::from(value.as_bytes().ct_eq(expected)))
}

fn build_snapshot(app: &App) -> StatusSnapshot {
    let (disc_mem, disc_disk, disc_miss, disc_entries) = app.search.engines.cache_stats();
    let (page_mem, page_disk, page_miss) = politeness::global().cache_stats();
    let (tracked, open_circuits, slow_hosts, page_entries) = politeness::global().host_summary();
    let (running, stored, crawl_available) = app.crawls.summary();

    StatusSnapshot {
        uptime_seconds: app.started.elapsed().as_secs(),
        discovery_mode: app.search.engines.mode_name(),
        slots: SlotStatus {
            api_available: app.slots.available_permits(),
            page_available: app.search.page_slots.available_permits(),
            crawl_available,
        },
        discovery_cache: CacheTierStats {
            memory_hits: disc_mem,
            disk_hits: disc_disk,
            misses: disc_miss,
            entries: disc_entries,
        },
        page_cache: CacheTierStats {
            memory_hits: page_mem,
            disk_hits: page_disk,
            misses: page_miss,
            entries: page_entries,
        },
        hosts: HostStatus {
            tracked,
            open_circuits,
            slow_hosts,
        },
        cdp: CdpStatus {
            pool_enabled: pages::cdp_pool_enabled(),
            subresource_ssrf_blocks: pages::cdp_subresource_blocks(),
        },
        crawls: CrawlStatusSummary { running, stored },
        engines: app.search.engines.engine_metrics(),
    }
}

pub(crate) fn render_prometheus(s: &StatusSnapshot) -> String {
    let mut out = String::with_capacity(2048);

    let _ = writeln!(
        out,
        "# HELP quelle_uptime_seconds Service uptime in seconds."
    );
    let _ = writeln!(out, "# TYPE quelle_uptime_seconds gauge");
    let _ = writeln!(out, "quelle_uptime_seconds {}", s.uptime_seconds);

    let _ = writeln!(
        out,
        "# HELP quelle_slots_available Available concurrency permits."
    );
    let _ = writeln!(out, "# TYPE quelle_slots_available gauge");
    let _ = writeln!(
        out,
        "quelle_slots_available{{pool=\"api\"}} {}",
        s.slots.api_available
    );
    let _ = writeln!(
        out,
        "quelle_slots_available{{pool=\"page\"}} {}",
        s.slots.page_available
    );
    let _ = writeln!(
        out,
        "quelle_slots_available{{pool=\"crawl\"}} {}",
        s.slots.crawl_available
    );

    let _ = writeln!(
        out,
        "# HELP quelle_cache_events_total Cache lookups broken down by store and tier."
    );
    let _ = writeln!(out, "# TYPE quelle_cache_events_total counter");
    let _ = writeln!(
        out,
        "quelle_cache_events_total{{cache=\"discovery\",tier=\"memory\"}} {}",
        s.discovery_cache.memory_hits
    );
    let _ = writeln!(
        out,
        "quelle_cache_events_total{{cache=\"discovery\",tier=\"disk\"}} {}",
        s.discovery_cache.disk_hits
    );
    let _ = writeln!(
        out,
        "quelle_cache_events_total{{cache=\"discovery\",tier=\"miss\"}} {}",
        s.discovery_cache.misses
    );
    let _ = writeln!(
        out,
        "quelle_cache_events_total{{cache=\"pages\",tier=\"memory\"}} {}",
        s.page_cache.memory_hits
    );
    let _ = writeln!(
        out,
        "quelle_cache_events_total{{cache=\"pages\",tier=\"disk\"}} {}",
        s.page_cache.disk_hits
    );
    let _ = writeln!(
        out,
        "quelle_cache_events_total{{cache=\"pages\",tier=\"miss\"}} {}",
        s.page_cache.misses
    );

    let _ = writeln!(out, "# HELP quelle_cache_entries In-memory cache entries.");
    let _ = writeln!(out, "# TYPE quelle_cache_entries gauge");
    let _ = writeln!(
        out,
        "quelle_cache_entries{{cache=\"discovery\"}} {}",
        s.discovery_cache.entries
    );
    let _ = writeln!(
        out,
        "quelle_cache_entries{{cache=\"pages\"}} {}",
        s.page_cache.entries
    );

    let _ = writeln!(
        out,
        "# HELP quelle_hosts_tracked Distinct origin hosts tracked."
    );
    let _ = writeln!(out, "# TYPE quelle_hosts_tracked gauge");
    let _ = writeln!(out, "quelle_hosts_tracked {}", s.hosts.tracked);
    let _ = writeln!(
        out,
        "# HELP quelle_hosts_circuit_open Hosts currently blocked by circuit breaker."
    );
    let _ = writeln!(out, "# TYPE quelle_hosts_circuit_open gauge");
    let _ = writeln!(out, "quelle_hosts_circuit_open {}", s.hosts.open_circuits);
    let _ = writeln!(
        out,
        "# HELP quelle_hosts_slow_ewma Hosts with EWMA latency >= 6000ms."
    );
    let _ = writeln!(out, "# TYPE quelle_hosts_slow_ewma gauge");
    let _ = writeln!(out, "quelle_hosts_slow_ewma {}", s.hosts.slow_hosts);

    let _ = writeln!(
        out,
        "# HELP quelle_cdp_subresource_ssrf_blocks_total Chromium CDP subresource/redirect requests blocked by SSRF guard."
    );
    let _ = writeln!(
        out,
        "# TYPE quelle_cdp_subresource_ssrf_blocks_total counter"
    );
    let _ = writeln!(
        out,
        "quelle_cdp_subresource_ssrf_blocks_total {}",
        s.cdp.subresource_ssrf_blocks
    );

    let _ = writeln!(
        out,
        "# HELP quelle_crawls_running Active background crawl jobs."
    );
    let _ = writeln!(out, "# TYPE quelle_crawls_running gauge");
    let _ = writeln!(out, "quelle_crawls_running {}", s.crawls.running);
    let _ = writeln!(
        out,
        "# HELP quelle_crawls_stored Stored crawl job snapshots."
    );
    let _ = writeln!(out, "# TYPE quelle_crawls_stored gauge");
    let _ = writeln!(out, "quelle_crawls_stored {}", s.crawls.stored);

    let _ = writeln!(
        out,
        "# HELP quelle_engine_requests_total Search engine calls."
    );
    let _ = writeln!(out, "# TYPE quelle_engine_requests_total counter");
    let _ = writeln!(
        out,
        "# HELP quelle_engine_hits_total Search engine candidates returned."
    );
    let _ = writeln!(out, "# TYPE quelle_engine_hits_total counter");
    let _ = writeln!(
        out,
        "# HELP quelle_engine_errors_total Search engine errors."
    );
    let _ = writeln!(out, "# TYPE quelle_engine_errors_total counter");
    let _ = writeln!(
        out,
        "# HELP quelle_engine_rate_limits_total Search engine 429/throttle events."
    );
    let _ = writeln!(out, "# TYPE quelle_engine_rate_limits_total counter");
    let _ = writeln!(
        out,
        "# HELP quelle_engine_latency_ms_sum Cumulative search engine latency in ms."
    );
    let _ = writeln!(out, "# TYPE quelle_engine_latency_ms_sum counter");
    let _ = writeln!(
        out,
        "# HELP quelle_engine_cooldown_remaining_seconds Active engine cooldown seconds."
    );
    let _ = writeln!(out, "# TYPE quelle_engine_cooldown_remaining_seconds gauge");

    for eng in &s.engines {
        let name = eng.name;
        let _ = writeln!(
            out,
            "quelle_engine_requests_total{{engine=\"{name}\"}} {}",
            eng.requests
        );
        let _ = writeln!(
            out,
            "quelle_engine_hits_total{{engine=\"{name}\"}} {}",
            eng.hits
        );
        let _ = writeln!(
            out,
            "quelle_engine_errors_total{{engine=\"{name}\"}} {}",
            eng.errors
        );
        let _ = writeln!(
            out,
            "quelle_engine_rate_limits_total{{engine=\"{name}\"}} {}",
            eng.rate_limits
        );
        let _ = writeln!(
            out,
            "quelle_engine_latency_ms_sum{{engine=\"{name}\"}} {}",
            eng.latency_ms_sum
        );
        let _ = writeln!(
            out,
            "quelle_engine_cooldown_remaining_seconds{{engine=\"{name}\"}} {}",
            eng.cooldown_remaining_secs
        );
    }

    out
}

pub async fn prometheus_handle(State(app): State<Arc<App>>, req: Request<Body>) -> Response {
    if !authorized(&app, &req) {
        return problem(StatusCode::UNAUTHORIZED, "backend_authentication_required");
    }
    let body = render_prometheus(&build_snapshot(&app));
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

pub async fn status_handle(State(app): State<Arc<App>>, req: Request<Body>) -> Response {
    if !authorized(&app, &req) {
        return problem(StatusCode::UNAUTHORIZED, "backend_authentication_required");
    }
    Json(build_snapshot(&app)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prometheus_exposition_renders_all_core_and_per_engine_metrics() {
        let snap = StatusSnapshot {
            uptime_seconds: 42,
            discovery_mode: "fuse",
            slots: SlotStatus {
                api_available: 8,
                page_available: 32,
                crawl_available: 4,
            },
            discovery_cache: CacheTierStats {
                memory_hits: 5,
                disk_hits: 3,
                misses: 2,
                entries: 7,
            },
            page_cache: CacheTierStats {
                memory_hits: 11,
                disk_hits: 4,
                misses: 6,
                entries: 9,
            },
            hosts: HostStatus {
                tracked: 12,
                open_circuits: 1,
                slow_hosts: 2,
            },
            cdp: CdpStatus {
                pool_enabled: true,
                subresource_ssrf_blocks: 3,
            },
            crawls: CrawlStatusSummary {
                running: 1,
                stored: 4,
            },
            engines: vec![EngineMetricSnapshot {
                name: "pubmed",
                primary: false,
                requests: 10,
                hits: 45,
                empty: 1,
                errors: 0,
                rate_limits: 0,
                latency_ms_sum: 2150,
                cooldown_remaining_secs: 0,
            }],
        };

        let text = render_prometheus(&snap);
        assert!(text.contains("quelle_uptime_seconds 42"));
        assert!(text.contains("quelle_cache_events_total{cache=\"discovery\",tier=\"disk\"} 3"));
        assert!(text.contains("quelle_cache_events_total{cache=\"pages\",tier=\"memory\"} 11"));
        assert!(text.contains("quelle_hosts_circuit_open 1"));
        assert!(text.contains("quelle_cdp_subresource_ssrf_blocks_total 3"));
        assert!(text.contains("quelle_engine_requests_total{engine=\"pubmed\"} 10"));
        assert!(text.contains("quelle_engine_hits_total{engine=\"pubmed\"} 45"));
        assert!(text.contains("quelle_engine_latency_ms_sum{engine=\"pubmed\"} 2150"));
    }
}
