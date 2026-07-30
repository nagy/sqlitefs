# Dual chunking: FastCDC (physical) + UnixFS v1-2025 (virtual)

Files are physically stored as content-defined chunks (FastCDC, addressed by BLAKE3 hash). The IPFS-public identity of each file is a UnixFS v1-2025 DAG (1 MiB fixed-size blocks, raw leaves). A `block_index` table maps each virtual v1-2025 block CID to the physical CDC chunks that back it. Virtual blocks are assembled on demand by concatenating physical chunks; no duplicate storage.

**Why**: CDC chunking gives cross-version deduplication — inserting a byte at the start of a file changes only the first chunk. v1-2025 CIDs give IPFS compatibility — every file has a publicly addressable CID that matches what the IPFS network (Kubo, Boxo) produces, enabling data recovery from the public network.

**Trade-off**: The block index mapping must be rebuilt whenever a file changes (all v1-2025 boundaries shift), but this is metadata, not data — cheap to recompute. A file's public CID changes when any byte is inserted/deleted before the end, even if the physical storage is 99% deduplicated.

**Alternative considered**: CDC-only with custom CIDs, IPFS as export target. Rejected because the user wants IPFS CIDs to be natively addressable for public data recovery, not just an export format.
