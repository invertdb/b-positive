//! Page layouts. Every page is [`PAGE_SIZE`] bytes and starts with its kind.
//!
//! Node pages (leaves and branches) are slotted: a 16-byte header, then a sorted array of 2-byte entry
//! offsets, with the entries themselves packed from the end of the page backwards.
//!
//! ```text
//! header   kind u8 | 0 | count u16 | entries start u16 | garbage u16 | leftmost child u64 (branches only)
//! leaf     key length u16 | value length u32 (top bit: overflow) | key | value, or overflow page u64
//! branch   key length u16 | child u64 | key           (the child holds keys >= this key)
//! overflow kind u8 | 0 | used u16 | 0 u32 | next page u64 | data
//! freelist kind u8 | 0 | count u16 | 0 u32 | next page u64 | (freeing transaction u64, page u64) × count
//! ```
//!
//! A branch's leftmost child holds the keys below its first key.
//!
//! "Entries start" is the lowest entry's offset and "garbage" the bytes of entries removed or replaced since the page
//! was last packed: a writer edits a page in place with them, and readers ignore them. (Pages written before they
//! existed hold 0 for both, and are packed, so the start is then found from the slots.)

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

// ---- editing a node page in place, for writing ---------------------------------------------------------------

/// A page in memory: a node a write transaction has copied (or made) and edits in place.
pub type Page = Box<[u8; PAGE_SIZE]>;

#[inline]
fn set_u16(page: &mut [u8], at: usize, value: usize) {
    page[at..at + 2].copy_from_slice(&(value as u16).to_le_bytes());
}

/// A new, empty node page of `kind`.
pub fn new_page(kind: u8) -> Page {
    let mut page: Page = vec![0u8; PAGE_SIZE].into_boxed_slice().try_into().unwrap();
    page[0] = kind;
    set_u16(&mut page[..], 4, PAGE_SIZE);
    page
}

/// A copy of a committed node page, to edit.
pub fn copy_page(page: &[u8]) -> Page {
    let mut copy: Page = page.to_vec().into_boxed_slice().try_into().unwrap();
    let start = entries_start(&copy[..]);
    set_u16(&mut copy[..], 4, start);
    copy
}

/// The lowest entry offset: where free space ends.
#[inline]
fn entries_start(page: &[u8]) -> usize {
    match u16_at(page, 4) as usize {
        0 => (0..count(page)).map(|i| entry_offset(page, i)).min().unwrap_or(PAGE_SIZE),
        start => start,
    }
}

#[inline]
fn garbage(page: &[u8]) -> usize {
    u16_at(page, 6) as usize
}

/// Bytes a node page needs (as [`Node::size`] counts them): header, slots and live entries.
#[inline]
pub fn used(page: &[u8]) -> usize {
    HEADER + 2 * count(page) + (PAGE_SIZE - entries_start(page)) - garbage(page)
}

/// The bytes of the entry at `at` (not its slot).
fn entry_len(page: &[u8], at: usize) -> usize {
    let key_len = u16_at(page, at) as usize;
    if kind(page) == KIND_BRANCH {
        10 + key_len
    } else {
        let value_len = u32_at(page, at + 2);
        6 + key_len + if value_len & OVERFLOW_FLAG != 0 { 8 } else { value_len as usize }
    }
}

/// Rewrites the entries back to back at the end of the page, so all its free space is in one piece.
fn pack(page: &mut [u8]) {
    let copy = page.to_vec();
    let mut end = PAGE_SIZE;
    for i in 0..count(page) {
        let at = entry_offset(&copy, i);
        let len = entry_len(&copy, at);
        end -= len;
        page[end..end + len].copy_from_slice(&copy[at..at + len]);
        set_u16(page, HEADER + 2 * i, end);
    }
    set_u16(page, 4, end);
    set_u16(page, 6, 0);
}

/// Room for an entry of `len` bytes and its slot, packing the page if its free space is in pieces: the entry's
/// offset, or none if the page can't hold it.
fn reserve(page: &mut [u8], len: usize) -> Option<usize> {
    if used(page) + 2 + len > PAGE_SIZE {
        return None;
    }
    if entries_start(page) < HEADER + 2 * (count(page) + 1) + len {
        pack(page);
    }
    let at = entries_start(page) - len;
    set_u16(page, 4, at);
    Some(at)
}

/// Puts a slot pointing at `at` in position `i`, after the slots before it.
fn insert_slot(page: &mut [u8], i: usize, at: usize) {
    let n = count(page);
    page.copy_within(HEADER + 2 * i..HEADER + 2 * n, HEADER + 2 * i + 2);
    set_u16(page, HEADER + 2 * i, at);
    set_u16(page, 2, n + 1);
}

fn value_len(value: &ValueRef) -> usize {
    match value {
        ValueRef::Inline(bytes) => bytes.len(),
        ValueRef::Overflow { .. } => 8,
    }
}

fn write_value(page: &mut [u8], at: usize, key_len: usize, value: &ValueRef) {
    let start = at + 6 + key_len;
    match *value {
        ValueRef::Inline(bytes) => {
            page[at + 2..at + 6].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
            page[start..start + bytes.len()].copy_from_slice(bytes);
        }
        ValueRef::Overflow { page: first, len } => {
            page[at + 2..at + 6].copy_from_slice(&(len | OVERFLOW_FLAG).to_le_bytes());
            page[start..start + 8].copy_from_slice(&first.to_le_bytes());
        }
    }
}

/// Inserts a leaf entry as entry `i`; false (and the page unchanged) if it doesn't fit.
pub fn leaf_insert(page: &mut [u8], i: usize, key: &[u8], value: &ValueRef) -> bool {
    let Some(at) = reserve(page, 6 + key.len() + value_len(value)) else { return false };
    set_u16(page, at, key.len());
    page[at + 6..at + 6 + key.len()].copy_from_slice(key);
    write_value(page, at, key.len(), value);
    insert_slot(page, i, at);
    true
}

/// Replaces leaf entry `i`'s value: in place if it is as long as the old one; false (and the page unchanged) if the
/// entry no longer fits.
pub fn leaf_set_value(page: &mut [u8], i: usize, value: &ValueRef) -> bool {
    let at = entry_offset(page, i);
    let key_len = u16_at(page, at) as usize;
    let old = entry_len(page, at);
    let new = 6 + key_len + value_len(value);
    if old == new {
        write_value(page, at, key_len, value);
        return true;
    }
    if used(page) - old + new > PAGE_SIZE {
        return false;
    }
    let key = page[at + 6..at + 6 + key_len].to_vec();
    remove_entry(page, i);
    leaf_insert(page, i, &key, value)
}

/// Removes entry `i` of a node page; its bytes become garbage until the page is packed.
pub fn remove_entry(page: &mut [u8], i: usize) {
    let n = count(page);
    let at = entry_offset(page, i);
    let len = entry_len(page, at);
    page.copy_within(HEADER + 2 * i + 2..HEADER + 2 * n, HEADER + 2 * i);
    set_u16(page, 2, n - 1);
    if n == 1 {
        set_u16(page, 4, PAGE_SIZE);
        set_u16(page, 6, 0);
    } else if at == entries_start(page) {
        set_u16(page, 4, at + len);
    } else {
        set_u16(page, 6, garbage(page) + len);
    }
}

/// Inserts a branch entry (separator and the child to its right) as entry `i`; false if it doesn't fit.
pub fn branch_insert(page: &mut [u8], i: usize, key: &[u8], child: PageId) -> bool {
    let Some(at) = reserve(page, 10 + key.len()) else { return false };
    set_u16(page, at, key.len());
    page[at + 2..at + 10].copy_from_slice(&child.to_le_bytes());
    page[at + 10..at + 10 + key.len()].copy_from_slice(key);
    insert_slot(page, i, at);
    true
}

/// Sets the child in a branch's child slot (0 is the leftmost; see [`branch_slot`]).
pub fn set_branch_child(page: &mut [u8], slot: usize, child: PageId) {
    let at = if slot == 0 { 8 } else { entry_offset(page, slot - 1) + 2 };
    page[at..at + 8].copy_from_slice(&child.to_le_bytes());
}

// ---- in-memory nodes, for writing ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Inline(Vec<u8>),
    Overflow { page: PageId, len: u32 },
}

impl Value {
    pub fn as_ref(&self) -> ValueRef<'_> {
        match self {
            Value::Inline(bytes) => ValueRef::Inline(bytes),
            Value::Overflow { page, len } => ValueRef::Overflow { page: *page, len: *len },
        }
    }
}

impl ValueRef<'_> {
    pub fn to_owned(&self) -> Value {
        match *self {
            ValueRef::Inline(bytes) => Value::Inline(bytes.to_vec()),
            ValueRef::Overflow { page, len } => Value::Overflow { page, len },
        }
    }
}

/// A node a write transaction changes. Normally its page, edited in place. A change the page can't hold (or a
/// batch merged in) turns it into its entries, as vectors, until the writer splits it back into pages.
#[derive(Clone, Debug)]
pub enum Node {
    Page(Page),
    Leaf(Vec<(Vec<u8>, Value)>),
    Branch { leftmost: PageId, entries: Vec<(Vec<u8>, PageId)> },
}

impl Node {
    pub fn is_branch(&self) -> bool {
        match self {
            Node::Page(page) => kind(&page[..]) == KIND_BRANCH,
            Node::Leaf(_) => false,
            Node::Branch { .. } => true,
        }
    }

    /// The node as entries (vectors), to change in ways its page can't.
    pub fn widen(&mut self) {
        if let Node::Page(page) = self {
            *self = Node::decode(&page[..]);
        }
    }

    /// The node as a page again, if it is entries that fit one.
    pub fn settle(&mut self) {
        if !matches!(self, Node::Page(_)) && self.size() <= PAGE_SIZE {
            let mut page = new_page(0);
            self.encode(&mut page[..]);
            *self = Node::Page(page);
        }
    }

    /// A branch's leftmost child (0 once its last child is removed).
    pub fn leftmost(&self) -> PageId {
        match self {
            Node::Page(page) => leftmost(&page[..]),
            Node::Branch { leftmost, .. } => *leftmost,
            Node::Leaf(_) => unreachable!("a leaf has no children"),
        }
    }

    /// A branch's child slot for `key`: the number of separators `<=` it.
    pub fn child_slot(&self, key: &[u8]) -> usize {
        match self {
            Node::Page(page) => branch_slot(&page[..], key),
            Node::Branch { entries, .. } => entries.partition_point(|(k, _)| k.as_slice() <= key),
            Node::Leaf(_) => unreachable!("a leaf has no children"),
        }
    }

    /// The child in a branch's child slot (0 is the leftmost).
    pub fn child(&self, slot: usize) -> PageId {
        match self {
            Node::Page(page) => branch_slot_child(&page[..], slot),
            Node::Branch { leftmost, entries } => if slot == 0 { *leftmost } else { entries[slot - 1].1 },
            Node::Leaf(_) => unreachable!("a leaf has no children"),
        }
    }

    pub fn set_child(&mut self, slot: usize, child: PageId) {
        match self {
            Node::Page(page) => set_branch_child(&mut page[..], slot, child),
            Node::Branch { leftmost, entries } => if slot == 0 { *leftmost = child } else { entries[slot - 1].1 = child },
            Node::Leaf(_) => unreachable!("a leaf has no children"),
        }
    }

    /// A branch's separator `i` (the lower bound of child slot `i + 1`), if it has one.
    pub fn separator(&self, i: usize) -> Option<Vec<u8>> {
        match self {
            Node::Page(page) => (i < count(&page[..])).then(|| branch_key(&page[..], i).to_vec()),
            Node::Branch { entries, .. } => entries.get(i).map(|(k, _)| k.clone()),
            Node::Leaf(_) => unreachable!("a leaf has no separators"),
        }
    }

    /// Inserts new children (each with the separator on its left) at child slot `slot + 1` on.
    pub fn insert_children(&mut self, slot: usize, children: Vec<(Vec<u8>, PageId)>) {
        if let Node::Page(page) = self {
            let needed: usize = children.iter().map(|(k, _)| Self::entry_size_branch(k)).sum();
            if used(&page[..]) + needed <= PAGE_SIZE {
                for (n, (key, child)) in children.iter().enumerate() {
                    let fitted = branch_insert(&mut page[..], slot + n, key, *child);
                    debug_assert!(fitted);
                }
                return;
            }
        }
        self.widen();
        if let Node::Branch { entries, .. } = self {
            entries.splice(slot..slot, children);
        }
    }

    /// Removes a branch's child slot: slot 0 makes the next child the leftmost (with none left, leftmost is 0).
    pub fn remove_child(&mut self, slot: usize) {
        match self {
            Node::Page(page) => {
                let page = &mut page[..];
                if slot == 0 {
                    let next = if count(page) == 0 { 0 } else { branch_child(page, 0) };
                    if count(page) > 0 {
                        remove_entry(page, 0);
                    }
                    set_branch_child(page, 0, next);
                } else {
                    remove_entry(page, slot - 1);
                }
            }
            Node::Branch { leftmost, entries } => {
                if slot == 0 {
                    *leftmost = if entries.is_empty() { 0 } else { entries.remove(0).1 };
                } else {
                    entries.remove(slot - 1);
                }
            }
            Node::Leaf(_) => unreachable!("a leaf has no children"),
        }
    }

    /// A leaf's value for `key`, as stored.
    pub fn leaf_get(&self, key: &[u8]) -> Option<ValueRef<'_>> {
        match self {
            Node::Page(page) => leaf_search(&page[..], key).ok().map(|i| leaf_value(&page[..], i)),
            Node::Leaf(entries) => entries.binary_search_by(|(k, _)| k.as_slice().cmp(key)).ok().map(|i| entries[i].1.as_ref()),
            Node::Branch { .. } => unreachable!("a branch has no values"),
        }
    }

    /// Sets `key` to `value` in a leaf. Returns the value it replaced, if any (an inline one without its bytes:
    /// only an overflow chain needs anything done with it), and whether the key is now the leaf's last.
    pub fn leaf_put(&mut self, key: &[u8], value: ValueRef) -> (Option<Value>, bool) {
        if let Node::Page(page) = self {
            let page = &mut page[..];
            match leaf_search(page, key) {
                Ok(i) => {
                    let old = match leaf_value(page, i) {
                        ValueRef::Inline(_) => Value::Inline(Vec::new()),
                        ValueRef::Overflow { page, len } => Value::Overflow { page, len },
                    };
                    let last = i + 1 == count(page);
                    if leaf_set_value(page, i, &value) {
                        return (Some(old), last);
                    }
                }
                Err(i) => {
                    let last = i == count(page);
                    if leaf_insert(page, i, key, &value) {
                        return (None, last);
                    }
                }
            }
        }
        self.widen();
        let Node::Leaf(entries) = self else { unreachable!("a branch has no values") };
        match entries.binary_search_by(|(k, _)| k.as_slice().cmp(key)) {
            Ok(i) => (Some(std::mem::replace(&mut entries[i].1, value.to_owned())), i + 1 == entries.len()),
            Err(i) => {
                entries.insert(i, (key.to_vec(), value.to_owned()));
                (None, i + 1 == entries.len())
            }
        }
    }

    /// Removes `key` from a leaf; its value, if it was there (an inline one without its bytes).
    pub fn leaf_remove(&mut self, key: &[u8]) -> Option<Value> {
        match self {
            Node::Page(page) => {
                let page = &mut page[..];
                let i = leaf_search(page, key).ok()?;
                let old = match leaf_value(page, i) {
                    ValueRef::Inline(_) => Value::Inline(Vec::new()),
                    ValueRef::Overflow { page, len } => Value::Overflow { page, len },
                };
                remove_entry(page, i);
                Some(old)
            }
            Node::Leaf(entries) => {
                let i = entries.binary_search_by(|(k, _)| k.as_slice().cmp(key)).ok()?;
                Some(entries.remove(i).1)
            }
            Node::Branch { .. } => unreachable!("a branch has no values"),
        }
    }

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
        if let Node::Page(page) = self {
            return used(&page[..]);
        }
        HEADER
            + match self {
                Node::Page(_) => unreachable!(),
                Node::Leaf(entries) => entries.iter().map(|(k, v)| Self::entry_size_leaf(k, v)).sum::<usize>(),
                Node::Branch { entries, .. } => entries.iter().map(|(k, _)| Self::entry_size_branch(k)).sum::<usize>(),
            }
    }

    pub fn len(&self) -> usize {
        match self {
            Node::Page(page) => count(&page[..]),
            Node::Leaf(entries) => entries.len(),
            Node::Branch { entries, .. } => entries.len(),
        }
    }

    pub fn encode(&self, page: &mut [u8]) {
        debug_assert!(self.size() <= PAGE_SIZE, "node too large for a page");
        if let Node::Page(bytes) = self {
            page.copy_from_slice(&bytes[..]);
            return;
        }
        page.fill(0);
        let mut end = PAGE_SIZE;
        match self {
            Node::Page(_) => unreachable!(),
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
        set_u16(page, 4, end);
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

    /// A small deterministic generator, so the test needs no dependency.
    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn pages_edited_in_place_match_their_entries() {
        let mut state = 0x9e37_79b9_7f4a_7c15;
        for round in 0..200 {
            let mut page = if round % 2 == 0 { new_page(KIND_LEAF) } else { copy_page(&new_page(KIND_LEAF)[..]) };
            let mut model: Vec<(Vec<u8>, Value)> = Vec::new();
            for _ in 0..400 {
                let key = format!("k{:03}", next(&mut state) % 300).into_bytes();
                let value = match next(&mut state) % 10 {
                    0 => Value::Overflow { page: next(&mut state) % 1000, len: 5000 },
                    n => Value::Inline(vec![n as u8; (next(&mut state) % (12 * n)) as usize]),
                };
                let found = model.binary_search_by(|(k, _)| k.cmp(&key));
                if next(&mut state) % 4 == 0 {
                    if let Ok(i) = found {
                        remove_entry(&mut page[..], i);
                        model.remove(i);
                    }
                } else {
                    let fitted = match found {
                        Ok(i) => leaf_set_value(&mut page[..], i, &value.as_ref()),
                        Err(i) => leaf_insert(&mut page[..], i, &key, &value.as_ref()),
                    };
                    let mut grown = model.clone();
                    match found {
                        Ok(i) => grown[i].1 = value,
                        Err(i) => grown.insert(i, (key, value)),
                    }
                    assert_eq!(fitted, Node::Leaf(grown.clone()).size() <= PAGE_SIZE, "fits exactly when the entries fit");
                    if fitted {
                        model = grown;
                    }
                }
                let Node::Leaf(decoded) = Node::decode(&page[..]) else { unreachable!() };
                assert_eq!(decoded, model);
                assert_eq!(used(&page[..]), Node::Leaf(model.clone()).size());
            }
        }
    }

    #[test]
    fn branches_edited_in_place_match_their_entries() {
        let mut node = Node::Page(new_page(KIND_BRANCH));
        node.set_child(0, 7);
        let mut model = Node::Branch { leftmost: 7, entries: Vec::new() };
        let mut state = 12345;
        for _ in 0..2000 {
            let slot = (next(&mut state) as usize) % (node.len() + 1);
            if next(&mut state) % 3 == 0 && node.len() > 0 {
                node.remove_child(slot);
                model.remove_child(slot);
            } else {
                let key = vec![b'a'; (next(&mut state) % 40) as usize];
                let child = next(&mut state) % 1000;
                node.insert_children(slot, vec![(key.clone(), child)]);
                model.insert_children(slot, vec![(key, child)]);
                if node.size() > PAGE_SIZE {
                    node = Node::Page(new_page(KIND_BRANCH));
                    node.set_child(0, 7);
                    model = Node::Branch { leftmost: 7, entries: Vec::new() };
                    continue;
                }
                node.settle();
            }
            let mut widened = node.clone();
            widened.widen();
            let (Node::Branch { leftmost: a, entries: x }, Node::Branch { leftmost: b, entries: y }) = (&widened, &model) else { unreachable!() };
            assert_eq!((a, x), (b, y));
            assert_eq!(node.size(), model.size());
        }
    }
}
