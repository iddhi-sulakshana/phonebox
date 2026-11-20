//! Port of `RequestTimings.cs`: how long the agent takes to answer each kind of request, capped so it never grows.

use std::sync::Mutex;

use indexmap::IndexMap;
use serde::Serialize;

pub const SLOW_MS: i64 = 300;
pub const OTHER_ROUTE: &str = "(other)";

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub route: String,
    pub count: i64,
    pub p50_ms: i64,
    pub p95_ms: i64,
    pub max_ms: i64,
    pub slow: i64,
}

pub struct RequestTimings {
    keep: usize,
    max_routes: usize,
    routes: Mutex<IndexMap<String, Series>>,
}

struct Series {
    count: i64,
    slow: i64,
    max: i64,
    recent: Vec<i64>,
    next: usize,
}

impl RequestTimings {
    pub fn new(keep: usize, max_routes: usize) -> Self {
        RequestTimings {
            keep,
            max_routes,
            routes: Mutex::new(IndexMap::new()),
        }
    }

    pub fn add(&self, route: &str, ms: i64) {
        let mut routes = self.routes.lock().unwrap_or_else(|e| e.into_inner());
        // the table is capped: a new route past the cap is counted with the others
        let route = if !routes.contains_key(route)
            && routes.len() >= self.max_routes
            && route != OTHER_ROUTE
        {
            OTHER_ROUTE
        } else {
            route
        };
        let keep = self.keep;
        let s = routes.entry(route.to_string()).or_insert_with(|| Series {
            count: 0,
            slow: 0,
            max: 0,
            recent: Vec::with_capacity(keep),
            next: 0,
        });
        s.count += 1;
        if ms >= SLOW_MS {
            s.slow += 1;
        }
        s.max = s.max.max(ms);
        if s.recent.len() < keep {
            s.recent.push(ms);
        } else {
            s.recent[s.next] = ms;
        }
        s.next = (s.next + 1) % keep;
    }

    pub fn summary(&self) -> Vec<Row> {
        let routes = self.routes.lock().unwrap_or_else(|e| e.into_inner());
        let mut rows: Vec<Row> = routes
            .iter()
            .map(|(route, s)| {
                let mut sorted = s.recent.clone();
                sorted.sort_unstable();
                let n = sorted.len();
                let at = |q: f64| {
                    if n == 0 {
                        0
                    } else {
                        sorted[((q * (n - 1) as f64 + 0.5).floor() as usize).min(n - 1)]
                    }
                };
                // the median of an even count is the mean of the middle two
                let p50 = if n > 0 && n % 2 == 0 {
                    (sorted[n / 2 - 1] + sorted[n / 2]) / 2
                } else {
                    at(0.5)
                };
                Row {
                    route: route.clone(),
                    count: s.count,
                    p50_ms: p50,
                    p95_ms: at(0.95),
                    max_ms: s.max,
                    slow: s.slow,
                }
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.count)); // stable, like OrderByDescending
        rows
    }
}

impl Default for RequestTimings {
    fn default() -> Self {
        Self::new(200, 64)
    }
}

/// "POST /api/approvals/{id}": the method and the matched route pattern; None for a request that matched no route.
pub fn route_of(matched: Option<&str>, method: &str) -> Option<String> {
    matched.map(|pattern| format!("{method} {pattern}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row<'a>(rows: &'a [Row], route: &str) -> &'a Row {
        rows.iter().find(|r| r.route == route).unwrap()
    }

    #[test]
    fn summary_gives_count_median_slowest_and_the_slow_share() {
        let t = RequestTimings::default();
        for ms in [10, 20, 30, 40, 50, 60, 70, 80, 90, 1000] {
            t.add("POST /api/volume", ms);
        }
        t.add("GET /api/state", 5);
        let rows = t.summary();
        let v = row(&rows, "POST /api/volume");
        assert_eq!((v.count, v.p50_ms, v.max_ms, v.slow), (10, 55, 1000, 1));
        assert_eq!(row(&rows, "GET /api/state").count, 1);
    }

    #[test]
    fn only_the_latest_requests_of_a_route_are_kept() {
        let t = RequestTimings::new(5, 64);
        for i in 1..=20 {
            t.add("POST /api/volume", i * 100);
        }
        let r = &t.summary()[0];
        assert_eq!((r.count, r.max_ms, r.p50_ms), (20, 2000, 1800));
    }

    #[test]
    fn distinct_routes_are_capped_and_the_rest_share_one_row() {
        let t = RequestTimings::new(5, 3);
        for r in ["GET /a", "GET /b", "GET /c"] {
            t.add(r, 10);
        }
        for i in 0..100 {
            t.add(&format!("GET /junk{i}"), 20);
        }
        let rows = t.summary();
        assert_eq!(rows.len(), 4);
        assert_eq!(row(&rows, OTHER_ROUTE).count, 100);
        t.add("GET /a", 10);
        assert_eq!(row(&t.summary(), "GET /a").count, 2);
    }

    #[test]
    fn only_a_request_that_matched_an_endpoint_has_a_route() {
        assert_eq!(
            route_of(Some("/api/approvals/{id}"), "POST").as_deref(),
            Some("POST /api/approvals/{id}")
        );
        assert_eq!(route_of(None, "GET"), None);
    }
}
