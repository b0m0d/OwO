//! Streaming, hash-verified UTF-8 pages and bounded prefixes for immutable CAS objects.
//! The whole object is verified on each call; only the requested page or prefix is retained.
use super::CasStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};

const BUFFER_BYTES: usize = 64 * 1024;
pub const MAX_PAGE_BYTES: usize = 64 * 1024;
pub const MAX_PREFIX_BYTES: usize = 256 * 1024;
/// Maximum text retained for a single all-at-once integrity-checked CAS read.
pub const MAX_FULL_TEXT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CasTextPage {
    pub content: String,
    pub offset_bytes: u64,
    pub next_offset_bytes: u64,
    pub total_bytes: u64,
    pub sha256: String,
    pub eof: bool,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "CAS text read cancelled")
}

impl CasStore {
    /// Offset must be a UTF-8 boundary; max_bytes=0 verifies without retaining text.
    /// Returned content never exceeds 64 KiB. Legacy get/get_text remain unchanged.
    pub fn read_text_page(
        &self,
        hash: &str,
        offset_bytes: u64,
        max_bytes: usize,
    ) -> io::Result<CasTextPage> {
        self.read_text_page_with_cancel(hash, offset_bytes, max_bytes, || false)
    }

    /// Read a hash-verified prefix in one pass while retaining at most 256 KiB.
    pub fn read_text_prefix(&self, hash: &str, max_bytes: usize) -> io::Result<CasTextPage> {
        self.read_text_prefix_with_cancel(hash, max_bytes, || false)
    }

    /// Read one complete UTF-8 CAS text object with a hard retained-memory ceiling.
    /// Oversized objects fail closed; callers that need them must use paginated reads.
    pub fn read_text_all(&self, hash: &str, max_bytes: usize) -> io::Result<CasTextPage> {
        self.read_text_all_with_cancel(hash, max_bytes, || false)
    }

    /// Cancellable complete read for validation paths that must inspect the whole text.
    /// The object is hashed in one pass and no truncated prefix is returned as success.
    pub fn read_text_all_with_cancel(
        &self,
        hash: &str,
        max_bytes: usize,
        should_cancel: impl Fn() -> bool,
    ) -> io::Result<CasTextPage> {
        if hash.len() != 64
            || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            || max_bytes > MAX_FULL_TEXT_BYTES
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid CAS hash or full text read size",
            ));
        }
        let path = self.dir.join(hash);
        let entry = std::fs::symlink_metadata(&path)?;
        if !entry.is_file() || entry.file_type().is_symlink() {
            return Err(invalid("CAS entry is not a regular file"));
        }
        if entry.len() > max_bytes as u64 {
            return Err(invalid("CAS text object exceeds its full-read size limit"));
        }
        let page =
            self.read_text_range_with_cancel(hash, 0, max_bytes, max_bytes, should_cancel)?;
        if !page.eof {
            return Err(invalid("CAS text object exceeds its full-read size limit"));
        }
        Ok(page)
    }

    /// Prefix reads keep bounded memory but hash the full immutable object once.
    pub fn read_text_prefix_with_cancel(
        &self,
        hash: &str,
        max_bytes: usize,
        should_cancel: impl Fn() -> bool,
    ) -> io::Result<CasTextPage> {
        self.read_text_range_with_cancel(hash, 0, max_bytes, MAX_PREFIX_BYTES, should_cancel)
    }

    /// Cancellation is checked before opening and between bounded read blocks.
    /// No runtime, Agent, policy or HTTP dependency is introduced into the kernel.
    pub fn read_text_page_with_cancel(
        &self,
        hash: &str,
        offset_bytes: u64,
        max_bytes: usize,
        should_cancel: impl Fn() -> bool,
    ) -> io::Result<CasTextPage> {
        self.read_text_range_with_cancel(
            hash,
            offset_bytes,
            max_bytes,
            MAX_PAGE_BYTES,
            should_cancel,
        )
    }

    fn read_text_range_with_cancel(
        &self,
        hash: &str,
        offset_bytes: u64,
        max_bytes: usize,
        max_retained_bytes: usize,
        should_cancel: impl Fn() -> bool,
    ) -> io::Result<CasTextPage> {
        if hash.len() != 64
            || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            || max_bytes > max_retained_bytes
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid CAS hash or text read size",
            ));
        }
        if should_cancel() {
            return Err(cancelled());
        }
        let path = self.dir.join(hash);
        let entry = std::fs::symlink_metadata(&path)?;
        if !entry.is_file() || entry.file_type().is_symlink() {
            return Err(invalid("CAS entry is not a regular file"));
        }
        let mut file = File::open(path)?;
        let expected_bytes = file.metadata()?.len();
        if offset_bytes > expected_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "offset exceeds CAS object length",
            ));
        }
        let page_end = offset_bytes.saturating_add(max_bytes as u64);
        let mut buffer = vec![0u8; BUFFER_BYTES + 4];
        let mut carry = 0usize;
        let mut total_bytes = 0u64;
        let mut hasher = Sha256::new();
        let mut content =
            String::with_capacity(max_bytes.min(expected_bytes.min(usize::MAX as u64) as usize));
        let mut offset_checked = offset_bytes == 0;
        loop {
            if should_cancel() {
                return Err(cancelled());
            }
            let count = file.read(&mut buffer[carry..carry + BUFFER_BYTES])?;
            if count == 0 {
                if carry != 0 {
                    return Err(invalid("CAS text ends with incomplete UTF-8"));
                }
                break;
            }
            hasher.update(&buffer[carry..carry + count]);
            let chunk_start = total_bytes
                .checked_sub(carry as u64)
                .ok_or_else(|| invalid("CAS byte counter underflow"))?;
            total_bytes = total_bytes
                .checked_add(count as u64)
                .ok_or_else(|| invalid("CAS byte counter overflow"))?;
            let combined = carry + count;
            let valid = match std::str::from_utf8(&buffer[..combined]) {
                Ok(_) => combined,
                Err(error) if error.error_len().is_none() => error.valid_up_to(),
                Err(_) => return Err(invalid("CAS object is not valid UTF-8")),
            };
            let text = std::str::from_utf8(&buffer[..valid])
                .map_err(|_| invalid("invalid UTF-8 prefix"))?;
            let chunk_end = chunk_start + valid as u64;
            if offset_bytes >= chunk_start && offset_bytes <= chunk_end {
                let local = (offset_bytes - chunk_start) as usize;
                if !text.is_char_boundary(local) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "offset is inside a UTF-8 character",
                    ));
                }
                offset_checked = true;
            }
            if max_bytes > 0 && page_end > chunk_start && offset_bytes < chunk_end {
                let begin = offset_bytes.saturating_sub(chunk_start) as usize;
                let mut end = page_end.saturating_sub(chunk_start).min(valid as u64) as usize;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                if begin < end {
                    content.push_str(&text[begin..end]);
                }
            }
            carry = combined - valid;
            if carry > 3 {
                return Err(invalid("invalid UTF-8 continuation size"));
            }
            buffer.copy_within(valid..combined, 0);
        }
        if should_cancel() {
            return Err(cancelled());
        }
        if total_bytes != expected_bytes || file.metadata()?.len() != expected_bytes {
            return Err(invalid("CAS object changed length during read"));
        }
        let actual = format!("{:x}", hasher.finalize());
        if actual != hash {
            return Err(invalid(
                "CAS object hash does not match its content reference",
            ));
        }
        if offset_bytes == total_bytes {
            offset_checked = true;
        }
        if !offset_checked {
            return Err(invalid("CAS offset was not a verified UTF-8 boundary"));
        }
        if max_bytes > 0 && content.is_empty() && offset_bytes < total_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "page budget is too small for the next UTF-8 character",
            ));
        }
        let next_offset_bytes = offset_bytes + content.len() as u64;
        Ok(CasTextPage {
            content,
            offset_bytes,
            next_offset_bytes,
            total_bytes,
            sha256: actual,
            eof: next_offset_bytes == total_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;

    struct Fixture {
        dir: PathBuf,
        store: CasStore,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("owo-cas-text-{}", uuid::Uuid::new_v4()));
            let store = CasStore::new(dir.clone()).unwrap();
            Self { dir, store }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Owned UUID directory created by this fixture, never a workspace path.
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
    #[test]
    fn pages_reconstruct_unicode_object_across_hash_buffer_boundaries() {
        let fixture = Fixture::new();
        let text = format!("{}😀{}", "x".repeat(BUFFER_BYTES - 1), "中文".repeat(14000));
        let hash = fixture.store.put(text.as_bytes()).unwrap();
        let mut offset = 0;
        let mut output = String::new();
        loop {
            let page = fixture.store.read_text_page(&hash, offset, 4097).unwrap();
            assert!(page.content.len() <= 4097);
            assert_eq!(page.sha256, hash);
            assert_eq!(page.total_bytes, text.len() as u64);
            assert_eq!(page.offset_bytes, offset);
            output.push_str(&page.content);
            if page.eof {
                assert_eq!(page.next_offset_bytes, text.len() as u64);
                break;
            }
            assert!(page.next_offset_bytes > offset);
            offset = page.next_offset_bytes;
        }
        assert_eq!(output, text);
    }
    #[test]
    fn full_text_read_is_hash_verified_and_rejects_truncation_or_oversize() {
        let fixture = Fixture::new();
        let text = "工作区证据😀".repeat(32);
        let hash = fixture.store.put(text.as_bytes()).unwrap();
        let full = fixture
            .store
            .read_text_all(&hash, text.len())
            .expect("object within the explicit bound");
        assert_eq!(full.content, text);
        assert!(full.eof);
        assert_eq!(full.sha256, hash);

        assert_eq!(
            fixture
                .store
                .read_text_all(&hash, text.len() - 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            fixture
                .store
                .read_text_all(&hash, MAX_FULL_TEXT_BYTES + 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn full_text_limit_rejects_large_object_from_metadata_before_reading() {
        let fixture = Fixture::new();
        let hash = "a".repeat(64);
        std::fs::File::create(fixture.dir.join(&hash))
            .unwrap()
            .set_len(MAX_FULL_TEXT_BYTES as u64 + 1)
            .unwrap();
        assert_eq!(
            fixture
                .store
                .read_text_all(&hash, MAX_FULL_TEXT_BYTES)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let small = fixture.store.put(b"ok").unwrap();
        assert_eq!(
            fixture
                .store
                .read_text_all_with_cancel(&small, 8, || true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn bounded_prefix_retains_more_than_one_page_but_hashes_full_object() {
        let fixture = Fixture::new();
        let text = "中文😀".repeat(40_000);
        let hash = fixture.store.put(text.as_bytes()).unwrap();
        let prefix = fixture.store.read_text_prefix(&hash, 200 * 1024).unwrap();
        assert!(prefix.content.len() <= 200 * 1024);
        assert!(text.starts_with(&prefix.content));
        assert_eq!(prefix.sha256, hash);
        assert_eq!(prefix.total_bytes, text.len() as u64);
        assert!(!prefix.eof);
        assert!(fixture
            .store
            .read_text_prefix(&hash, MAX_PREFIX_BYTES + 1)
            .is_err());
    }

    #[test]
    fn offset_boundary_empty_object_eof_and_verify_only_are_explicit() {
        let fixture = Fixture::new();
        let hash = fixture.store.put("中文".as_bytes()).unwrap();
        assert_eq!(
            fixture
                .store
                .read_text_page(&hash, 1, 10)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            fixture
                .store
                .read_text_page(&hash, 0, 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(fixture.store.read_text_page(&hash, 7, 10).is_err());
        let eof = fixture.store.read_text_page(&hash, 6, 10).unwrap();
        assert!(eof.eof && eof.content.is_empty());
        let verified = fixture.store.read_text_page(&hash, 0, 0).unwrap();
        assert!(verified.content.is_empty() && !verified.eof);
        assert_eq!(verified.total_bytes, 6);
        let empty = fixture.store.put(b"").unwrap();
        assert!(fixture.store.read_text_page(&empty, 0, 10).unwrap().eof);
    }
    #[test]
    fn corruption_outside_requested_page_is_detected_before_returning_content() {
        let fixture = Fixture::new();
        let mut bytes = vec![b'x'; BUFFER_BYTES + 100];
        let hash = fixture.store.put(&bytes).unwrap();
        *bytes.last_mut().unwrap() = b'y';
        std::fs::write(fixture.dir.join(&hash), &bytes).unwrap();
        assert_eq!(
            fixture
                .store
                .read_text_page(&hash, 0, 16)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
    #[test]
    fn invalid_utf8_after_page_and_incomplete_tail_are_rejected() {
        let fixture = Fixture::new();
        for tail in [&[0xff][..], &[0xe4, 0xb8][..]] {
            let mut bytes = vec![b'x'; BUFFER_BYTES - 1];
            bytes.extend_from_slice(tail);
            let hash = fixture.store.put(&bytes).unwrap();
            assert_eq!(
                fixture
                    .store
                    .read_text_page(&hash, 0, 10)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
    #[test]
    fn callback_cancels_before_io_or_between_bounded_blocks() {
        let fixture = Fixture::new();
        let hash = fixture.store.put(&vec![b'x'; BUFFER_BYTES * 3]).unwrap();
        assert_eq!(
            fixture
                .store
                .read_text_page_with_cancel(&hash, 0, 10, || true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(
            fixture
                .store
                .read_text_prefix_with_cancel(&hash, 10, || true)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        let checks = Cell::new(0usize);
        let error = fixture
            .store
            .read_text_page_with_cancel(&hash, 0, 10, || {
                checks.set(checks.get() + 1);
                checks.get() >= 3
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(checks.get(), 3);
    }
    #[test]
    fn hash_path_escape_and_oversized_page_are_rejected() {
        let fixture = Fixture::new();
        for hash in ["../secret", "C:/secret", "abc"] {
            assert_eq!(
                fixture
                    .store
                    .read_text_page(hash, 0, 10)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        let hash = fixture.store.put(b"safe").unwrap();
        assert!(fixture
            .store
            .read_text_page(&hash, 0, MAX_PAGE_BYTES + 1)
            .is_err());
    }
}
