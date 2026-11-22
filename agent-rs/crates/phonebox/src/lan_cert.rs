//! Port of the certificate half of `Lan.cs` (`Certificate`, `Fingerprint`): the self-signed TLS certificate the phone
//! pins when it talks to the agent over Wi-Fi. It lives in `<config folder>/lan-cert.pfx`, the file the C# agent
//! wrote, so every phone pairing survives the switch to the Rust agent.

use std::io;
use std::path::Path;

use chrono::{DateTime, Utc};

use crate::pkcs12::Pfx;

const CATEGORY: &str = "PhoneBox.Agent.Lan";
pub const FILE_NAME: &str = "lan-cert.pfx";

/// The certificate the Wi-Fi port serves, its key, and the SHA-256 fingerprint the phone pins.
#[derive(Clone)]
pub struct LanCert {
    pub der: Vec<u8>,
    pub key_pkcs8: Vec<u8>,
    pub fingerprint: String,
}

impl std::fmt::Debug for LanCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // never the key
        f.debug_struct("LanCert")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

/// Loads the agent's TLS certificate from `config_dir`, creating it on first use (or when the file cannot be read).
pub fn certificate(config_dir: &Path) -> io::Result<LanCert> {
    load_or_create(config_dir, &crate::state::machine_name(), &mut |m| {
        crate::log::warn(CATEGORY, m)
    })
}

fn load_or_create(
    config_dir: &Path,
    machine_name: &str,
    warn: &mut dyn FnMut(&str),
) -> io::Result<LanCert> {
    let path = config_dir.join(FILE_NAME);
    if path.is_file() {
        crate::app_paths::restrict_to_owner(&path);
        match std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| crate::pkcs12::read(&bytes))
        {
            Ok(pfx) => return Ok(LanCert::from(pfx)),
            Err(reason) => warn(&format!(
                "The Wi-Fi certificate {} could not be read ({reason}); a new one is made, and phones paired over \
                 Wi-Fi only must pair again",
                path.display()
            )),
        }
    }
    let pfx = create(machine_name, Utc::now());
    std::fs::create_dir_all(config_dir)?;
    let mut file = crate::app_paths::create_private(&path)?;
    io::Write::write_all(&mut file, &crate::pkcs12::write(&pfx))?;
    file.sync_all()?;
    Ok(LanCert::from(pfx))
}

impl From<Pfx> for LanCert {
    fn from(pfx: Pfx) -> LanCert {
        LanCert {
            fingerprint: fingerprint(&pfx.cert_der),
            der: pfx.cert_der,
            key_pkcs8: pfx.key_pkcs8,
        }
    }
}

/// SHA-256 of the certificate DER, as 64 lower-case hex characters.
pub fn fingerprint(der: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

const OID_ECDSA_SHA256: &str = "1.2.840.10045.4.3.2";
const OID_EC_PUBLIC_KEY: &str = "1.2.840.10045.2.1";
const OID_P256: &str = "1.2.840.10045.3.1.7";
const OID_COMMON_NAME: &str = "2.5.4.3";
const OID_BASIC_CONSTRAINTS: &str = "2.5.29.19";
const OID_KEY_USAGE: &str = "2.5.29.15";
const OID_EXT_KEY_USAGE: &str = "2.5.29.37";
const OID_SERVER_AUTH: &str = "1.3.6.1.5.5.7.3.1";

/// .NET writes a name component as PrintableString when every character allows it, else as UTF8String.
fn directory_string(text: &str) -> Vec<u8> {
    let printable = text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || " '()+,-./:=?".contains(c));
    let tag = if printable {
        crate::der::PRINTABLE_STRING
    } else {
        crate::der::UTF8_STRING
    };
    crate::der::tlv(tag, text.as_bytes())
}

fn new_key() -> p256::SecretKey {
    loop {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("the system random source");
        // a value of zero or at least the group order is not a key: draw again (practically never)
        if let Ok(key) = p256::SecretKey::from_slice(&bytes) {
            return key;
        }
    }
}

/// A new self-signed certificate in the C# layout: ECDSA P-256 / SHA-256, `CN=PhoneBox agent (<machine name>)`,
/// BasicConstraints CA:false, KeyUsage digitalSignature and EKU serverAuth (all non-critical), an 8-byte positive
/// serial, valid from `now` − 1 day to `now` + 20 years.
pub fn create(machine_name: &str, now: DateTime<Utc>) -> Pfx {
    use crate::der::{self, seq, write_oid};
    use p256::ecdsa::signature::Signer;
    use p256::elliptic_curve::sec1::ToEncodedPoint;
    use p256::pkcs8::EncodePrivateKey;

    let key = new_key();
    let mut serial = [0u8; 8];
    getrandom::fill(&mut serial).expect("the system random source");
    serial[0] = (serial[0] & 0x7F).max(1); // positive, and still 8 bytes in DER
    let algorithm = seq(&[&write_oid(OID_ECDSA_SHA256)]);
    let name = seq(&[&der::set(&[&seq(&[
        &write_oid(OID_COMMON_NAME),
        &directory_string(&format!("PhoneBox agent ({machine_name})")),
    ])])]);
    let not_before = now - chrono::Duration::days(1);
    let not_after = now
        .checked_add_months(chrono::Months::new(20 * 12))
        .unwrap_or(now);
    let point = key.public_key().to_encoded_point(false);
    let spki = seq(&[
        &seq(&[&write_oid(OID_EC_PUBLIC_KEY), &write_oid(OID_P256)]),
        &der::tlv(der::BIT_STRING, &[&[0u8][..], point.as_bytes()].concat()),
    ]);
    let extension = |oid: &str, value: &[u8]| seq(&[&write_oid(oid), &der::write_octets(value)]);
    let extensions = der::constructed(
        der::context(3, true),
        &[&seq(&[
            &extension(OID_BASIC_CONSTRAINTS, &seq(&[])),
            &extension(OID_KEY_USAGE, &der::tlv(der::BIT_STRING, &[0x07, 0x80])),
            &extension(OID_EXT_KEY_USAGE, &seq(&[&write_oid(OID_SERVER_AUTH)])),
        ])],
    );
    let tbs = seq(&[
        &der::constructed(der::context(0, true), &[&der::write_uint(2)]),
        &der::write_uint_bytes(&serial),
        &algorithm,
        &name,
        &seq(&[
            &der::write_time(trim_to_second(not_before)),
            &der::write_time(trim_to_second(not_after)),
        ]),
        &name,
        &spki,
        &extensions,
    ]);
    let signature: p256::ecdsa::Signature = p256::ecdsa::SigningKey::from(&key).sign(&tbs);
    let signature = signature.to_der();
    let cert_der = seq(&[
        &tbs,
        &algorithm,
        &der::tlv(
            der::BIT_STRING,
            &[&[0u8][..], signature.as_bytes()].concat(),
        ),
    ]);
    Pfx {
        cert_der,
        key_pkcs8: key
            .to_pkcs8_der()
            .expect("a P-256 key encodes")
            .as_bytes()
            .to_vec(),
    }
}

fn trim_to_second(t: DateTime<Utc>) -> DateTime<Utc> {
    use chrono::Timelike;
    t.with_nanosecond(0).unwrap_or(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::der::{self, Reader};
    use chrono::TimeZone;

    const DOTNET_FP: &str = "2c642ac96f9bb9c03376e28e57e7971a1bf010ed3d54b0d4bc609bbdf209a461";
    const OPENSSL_FP: &str = "a66b25c96b59b9d7c7b5dbfb5ac75d45df46c03f1f2d664988d9779283b2527f";

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/testdata/lan")
                .join(name),
        )
        .unwrap()
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn lan_certificate_isolates_to_config_directory_and_colliding_lan_port_is_detected() {
        use crate::server::routes_settings::lan_port_collides_with_local;
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("lan-cert.pfx");
        std::fs::write(&cert_path, "corrupt").unwrap();
        let first = certificate(dir.path()).unwrap();
        assert!(cert_path.is_file());
        let fp = first.fingerprint.clone();
        let second = certificate(dir.path()).unwrap();
        assert_eq!(fp, second.fingerprint);
        assert_eq!(fp, fingerprint(&second.der));
        #[cfg(unix)]
        assert_eq!(mode(&cert_path), 0o600);

        assert!(lan_port_collides_with_local(
            r#"{"lan":{"enabled":true,"port":8765}}"#,
            8765
        ));
        assert!(lan_port_collides_with_local(
            r#"{"lan":{"enabled":true}}"#,
            8766
        ));
        assert!(!lan_port_collides_with_local(
            r#"{"lan":{"enabled":false,"port":8765}}"#,
            8765
        ));
        assert!(!lan_port_collides_with_local(
            r#"{"lan":{"enabled":true,"port":8766}}"#,
            8765
        ));
    }

    fn elements(data: &[u8]) -> Vec<(u8, Vec<u8>, Vec<u8>)> {
        let mut r = Reader::new(data);
        let mut out = Vec::new();
        while !r.is_empty() {
            let t = r.take().unwrap();
            out.push((t.tag, t.value.to_vec(), t.raw.to_vec()));
        }
        out
    }

    #[test]
    fn a_new_certificate_has_the_csharp_layout() {
        use p256::ecdsa::signature::Verifier;
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        use p256::pkcs8::DecodePrivateKey;
        let now = Utc.with_ymd_and_hms(2035, 3, 10, 12, 34, 56).unwrap()
            + chrono::Duration::milliseconds(789);
        let pfx = create("phonebox-test", now);
        let key = p256::SecretKey::from_pkcs8_der(&pfx.key_pkcs8).unwrap();

        let cert = elements(&pfx.cert_der);
        assert_eq!(cert.len(), 1);
        let parts = elements(&cert[0].1);
        assert_eq!(parts.len(), 3, "tbs, algorithm, signature");
        let ecdsa_sha256 = der::seq(&[&der::write_oid("1.2.840.10045.4.3.2")]);
        assert_eq!(parts[1].2, ecdsa_sha256);
        assert_eq!(parts[2].0, der::BIT_STRING);

        let tbs = elements(&parts[0].1);
        assert_eq!(
            tbs.len(),
            8,
            "version, serial, algorithm, issuer, validity, subject, key, extensions"
        );
        assert_eq!(
            tbs[0].2,
            der::constructed(der::context(0, true), &[&der::write_uint(2)])
        );
        assert_eq!(tbs[1].0, der::INTEGER);
        assert_eq!(tbs[1].1.len(), 8);
        assert!(
            (1..0x80).contains(&tbs[1].1[0]),
            "positive, and exactly 8 bytes"
        );
        assert_eq!(tbs[2].2, ecdsa_sha256);
        let name = der::seq(&[&der::set(&[&der::seq(&[
            &der::write_oid("2.5.4.3"),
            &der::tlv(der::PRINTABLE_STRING, b"PhoneBox agent (phonebox-test)"),
        ])])]);
        assert_eq!(tbs[3].2, name);
        assert_eq!(tbs[5].2, name);
        assert_eq!(
            tbs[4].2,
            der::seq(&[
                &der::tlv(der::UTC_TIME, b"350309123456Z"),
                &der::tlv(der::GENERALIZED_TIME, b"20550310123456Z"),
            ])
        );
        let point = key.public_key().to_encoded_point(false);
        let spki = der::seq(&[
            &der::seq(&[
                &der::write_oid("1.2.840.10045.2.1"),
                &der::write_oid("1.2.840.10045.3.1.7"),
            ]),
            &der::tlv(der::BIT_STRING, &[&[0u8][..], point.as_bytes()].concat()),
        ]);
        assert_eq!(tbs[6].2, spki);
        let ext =
            |oid: &str, value: &[u8]| der::seq(&[&der::write_oid(oid), &der::write_octets(value)]);
        assert_eq!(
            tbs[7].2,
            der::constructed(
                der::context(3, true),
                &[&der::seq(&[
                    &ext("2.5.29.19", &[0x30, 0x00]),
                    &ext("2.5.29.15", &[0x03, 0x02, 0x07, 0x80]),
                    &ext(
                        "2.5.29.37",
                        &der::seq(&[&der::write_oid("1.3.6.1.5.5.7.3.1")])
                    ),
                ])]
            ),
            "the three extensions, non-critical, nothing else"
        );

        let signature = p256::ecdsa::Signature::from_der(&parts[2].1[1..]).unwrap();
        assert_eq!(parts[2].1[0], 0, "no unused bits");
        p256::ecdsa::VerifyingKey::from(key.public_key())
            .verify(&parts[0].2, &signature)
            .expect("self-signed with its own key");

        // a name PrintableString cannot hold is written as UTF8String
        let other = create("my_pc", now);
        let tbs = elements(&elements(&elements(&other.cert_der)[0].1)[0].1);
        let rdn = elements(&elements(&elements(&tbs[5].1)[0].1)[0].1);
        assert_eq!(
            rdn[1].2,
            der::tlv(der::UTF8_STRING, b"PhoneBox agent (my_pc)")
        );
        // two certificates never share a key or a serial
        let first = elements(&elements(&elements(&pfx.cert_der)[0].1)[0].1);
        assert_ne!(first[1], tbs[1]);
        assert_ne!(pfx.key_pkcs8, other.key_pkcs8);
    }

    #[test]
    fn the_dotnet_fixture_keeps_its_fingerprint() {
        for (file, fp) in [
            ("dotnet.pfx", DOTNET_FP),
            ("openssl-rc2.pfx", OPENSSL_FP),
            ("openssl-aes.pfx", OPENSSL_FP),
            ("openssl-nopub.pfx", OPENSSL_FP),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(FILE_NAME);
            let bytes = fixture(file);
            std::fs::write(&path, &bytes).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            let mut warnings = Vec::new();
            let cert = load_or_create(dir.path(), "someone-else", &mut |m| {
                warnings.push(m.to_string())
            })
            .unwrap();
            assert_eq!(cert.fingerprint, fp, "{file}");
            assert!(warnings.is_empty(), "{file}: {warnings:?}");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                bytes,
                "{file} is left as it is"
            );
            #[cfg(unix)]
            assert_eq!(mode(&path), 0o600, "{file} is restricted to its owner");
        }
    }

    #[test]
    fn an_unreadable_file_is_replaced_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, b"\x30\x03\x02\x01\x03").unwrap();
        let mut warnings = Vec::new();
        let cert = load_or_create(dir.path(), "phonebox-test", &mut |m| {
            warnings.push(m.to_string())
        })
        .unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("phones paired over Wi-Fi only must pair again"),
            "{warnings:?}"
        );
        let written = crate::pkcs12::read(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(fingerprint(&written.cert_der), cert.fingerprint);
        assert_eq!(written.cert_der, cert.der);
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);

        // a missing file is made without a warning, and the folder too
        let nested = dir.path().join("new");
        let mut warnings = Vec::new();
        load_or_create(&nested, "phonebox-test", &mut |m| {
            warnings.push(m.to_string())
        })
        .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(nested.join(FILE_NAME).is_file());
    }
}
