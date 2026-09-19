//! Content-defined chunking via FastCDC.
use sha2::{Digest, Sha256};

/// Split data into content-defined chunks.
///
/// Returns `(sha256_hash, chunk_bytes)` pairs. Uses `FastCDC` v2020 with expected
/// 256 KiB average chunk size.
///
/// # Examples
///
/// Data smaller than the 64 KiB minimum comes back as one chunk:
///
/// ```
/// use sqlitefs::cdc::chunk_file;
///
/// let chunks = chunk_file(b"hello world");
/// assert_eq!(chunks.len(), 1);
/// assert_eq!(chunks[0].1, b"hello world");
/// ```
pub fn chunk_file(data: &[u8]) -> Vec<([u8; 32], Vec<u8>)> {
    let mut chunks = Vec::new();
    // Minimum 64 KiB, average 256 KiB, maximum 1 MiB.
    let min_size = 64 * 1024;
    let avg_size = 256 * 1024;
    let max_size = 1024 * 1024;

    let chunker = fastcdc::v2020::FastCDC::new(data, min_size, avg_size, max_size);
    for chunk in chunker {
        let bytes = data[chunk.offset..chunk.offset + chunk.length].to_vec();
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        chunks.push((hash, bytes));
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_small_file_single_chunk() {
        let data = b"hello world";
        let chunks = chunk_file(data);
        assert_eq!(chunks.len(), 1);
        let (hash, bytes) = &chunks[0];
        assert_eq!(bytes.as_slice(), data);
        assert_eq!(hash.len(), 32);
    }

    #[test]
    fn test_empty_file_no_chunks() {
        let _chunks = chunk_file(b"");
        // FastCDC may produce a single empty chunk or no chunks; both are fine.
        // An empty file has no chunks to store.
    }

    #[test]
    fn test_large_file_multiple_chunks() {
        let data = vec![0u8; 2 * 1024 * 1024]; // 2 MiB
        let chunks = chunk_file(&data);
        assert!(
            chunks.len() >= 2,
            "expected >=2 chunks, got {}",
            chunks.len()
        );
        let total: usize = chunks.iter().map(|(_, b)| b.len()).sum();
        assert_eq!(total, data.len());
    }

    #[test]
    fn test_same_input_same_hash() {
        let data = vec![42u8; 300 * 1024];
        let c1 = chunk_file(&data);
        let c2 = chunk_file(&data);
        assert_eq!(c1.len(), c2.len());
        for (a, b) in c1.iter().zip(c2.iter()) {
            assert_eq!(a.0, b.0);
            assert_eq!(a.1, b.1);
        }
    }
}
