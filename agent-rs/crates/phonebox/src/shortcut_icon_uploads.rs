//! `ShortcutIconUploads`: custom icon images the user uploads for a shortcut in the settings page (PNG, WEBP or SVG),
//! kept under the config folder as `icons/<shortcut id>.<ext>`. A custom icon takes priority over the shortcut's
//! auto-detected program icon ([`crate::shortcut_icons`]) and, below that, the emoji.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

pub const MAX_BYTES: usize = 512 * 1024;

/// Content type and file extension, in the order [`ShortcutIconUploads::read`] looks for them.
const TYPES: [(&str, &str); 3] = [
    ("image/png", ".png"),
    ("image/webp", ".webp"),
    ("image/svg+xml", ".svg"),
];

pub struct ShortcutIconUploads {
    dir: PathBuf,
    /// The store's lock, guarding the revisions: a file's own last-write time is not reliable for this (on Windows it
    /// can tick in steps of ~15 ms, so two uploads in close succession can land on the same value). Keyed by id,
    /// filled from the bytes already in memory on save, evicted by remove; `revision` fills it lazily for an icon that
    /// was already on disk when this process started.
    gate: Mutex<HashMap<String, i64>>,
}

impl ShortcutIconUploads {
    pub fn new(dir: PathBuf) -> Self {
        ShortcutIconUploads {
            dir,
            gate: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, i64>> {
        self.gate.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether a custom icon was uploaded for a shortcut, without reading it.
    pub fn exists(&self, id: &str) -> bool {
        TYPES
            .iter()
            .any(|(_, ext)| is_file_like(&self.path_for(id, ext)))
    }

    /// A value that changes when the uploaded icon is replaced or removed, or None when none was uploaded.
    pub fn revision(&self, id: &str) -> Option<i64> {
        let mut cache = self.lock();
        if let Some(cached) = cache.get(id) {
            return Some(*cached);
        }
        let (bytes, _) = self.read_locked(id)?;
        let revision = content_revision(&bytes);
        cache.insert(id.to_string(), revision);
        Some(revision)
    }

    /// The uploaded icon for a shortcut and its content type, or None when none was uploaded.
    pub fn read(&self, id: &str) -> Option<(Vec<u8>, &'static str)> {
        let _gate = self.lock();
        self.read_locked(id)
    }

    /// The first format on disk; one that exists but cannot be read is None, not the next format.
    fn read_locked(&self, id: &str) -> Option<(Vec<u8>, &'static str)> {
        for (content_type, ext) in TYPES {
            let path = self.path_for(id, ext);
            if !is_file_like(&path) {
                continue;
            }
            return std::fs::read(&path).ok().map(|b| (b, content_type));
        }
        None
    }

    /// Reads a request body up to [`MAX_BYTES`] (plus one byte, to tell "too large" from "exactly the limit"); Ok(None)
    /// means the upload is too large. Bounded so a huge body cannot be read fully into memory first.
    pub async fn read_bounded(body: axum::body::Body) -> Result<Option<Vec<u8>>, axum::Error> {
        use futures_util::StreamExt;
        let mut stream = body.into_data_stream();
        let mut buf = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            if buf.len() + chunk.len() > MAX_BYTES {
                return Ok(None);
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(Some(buf))
    }

    /// Ok(Some(message)) when the upload is refused, Ok(None) on success; Err when the file could not be written or
    /// moved into place (the C# agent throws there).
    pub fn save(
        &self,
        id: &str,
        content_type: Option<&str>,
        bytes: Option<&[u8]>,
    ) -> io::Result<Option<String>> {
        let Some(bytes) = bytes else {
            return Ok(Some(format!(
                "The icon is larger than {} KB",
                MAX_BYTES / 1024
            )));
        };
        if bytes.is_empty() {
            return Ok(Some("The file is empty".into()));
        }
        let wanted = content_type
            .and_then(|t| t.split(';').next())
            .unwrap_or("")
            .trim();
        let Some(&(_, ext)) = TYPES.iter().find(|(t, _)| t.eq_ignore_ascii_case(wanted)) else {
            return Ok(Some("Only PNG, WEBP or SVG icons are supported".into()));
        };
        if !looks_valid(ext, bytes) {
            return Ok(Some(format!(
                "That does not look like a valid {} file",
                ext[1..].to_ascii_uppercase()
            )));
        }

        std::fs::create_dir_all(&self.dir)?;
        let target = self.path_for(id, ext);
        let tmp = crate::app_paths::with_suffix(&target, ".tmp");
        let mut cache = self.lock();
        // publish the replacement before removing other formats, so a failed write or move keeps the old icon
        let written = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, &target));
        let _ = std::fs::remove_file(&tmp);
        written?;
        self.remove_locked(id, Some(ext));
        // reads look for the formats in a fixed order, so an old file in another format that could not be deleted
        // (open elsewhere, on Windows) would still be served under the new revision: undo the change instead
        if TYPES
            .iter()
            .any(|(_, other)| *other != ext && is_file_like(&self.path_for(id, other)))
        {
            let _ = std::fs::remove_file(&target);
            return Ok(Some(
                "The previous icon is in use and could not be replaced. Try again in a moment."
                    .into(),
            ));
        }
        cache.insert(id.to_string(), content_revision(bytes));
        Ok(None)
    }

    /// Deletes a shortcut's custom icon, in whichever format it was saved.
    pub fn remove(&self, id: &str) {
        let mut cache = self.lock();
        self.remove_locked(id, None);
        cache.remove(id);
    }

    fn remove_locked(&self, id: &str, except: Option<&str>) {
        for (_, ext) in TYPES {
            if Some(ext) != except {
                let _ = std::fs::remove_file(self.path_for(id, ext));
            }
        }
    }

    /// Deletes uploads for shortcuts that no longer exist (deleted, or never saved past "New shortcut").
    pub fn cleanup<I: IntoIterator<Item = String>>(&self, live_ids: impl FnOnce() -> I) {
        if !self.dir.is_dir() {
            return;
        }
        let mut cache = self.lock();
        // read inside the lock, not before it: a shortcut added and its icon uploaded between an outdated snapshot and
        // this lock would otherwise look dead to a save that started before either of them
        let keep: HashSet<String> = live_ids().into_iter().collect();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                continue; // files only, as Directory.EnumerateFiles
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            // never an orphaned icon: an upload in progress under save's lock, or dead weight left by a process that
            // crashed mid-write, harmless either way
            if name.to_ascii_lowercase().ends_with(".tmp") {
                continue;
            }
            let id = match name.rfind('.') {
                Some(dot) => &name[..dot],
                None => name.as_str(),
            };
            if keep.contains(id) {
                continue;
            }
            let _ = std::fs::remove_file(&path);
            cache.remove(id);
        }
    }

    fn path_for(&self, id: &str, ext: &str) -> PathBuf {
        self.dir.join(format!("{}{ext}", safe_name(id)))
    }
}

/// `File.Exists`: anything there that is not a folder.
fn is_file_like(path: &std::path::Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| !m.is_dir())
}

fn content_revision(bytes: &[u8]) -> i64 {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(bytes);
    i64::from_le_bytes(hash[..8].try_into().expect("a SHA-256 is 32 bytes"))
}

/// Shortcut ids are already slugs (`[a-z0-9-]+`, see the config's normalization); this is a last defense against
/// writing outside the icons folder from an id that does not match any real shortcut. Letters, digits, `-` and `_`
/// stay; anything else is `_`, one per UTF-16 unit as in .NET (a character outside the BMP is two).
fn safe_name(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for c in id.chars() {
        if c.len_utf16() == 2 {
            out.push_str("__");
        } else if c.is_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

fn looks_valid(ext: &str, b: &[u8]) -> bool {
    match ext {
        ".png" => b.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]),
        ".webp" => b.len() >= 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP",
        ".svg" => is_svg(b),
        _ => false,
    }
}

/// A well-formed XML document whose root is `svg` in no namespace or the SVG namespace. A DTD is allowed and never
/// fetched (the C# agent ignores it).
fn is_svg(bytes: &[u8]) -> bool {
    let Some(text) = decode_xml(bytes) else {
        return false;
    };
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let Ok(doc) = roxmltree::Document::parse_with_options(&text, options) else {
        return false;
    };
    let root = doc.root_element().tag_name();
    root.name() == "svg" && matches!(root.namespace(), None | Some("http://www.w3.org/2000/svg"))
}

/// The document's text: UTF-8 (with or without a byte order mark), or UTF-16 with one.
fn decode_xml(bytes: &[u8]) -> Option<String> {
    let utf16 = |rest: &[u8], unit: fn([u8; 2]) -> u16| {
        let (pairs, odd) = rest.as_chunks::<2>();
        if !odd.is_empty() {
            return None;
        }
        String::from_utf16(&pairs.iter().map(|p| unit(*p)).collect::<Vec<_>>()).ok()
    };
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8(rest.to_vec()).ok();
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, u16::from_be_bytes);
    }
    String::from_utf8(bytes.to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Shortcut;
    use crate::shortcut_icons;

    const PNG: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0];
    const WEBP: &[u8] = b"RIFF\0\0\0\0WEBP";
    const SVG: &[u8] = b"<svg xmlns='http://www.w3.org/2000/svg'></svg>";

    fn uploads() -> (tempfile::TempDir, ShortcutIconUploads) {
        let dir = tempfile::tempdir().unwrap();
        let uploads = ShortcutIconUploads::new(dir.path().join("icons"));
        (dir, uploads)
    }

    #[test]
    fn nothing_uploaded_reads_as_null() {
        let (_d, u) = uploads();
        assert!(u.read("terminal").is_none());
    }

    #[test]
    fn exists_agrees_with_read() {
        let (_d, u) = uploads();
        assert!(!u.exists("terminal"));
        assert_eq!(
            u.save("terminal", Some("image/png"), Some(PNG)).unwrap(),
            None
        );
        assert!(u.exists("terminal"));
    }

    #[test]
    fn revision_changes_when_the_upload_is_replaced() {
        let (_d, u) = uploads();
        assert_eq!(u.revision("terminal"), None);
        assert_eq!(
            u.save("terminal", Some("image/png"), Some(PNG)).unwrap(),
            None
        );
        let first = u.revision("terminal");
        assert!(first.is_some());

        assert_eq!(
            u.save("terminal", Some("image/svg+xml"), Some(SVG))
                .unwrap(),
            None
        );
        assert_ne!(first, u.revision("terminal"));

        u.remove("terminal");
        assert_eq!(u.revision("terminal"), None);
    }

    #[test]
    fn the_revision_is_the_first_eight_bytes_of_the_sha256_little_endian() {
        let (_d, u) = uploads();
        u.save("terminal", Some("image/png"), Some(PNG)).unwrap();
        use sha2::Digest;
        let hash = sha2::Sha256::digest(PNG);
        let expected = i64::from_le_bytes(hash[..8].try_into().unwrap());
        assert_eq!(u.revision("terminal"), Some(expected));
        // and the same from disk, for an icon saved by an earlier process
        let again = ShortcutIconUploads::new(u.dir.clone());
        assert_eq!(again.revision("terminal"), Some(expected));
    }

    #[test]
    fn an_upload_only_request_never_falls_back_to_the_program_icon() {
        let (_d, u) = uploads();
        // a real file as the command, so the program's own icon exists wherever the platform can read one
        let exe = std::env::current_exe().unwrap();
        let sc = Shortcut {
            id: "terminal".into(),
            cmd: exe.to_string_lossy().into_owned(),
            ..Default::default()
        };
        assert!(shortcut_icons::picture(&sc, &u, true).is_none());
        if shortcut_icons::icon(&sc).is_some() {
            assert_eq!(
                shortcut_icons::picture(&sc, &u, false).unwrap().1,
                "image/png"
            );
        }

        assert_eq!(
            u.save("terminal", Some("image/svg+xml"), Some(SVG))
                .unwrap(),
            None
        );
        assert_eq!(
            shortcut_icons::picture(&sc, &u, true).unwrap().1,
            "image/svg+xml"
        );
        assert_eq!(
            shortcut_icons::picture(&sc, &u, false).unwrap().1,
            "image/svg+xml"
        );

        // removed after the phone read its config: the upload-only request finds nothing, not the program icon
        u.remove("terminal");
        assert!(shortcut_icons::picture(&sc, &u, true).is_none());
    }

    #[test]
    fn a_valid_upload_of_each_supported_type_can_be_read_back() {
        for (content_type, bytes) in [
            ("image/png", PNG),
            ("image/webp", WEBP),
            ("image/svg+xml", SVG),
        ] {
            let (_d, u) = uploads();
            assert_eq!(
                u.save("terminal", Some(content_type), Some(bytes)).unwrap(),
                None
            );
            let (read, read_type) = u.read("terminal").unwrap();
            assert_eq!((read.as_slice(), read_type), (bytes, content_type));
        }
    }

    #[test]
    fn a_content_type_with_parameters_is_still_recognised() {
        let (_d, u) = uploads();
        assert_eq!(
            u.save("terminal", Some("image/png; charset=binary"), Some(PNG))
                .unwrap(),
            None
        );
        assert!(u.read("terminal").is_some());
        assert_eq!(
            u.save("other", Some(" IMAGE/WEBP "), Some(WEBP)).unwrap(),
            None
        );
        assert_eq!(u.read("other").unwrap().1, "image/webp");
    }

    #[test]
    fn unsupported_content_types_are_rejected() {
        let (_d, u) = uploads();
        let refused = Some("Only PNG, WEBP or SVG icons are supported".to_string());
        assert_eq!(
            u.save("terminal", Some("image/gif"), Some(&[1, 2, 3]))
                .unwrap(),
            refused
        );
        assert_eq!(u.save("terminal", None, Some(&[1, 2, 3])).unwrap(), refused);
    }

    #[test]
    fn content_that_does_not_match_its_declared_type_is_rejected() {
        let (_d, u) = uploads();
        assert_eq!(
            u.save("terminal", Some("image/png"), Some(b"not a png"))
                .unwrap(),
            Some("That does not look like a valid PNG file".into())
        );
        assert_eq!(
            u.save("terminal", Some("image/svg+xml"), Some(PNG))
                .unwrap(),
            Some("That does not look like a valid SVG file".into())
        );
        assert_eq!(
            u.save("terminal", Some("image/webp"), Some(b"RIFF\0\0\0\0WEBX"))
                .unwrap(),
            Some("That does not look like a valid WEBP file".into())
        );
        assert!(u.read("terminal").is_none()); // the rejected upload left no file behind
    }

    #[test]
    fn a_file_that_cannot_be_rendered_as_svg_does_not_replace_an_existing_icon() {
        for text in [
            "<?xml version='1.0'?><note>Office</note>",
            "<svg xmlns='http://www.w3.org/2000/svg'><rect></svg>",
            "<svg xmlns='http://example.com/other'></svg>",
            "<svg xmlns='http://www.w3.org/2000/svg'></svg><svg/>",
        ] {
            let (_d, u) = uploads();
            assert_eq!(
                u.save("terminal", Some("image/png"), Some(PNG)).unwrap(),
                None
            );
            let revision = u.revision("terminal");
            assert!(
                u.save("terminal", Some("image/svg+xml"), Some(text.as_bytes()))
                    .unwrap()
                    .unwrap()
                    .contains("SVG"),
                "{text}"
            );
            assert_eq!(u.read("terminal").unwrap().0, PNG);
            assert_eq!(u.revision("terminal"), revision);
        }
    }

    #[test]
    fn an_svg_can_start_with_a_comment() {
        for text in [
            "<!-- Icon -->\n<svg xmlns='http://www.w3.org/2000/svg' />",
            "\u{feff}<svg/>",
            "<?xml version=\"1.0\"?>\n<!DOCTYPE svg PUBLIC \"-//W3C//DTD SVG 1.1//EN\" \
             \"http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd\">\n<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
        ] {
            let (_d, u) = uploads();
            assert_eq!(
                u.save("terminal", Some("image/svg+xml"), Some(text.as_bytes()))
                    .unwrap(),
                None,
                "{text}"
            );
            assert_eq!(u.read("terminal").unwrap().0, text.as_bytes());
        }
    }

    #[test]
    fn a_png_with_a_corrupted_signature_is_rejected() {
        let (_d, u) = uploads();
        assert!(
            u.save(
                "terminal",
                Some("image/png"),
                Some(&[0x89, 0x50, 0x4E, 0x47, 0, 0, 0, 0])
            )
            .unwrap()
            .unwrap()
            .contains("PNG")
        );
    }

    #[test]
    fn an_empty_file_is_rejected() {
        let (_d, u) = uploads();
        assert_eq!(
            u.save("terminal", Some("image/png"), Some(&[])).unwrap(),
            Some("The file is empty".into())
        );
    }

    #[test]
    fn too_large_an_upload_is_rejected() {
        let (_d, u) = uploads();
        assert_eq!(
            u.save("terminal", Some("image/png"), None).unwrap(),
            Some(format!("The icon is larger than {} KB", MAX_BYTES / 1024))
        );
        assert_eq!(MAX_BYTES / 1024, 512);
    }

    #[tokio::test]
    async fn a_body_is_read_up_to_the_limit_and_no_further() {
        let exactly = vec![7u8; MAX_BYTES];
        let read = ShortcutIconUploads::read_bounded(axum::body::Body::from(exactly.clone()))
            .await
            .unwrap();
        assert_eq!(read, Some(exactly));
        let over = vec![7u8; MAX_BYTES + 1];
        let read = ShortcutIconUploads::read_bounded(axum::body::Body::from(over))
            .await
            .unwrap();
        assert_eq!(read, None);
        let empty = ShortcutIconUploads::read_bounded(axum::body::Body::empty())
            .await
            .unwrap();
        assert_eq!(empty, Some(vec![]));
    }

    #[test]
    fn uploading_again_in_a_different_format_replaces_the_old_one() {
        let (_d, u) = uploads();
        u.save("terminal", Some("image/png"), Some(PNG)).unwrap();
        u.save("terminal", Some("image/svg+xml"), Some(SVG))
            .unwrap();
        let (bytes, content_type) = u.read("terminal").unwrap();
        assert_eq!((bytes.as_slice(), content_type), (SVG, "image/svg+xml"));
        assert!(!u.dir.join("terminal.png").exists());
    }

    #[test]
    fn a_failed_replacement_keeps_the_existing_icon_and_its_revision() {
        let (_d, u) = uploads();
        assert_eq!(
            u.save("terminal", Some("image/png"), Some(PNG)).unwrap(),
            None
        );
        let revision = u.revision("terminal");
        let destination = u.dir.join("terminal.svg");
        std::fs::create_dir(&destination).unwrap();

        assert!(
            u.save("terminal", Some("image/svg+xml"), Some(SVG))
                .is_err()
        );

        let (bytes, content_type) = u.read("terminal").unwrap();
        assert_eq!((bytes.as_slice(), content_type), (PNG, "image/png"));
        assert_eq!(u.revision("terminal"), revision);
        assert!(!u.dir.join("terminal.svg.tmp").exists());

        std::fs::remove_dir(&destination).unwrap();
        assert_eq!(
            u.save("terminal", Some("image/svg+xml"), Some(SVG))
                .unwrap(),
            None
        );
        assert_eq!(u.read("terminal").unwrap().0, SVG);
    }

    #[test]
    fn remove_deletes_whichever_format_was_saved() {
        let (_d, u) = uploads();
        u.save("terminal", Some("image/webp"), Some(WEBP)).unwrap();
        u.remove("terminal");
        assert!(u.read("terminal").is_none());
    }

    #[test]
    fn remove_of_a_never_uploaded_icon_is_harmless() {
        let (_d, u) = uploads();
        u.remove("never-uploaded");
    }

    #[test]
    fn cleanup_drops_uploads_for_shortcuts_that_no_longer_exist() {
        let (_d, u) = uploads();
        u.save("keep-me", Some("image/png"), Some(PNG)).unwrap();
        u.save("drop-me", Some("image/png"), Some(PNG)).unwrap();
        std::fs::write(u.dir.join("busy.png.tmp"), b"in progress").unwrap();
        u.cleanup(|| vec!["keep-me".to_string()]);
        assert!(u.read("keep-me").is_some());
        assert!(u.read("drop-me").is_none());
        assert_eq!(u.revision("drop-me"), None);
        // an upload in progress (or left by a crash) is never taken for an orphan
        assert!(u.dir.join("busy.png.tmp").exists());
    }

    #[test]
    fn cleanup_on_a_folder_that_was_never_created_does_nothing() {
        let (d, _u) = uploads();
        ShortcutIconUploads::new(d.path().join("never-created"))
            .cleanup(|| vec!["anything".to_string()]);
        assert!(!d.path().join("never-created").exists());
    }

    #[test]
    fn an_id_with_path_characters_cannot_escape_the_icons_folder() {
        let (d, u) = uploads();
        assert_eq!(
            u.save("../escape", Some("image/png"), Some(PNG)).unwrap(),
            None
        );
        assert!(!d.path().join("escape.png").exists()); // did not escape upward
        assert!(u.dir.join("___escape.png").exists()); // written safely under the folder, as a sanitised name
        assert!(u.read("../escape").is_some());
    }

    /// Windows: an open handle can block deleting the old PNG while the new WEBP is moved in. Reads prefer the PNG, so
    /// the change must be undone instead of reporting the WEBP's revision while the PNG is served.
    #[cfg(windows)]
    #[test]
    fn a_format_change_that_cannot_retire_the_old_file_keeps_the_old_icon() {
        use std::os::windows::fs::OpenOptionsExt;
        let (_d, u) = uploads();
        assert_eq!(u.save("sc", Some("image/png"), Some(PNG)).unwrap(), None);
        let revision = u.revision("sc");
        {
            // FILE_SHARE_READ only: no delete while it is open
            let _open = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(u.dir.join("sc.png"))
                .unwrap();
            assert!(
                u.save("sc", Some("image/webp"), Some(WEBP))
                    .unwrap()
                    .is_some()
            );
        }
        assert_eq!(u.read("sc").map(|r| r.1), Some("image/png"));
        assert_eq!(u.revision("sc"), revision);
        assert!(!u.dir.join("sc.webp").exists());

        assert_eq!(u.save("sc", Some("image/webp"), Some(WEBP)).unwrap(), None); // nothing holds it open now
        assert_eq!(u.read("sc").map(|r| r.1), Some("image/webp"));
    }
}
