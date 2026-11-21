//! `FeedTests.cs` class `FeedParserTests`, plus the encodings, entities and dates the C# baseline was probed for.

use super::*;

fn base() -> Url {
    Url::parse("https://example.com/blog/feed.xml").unwrap()
}

fn parse_str(xml: &str, max: usize) -> Result<Vec<FeedItem>, String> {
    parse(xml.as_bytes(), &base(), "k", max)
}

fn items(xml: &str) -> Vec<FeedItem> {
    parse_str(xml, 50).expect("a feed")
}

fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> i64 {
    use chrono::TimeZone;
    chrono::Utc
        .with_ymd_and_hms(y, mo, d, h, mi, s)
        .unwrap()
        .timestamp_millis()
}

#[test]
fn rss2_items_keep_title_link_and_date() {
    let items = items(
        r#"<?xml version="1.0"?>
<rss version="2.0"><channel><title>Blog</title>
  <item><title>First &amp;amp; best</title><link>https://example.com/a</link><pubDate>Mon, 16 Nov 2026 09:30:00 GMT</pubDate></item>
  <item><title><![CDATA[<b>Bold</b> move]]></title><link>/b</link><pubDate>Tue, 17 Nov 26 10:00 +0200</pubDate></item>
</channel></rss>"#,
    );
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].title, "First & best");
    assert_eq!(items[0].link.as_deref(), Some("https://example.com/a"));
    assert_eq!(items[0].published, utc(2026, 11, 16, 9, 30, 0));
    assert_eq!(items[1].title, "Bold move");
    assert_eq!(items[1].link.as_deref(), Some("https://example.com/b")); // relative links resolve against the feed
    assert_eq!(items[1].published, utc(2026, 11, 17, 8, 0, 0));
}

#[test]
fn atom_prefers_the_alternate_link_and_html_titles_become_text() {
    let items = items(
        r#"<feed xmlns="http://www.w3.org/2005/Atom"><title>A</title>
  <entry><id>urn:1</id><title type="html">&lt;i&gt;Hi&lt;/i&gt; there</title>
    <link rel="self" href="https://example.com/self"/><link rel="alternate" href="https://example.com/post"/>
    <updated>2026-11-16T08:00:00Z</updated></entry>
  <entry><id>urn:2</id><title>Plain &lt;3</title><link href="post2"/><published>2026-11-15T08:00:00+01:00</published></entry>
</feed>"#,
    );
    assert_eq!(items[0].title, "Hi there");
    assert_eq!(items[0].link.as_deref(), Some("https://example.com/post"));
    assert_eq!(items[1].title, "Plain <3");
    assert_eq!(
        items[1].link.as_deref(),
        Some("https://example.com/blog/post2")
    );
    assert_eq!(items[1].published, utc(2026, 11, 15, 7, 0, 0));
}

#[test]
fn atom_links_follow_xml_base() {
    let items = items(
        r#"<feed xmlns="http://www.w3.org/2005/Atom" xml:base="https://publisher.example/blog/">
  <entry xml:base="posts/"><id>1</id><title>Nested</title><link href="42"/></entry>
  <entry><id>2</id><title>Feed base</title><link href="7"/></entry>
  <entry><id>3</id><title>On the link</title><link xml:base="https://cdn.example/a/" href="b"/></entry>
</feed>"#,
    );
    let links: Vec<Option<&str>> = items.iter().map(|i| i.link.as_deref()).collect();
    assert_eq!(
        links,
        [
            Some("https://publisher.example/blog/posts/42"),
            Some("https://publisher.example/blog/7"),
            Some("https://cdn.example/a/b")
        ]
    );
}

#[test]
fn rss1_rdf_items_are_read() {
    let items = items(
        r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns="http://purl.org/rss/1.0/" xmlns:dc="http://purl.org/dc/elements/1.1/">
  <channel><title>R</title></channel>
  <item><title>Old school</title><link>https://example.com/rdf</link><dc:date>2026-11-01T00:00:00Z</dc:date></item>
</rdf:RDF>"#,
    );
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "Old school");
    assert_eq!(items[0].published, utc(2026, 11, 1, 0, 0, 0));
}

#[test]
fn only_http_links_are_kept() {
    let items = items(
        r#"<rss><channel>
  <item><title>Script</title><link>javascript:alert(1)</link></item>
  <item><title>File</title><link>file:///C:/Windows/System32/calc.exe</link></item>
  <item><title>App</title><link>ms-settings:privacy</link></item>
  <item><title>Guid</title><guid>https://example.com/g</guid></item>
  <item><title>Not a link</title><guid isPermaLink="false">https://example.com/x</guid></item>
</channel></rss>"#,
    );
    let links: Vec<Option<&str>> = items.iter().map(|i| i.link.as_deref()).collect();
    assert_eq!(
        links,
        [None, None, None, Some("https://example.com/g"), None]
    );
    assert!(items.iter().take(3).all(|i| !i.openable()));
}

#[test]
fn external_entities_are_never_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("phonebox-xxe.txt");
    std::fs::write(&secret, "TOPSECRET").unwrap();
    let xml = format!(
        r#"<?xml version="1.0"?><!DOCTYPE rss [<!ENTITY x SYSTEM "file:///{}">]><rss><channel><item><title>&x;</title></item></channel></rss>"#,
        secret.display().to_string().replace('\\', "/")
    );
    match parse_str(&xml, 50) {
        Err(_) => {}
        Ok(items) => assert!(!items.iter().any(|i| i.title.contains("TOPSECRET"))),
    }
}

#[test]
fn a_doctype_alone_does_not_reject_an_old_feed() {
    let items = items(
        r#"<?xml version="1.0"?><!DOCTYPE rss PUBLIC "-//Netscape Communications//DTD RSS 0.91//EN" "http://my.netscape.com/publish/formats/rss-0.91.dtd"><rss version="0.91"><channel><item><title>Ninety-one</title><link>https://example.com/91</link></item></channel></rss>"#,
    );
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "Ninety-one");
}

#[test]
fn titles_lose_control_and_direction_characters_and_are_capped() {
    let items = items(&format!(
        "<rss><channel><item><title>evil\u{202E}gnp.exe&#x9; line\nbreak\u{2066}</title></item><item><title>{}</title></item></channel></rss>",
        "a".repeat(1000)
    ));
    assert_eq!(items[0].title, "evilgnp.exe line break");
    assert!(items[1].title.encode_utf16().count() <= 301);
    assert_eq!(clean("a\u{0007}\u{0000}b", true), "a b"); // control characters XML itself would refuse
}

#[test]
fn not_a_feed_is_refused() {
    assert!(parse_str("<html><body>hi</body></html>", 50).is_err());
}

#[test]
fn the_item_count_is_capped() {
    let body: String = (0..10)
        .map(|i| format!("<item><title>t{i}</title></item>"))
        .collect();
    assert_eq!(
        items_max(&format!("<rss><channel>{body}</channel></rss>"), 3).len(),
        3
    );
}

fn items_max(xml: &str, max: usize) -> Vec<FeedItem> {
    parse_str(xml, max).unwrap()
}

#[test]
fn ids_are_stable_and_differ_per_feed() {
    assert_eq!(id_of("a", "x"), id_of("a", "x"));
    assert_ne!(id_of("a", "x"), id_of("b", "x"));
    assert_eq!(id_of("a", "x").len(), 16);
}

#[test]
fn dates_in_the_usual_formats() {
    for (raw, want) in [
        ("Mon, 16 Nov 2026 09:30:00 GMT", utc(2026, 11, 16, 9, 30, 0)),
        ("16 Nov 2026 09:30:00 +0000", utc(2026, 11, 16, 9, 30, 0)),
        ("Mon, 16 Nov 2026 04:30:00 EST", utc(2026, 11, 16, 9, 30, 0)),
        (
            "Monday, 16 November 2026 09:30 PDT",
            utc(2026, 11, 16, 16, 30, 0),
        ),
        ("Wed, 02 Oct 02 13:00:00 GMT", utc(2002, 10, 2, 13, 0, 0)),
        ("Wed, 02 Oct 99 13:00:00 GMT", utc(1999, 10, 2, 13, 0, 0)),
        ("2026-11-16T09:30:00Z", utc(2026, 11, 16, 9, 30, 0)),
        ("2026-11-16T11:30:00+02:00", utc(2026, 11, 16, 9, 30, 0)),
    ] {
        assert_eq!(parse_date(Some(raw)), want, "{raw}");
    }
}

#[test]
fn unreadable_dates_are_zero() {
    for raw in [
        "",
        "yesterday",
        "31 Foo 2026 10:00 GMT",
        "31 Feb 2026 10:00 GMT",
    ] {
        assert_eq!(parse_date(Some(raw)), 0, "{raw}");
    }
    assert_eq!(parse_date(None), 0);
}

/// `DateTimeOffset.TryParse` (invariant, assume UTC) as probed on the C# baseline, for the subset the port reads.
#[test]
fn the_iso_fallback_reads_what_the_csharp_agent_read() {
    let at = utc(2026, 11, 16, 9, 30, 0);
    for (raw, want) in [
        ("2026-11-16", utc(2026, 11, 16, 0, 0, 0)),
        ("2026-11", utc(2026, 11, 1, 0, 0, 0)),
        ("2026-11-16 09:30", at),
        ("2026-11-16T09:30", at),
        ("2026-11-16T09:30Z", at),
        ("2026-11-16T09:30:00", at),
        ("2026-11-16T09:30:00.123Z", at + 123),
        ("2026-11-16T09:30:00.1234567+02:00", at - 7_200_000 + 123),
        ("2026-11-16T09:30:00.123456789Z", at + 123),
        ("2026-11-16T09:30:00,5Z", at + 500),
        ("2026-11-16T09:30:00+0200", at - 7_200_000),
        ("2026-11-16T09:30:00+02", at - 7_200_000),
        ("2026-11-16T09:30:00 +02:00", at - 7_200_000),
        ("2026-11-16T09:30:00+14:00", at - 14 * 3_600_000),
        ("2026-11-16T09:30:00-00:00", at),
        ("2026-11-16 09:30:00Z", at),
        ("2026-11-16t09:30:00z", at),
        ("  2026-11-16T09:30:00Z  ", at),
        // RFC 822 without a time
        ("16 Nov 2026", utc(2026, 11, 16, 0, 0, 0)),
        ("Mon, 16 Nov 2026", utc(2026, 11, 16, 0, 0, 0)),
        // RFC 822 without a zone counts as UTC
        ("Mon, 16 Nov 2026 09:30:00", at),
    ] {
        assert_eq!(parse_date(Some(raw)), want, "{raw}");
    }
    for raw in [
        "20261116T093000Z",
        "2026-11-16T9:30:00Z",
        "2026-11-16T24:00:00Z",
        "2026-11-16T09:30:60Z",
        "2026-11-16T09:30:00+15:00",
        "2026-02-30",
        "0001-01-01T00:00:00+01:00",
        "9999-12-31T23:59:59-01:00",
        "2026",
        "+2026-11-16",
        "2026-W47-1",
        "2026-11-16T09:30:00GMT",
        "2026-11-16T09:30:00 GMT",
        "16/11/2026",
    ] {
        assert_eq!(parse_date(Some(raw)), 0, "{raw}");
    }
}

#[test]
fn rfc822_dates_outside_the_calendar_or_offsets_beyond_14_hours_are_zero() {
    assert_eq!(parse_date(Some("16 Nov 2026 24:00 GMT")), 0);
    assert_eq!(parse_date(Some("16 Nov 2026 09:60 GMT")), 0);
    assert_eq!(parse_date(Some("16 Nov 2026 09:30 +9999")), 0);
    assert_eq!(parse_date(Some("1 Jan 0001 00:00 +0100")), 0);
    assert_eq!(
        parse_date(Some("16 Nov 2026 09:30:60 GMT")),
        utc(2026, 11, 16, 9, 30, 59)
    ); // seconds clamp to 59
    assert_eq!(parse_date(Some("16 Nov 999 09:30 GMT")), {
        use chrono::TimeZone;
        chrono::Utc
            .with_ymd_and_hms(999, 11, 16, 9, 30, 0)
            .unwrap()
            .timestamp_millis()
    });
    assert_eq!(
        parse_date(Some(&format!("16 Nov 2026 09:30 {}", "x".repeat(60)))),
        0
    ); // longer than 64
}

/// `XmlReader` without code pages, as probed on the C# baseline.
#[test]
fn only_the_encodings_dotnet_reads_without_code_pages_are_accepted() {
    let doc = |decl: &str, title: &[u8]| {
        let mut bytes = Vec::new();
        if !decl.is_empty() {
            bytes.extend_from_slice(
                format!(r#"<?xml version="1.0" encoding="{decl}"?>"#).as_bytes(),
            );
        }
        bytes.extend_from_slice(b"<rss><channel><item><title>");
        bytes.extend_from_slice(title);
        bytes.extend_from_slice(b"</title></item></channel></rss>");
        bytes
    };
    let title = |bytes: Vec<u8>| parse(&bytes, &base(), "k", 50).map(|i| i[0].title.clone());
    let high = [0x41, 0x93, 0xE9, 0x42];
    for refused in [
        "windows-1252",
        "koi8-r",
        "iso-8859-15",
        "macintosh",
        "shift_jis",
        "UTF8",
        "bogus",
        "utf-16",
    ] {
        assert!(title(doc(refused, &high)).is_err(), "{refused}");
    }
    for latin1 in ["iso-8859-1", "ISO-8859-1", "latin1"] {
        assert_eq!(
            title(doc(latin1, &high)).unwrap(),
            "A\u{93}éB".replace('\u{93}', " "),
            "{latin1}"
        );
    }
    assert_eq!(title(doc("us-ascii", &high)).unwrap(), "A??B");
    assert!(title(doc("utf-8", &high)).is_err()); // invalid UTF-8
    assert!(title(doc("", &high)).is_err());
    assert_eq!(title(doc("", "Aé€B".as_bytes())).unwrap(), "Aé€B");
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend(doc("", "Aé".as_bytes()));
    assert_eq!(title(bom).unwrap(), "Aé");
    // a Latin-1 declaration wins over a UTF-8 byte order mark
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend(doc("iso-8859-1", "Aé".as_bytes()));
    assert_eq!(title(bom).unwrap(), "AÃ©");
    let utf16 = |text: &str, le: bool| -> Vec<u8> {
        text.encode_utf16()
            .flat_map(|u| if le { u.to_le_bytes() } else { u.to_be_bytes() })
            .collect()
    };
    let text = r#"<?xml version="1.0" encoding="utf-16"?><rss><channel><item><title>Aé€</title></item></channel></rss>"#;
    let mut le = vec![0xFF, 0xFE];
    le.extend(utf16(text, true));
    assert_eq!(title(le).unwrap(), "Aé€");
    assert_eq!(title(utf16(text, true)).unwrap(), "Aé€"); // no BOM: the first bytes tell
    let mut be = vec![0xFE, 0xFF];
    be.extend(utf16(
        "<rss><channel><item><title>Aé</title></item></channel></rss>",
        false,
    ));
    assert_eq!(title(be).unwrap(), "Aé");
    let mut le32 = vec![0xFF, 0xFE, 0x00, 0x00];
    le32.extend(
        "<rss><channel><item><title>Aé</title></item></channel></rss>"
            .chars()
            .flat_map(|c| (c as u32).to_le_bytes()),
    );
    assert_eq!(title(le32).unwrap(), "Aé");
    assert!(title(Vec::new()).is_err());
    assert!(title(b"   ".to_vec()).is_err());
}

#[test]
fn entities_a_doctype_declares_or_html_names_fail_the_feed() {
    assert!(parse_str(r#"<!DOCTYPE rss [<!ENTITY x "hello">]><rss><channel><item><title>&x;</title></item></channel></rss>"#, 50).is_err());
    assert!(
        parse_str(
            "<rss><channel><item><title>a&nbsp;b</title></item></channel></rss>",
            50
        )
        .is_err()
    );
    // a DOCTYPE with an internal subset holding quotes, brackets and a comment is cut out whole
    let items = items(
        r#"<!DOCTYPE rss [<!-- ] > " --><!ATTLIST rss v CDATA "]>">]><rss><channel><item><title>a &amp; b</title></item></channel></rss>"#,
    );
    assert_eq!(items[0].title, "a & b");
}

/// `WebUtility.HtmlDecode`, as probed on the C# baseline.
#[test]
fn html_decoding_matches_webutility() {
    for (raw, want) in [
        ("&apos;", "'"),
        ("&#xD800;", "&#xD800;"),
        ("&#0;", "\0"),
        ("&# 65;", "A"),
        ("&#+65;", "A"),
        ("&#-0;", "\0"),
        ("&#-1;", "&#-1;"),
        ("&#x41;", "A"),
        ("&#X41;", "A"),
        ("&#x;", "&#x;"),
        ("&#;", "&#;"),
        ("&amp", "&amp"),
        ("&a&amp;", "&a&"),
        ("&AMP;", "&AMP;"),
        ("&#1114111;", "\u{10FFFF}"),
        ("&#1114112;", "&#1114112;"),
        ("&#x0041 ;", "&#x0041 ;"),
        ("&eacute;t&eacute; &hellip; &euro;", "été … €"),
        ("no entity", "no entity"),
    ] {
        assert_eq!(html_decode(raw), want, "{raw}");
    }
    assert_eq!(super::super::html_entities::ENTITIES.len(), 253);
}

#[test]
fn stripping_tags_stays_linear_and_keeps_an_unclosed_bracket() {
    assert_eq!(strip_tags("a<b>c</b>d"), "a c d");
    assert_eq!(strip_tags("a < b"), "a < b");
    assert_eq!(strip_tags("<i>x</i> <y"), " x  <y");
    let many = "<".repeat(2_000_000);
    let started = std::time::Instant::now();
    assert_eq!(strip_tags(&many).len(), 2_000_000);
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}

#[test]
fn a_title_is_cut_at_300_utf16_units_with_an_ellipsis() {
    assert_eq!(
        clean(&"a".repeat(300), true),
        format!("{}…", "a".repeat(300))
    );
    assert_eq!(clean(&"a".repeat(299), true), "a".repeat(299));
    // a final astral character can take the length to 302 units
    let title = clean(&format!("{}😀b", "a".repeat(299)), true);
    assert_eq!(title, format!("{}😀…", "a".repeat(299)));
    assert_eq!(title.encode_utf16().count(), 302);
    // the raw cut at 2400 units can split a pair: the half becomes U+FFFD (then the 300 cap applies)
    assert_eq!(clean(&format!("{}😀", " ".repeat(2399)), true), "\u{FFFD}");
    assert_eq!(clean("  a \t\n b  ", false), "a b");
    assert_eq!(clean("&lt;x&gt;", false), "&lt;x&gt;");
}

#[test]
fn an_item_without_a_title_is_named_by_its_links_host_or_skipped() {
    let items = items(
        r#"<rss><channel>
  <item><link>https://news.example/a</link></item>
  <item><description>nothing to show</description></item>
  <item><title> </title><guid>urn:x</guid></item>
</channel></rss>"#,
    );
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "news.example");
    // the key is the link when there is no guid
    assert_eq!(items[0].id, id_of("k", "https://news.example/a"));
}

#[test]
fn the_first_child_with_a_name_wins_in_any_namespace() {
    let items = items(
        r#"<rss xmlns:media="http://search.yahoo.com/mrss/"><channel>
  <item><media:title>Media</media:title><title>Plain</title><guid> g1 </guid></item>
  <item><title>Nested <b>bold</b> text</title></item>
</channel></rss>"#,
    );
    assert_eq!(items[0].title, "Media");
    assert_eq!(items[0].id, id_of("k", "g1"));
    assert_eq!(items[1].title, "Nested bold text");
}
