#![expect(
    clippy::indexing_slicing,
    clippy::missing_panics_doc,
    clippy::unwrap_used,
    reason = "test support: a panic fails the test that called it, and is its report"
)]

/// The data of a chunked HTTP/1.1 body, up to its last chunk.
#[must_use]
pub fn dechunk(mut rest: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(line) = rest.windows(2).position(|window| window == b"\r\n") {
        let size =
            usize::from_str_radix(std::str::from_utf8(&rest[..line]).unwrap().trim(), 16).unwrap();
        if size == 0 {
            break;
        }
        out.extend_from_slice(&rest[line + 2..line + 2 + size]);
        rest = &rest[line + 2 + size + 2..];
    }
    out
}
