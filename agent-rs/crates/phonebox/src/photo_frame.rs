//! `PhotoFrame.cs` (the rest comes with the photo frame task): what kind of picture some bytes are.

/// `MimeOf`: JPEG, PNG, GIF or WebP by their first bytes; anything else (SVG, HTML…) is None.
pub fn mime_of(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() >= 12 && b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_type_comes_from_the_bytes_not_the_name() {
        assert_eq!(mime_of(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]), Some("image/jpeg"));
        assert_eq!(mime_of(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"), Some("image/png"));
        assert_eq!(mime_of(b"\x89PNG\r\n"), None); // the whole 8-byte signature
        assert_eq!(mime_of(b"GIF89a\x01\x00"), Some("image/gif"));
        assert_eq!(mime_of(b"GIF87a\x01\x00"), Some("image/gif"));
        assert_eq!(mime_of(b"RIFF\x24\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(
            mime_of(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"),
            None
        );
        assert_eq!(mime_of(b"<html><body></body></html>"), None);
        assert_eq!(mime_of(b""), None);
    }
}
