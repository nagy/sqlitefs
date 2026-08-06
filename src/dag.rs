// dag.rs — IPLD DAG construction for sqlitefs

use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// CIDv1: <version=0x01> <multicodec-varint> <multihash>
// multihash: <sha2-256=0x12> <length=0x20> <32-byte-digest>
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub const RAW: u64 = 0x55;
pub const DAG_PB: u64 = 0x70;
pub const DAG_CBOR: u64 = 0x71;
pub const SHA256_CODE: u8 = 0x12;
pub const SHA256_LEN: u8 = 0x20;
#[allow(dead_code)]
pub const CID_PREFIX_LEN: usize = 4; // version + codec varint + mh-code + mh-len

/// RFC 4648 base32 alphabet (lowercase, no padding).
const BASE32: [char; 32] = [
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z', '2', '3', '4', '5', '6', '7',
];

/// CIDv1 with sha2-256 and a given codec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cid {
    codec: u64,
    hash: [u8; 32],
}

impl Cid {
    #[allow(dead_code)]
    pub fn from_raw(codec: u64, hash: [u8; 32]) -> Self {
        Self { codec, hash }
    }

    pub fn hash(codec: u64, data: &[u8]) -> Self {
        let hash: [u8; 32] = Sha256::digest(data).into();
        Self { codec, hash }
    }

    pub fn hash_dag_pb(data: &[u8]) -> Self {
        Self::hash(DAG_PB, data)
    }

    #[allow(dead_code)]
    pub fn hash_raw(data: &[u8]) -> Self {
        Self::hash(RAW, data)
    }

    pub fn hash_dag_cbor(data: &[u8]) -> Self {
        Self::hash(DAG_CBOR, data)
    }

    #[allow(dead_code)]
    pub fn codec(&self) -> u64 {
        self.codec
    }

    pub fn hash_bytes(&self) -> &[u8; 32] {
        &self.hash
    }

    /// CIDv1 string, base32 (multibase prefix 'b'), no padding — the standard
    /// human-readable form (e.g. `bafyrei…`). Algorithm matches RFC 4648.
    pub fn to_string_base32(&self) -> String {
        base32_encode(&self.to_bytes())
    }

    /// Full CID bytes: 0x01 <codec-varint> 0x12 0x20 <32 hash bytes>
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = vec![0x01];
        encode_varint(self.codec, &mut buf);
        buf.push(SHA256_CODE);
        buf.push(SHA256_LEN);
        buf.extend_from_slice(&self.hash);
        buf
    }
}

fn encode_varint(mut v: u64, buf: &mut Vec<u8>) {
    while v >= 0x80 {
        buf.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

/// RFC 4648 base32, multibase prefix 'b', no padding.
fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(60);
    out.push('b');
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &byte in data {
        buf = (buf << 8) | byte as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[(buf >> bits) as usize & 0x1f]);
        }
    }
    if bits > 0 {
        out.push(BASE32[(buf << (5 - bits)) as usize & 0x1f]);
    }
    out
}

// ---------------------------------------------------------------------------
// dag-pb: UnixFS directory node
// ---------------------------------------------------------------------------

/// Build an empty UnixFS directory dag-pb node.
/// Returns (`encoded_dag_pb_bytes`, CID).
/// `PBNode` { Data: [0x08, 0x01] (UnixFS Type=Directory), Links: [] }
pub fn empty_dir() -> (Vec<u8>, Cid) {
    let data = encode_pb_node(&[0x08, 0x01], &[]);
    let cid = Cid::hash_dag_pb(&data);
    (data, cid)
}

/// Build a UnixFS directory dag-pb node with children.
/// Each link: (name, `cid_bytes`, tsize=0).
#[allow(dead_code)]
pub fn dir_with_links(links: &[(&str, &[u8])]) -> (Vec<u8>, Cid) {
    let link_data: Vec<PbLink> = links
        .iter()
        .map(|(name, cid)| PbLink {
            hash: cid.to_vec(),
            name: name.to_string(),
            tsize: 0,
        })
        .collect();
    let data = encode_pb_node(&[0x08, 0x01], &link_data);
    let cid = Cid::hash_dag_pb(&data);
    (data, cid)
}

struct PbLink {
    hash: Vec<u8>,
    name: String,
    tsize: u64,
}

// protobuf wire types
const WIRE_VARINT: u8 = 0;
const WIRE_LEN: u8 = 2;

fn pb_tag(field: u64, wire: u8) -> u64 {
    (field << 3) | u64::from(wire)
}

/// Encode a `PBNode`: Data (field 1, bytes) + Links (field 2, repeated message)
fn encode_pb_node(data: &[u8], links: &[PbLink]) -> Vec<u8> {
    let mut buf = Vec::new();

    // field 1: Data (bytes)
    encode_pb_bytes(1, data, &mut buf);

    // field 2: Links (repeated message)
    for link in links {
        let mut link_buf = Vec::new();
        // Hash (field 1 of PBLink)
        encode_pb_bytes(1, &link.hash, &mut link_buf);
        // Name (field 2 of PBLink)
        encode_pb_bytes(2, link.name.as_bytes(), &mut link_buf);
        // Tsize (field 3 of PBLink)
        encode_pb_varint(3, link.tsize, &mut link_buf);
        // Encode link message as length-delimited field 2 of PBNode
        encode_pb_wire_type(WIRE_LEN, pb_tag(2, WIRE_LEN), &mut buf);
        encode_varint(link_buf.len() as u64, &mut buf);
        buf.extend_from_slice(&link_buf);
    }

    buf
}

fn encode_pb_bytes(field: u64, data: &[u8], buf: &mut Vec<u8>) {
    if data.is_empty() {
        return;
    }
    encode_pb_wire_type(WIRE_LEN, pb_tag(field, WIRE_LEN), buf);
    encode_varint(data.len() as u64, buf);
    buf.extend_from_slice(data);
}

fn encode_pb_varint(field: u64, value: u64, buf: &mut Vec<u8>) {
    if value == 0 {
        return; // default, skip
    }
    encode_pb_wire_type(WIRE_VARINT, pb_tag(field, WIRE_VARINT), buf);
    encode_varint(value, buf);
}

fn encode_pb_wire_type(_wire: u8, tag: u64, buf: &mut Vec<u8>) {
    encode_varint(tag, buf);
}

// ---------------------------------------------------------------------------
// dag-cbor: minimal CBOR encoder for commit objects
// ---------------------------------------------------------------------------

/// Encode a dag-cbor map: {parent: `null_or_cid`, tree: cid, timestamp: u64}
/// `parent_cid` = None encodes as CBOR null.
///
/// Canonical dag-cbor: map keys sorted by (length, byte-order) — tree (4),
/// parent (6), timestamp (9) — and CIDs as tag-42 byte strings (RFC 7049 §3.9,
/// IPLD dag-cbor spec). Bare bytes would parse but produce a non-canonical CID.
pub fn encode_commit(parent_cid: Option<&[u8]>, tree_cid: &[u8], timestamp: u64) -> (Vec<u8>, Cid) {
    let mut buf = Vec::new();
    // CBOR map with 3 items
    let num_items = if parent_cid.is_some() { 3 } else { 2 };
    buf.push(0xa0 | num_items);

    // Keys in canonical order: "tree" (4) < "parent" (6) < "timestamp" (9)

    // "tree" (4 bytes)
    cbor_string("tree", &mut buf);
    cbor_cid(tree_cid, &mut buf);

    if let Some(pcid) = parent_cid {
        // "parent" (6 bytes)
        cbor_string("parent", &mut buf);
        cbor_cid(pcid, &mut buf);
    }

    // "timestamp" (9 bytes)
    cbor_string("timestamp", &mut buf);
    // positive integer
    if timestamp <= 23 {
        buf.push(timestamp as u8);
    } else if timestamp <= 0xff {
        buf.push(0x18);
        buf.push(timestamp as u8);
    } else if timestamp <= 0xffff {
        buf.push(0x19);
        buf.extend_from_slice(&(timestamp as u16).to_be_bytes());
    } else if timestamp <= 0xffff_ffff {
        buf.push(0x1a);
        buf.extend_from_slice(&(timestamp as u32).to_be_bytes());
    } else {
        buf.push(0x1b);
        buf.extend_from_slice(&timestamp.to_be_bytes());
    }

    let cid = Cid::hash_dag_cbor(&buf);
    (buf, cid)
}

/// Encode a CID as a dag-cbor tagged byte string: tag 42 (0xd8 0x2a) + bytes.
fn cbor_cid(data: &[u8], buf: &mut Vec<u8>) {
    buf.push(0xd8);
    buf.push(0x2a);
    cbor_bytes(data, buf);
}

fn cbor_string(s: &str, buf: &mut Vec<u8>) {
    let len = s.len() as u64;
    if len <= 23 {
        buf.push(0x60 | len as u8);
    } else {
        buf.push(0x78);
        buf.push(len as u8);
    }
    buf.extend_from_slice(s.as_bytes());
}

fn cbor_bytes(data: &[u8], buf: &mut Vec<u8>) {
    let len = data.len() as u64;
    if len <= 23 {
        buf.push(0x40 | len as u8);
    } else if len <= 0xff {
        buf.push(0x58);
        buf.push(len as u8);
    } else if len <= 0xffff {
        buf.push(0x59);
        buf.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        buf.push(0x5a);
        buf.extend_from_slice(&(len as u32).to_be_bytes());
    }
    buf.extend_from_slice(data);
}

// ---------------------------------------------------------------------------
// file manifest — CBOR array of chunk hashes
// ---------------------------------------------------------------------------

/// Encode a file manifest: CBOR array [<size-bytestring>, <mode-bytestring>, hash1..hashN].
/// The first element is the file size encoded as a byte string (1–8 bytes, never
/// 32, so it is distinguishable from a chunk hash); the second is the mode as a
/// byte string (1–4 bytes, never 32, and 0 for the default 0644 so old files
/// stay canonical). Chunk hashes are 32-byte byte strings. Embedding size+mode
/// means `stat()` never has to fetch chunk data, and a chmod changes the
/// manifest CID (so it's part of the content-addressed tree).
pub fn encode_file_manifest(chunk_hashes: &[[u8; 32]], size: u64, mode: u32) -> (Vec<u8>, Cid) {
    let mut buf = Vec::new();
    let n = chunk_hashes.len() + 2; // +2 for the size and mode elements
    if n <= 23 {
        buf.push(0x80 | n as u8);
    } else {
        buf.push(0x98);
        buf.push(n as u8);
    }
    // Size as a byte string: encode the u64 in its minimal big-endian form.
    let size_bytes = size.to_be_bytes();
    let lead = size_bytes.iter().position(|&b| b != 0).unwrap_or(7);
    cbor_bytes(&size_bytes[lead..], &mut buf);
    // Mode as a byte string (minimal BE). 0 → 0644 (canonical for unset).
    let m = if mode == 0 { 0o644 } else { mode & 0o7777 };
    let m_bytes = m.to_be_bytes();
    let m_lead = m_bytes.iter().position(|&b| b != 0).unwrap_or(7);
    cbor_bytes(&m_bytes[m_lead..], &mut buf);
    for hash in chunk_hashes {
        cbor_bytes(hash, &mut buf);
    }
    let cid = Cid::hash_dag_cbor(&buf);
    (buf, cid)
}

// ---------------------------------------------------------------------------
// symlink — dag-pb node with UnixFS type Symlink (0x04)
// ---------------------------------------------------------------------------

/// Encode a symlink as a dag-pb node. Data = [0x08, 0x04] + target bytes.
/// No links. CIDv1 dag-pb.
pub fn encode_symlink(target: &str) -> (Vec<u8>, Cid) {
    let mut unixfs = vec![0x08, 0x04];
    unixfs.extend_from_slice(target.as_bytes());
    let data = encode_pb_node(&unixfs, &[]);
    let cid = Cid::hash_dag_pb(&data);
    (data, cid)
}

/// Decode a symlink dag-pb node, returning the target path.
/// Returns None if the node is not a symlink.
pub fn decode_symlink(data: &[u8]) -> Option<String> {
    // dag-pb: field 1 (Data) = 0x0a <len> <unixfs_type: 0x08, 0x04> <target...>
    if data.len() < 5 || data[0] != 0x0a {
        return None;
    }
    let data_len = data[1] as usize;
    if data.len() < 2 + data_len || data_len < 2 || &data[2..4] != b"\x08\x04" {
        return None;
    }
    let target = &data[4..2 + data_len];
    Some(String::from_utf8_lossy(target).into_owned())
}

/// Peek at a chunk data blob and return true if it's a dag-pb directory node.
pub fn is_dir_dag(data: &[u8]) -> bool {
    // dag-pb: field 1 (Data) = 0x0a <len> <unixfs_type: 0x08, 0x01>
    data.len() >= 4 && data[0] == 0x0a && data[1] >= 2 && data[2] == 0x08 && data[3] == 0x01
}

/// Decode the Links of a dag-pb node into (name, cid_bytes) pairs.
/// Returns None on parse failure. Matches the format produced by `dir_with_links`:
/// repeated field 2 (0x12) messages, each containing hash (field 1), name
/// (field 2), tsize (field 3).
pub fn decode_dir_links(data: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let mut pos = 0usize;
    let mut links = Vec::new();
    while pos < data.len() {
        let tag = *data.get(pos)?;
        let field = tag >> 3;
        let wire = tag & 0x07;
        pos += 1;
        match (field, wire) {
            (2, 2) => {
                // length-delimited link message
                let (len, adv) = decode_uvarint(data.get(pos..)?)?;
                pos += adv;
                let msg = data.get(pos..pos + len)?;
                pos += len;
                let mut hash: Option<Vec<u8>> = None;
                let mut name: String = String::new();
                let mut mp = 0usize;
                while mp < msg.len() {
                    let mtag = *msg.get(mp)?;
                    let mfield = mtag >> 3;
                    let mwire = mtag & 0x07;
                    mp += 1;
                    match (mfield, mwire) {
                        (1, 2) => {
                            let (l, a) = decode_uvarint(msg.get(mp..)?)?;
                            mp += a;
                            hash = Some(msg.get(mp..mp + l)?.to_vec());
                            mp += l;
                        }
                        (2, 2) => {
                            let (l, a) = decode_uvarint(msg.get(mp..)?)?;
                            mp += a;
                            name = String::from_utf8_lossy(msg.get(mp..mp + l)?).into_owned();
                            mp += l;
                        }
                        (3, 0) => {
                            let (_, a) = decode_uvarint(msg.get(mp..)?)?;
                            mp += a;
                        }
                        _ => return None,
                    }
                }
                links.push((name, hash?));
            }
            (1, 2) => {
                // Data field — skip
                let (len, adv) = decode_uvarint(data.get(pos..)?)?;
                pos += adv + len;
            }
            _ => return None,
        }
    }
    Some(links)
}

fn decode_uvarint(data: &[u8]) -> Option<(usize, usize)> {
    let mut v: usize = 0;
    let mut shift = 0u32;
    for (i, &b) in data.iter().take(10).enumerate() {
        v |= ((b & 0x7f) as usize) << shift;
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
        shift += 7;
    }
    None
}

/// Peek at a chunk data blob and return true if it's a file manifest (dag-cbor array).
pub fn is_file_manifest(data: &[u8]) -> bool {
    !data.is_empty() && data[0] >= 0x80 && data[0] <= 0x9f
}

/// (size, chunk hashes, mode) from a file manifest. `size` is None for the old
/// all-hash format; `mode` is None for formats without an embedded mode.
pub type FileManifest = (Option<u64>, Vec<[u8; 32]>, Option<u32>);

/// Parse a file manifest CBOR array. See `FileManifest` for the shape.
/// Returns None on parse failure.
pub fn decode_file_manifest(data: &[u8]) -> Option<FileManifest> {
    if data.is_empty() {
        return None;
    }
    let (count, mut pos): (usize, usize) = match data[0] {
        b @ 0x80..=0x97 => ((b - 0x80) as usize, 1),
        0x98 => (data.get(1).copied()? as usize, 2),
        _ => return None,
    };
    let mut size = None;
    let mut mode = None;
    let mut hashes = Vec::with_capacity(count);
    for i in 0..count {
        let tag = *data.get(pos)?;
        let (len, tag_len): (usize, usize) = match tag {
            0x58 => (*data.get(pos + 1)? as usize, 2),
            0x59 => {
                let l = u16::from_be_bytes([*data.get(pos + 1)?, *data.get(pos + 2)?]) as usize;
                (l, 3)
            }
            0x40..=0x57 => ((tag - 0x40) as usize, 1),
            0x5a => {
                let l = u32::from_be_bytes([
                    *data.get(pos + 1)?,
                    *data.get(pos + 2)?,
                    *data.get(pos + 3)?,
                    *data.get(pos + 4)?,
                ]) as usize;
                (l, 5)
            }
            _ => return None,
        };
        let start = pos + tag_len;
        if i == 0 && len != 32 {
            // Size: first element is a 1–8 byte string.
            if len > 8 {
                return None;
            }
            let mut sz = [0u8; 8];
            sz[8 - len..].copy_from_slice(data.get(start..start + len)?);
            size = Some(u64::from_be_bytes(sz));
        } else if i == 1 && len != 32 && size.is_some() {
            // Mode: second element is a 1–4 byte string (only when the first
            // was the size — old all-hash format never has a second element).
            if len > 4 {
                return None;
            }
            let mut m = [0u8; 4];
            m[4 - len..].copy_from_slice(data.get(start..start + len)?);
            let m = u32::from_be_bytes(m);
            mode = Some(if m == 0 { 0o644 } else { m & 0o7777 });
        } else {
            if len != 32 {
                return None;
            }
            let mut hash = [0u8; 32];
            hash.copy_from_slice(data.get(start..start + 32)?);
            hashes.push(hash);
        }
        pos = start + len;
    }
    Some((size, hashes, mode))
}

/// Re-encode an existing manifest with a new mode, returning the new bytes and CID.
/// Used by chmod: the mode lives in the manifest, so changing it must produce a
/// new manifest chunk (and thus a new CID the tree links). Falls back to
/// encoding from (size, hashes) when the old format is all-hash (size unknown).
pub fn reencode_manifest_with_mode(old_data: &[u8], new_mode: u32) -> Option<(Vec<u8>, Cid)> {
    let (size, hashes, _) = decode_file_manifest(old_data)?;
    let size = size?; // old all-hash format can't re-encode (size unknown)
    Some(encode_file_manifest(&hashes, size, new_mode))
}

// ---------------------------------------------------------------------------
// v1-2025: UnixFS file DAG with 1 MiB raw leaves
// ---------------------------------------------------------------------------

const V1_CHUNK_SIZE: usize = 1024 * 1024; // 1 MiB
const UNIXFS_FILE: &[u8] = &[0x08, 0x02];

/// One block in a v1-2025 DAG.
#[derive(Clone)]
pub struct V1Block {
    pub data: Vec<u8>,
    pub cid: Cid,
    /// Byte offset within the file (0 for non-leaf dag-pb nodes).
    pub file_offset: u64,
}

/// All blocks produced during DAG construction.
pub struct V1Dag {
    pub blocks: Vec<V1Block>,
    pub root: Cid,
}

/// Build a v1-2025 UnixFS DAG for a file's worth of bytes.
/// The physical CDC chunks must already be concatenated into `data`.
pub fn build_v1_2025_dag(data: &[u8]) -> V1Dag {
    if data.is_empty() {
        let r = Cid::hash_raw(b"");
        return V1Dag {
            blocks: vec![V1Block {
                data: vec![],
                cid: r.clone(),
                file_offset: 0,
            }],
            root: r,
        };
    }

    let total = data.len() as u64;

    // Leaf level: split into 1 MiB raw blocks with file offsets.
    let leaf_blocks: Vec<V1Block> = data
        .chunks(V1_CHUNK_SIZE)
        .enumerate()
        .map(|(i, chunk)| {
            let cid = Cid::hash_raw(chunk);
            V1Block {
                data: chunk.to_vec(),
                cid,
                file_offset: (i * V1_CHUNK_SIZE) as u64,
            }
        })
        .collect();

    if leaf_blocks.len() == 1 {
        let b = &leaf_blocks[0];
        return V1Dag {
            blocks: vec![b.clone()],
            root: b.cid.clone(),
        };
    }

    // Build balanced tree of dag-pb intermediate nodes.
    let mut blocks: Vec<V1Block> = leaf_blocks.clone();
    let mut current: Vec<(Vec<u8>, Cid)> = leaf_blocks
        .iter()
        .map(|b| (b.data.clone(), b.cid.clone()))
        .collect();

    while current.len() > 1 {
        let mut next_level = Vec::new();
        for group in current.chunks(128) {
            let links: Vec<PbLink> = group
                .iter()
                .map(|(_, cid)| PbLink {
                    hash: cid.to_bytes(),
                    name: String::new(),
                    tsize: total,
                })
                .collect();

            let mut unixfs_data = Vec::from(UNIXFS_FILE);
            encode_uvarint(total, &mut unixfs_data);
            let node_data = encode_pb_node(&unixfs_data, &links);
            let node_cid = Cid::hash_dag_pb(&node_data);
            blocks.push(V1Block {
                data: node_data.clone(),
                cid: node_cid.clone(),
                file_offset: 0,
            });
            next_level.push((node_data, node_cid));
        }
        current = next_level;
    }

    let (_, root_cid) = current.into_iter().next().unwrap();
    V1Dag {
        blocks,
        root: root_cid,
    }
}

/// Encode a uint64 as a varint into the buffer.
fn encode_uvarint(value: u64, buf: &mut Vec<u8>) {
    let mut v = value;
    while v >= 0x80 {
        buf.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_dir_cid_is_stable() {
        let (data, cid) = empty_dir();
        // dag-pb of [0x08, 0x01] with no links: 0x0a 0x02 0x08 0x01
        assert_eq!(data, vec![0x0a, 0x02, 0x08, 0x01]);
        // CID prefix: 01 70 12 20
        let cid_bytes = cid.to_bytes();
        assert_eq!(&cid_bytes[0..4], &[0x01, 0x70, 0x12, 0x20]);
        assert_eq!(cid_bytes.len(), 4 + 32);
    }

    #[test]
    fn test_decode_dir_links_roundtrip() {
        let (data, _) = dir_with_links(&[
            ("a.txt", &[1u8; 36]),
            ("sub", &[2u8; 36]),
            ("empty", &[3u8; 36]),
        ]);
        let links = decode_dir_links(&data).unwrap();
        assert_eq!(links.len(), 3);
        assert_eq!(links[0], ("a.txt".to_string(), vec![1u8; 36]));
        assert_eq!(links[1], ("sub".to_string(), vec![2u8; 36]));
        assert_eq!(links[2], ("empty".to_string(), vec![3u8; 36]));
    }

    #[test]
    fn test_decode_dir_links_empty() {
        let (data, _) = empty_dir();
        let links = decode_dir_links(&data).unwrap();
        assert!(links.is_empty());
    }

    #[test]
    fn test_same_input_same_cid() {
        let (_, cid1) = empty_dir();
        let (_, cid2) = empty_dir();
        assert_eq!(cid1.to_bytes(), cid2.to_bytes());
    }

    /// RFC 4648 §10 test vectors against the raw base32 core, prefixed with 'b'
    /// (multibase base32, no padding).
    #[test]
    fn test_base32_rfc4648_vectors() {
        let cases = [
            (b"".as_slice(), "b"),
            (b"f", "bmy"),
            (b"fo", "bmzxq"),
            (b"foo", "bmzxw6"),
            (b"foob", "bmzxw6yq"),
            (b"fooba", "bmzxw6ytb"),
            (b"foobar", "bmzxw6ytboi"),
        ];
        for (input, want) in cases {
            assert_eq!(base32_encode(input), want, "input {input:?}");
        }
    }

    /// A real dag-cbor CID string: prefix `bafyrei`, 59 chars.
    #[test]
    fn test_base32_real_cid() {
        let (_, commit_cid) = encode_commit(None, &empty_dir().0, 1700000000);
        let s = commit_cid.to_string_base32();
        assert!(s.starts_with("bafyrei"));
        assert_eq!(s.len(), 59);
    }

    #[test]
    fn test_commit_roundtrip() {
        let (_tree_data, tree_cid) = empty_dir();
        let tree_bytes = tree_cid.to_bytes();
        let (cbor, commit_cid) = encode_commit(None, &tree_bytes, 1700000000);
        // CID prefix: 01 71 12 20
        let cid_bytes = commit_cid.to_bytes();
        assert_eq!(&cid_bytes[0..4], &[0x01, 0x71, 0x12, 0x20]);
        assert_eq!(cid_bytes.len(), 4 + 32);
        // Verify CBOR starts with map(2)
        assert_eq!(cbor[0] & 0xf0, 0xa0);
        // Canonical key order: tree (len 4) before parent (len 6) before timestamp (len 9).
        // With no parent, first key must be "tree". cbor[1] = 0x64 (text string len 4).
        assert_eq!(cbor[1], 0x64);
        assert_eq!(&cbor[2..6], b"tree");
        // tree CID is a tag-42 byte string: 0xd8 0x2a then byte-string length then CID bytes.
        // 36-byte CID > 23, so length is two bytes: 0x58 0x24.
        assert_eq!(cbor[6], 0xd8);
        assert_eq!(cbor[7], 0x2a);
        assert_eq!(cbor[8], 0x58);
        assert_eq!(cbor[9], 36); // 4-byte prefix + 32-byte hash
        assert_eq!(&cbor[10..10 + tree_bytes.len()], &tree_bytes[..]);
    }

    #[test]
    fn test_commit_key_order_with_parent() {
        let (_tree_data, tree_cid) = empty_dir();
        let tree_bytes = tree_cid.to_bytes();
        let (cbor, _) = encode_commit(Some(&tree_bytes), &tree_bytes, 1);
        // Canonical order: "tree" then "parent" then "timestamp".
        assert_eq!(cbor[1], 0x64);
        assert_eq!(&cbor[2..6], b"tree");
        // After tree key (1+1+4) + tag42 (2) + len (0x58 0x24 = 2) + cid (36),
        // next key is "parent".
        let parent_key_off = 1 + 1 + 4 + 2 + 2 + 36;
        assert_eq!(cbor[parent_key_off], 0x66); // text string len 6
        assert_eq!(&cbor[parent_key_off + 1..parent_key_off + 7], b"parent");
        assert_eq!(cbor[parent_key_off + 7], 0xd8);
        assert_eq!(cbor[parent_key_off + 8], 0x2a);
    }

    #[test]
    fn test_file_manifest_roundtrip() {
        let chunk = [42u8; 32];
        let (data, cid) = encode_file_manifest(&[chunk], 42, 0o755);
        let (size, hashes, mode) = decode_file_manifest(&data).unwrap();
        assert_eq!(size, Some(42));
        assert_eq!(mode, Some(0o755));
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0], chunk);
        assert_eq!(cid.codec(), DAG_CBOR);
    }

    #[test]
    fn test_file_manifest_multi_chunk() {
        let mut chunks = Vec::new();
        for i in 0..5 {
            let mut h = [0u8; 32];
            h[0] = i;
            chunks.push(h);
        }
        let (data, _) = encode_file_manifest(&chunks, 123456789, 0);
        let (size, decoded, mode) = decode_file_manifest(&data).unwrap();
        assert_eq!(size, Some(123456789));
        assert_eq!(mode, Some(0o644)); // 0 → canonical default
        assert_eq!(decoded.len(), 5);
        assert_eq!(decoded, chunks);
    }

    #[test]
    fn test_empty_file_manifest() {
        let (data, _) = encode_file_manifest(&[], 0, 0o644);
        let (size, decoded, mode) = decode_file_manifest(&data).unwrap();
        assert_eq!(size, Some(0));
        assert_eq!(mode, Some(0o644));
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_old_format_manifest_no_size() {
        // Old format: array of 32-byte hashes only, no leading size element.
        // First element is a 32-byte hash → size = None, all are hashes.
        let chunk = [7u8; 32];
        let mut buf = vec![0x81]; // array(1)
        cbor_bytes(&chunk, &mut buf);
        let (size, hashes, mode) = decode_file_manifest(&buf).unwrap();
        assert_eq!(size, None);
        assert_eq!(mode, None);
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0], chunk);
    }

    #[test]
    fn test_symlink_roundtrip() {
        let target = "/some/path/to/target";
        let (data, cid) = encode_symlink(target);
        assert_eq!(cid.codec(), DAG_PB);
        let decoded = decode_symlink(&data).unwrap();
        assert_eq!(decoded, target);
    }

    #[test]
    fn test_symlink_not_a_file_manifest() {
        let (data, _) = encode_symlink("foo");
        assert!(!is_file_manifest(&data));
    }

    #[test]
    fn test_v1_2025_empty() {
        let dag = build_v1_2025_dag(b"");
        assert_eq!(dag.blocks.len(), 1);
        assert_eq!(dag.blocks[0].data.len(), 0);
        assert_eq!(dag.blocks[0].cid.codec(), RAW);
    }

    #[test]
    fn test_v1_2025_small_file() {
        let data = b"hello";
        let dag = build_v1_2025_dag(data);
        assert_eq!(dag.blocks.len(), 1);
        assert_eq!(dag.blocks[0].data, data);
        assert_eq!(dag.blocks[0].file_offset, 0);
        assert_eq!(dag.blocks[0].cid.codec(), RAW);
        // Same input, same CID.
        let dag2 = build_v1_2025_dag(data);
        assert_eq!(dag.root.to_bytes(), dag2.root.to_bytes());
    }

    #[test]
    fn test_v1_2025_multi_block() {
        let data = vec![0x42u8; 3 * 1024 * 1024]; // 3 MiB → 3 raw leaves + intermediates
        let dag = build_v1_2025_dag(&data);
        // At least 3 leaf blocks.
        let leaves: Vec<_> = dag.blocks.iter().filter(|b| b.cid.codec() == RAW).collect();
        assert_eq!(leaves.len(), 3);
        assert_eq!(leaves[0].file_offset, 0);
        assert_eq!(leaves[1].file_offset, 1 * 1024 * 1024);
        assert_eq!(leaves[2].file_offset, 2 * 1024 * 1024);
        // Root is dag-pb.
        assert_eq!(dag.root.codec(), DAG_PB);
    }
}
