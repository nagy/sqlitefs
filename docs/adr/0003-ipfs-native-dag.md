# IPFS-native DAG: UnixFS dag-pb directories, v1-2025 file layout

All filesystem structure is encoded as IPFS-compatible DAG objects. Directories use UnixFS dag-pb nodes. Files use the v1-2025 profile (1 MiB raw leaf blocks, balanced intermediate dag-pb nodes). Commits use dag-cbor. The entire DAG is content-addressed with CIDv1 + BLAKE3.

**Why**: Being IPFS-native means every object in the repository is addressable on the public IPFS network. If local data is corrupted, chunks can be re-fetched from public gateways or peers. The v1-2025 profile was chosen over v0-2015 because raw leaves are simpler and 1 MiB block size is a better match for modern storage and CDC-backed chunk assembly.

**Trade-off**: dag-pb directories are not optimized for large flat directories — changing one entry rewrites the entire directory object. UnixFS has no native support for xattrs or hardlinks. We accept the directory rewrite cost (mitigated by CDC dedup in the chunk store) and defer xattrs/hardlinks.

**Alternative considered**: Custom DAG with richer metadata (inode numbers, xattrs, delta-friendly directory objects). Rejected to stay compatible with the IPFS ecosystem and avoid reinventing a content-addressing scheme for which mature tooling exists.
