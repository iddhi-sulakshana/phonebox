//! Outbound HTTP building blocks for the integrations: a resolver that hands the connector only vetted addresses (the C#
//! clients' `ConnectCallback`s: DNS is resolved there and the socket connects to the vetted IP) and error texts that
//! never carry the request's URL (feed, calendar and Home Assistant addresses are secrets, and the phone shows these
//! texts).
//!
//! reqwest never asks a resolver about an IP-literal host: a client that must refuse private literals checks them
//! itself before each request (as `FeedHttp.GetAsync` does per hop).

use std::error::Error;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

/// Why the resolver gave the connector no address: the client's own text (C# threw it from its connect callback).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for Refused {}

/// Which of the vetted addresses the connector may try.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// The first allowed address in DNS order (`FeedHttp`: no fallback).
    First,
    /// Every allowed address, IPv4 first (`WidgetFeedProxy`: each one tried in turn).
    Ipv4First,
}

type Lookup = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send>>
        + Send
        + Sync,
>;

/// A reqwest resolver that keeps only the addresses `allow` accepts.
#[derive(Clone)]
pub struct FilteredResolver {
    allow: Arc<dyn Fn(IpAddr) -> bool + Send + Sync>,
    refusal: Arc<dyn Fn(&str) -> String + Send + Sync>,
    pick: Pick,
    lookup: Lookup,
}

impl FilteredResolver {
    /// `refusal(host)`: the text when no address is allowed.
    pub fn new(
        allow: impl Fn(IpAddr) -> bool + Send + Sync + 'static,
        refusal: impl Fn(&str) -> String + Send + Sync + 'static,
        pick: Pick,
    ) -> FilteredResolver {
        FilteredResolver {
            allow: Arc::new(allow),
            refusal: Arc::new(refusal),
            pick,
            lookup: Arc::new(|host| {
                Box::pin(async move {
                    // Dns.GetHostAddressesAsync: every address, in the order the system gives them
                    let found = tokio::net::lookup_host((host.as_str(), 0)).await?;
                    Ok(found.map(|a| a.ip()).collect())
                })
            }),
        }
    }

    /// The same resolver over another name lookup (tests).
    pub fn with_lookup(
        mut self,
        lookup: impl Fn(String) -> Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send>>
        + Send
        + Sync
        + 'static,
    ) -> FilteredResolver {
        self.lookup = Arc::new(lookup);
        self
    }

    /// The addresses the connector gets for `host`: resolved, filtered and picked; none allowed is [`Refused`].
    pub async fn addresses(&self, host: &str) -> Result<Vec<IpAddr>, Box<dyn Error + Send + Sync>> {
        let resolved = (self.lookup)(host.to_string()).await?;
        let allowed = resolved.into_iter().filter(|ip| (self.allow)(*ip));
        let picked: Vec<IpAddr> = match self.pick {
            Pick::First => allowed.take(1).collect(),
            Pick::Ipv4First => {
                let (v4, v6): (Vec<IpAddr>, Vec<IpAddr>) = allowed.partition(IpAddr::is_ipv4);
                v4.into_iter().chain(v6).collect()
            }
        };
        if picked.is_empty() {
            return Err(Box::new(Refused((self.refusal)(host))));
        }
        Ok(picked)
    }
}

impl reqwest::dns::Resolve for FilteredResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let me = self.clone();
        let host = name.as_str().to_string();
        Box::pin(async move {
            let ips = me.addresses(&host).await?;
            let addrs: reqwest::dns::Addrs =
                Box::new(ips.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

/// The resolver's refusal somewhere in `err`'s chain.
pub fn refusal<'a>(err: &'a (dyn Error + 'static)) -> Option<&'a Refused> {
    let mut next = Some(err);
    while let Some(e) = next {
        if let Some(r) = e.downcast_ref::<Refused>() {
            return Some(r);
        }
        next = e.source();
    }
    None
}

/// A request error as a short text without the URL: the resolver's refusal, "timed out", or the innermost cause
/// ("Connection refused") without its "(os error N)".
pub fn error_text(err: &reqwest::Error) -> String {
    if let Some(r) = refusal(err) {
        return r.0.clone();
    }
    if err.is_timeout() {
        return "the request timed out".into();
    }
    if let Some(status) = err.status() {
        return format!("HTTP {}", status.as_u16());
    }
    // the innermost cause says what happened ("Connection refused", "failed to lookup address information: …");
    // reqwest's own text names the URL
    let mut innermost: Option<&(dyn Error + 'static)> = None;
    let mut next = err.source();
    while let Some(e) = next {
        innermost = Some(e);
        next = e.source();
    }
    match innermost {
        Some(e) => without_os_error(&e.to_string()),
        None if err.is_builder() => "the address is not valid".into(),
        None if err.is_redirect() => "too many redirects".into(),
        None if err.is_body() || err.is_decode() => "the answer could not be read".into(),
        None => "the request failed".into(),
    }
}

/// "Connection refused (os error 111)" → "Connection refused".
fn without_os_error(text: &str) -> String {
    match text.rfind(" (os error ") {
        Some(i) if text.ends_with(')') => text[..i].to_string(),
        _ => text.to_string(),
    }
}

/// `Uri.EscapeDataString`: RFC 3986 unreserved characters stay, everything else is UTF-8 in upper-case `%XX`.
pub(crate) fn escape_data_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver(answer: Vec<&'static str>, pick: Pick) -> FilteredResolver {
        FilteredResolver::new(
            crate::widget_feed_proxy::is_public_address,
            |host| format!("refused: {host} is not a public address"),
            pick,
        )
        .with_lookup(move |_| {
            let ips = answer.iter().map(|a| a.parse().unwrap()).collect();
            Box::pin(async move { Ok(ips) })
        })
    }

    #[tokio::test]
    async fn only_allowed_addresses_reach_the_connector() {
        let mixed = vec!["127.0.0.1", "2606:4700::1111", "10.0.0.1", "1.1.1.1"];
        let first = resolver(mixed.clone(), Pick::First)
            .addresses("x.example")
            .await
            .unwrap();
        assert_eq!(first, ["2606:4700::1111".parse::<IpAddr>().unwrap()]);
        let all = resolver(mixed, Pick::Ipv4First)
            .addresses("x.example")
            .await
            .unwrap();
        let expected: Vec<IpAddr> = vec![
            "1.1.1.1".parse().unwrap(),
            "2606:4700::1111".parse().unwrap(),
        ];
        assert_eq!(all, expected);
        let none = resolver(vec!["127.0.0.1", "192.168.1.5"], Pick::First)
            .addresses("intranet.example")
            .await
            .unwrap_err();
        assert_eq!(
            refusal(none.as_ref()),
            Some(&Refused(
                "refused: intranet.example is not a public address".into()
            ))
        );
    }

    #[tokio::test]
    async fn a_refused_name_never_connects_and_its_text_has_no_url() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let resolver = FilteredResolver::new(
            crate::widget_feed_proxy::is_public_address,
            |host| format!("refused: {host} is not a public address"),
            Pick::First,
        );
        let client = reqwest::Client::builder()
            .no_proxy()
            .dns_resolver(Arc::new(resolver))
            .build()
            .unwrap();
        let err = client
            .get(format!("http://localhost:{port}/feed.xml?key=SECRET-KEY"))
            .send()
            .await
            .unwrap_err();
        let text = error_text(&err);
        assert_eq!(text, "refused: localhost is not a public address");
        // nothing reached the listener
        let accepted =
            tokio::time::timeout(std::time::Duration::from_millis(200), listener.accept()).await;
        assert!(accepted.is_err(), "the refused name was connected to");
    }

    #[tokio::test]
    async fn a_connection_error_text_carries_no_url() {
        let port = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            l.local_addr().unwrap().port()
        }; // closed again: nothing listens there
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let err = client
            .get(format!(
                "http://127.0.0.1:{port}/calendar.ics?token=SECRET-KEY"
            ))
            .send()
            .await
            .unwrap_err();
        let text = error_text(&err);
        assert!(!text.contains("SECRET"), "{text}");
        assert!(!text.contains("calendar.ics"), "{text}");
        assert!(!text.contains("os error"), "{text}");
        assert!(text.to_lowercase().contains("refused"), "{text}");
    }
}
