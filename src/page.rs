//! Page layouts. Every page is [`PAGE_SIZE`] bytes and starts with its kind.
//!
//! Node pages (leaves and branches) are slotted: a 16-byte header, then a sorted array of 2-byte entry
//! offsets, with the entries themselves packed from the end of the page backwards.
//!
//! ```text
//! header   kind u8 | 0 | count u16 | 0 u32 | leftmost child u64 (branches only)
//! leaf     key length u16 | value length u32 (top bit: overflow) | key | value, or overflow page u64
//! branch   key length u16 | child u64 | key           (the child holds keys >= this key)
//! overflow kind u8 | 0 | used u16 | 0 u32 | next page u64 | data
//! freelist kind u8 | 0 | count u16 | 0 u32 | next page u64 | (freeing transaction u64, page u64) × count
//! ```
//!
//! A branch's leftmost child holds the keys below its first key.

pub const PAGE_SIZE: usize = 4096;
pub const HEADER: usize = 16;

pub const KIND_LEAF: u8 = 1;
pub const KIND_BRANCH: u8 = 2;
pub const KIND_OVERFLOW: u8 = 3;
pub const KIND_FREELIST: u8 = 4;

/// Longest key, in bytes. Two entries of the largest size still fit a page.
pub const MAX_KEY: usize = 1024;

/// Longest value kept in its leaf; longer ones go to a chain of overflow pages.
pub const MAX_INLINE_VALUE: usize = 1024;

pub const OVERFLOW_DATA: usize = PAGE_SIZE - HEADER;
pub const FREELIST_ENTRIES: usize = (PAGE_SIZE - HEADER) / 16;

const OVERFLOW_FLAG: u32 = 1 << 31;

pub type PageId = u64;

#[inline]
fn u16_at(page: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([page[at], page[at + 1]])
}

#[inline]
fn u32_at(page: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(page[at..at + 4].try_into().unwrap())
}

#[inline]
pub fn u64_at(page: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(page[at..at + 8].try_into().unwrap())
}

#[inline]
pub fn kind(page: &[u8]) -> u8 {
    page[0]
}

#[inline]
pub fn count(page: &[u8]) -> usize {
    u16_at(page, 2) as usize
}

#[inline]
fn entry_offset(page: &[u8], i: usize) -> usize {
    u16_at(page, HEADER + 2 * i) as usize
}

/// A value as a leaf holds it: the bytes, or where its overflow chain starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueRef<'a> {
    Inline(&'a [u8]),
    Overflow { page: PageId, len: u32 },
}

#[inline]
pub fn leaf_key(page: &[u8], i: usize) -> &[u8] {
    let at = entry_offset(page, i);
    let key_len = u16_at(page, at) as usize;
    &page[at + 6..at + 6 + key_len]
}

#[inline]
pub fn leaf_value(page: &[u8], i: usize) -> ValueRef<'_> {
    let at = entry_offset(page, i);
    let key_len = u16_at(page, at) as usize;
    let value_len = u32_at(page, at + 2);
    let start = at + 6 + key_len;
    if value_len & OVERFLOW_FLAG != 0 {
        ValueRef::Overflow { page: u64_at(page, start), len: value_len & !OVERFLOW_FLAG }
    } else {
        ValueRef::Inline(&page[start..start + value_len as usize])
    }
}

#[inline]
pub fn leftmost(page: &[u8]) -> PageId {
    u64_at(page, 8)
}

#[inline]
pub fn branch_key(page: &[u8], i: usize) -> &[u8] {
    let at = entry_offset(page, i);
    let key_len = u16_at(page, at) as usize;
    &page[at + 10..at + 10 + key_len]
}

#[inline]
pub fn branch_child(page: &[u8], i: usize) -> PageId {
    u64_at(page, entry_offset(page, i) + 2)
}

/// In a leaf: Ok(index) of the key, or Err(index where it would go).
pub fn leaf_search(page: &[u8], key: &[u8]) -> Result<usize, usize> {
    let (mut low, mut high) = (0, count(page));
    while low < high {
        let mid = (low + high) / 2;
        match leaf_key(page, mid).cmp(key) {
            std::cmp::Ordering::Less => low = mid + 1,
            std::cmp::Ordering::Greater => high = mid,
            std::cmp::Ordering::Equal => return Ok(mid),
        }
    }
    Err(low)
}

/// In a branch: which child holds `key`, as a child slot: 0 is the leftmost child, i + 1 the child of entry i.
pub fn branch_slot(page: &[u8], key: &[u8]) -> usize {
    // the number of entries whose key is <= key
    let (mut low, mut high) = (0, count(page));
    while low < high {
        let mid = (low + high) / 2;
        if branch_key(page, mid) <= key {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    low
}

/// The child in a branch's child slot (see [`branch_slot`]).
#[inline]
pub fn branch_slot_child(page: &[u8], slot: usize) -> PageId {
    if slot == 0 { leftmost(page) } else { branch_child(page, slot - 1) }
}

// ---- in-memory nodes, for writing ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Inline(Vec<u8>),
    Overflow { page: PageId, len: u32 },
}

#[derive(Clone, Debug)]
pub enum Node {
    Leaf(Vec<(Vec<u8>, Value)>),
    Branch { leftmost: PageId, entries: Vec<(Vec<u8>, PageId)> },
}

impl Node {
    pub fn decode(page: &[u8]) -> Node {
        let n = count(page);
        match kind(page) {
            KIND_LEAF => Node::Leaf(
                (0..n)
                    .map(|i| {
                        let value = match leaf_value(page, i) {
                            ValueRef::Inline(bytes) => Value::Inline(bytes.to_vec()),
                            ValueRef::Overflow { page, len } => Value::Overflow { page, len },
                        };
                        (leaf_key(page, i).to_vec(), value)
                    })
                    .collect(),
            ),
            KIND_BRANCH => Node::Branch {
                leftmost: leftmost(page),
                entries: (0..n).map(|i| (branch_key(page, i).to_vec(), branch_child(page, i))).collect(),
            },
            other => panic!("not a node page (kind {other})"),
        }
    }

    pub fn entry_size_leaf(key: &[u8], value: &Value) -> usize {
        2 + 6 + key.len() + match value {
            Value::Inline(bytes) => bytes.len(),
            Value::Overflow { .. } => 8,
        }
    }

    pub fn entry_size_branch(key: &[u8]) -> usize {
        2 + 10 + key.len()
    }

    /// Bytes this node needs as a page.
    pub fn size(&self) -> usize {
        HEADER
            + match self {
                Node::Leaf(entries) => entries.iter().map(|(k, v)| Self::entry_size_leaf(k, v)).sum::<usize>(),
                Node::Branch { entries, .. } => entries.iter().map(|(k, _)| Self::entry_size_branch(k)).sum::<usize>(),
            }
    }

    pub fn len(&self) -> usize {
        match self {
            Node::Leaf(entries) => entries.len(),
            Node::Branch { entries, .. } => entries.len(),
        }
    }

    pub fn encode(&self, page: &mut [u8]) {
        debug_assert!(self.size() <= PAGE_SIZE, "node too large for a page");
        page.fill(0);
        let mut end = PAGE_SIZE;
        match self {
            Node::Leaf(entries) => {
                page[0] = KIND_LEAF;
                page[2..4].copy_from_slice(&(entries.len() as u16).to_le_bytes());
                for (i, (key, value)) in entries.iter().enumerate() {
                    let size = Self::entry_size_leaf(key, value) - 2;
                    end -= size;
                    page[HEADER + 2 * i..HEADER + 2 * i + 2].copy_from_slice(&(end as u16).to_le_bytes());
                    page[end..end + 2].copy_from_slice(&(key.len() as u16).to_le_bytes());
                    page[end + 6..end + 6 + key.len()].copy_from_slice(key);
                    let start = end + 6 + key.len();
                    match value {
                        Value::Inline(bytes) => {
                            page[end + 2..end + 6].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
                            page[start..start + bytes.len()].copy_from_slice(bytes);
                        }
                        Value::Overflow { page: first, len } => {
                            page[end + 2..end + 6].copy_from_slice(&(len | OVERFLOW_FLAG).to_le_bytes());
                            page[start..start + 8].copy_from_slice(&first.to_le_bytes());
                        }
                    }
                }
            }
            Node::Branch { leftmost, entries } => {
                page[0] = KIND_BRANCH;
                page[2..4].copy_from_slice(&(entries.len() as u16).to_le_bytes());
                page[8..16].copy_from_slice(&leftmost.to_le_bytes());
                for (i, (key, child)) in entries.iter().enumerate() {
                    let size = Self::entry_size_branch(key) - 2;
                    end -= size;
                    page[HEADER + 2 * i..HEADER + 2 * i + 2].copy_from_slice(&(end as u16).to_le_bytes());
                    page[end..end + 2].copy_from_slice(&(key.len() as u16).to_le_bytes());
                    page[end + 2..end + 10].copy_from_slice(&child.to_le_bytes());
                    page[end + 10..end + 10 + key.len()].copy_from_slice(key);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_round_trip_through_pages() {
        let leaf = Node::Leaf(vec![
            (b"apple".to_vec(), Value::Inline(b"red".to_vec())),
            (b"banana".to_vec(), Value::Overflow { page: 42, len: 5000 }),
            (b"cherry".to_vec(), Value::Inline(vec![])),
        ]);
        let mut page = vec![0u8; PAGE_SIZE];
        leaf.encode(&mut page);
        assert_eq!(leaf_search(&page, b"banana"), Ok(1));
        assert_eq!(leaf_search(&page, b"blueberry"), Err(2));
        assert_eq!(leaf_value(&page, 0), ValueRef::Inline(b"red"));
        assert_eq!(leaf_value(&page, 1), ValueRef::Overflow { page: 42, len: 5000 });
        assert!(matches!(Node::decode(&page), Node::Leaf(e) if e.len() == 3));

        let branch = Node::Branch { leftmost: 7, entries: vec![(b"m".to_vec(), 8), (b"t".to_vec(), 9)] };
        branch.encode(&mut page);
        assert_eq!(branch_slot_child(&page, branch_slot(&page, b"a")), 7);
        assert_eq!(branch_slot_child(&page, branch_slot(&page, b"m")), 8);
        assert_eq!(branch_slot_child(&page, branch_slot(&page, b"s")), 8);
        assert_eq!(branch_slot_child(&page, branch_slot(&page, b"zz")), 9);
    }
}
