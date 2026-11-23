//! The two hashes TCL's cloud needs: MD5 (TCL's sign-in and device-list signatures) and AWS Signature Version 4
//! (the AWS IoT shadow read and publish).

use hmac::{Hmac, Mac};
use md5::Md5;
use sha2::{Digest, Sha256};

/// MD5 as lower-case hex (TCL's sign-in sends the password so, and signs the device list with it).
pub fn md5_hex(text: &str) -> String {
    hex(&Md5::digest(text.as_bytes()))
}

/// The temporary AWS credentials Cognito hands out for TCL's identity. `Debug` shows only the key id.
pub struct AwsCredentials {
    pub access_key: String,
    pub secret: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsCredentials")
            .field("access_key", &self.access_key)
            .finish_non_exhaustive()
    }
}

/// What a signature covers: the method, the URL (path already percent-encoded once), the headers sent besides
/// `host` and the ones the signature adds, the body, and the region and service of the scope.
pub struct SignRequest<'a> {
    pub method: &'a str,
    pub url: &'a reqwest::Url,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
    pub region: &'a str,
    pub service: &'a str,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac(key: &[u8], message: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(message.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// RFC 3986 encoding: only the unreserved characters stay.
pub(crate) fn uri_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The canonical path: every segment of the (already encoded) path encoded again, as AWS signs every service but S3.
pub fn canonical_path(path: &str) -> String {
    if path.is_empty() {
        return "/".into();
    }
    path.split('/')
        .map(uri_encode)
        .collect::<Vec<_>>()
        .join("/")
}

fn canonical_query(url: &reqwest::Url) -> String {
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (uri_encode(&k), uri_encode(&v)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Signs `req` at `at_ms` (Unix ms): the headers to send with it, `x-amz-date`, `x-amz-security-token` when the
/// credentials have one, and `authorization`.
pub fn sign(req: &SignRequest<'_>, creds: &AwsCredentials, at_ms: i64) -> Vec<(String, String)> {
    let amz_date = chrono::DateTime::from_timestamp_millis(at_ms)
        .unwrap_or_default()
        .format("%Y%m%dT%H%M%SZ")
        .to_string();
    let day = &amz_date[..8];
    let host = match (req.url.host_str(), req.url.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => String::new(),
    };
    let mut added = vec![("x-amz-date".to_string(), amz_date.clone())];
    if let Some(token) = &creds.session_token {
        added.push(("x-amz-security-token".to_string(), token.clone()));
    }
    let mut headers: Vec<(String, String)> = req
        .headers
        .iter()
        .chain(added.iter())
        .map(|(n, v)| {
            (
                n.to_ascii_lowercase(),
                v.split_whitespace().collect::<Vec<_>>().join(" "),
            )
        })
        .chain(std::iter::once(("host".to_string(), host)))
        .collect();
    headers.sort();
    let canonical_headers: String = headers.iter().map(|(n, v)| format!("{n}:{v}\n")).collect();
    let signed = headers
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed}\n{}",
        req.method,
        canonical_path(req.url.path()),
        canonical_query(req.url),
        hex(&Sha256::digest(req.body)),
    );
    let scope = format!("{day}/{}/{}/aws4_request", req.region, req.service);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let mut key = hmac(format!("AWS4{}", creds.secret).as_bytes(), day);
    for part in [req.region, req.service, "aws4_request"] {
        key = hmac(&key, part);
    }
    let signature = hex(&hmac(&key, &to_sign));
    added.push((
        "authorization".to_string(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
            creds.access_key
        ),
    ));
    added
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
        headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn md5_matches_rfc_1321() {
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn sigv4_matches_awss_get_vanilla_example() {
        let url = reqwest::Url::parse("https://example.amazonaws.com/").unwrap();
        let creds = AwsCredentials {
            access_key: "AKIDEXAMPLE".into(),
            secret: SECRET.into(),
            session_token: None,
        };
        let req = SignRequest {
            method: "GET",
            url: &url,
            headers: &[],
            body: b"",
            region: "us-east-1",
            service: "service",
        };
        // 2015-08-30T12:36:00Z
        let added = sign(&req, &creds, 1_440_938_160_000);
        assert_eq!(header(&added, "x-amz-date"), Some("20150830T123600Z"));
        assert_eq!(header(&added, "x-amz-security-token"), None);
        assert_eq!(
            header(&added, "authorization"),
            Some(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
                 SignedHeaders=host;x-amz-date, \
                 Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
            )
        );
    }

    #[test]
    fn sigv4_signs_the_session_token_and_the_body() {
        let url = reqwest::Url::parse(
            "https://data-ats.iot.eu-central-1.amazonaws.com/topics/%24aws%2Fthings%2Fac1%2Fshadow%2Fupdate?qos=1",
        )
        .unwrap();
        let creds = AwsCredentials {
            access_key: "AKIDEXAMPLE".into(),
            secret: SECRET.into(),
            session_token: Some("SESSIONTOKEN".into()),
        };
        let headers = [("Content-Type".to_string(), "application/json".to_string())];
        let req = SignRequest {
            method: "POST",
            url: &url,
            headers: &headers,
            body: br#"{"state":{"desired":{"powerSwitch":1}},"clientToken":"mobile_1795428000"}"#,
            region: "eu-central-1",
            service: "iotdata",
        };
        // 2026-11-23T10:00:00Z; the signature was computed apart, with Python's hmac and hashlib
        let added = sign(&req, &creds, 1_795_428_000_000);
        assert_eq!(header(&added, "x-amz-security-token"), Some("SESSIONTOKEN"));
        assert_eq!(
            header(&added, "authorization"),
            Some(
                "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20261123/eu-central-1/iotdata/aws4_request, \
                 SignedHeaders=content-type;host;x-amz-date;x-amz-security-token, \
                 Signature=4e2966cfe43d01ac2b4cc1f8018ef4e1186253493344694b5e5ad2df930c123a"
            )
        );
    }

    #[test]
    fn the_canonical_path_encodes_each_segment_again() {
        assert_eq!(
            canonical_path("/topics/%24aws%2Fthings%2Fac1%2Fshadow%2Fupdate"),
            "/topics/%2524aws%252Fthings%252Fac1%252Fshadow%252Fupdate"
        );
        assert_eq!(
            canonical_path("/things/ac-1_x.y~z/shadow"),
            "/things/ac-1_x.y~z/shadow"
        );
        assert_eq!(canonical_path(""), "/");
    }

    #[test]
    fn debug_hides_the_secret_and_the_token() {
        let creds = AwsCredentials {
            access_key: "AKID".into(),
            secret: "s3cr3t".into(),
            session_token: Some("t0k3n".into()),
        };
        let text = format!("{creds:?}");
        assert!(
            text.contains("AKID") && !text.contains("s3cr3t") && !text.contains("t0k3n"),
            "{text}"
        );
    }
}
