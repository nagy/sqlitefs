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

// ---------------------------------------------------------------------------
// dag-pb: UnixFS directory node
// ---------------------------------------------------------------------------

/// Build an empty UnixFS directory dag-pb node.
/// Returns (encoded_dag_pb_bytes, CID).
/// PBNode { Data: [0x08, 0x01] (UnixFS Type=Directory), Links: [] }
pub fn empty_dir() -> (Vec<u8>, Cid) {
    let data = encode_pb_node(&[0x08, 0x01], &[]);
    let cid = Cid::hash_dag_pb(&data);
    (data, cid)
}

/// Build a UnixFS directory dag-pb node with children.
/// Each link: (name, cid_bytes, tsize=0).
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
    (field << 3) | wire as u64
}

/// Encode a PBNode: Data (field 1, bytes) + Links (field 2, repeated message)
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

/// Encode a dag-cbor map: {parent: null_or_cid, tree: cid, timestamp: u64}
/// parent_cid = None encodes as CBOR null.
pub fn encode_commit(parent_cid: Option<&[u8]>, tree_cid: &[u8], timestamp: u64) -> (Vec<u8>, Cid) {
    let mut buf = Vec::new();
    // CBOR map with 3 items
    let num_items = if parent_cid.is_some() { 3 } else { 2 };
    buf.push(0xa0 | num_items);

    if let Some(pcid) = parent_cid {
        // "parent" (6 bytes)
        cbor_string("parent", &mut buf);
        cbor_bytes(pcid, &mut buf);
    }

    // "tree" (4 bytes)
    cbor_string("tree", &mut buf);
    cbor_bytes(tree_cid, &mut buf);

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

/// Encode a file manifest: CBOR array of 32-byte chunk hash blobs.
pub fn encode_file_manifest(chunk_hashes: &[[u8; 32]]) -> (Vec<u8>, Cid) {
    let mut buf = Vec::new();
    let n = chunk_hashes.len();
    if n <= 23 {
        buf.push(0x80 | n as u8);
    } else {
        buf.push(0x98);
        buf.push(n as u8);
    }
    for hash in chunk_hashes {
        cbor_bytes(hash, &mut buf);
    }
    let cid = Cid::hash_dag_cbor(&buf);
    (buf, cid)
}

/// Parse a file manifest CBOR array, returning chunk hashes.
/// Returns None on parse failure.
pub fn decode_file_manifest(data: &[u8]) -> Option<Vec<[u8; 32]>> {
    if data.is_empty() {
        return None;
    }
    let (count, mut pos): (usize, usize) = match data[0] {
        b @ 0x80..=0x97 => ((b - 0x80) as usize, 1),
        0x98 => (data.get(1).copied()? as usize, 2),
        _ => return None,
    };
    let mut hashes = Vec::with_capacity(count);
    for _ in 0..count {
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
        if len != 32 {
            return None;
        }
        let start = pos + tag_len;
        let mut hash = [0u8; 32];
        hash.copy_from_slice(data.get(start..start + 32)?);
        hashes.push(hash);
        pos = start + 32;
    }
    Some(hashes)
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
    fn test_same_input_same_cid() {
        let (_, cid1) = empty_dir();
        let (_, cid2) = empty_dir();
        assert_eq!(cid1.to_bytes(), cid2.to_bytes());
    }

    #[test]
    fn test_commit_roundtrip() {
        let (_tree_data, tree_cid) = empty_dir();
        let (cbor, commit_cid) = encode_commit(None, &tree_cid.to_bytes(), 1700000000);
        // CID prefix: 01 71 12 20
        let cid_bytes = commit_cid.to_bytes();
        assert_eq!(&cid_bytes[0..4], &[0x01, 0x71, 0x12, 0x20]);
        assert_eq!(cid_bytes.len(), 4 + 32);
        // Verify CBOR starts with map(2)
        assert_eq!(cbor[0] & 0xf0, 0xa0);
    }

    #[test]
    fn test_file_manifest_roundtrip() {
        let chunk = [42u8; 32];
        let (data, cid) = encode_file_manifest(&[chunk]);
        let hashes = decode_file_manifest(&data).unwrap();
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
        let (data, _) = encode_file_manifest(&chunks);
        let decoded = decode_file_manifest(&data).unwrap();
        assert_eq!(decoded.len(), 5);
        assert_eq!(decoded, chunks);
    }

    #[test]
    fn test_empty_file_manifest() {
        let (data, _) = encode_file_manifest(&[]);
        assert_eq!(data, vec![0x80]); // CBOR empty array
        let decoded = decode_file_manifest(&data).unwrap();
        assert!(decoded.is_empty());
    }
}
