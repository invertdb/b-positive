//! The database: a file of pages, two meta pages, snapshot readers and one copy-on-write writer.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use memmap2::Mmap;

use crate::page::*;

const MAGIC: &[u8; 8] = b"BPOSITIV";
const VERSION: u32 = 1;
const META_BYTES: usize = 8 + 4 + 4 + 8 * 5;
const INITIAL_PAGES: u64 = 64;

/// How durable a commit is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Durability {
    /// Written to the operating system: survives the process dying, not the machine.
    #[default]
    Process,
    /// Forced to stable storage (data, then the meta page that points at it): survives power loss.
    Full,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub durability: Durability,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Meta {
    txn: u64,
    root: PageId,
    freelist: PageId,
    page_count: u64,
    entries: u64,
}

impl Meta {
    fn encode(&self, page: &mut [u8]) {
        page.fill(0);
        page[0..8].copy_from_slice(MAGIC);
        page[8..12].copy_from_slice(&VERSION.to_le_bytes());
        page[12..16].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        for (i, v) in [self.txn, self.root, self.freelist, self.page_count, self.entries].iter().enumerate() {
            page[16 + 8 * i..24 + 8 * i].copy_from_slice(&v.to_le_bytes());
        }
        let crc = crc32fast::hash(&page[..META_BYTES]);
        page[META_BYTES..META_BYTES + 4].copy_from_slice(&crc.to_le_bytes());
    }

    fn decode(page: &[u8]) -> Option<Meta> {
        if &page[0..8] != MAGIC {
            return None;
        }
        let crc = u32::from_le_bytes(page[META_BYTES..META_BYTES + 4].try_into().unwrap());
        if crc != crc32fast::hash(&page[..META_BYTES]) {
            return None;
        }
        if u32::from_le_bytes(page[8..12].try_into().unwrap()) != VERSION || u32::from_le_bytes(page[12..16].try_into().unwrap()) as usize != PAGE_SIZE {
            return None;
        }
        let at = |i: usize| u64_at(page, 16 + 8 * i);
        Some(Meta { txn: at(0), root: at(1), freelist: at(2), page_count: at(3), entries: at(4) })
    }
}

struct Committed {
    meta: Meta,
    /// Free pages: (the transaction that freed it, page). A page is reusable once no reader holds an older snapshot.
    free: Vec<(u64, PageId)>,
}

struct Inner {
    file: File,
    map: RwLock<Arc<Mmap>>,
    committed: Mutex<Committed>,
    writer: Mutex<()>,
    readers: Mutex<BTreeMap<u64, usize>>,
    options: Options,
}

/// A persistent B+tree of byte-string keys and values, ordered by key bytes.
///
/// Readers ([`ReadTxn`]) see the last commit as of when they began, without locks, from any number of threads.
/// One writer at a time ([`WriteTxn`]) changes it copy-on-write: pages are never overwritten while any
/// committed tree or open reader may use them, and a commit becomes visible (and durable) only when its meta
/// page is written, so a crash at any moment leaves the previous commit intact.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

impl Db {
    pub fn open(path: impl AsRef<Path>, options: Options) -> io::Result<Db> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
        if file.metadata()?.len() < 2 * PAGE_SIZE as u64 {
            let meta = Meta { txn: 0, root: 0, freelist: 0, page_count: 2, entries: 0 };
            file.set_len(INITIAL_PAGES * PAGE_SIZE as u64)?;
            let mut page = vec![0u8; PAGE_SIZE];
            meta.encode(&mut page);
            file.write_all_at(&page, 0)?;
            file.sync_all()?;
        }
        let map = unsafe { Mmap::map(&file)? };
        let meta = [Meta::decode(&map[0..PAGE_SIZE]), Meta::decode(&map[PAGE_SIZE..2 * PAGE_SIZE])]
            .into_iter()
            .flatten()
            .max_by_key(|m| m.txn)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no valid meta page: not a b-positive file"))?;

        // the free list, from its chain of pages
        let mut free = Vec::new();
        let mut next = meta.freelist;
        while next != 0 {
            let page = &map[next as usize * PAGE_SIZE..(next as usize + 1) * PAGE_SIZE];
            for i in 0..count(page) {
                free.push((u64_at(page, HEADER + 16 * i), u64_at(page, HEADER + 16 * i + 8)));
            }
            next = u64_at(page, 8);
        }

        Ok(Db {
            inner: Arc::new(Inner {
                file,
                map: RwLock::new(Arc::new(map)),
                committed: Mutex::new(Committed { meta, free }),
                writer: Mutex::new(()),
                readers: Mutex::new(BTreeMap::new()),
                options,
            }),
        })
    }

    /// A snapshot of the last commit.
    pub fn begin_read(&self) -> ReadTxn {
        let committed = self.inner.committed.lock().unwrap();
        let meta = committed.meta;
        *self.inner.readers.lock().unwrap().entry(meta.txn).or_insert(0) += 1;
        let map = self.inner.map.read().unwrap().clone();
        drop(committed);
        ReadTxn { inner: self.inner.clone(), map, meta }
    }

    /// The writer; waits for any other writer to finish.
    pub fn begin_write(&self) -> WriteTxn<'_> {
        let guard = self.inner.writer.lock().unwrap();
        let committed = self.inner.committed.lock().unwrap();
        let meta = committed.meta;
        // pages freed by transactions no open reader predates can be reused now
        let oldest = self.inner.readers.lock().unwrap().keys().next().copied().unwrap_or(u64::MAX);
        let (reusable, kept): (Vec<_>, Vec<_>) = committed.free.iter().copied().partition(|&(freed_by, _)| freed_by <= oldest);
        drop(committed);
        let map = self.inner.map.read().unwrap().clone();
        WriteTxn {
            db: self,
            _guard: guard,
            map,
            meta,
            dirty: HashMap::new(),
            overflow: HashMap::new(),
            allocated: HashSet::new(),
            reusable: reusable.into_iter().map(|(_, p)| p).collect(),
            kept,
            released: Vec::new(),
            recycled: Vec::new(),
        }
    }
}

fn page_of(map: &Mmap, id: PageId) -> &[u8] {
    &map[id as usize * PAGE_SIZE..(id as usize + 1) * PAGE_SIZE]
}

fn read_overflow(map: &Mmap, mut page: PageId, len: u32) -> Vec<u8> {
    let mut value = Vec::with_capacity(len as usize);
    while page != 0 {
        let p = page_of(map, page);
        let used = count(p);
        value.extend_from_slice(&p[HEADER..HEADER + used]);
        page = u64_at(p, 8);
    }
    value
}

/// A snapshot of the database as of its last commit when this began. Cheap; any number may be open, on any
/// threads; it keeps the pages it can see from being reused until it is dropped.
pub struct ReadTxn {
    inner: Arc<Inner>,
    map: Arc<Mmap>,
    meta: Meta,
}

impl Drop for ReadTxn {
    fn drop(&mut self) {
        let mut readers = self.inner.readers.lock().unwrap();
        if let Some(n) = readers.get_mut(&self.meta.txn) {
            *n -= 1;
            if *n == 0 {
                readers.remove(&self.meta.txn);
            }
        }
    }
}

impl ReadTxn {
    /// Number of keys.
    pub fn len(&self) -> u64 {
        self.meta.entries
    }

    pub fn is_empty(&self) -> bool {
        self.meta.entries == 0
    }

    /// The transaction id of the commit this snapshot shows.
    pub fn txn(&self) -> u64 {
        self.meta.txn
    }

    pub fn get(&self, key: &[u8]) -> Option<Cow<'_, [u8]>> {
        let mut id = self.meta.root;
        if id == 0 {
            return None;
        }
        loop {
            let page = page_of(&self.map, id);
            if kind(page) == KIND_BRANCH {
                id = branch_slot_child(page, branch_slot(page, key));
                continue;
            }
            return match leaf_search(page, key) {
                Ok(i) => Some(match leaf_value(page, i) {
                    ValueRef::Inline(bytes) => Cow::Borrowed(bytes),
                    ValueRef::Overflow { page, len } => Cow::Owned(read_overflow(&self.map, page, len)),
                }),
                Err(_) => None,
            };
        }
    }

    /// Keys from `start` (inclusive) on, in order, with their values.
    pub fn range_from<'a>(&'a self, start: &[u8]) -> Cursor<'a> {
        let mut cursor = Cursor { txn: self, stack: Vec::new() };
        cursor.seek(start);
        cursor
    }

    /// Every key, in order, with its value.
    pub fn iter(&self) -> Cursor<'_> {
        self.range_from(&[])
    }
}

/// Iterates keys in order. Holds a path from the root: (page, slot) per level.
pub struct Cursor<'a> {
    txn: &'a ReadTxn,
    stack: Vec<(PageId, usize)>,
}

impl<'a> Cursor<'a> {
    fn seek(&mut self, key: &[u8]) {
        self.stack.clear();
        let mut id = self.txn.meta.root;
        if id == 0 {
            return;
        }
        loop {
            let page = page_of(&self.txn.map, id);
            if kind(page) == KIND_BRANCH {
                let slot = branch_slot(page, key);
                self.stack.push((id, slot));
                id = branch_slot_child(page, slot);
            } else {
                let i = match leaf_search(page, key) {
                    Ok(i) | Err(i) => i,
                };
                self.stack.push((id, i));
                return;
            }
        }
    }

    /// Down the leftmost path from a child, onto the stack.
    fn descend(&mut self, mut id: PageId) {
        loop {
            let page = page_of(&self.txn.map, id);
            if kind(page) == KIND_BRANCH {
                self.stack.push((id, 0));
                id = leftmost(page);
            } else {
                self.stack.push((id, 0));
                return;
            }
        }
    }
}

impl<'a> Iterator for Cursor<'a> {
    type Item = (&'a [u8], Cow<'a, [u8]>);

    fn next(&mut self) -> Option<Self::Item> {
        let map: &'a Mmap = &self.txn.map;
        loop {
            let &(id, i) = self.stack.last()?;
            let page = page_of(map, id);
            if i < count(page) {
                self.stack.last_mut().unwrap().1 += 1;
                let value = match leaf_value(page, i) {
                    ValueRef::Inline(bytes) => Cow::Borrowed(bytes),
                    ValueRef::Overflow { page, len } => Cow::Owned(read_overflow(map, page, len)),
                };
                return Some((leaf_key(page, i), value));
            }
            // this leaf is done: up to the first branch with a next child, then down its leftmost path
            self.stack.pop();
            loop {
                let Some(&mut (branch, ref mut slot)) = self.stack.last_mut() else { return None };
                let page = page_of(map, branch);
                if *slot < count(page) {
                    *slot += 1;
                    let child = branch_slot_child(page, *slot);
                    self.descend(child);
                    break;
                }
                self.stack.pop();
            }
        }
    }
}

/// The one writer: changes are made copy-on-write, invisible to readers until [`WriteTxn::commit`]; dropping it
/// without committing abandons them.
pub struct WriteTxn<'db> {
    db: &'db Db,
    _guard: MutexGuard<'db, ()>,
    map: Arc<Mmap>,
    meta: Meta,
    /// Nodes written in this transaction, by their (new) page.
    dirty: HashMap<PageId, Node>,
    /// Overflow pages written in this transaction.
    overflow: HashMap<PageId, Box<[u8]>>,
    /// Pages this transaction allocated (dirty or overflow).
    allocated: HashSet<PageId>,
    /// Free pages this transaction may reuse.
    reusable: Vec<PageId>,
    /// Free pages it may not (an open reader may still see them), kept on the free list.
    kept: Vec<(u64, PageId)>,
    /// Committed pages this transaction no longer uses: free once it commits.
    released: Vec<PageId>,
    /// Pages it allocated and then dropped again: free at once.
    recycled: Vec<PageId>,
}

impl<'db> WriteTxn<'db> {
    pub fn len(&self) -> u64 {
        self.meta.entries
    }

    pub fn is_empty(&self) -> bool {
        self.meta.entries == 0
    }

    fn alloc(&mut self) -> PageId {
        let id = self.recycled.pop().or_else(|| self.reusable.pop()).unwrap_or_else(|| {
            self.meta.page_count += 1;
            self.meta.page_count - 1
        });
        self.allocated.insert(id);
        id
    }

    fn free(&mut self, id: PageId) {
        if self.allocated.remove(&id) {
            self.dirty.remove(&id);
            self.overflow.remove(&id);
            self.recycled.push(id);
        } else {
            self.released.push(id);
        }
    }

    /// A node this transaction may change: the page itself if it wrote it, else a copy in a new page.
    fn writable(&mut self, id: PageId) -> PageId {
        if self.dirty.contains_key(&id) {
            return id;
        }
        let node = Node::Page(copy_page(page_of(&self.map, id)));
        let new = self.alloc();
        self.dirty.insert(new, node);
        self.released.push(id);
        new
    }

    fn free_value(&mut self, value: &Value) {
        if let Value::Overflow { page, .. } = *value {
            let mut next = page;
            while next != 0 {
                let following = match self.overflow.get(&next) {
                    Some(p) => u64_at(p, 8),
                    None => u64_at(page_of(&self.map, next), 8),
                };
                self.free(next);
                next = following;
            }
        }
    }

    fn store_value(&mut self, value: &[u8]) -> Value {
        if value.len() <= MAX_INLINE_VALUE {
            return Value::Inline(value.to_vec());
        }
        let pages: Vec<PageId> = (0..value.len().div_ceil(OVERFLOW_DATA)).map(|_| self.alloc()).collect();
        for (i, chunk) in value.chunks(OVERFLOW_DATA).enumerate() {
            let mut page = vec![0u8; PAGE_SIZE].into_boxed_slice();
            page[0] = KIND_OVERFLOW;
            page[2..4].copy_from_slice(&(chunk.len() as u16).to_le_bytes());
            page[8..16].copy_from_slice(&pages.get(i + 1).copied().unwrap_or(0).to_le_bytes());
            page[HEADER..HEADER + chunk.len()].copy_from_slice(chunk);
            self.overflow.insert(pages[i], page);
        }
        Value::Overflow { page: pages[0], len: value.len() as u32 }
    }

    fn store_value_owned(&mut self, value: Vec<u8>) -> Value {
        if value.len() <= MAX_INLINE_VALUE { Value::Inline(value) } else { self.store_value(&value) }
    }

    fn read_value(&self, value: &ValueRef) -> Vec<u8> {
        match *value {
            ValueRef::Inline(bytes) => bytes.to_vec(),
            ValueRef::Overflow { page, len } => {
                let mut out = Vec::with_capacity(len as usize);
                let mut next = page;
                while next != 0 {
                    let p: &[u8] = match self.overflow.get(&next) {
                        Some(p) => p,
                        None => page_of(&self.map, next),
                    };
                    out.extend_from_slice(&p[HEADER..HEADER + count(p)]);
                    next = u64_at(p, 8);
                }
                out
            }
        }
    }

    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let mut id = self.meta.root;
        if id == 0 {
            return None;
        }
        loop {
            if let Some(node) = self.dirty.get(&id) {
                if node.is_branch() {
                    id = node.child(node.child_slot(key));
                } else {
                    return node.leaf_get(key).map(|v| self.read_value(&v));
                }
            } else {
                let page = page_of(&self.map, id);
                if kind(page) == KIND_BRANCH {
                    id = branch_slot_child(page, branch_slot(page, key));
                } else {
                    return leaf_search(page, key).ok().map(|i| match leaf_value(page, i) {
                        ValueRef::Inline(bytes) => bytes.to_vec(),
                        ValueRef::Overflow { page, len } => read_overflow(&self.map, page, len),
                    });
                }
            }
        }
    }

    /// The path to the leaf for `key`, made writable: (branch, child slot) per level, and the leaf.
    fn writable_path(&mut self, key: &[u8]) -> (Vec<(PageId, usize)>, PageId) {
        let mut path = Vec::new();
        let mut id = self.writable(self.meta.root);
        self.meta.root = id;
        loop {
            let node = &self.dirty[&id];
            if !node.is_branch() {
                return (path, id);
            }
            let slot = node.child_slot(key);
            let old = node.child(slot);
            let child = self.writable(old);
            if child != old {
                self.dirty.get_mut(&id).unwrap().set_child(slot, child);
            }
            path.push((id, slot));
            id = child;
        }
    }

    /// Sets `key` to `value`, replacing any value it had.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        if key.len() > MAX_KEY {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("key longer than {MAX_KEY} bytes")));
        }
        // a value that fits its leaf goes in from the caller's bytes; a longer one to overflow pages first
        let overflow = (value.len() > MAX_INLINE_VALUE).then(|| self.store_value(value));
        let value = overflow.as_ref().map_or(ValueRef::Inline(value), Value::as_ref);
        if self.meta.root == 0 {
            let id = self.alloc();
            self.dirty.insert(id, Node::Page(new_page(KIND_LEAF)));
            self.meta.root = id;
        }

        let (path, leaf) = self.writable_path(key);
        // whether the key landed at the node's right edge: a sorted bulk load appends, and an appended node splits
        // so the left half stays full (see `split`)
        let (old, appended) = self.dirty.get_mut(&leaf).unwrap().leaf_put(key, value);
        match old {
            Some(old) => self.free_value(&old),
            None => self.meta.entries += 1,
        }

        self.split_upwards(path, leaf, appended);
        Ok(())
    }

    /// Sets each of `keys` (ascending, no duplicates) to the value `value(i, old)` makes from the value key `i` had,
    /// in one pass: each leaf is found once for the run of keys it takes, the run is merged into it in order, and an
    /// overflowing leaf splits into as many pages as it needs. A run past the tree's last key fills its pages left to
    /// right (see `split`), so a sorted bulk load leaves them full. Values are made in key order.
    pub fn put_sorted<K: AsRef<[u8]>>(&mut self, keys: &[K], mut value: impl FnMut(usize, Option<&[u8]>) -> Vec<u8>) -> io::Result<()> {
        if let Some(key) = keys.iter().find(|k| k.as_ref().len() > MAX_KEY) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("key longer than {MAX_KEY} bytes ({})", key.as_ref().len())));
        }
        debug_assert!(keys.windows(2).all(|w| w[0].as_ref() < w[1].as_ref()), "keys must ascend");
        let mut i = 0;
        while i < keys.len() {
            if self.meta.root == 0 {
                let id = self.alloc();
                self.dirty.insert(id, Node::Page(new_page(KIND_LEAF)));
                self.meta.root = id;
            }
            let (path, leaf) = self.writable_path(keys[i].as_ref());
            // the leaf takes the keys below the nearest separator to its right (none: the tree's right edge)
            let upper = path.iter().rev().find_map(|&(branch, slot)| self.dirty[&branch].separator(slot));
            let end = i + keys[i..].partition_point(|k| upper.as_deref().is_none_or(|u| k.as_ref() < u));

            // into the page in place while it holds them; the rest (from the first key it can't hold, whose value is
            // already made) merged into its entries, to split into pages
            let mut j = i;
            let mut pending = None;
            if matches!(self.dirty[&leaf], Node::Page(_)) {
                while j < end {
                    let key = keys[j].as_ref();
                    let previous = self.dirty[&leaf].leaf_get(key).map(|v| v.to_owned());
                    let bytes = match &previous {
                        Some(Value::Inline(bytes)) => value(j, Some(bytes)),
                        Some(overflow) => value(j, Some(&self.read_value(&overflow.as_ref()))),
                        None => value(j, None),
                    };
                    let stored = self.store_value_owned(bytes);
                    let Node::Page(page) = self.dirty.get_mut(&leaf).unwrap() else { unreachable!() };
                    let fitted = match leaf_search(&page[..], key) {
                        Ok(at) => leaf_set_value(&mut page[..], at, &stored.as_ref()),
                        Err(at) => leaf_insert(&mut page[..], at, key, &stored.as_ref()),
                    };
                    if !fitted {
                        pending = Some(stored);
                        break;
                    }
                    match previous {
                        Some(previous) => self.free_value(&previous),
                        None => self.meta.entries += 1,
                    }
                    j += 1;
                }
            }
            // appended: the run ended past every key the tree had
            let appended = if j == end {
                let Node::Page(page) = &self.dirty[&leaf] else { unreachable!() };
                upper.is_none() && leaf_key(&page[..], count(&page[..]) - 1) == keys[end - 1].as_ref()
            } else {
                self.dirty.get_mut(&leaf).unwrap().widen();
                let Node::Leaf(old) = std::mem::replace(self.dirty.get_mut(&leaf).unwrap(), Node::Leaf(Vec::new())) else { unreachable!() };
                let mut merged = Vec::with_capacity(old.len() + end - j);
                let mut old = old.into_iter().peekable();
                for (n, key) in keys[j..end].iter().enumerate() {
                    let key = key.as_ref();
                    while let Some(entry) = old.next_if(|(k, _)| k.as_slice() < key) {
                        merged.push(entry);
                    }
                    let previous = old.next_if(|(k, _)| k.as_slice() == key).map(|(_, v)| v);
                    let stored = match pending.take() {
                        Some(stored) => stored,
                        None => {
                            let bytes = match &previous {
                                Some(Value::Inline(bytes)) => value(j + n, Some(bytes)),
                                Some(overflow) => value(j + n, Some(&self.read_value(&overflow.as_ref()))),
                                None => value(j + n, None),
                            };
                            self.store_value_owned(bytes)
                        }
                    };
                    match previous {
                        Some(previous) => self.free_value(&previous),
                        None => self.meta.entries += 1,
                    }
                    merged.push((key.to_vec(), stored));
                }
                let appended = upper.is_none() && old.peek().is_none();
                merged.extend(old);
                *self.dirty.get_mut(&leaf).unwrap() = Node::Leaf(merged);
                appended
            };
            self.split_upwards(path, leaf, appended);
            i = end;
        }
        Ok(())
    }

    /// Splits a node that overflows its page, then its parent if the new nodes overflow it, and so on to the root.
    fn split_upwards(&mut self, mut path: Vec<(PageId, usize)>, mut id: PageId, mut appended: bool) {
        loop {
            let pieces = self.split(id, appended);
            if pieces.is_empty() {
                return;
            }
            match path.pop() {
                Some((parent, slot)) => {
                    let node = self.dirty.get_mut(&parent).unwrap();
                    appended = appended && slot == node.len();
                    node.insert_children(slot, pieces);
                    id = parent;
                }
                None => {
                    // a new root, which may itself overflow if the old one split many ways
                    let root = self.alloc();
                    self.dirty.insert(root, Node::Branch { leftmost: id, entries: pieces });
                    self.meta.root = root;
                    id = root;
                }
            }
        }
    }

    /// Splits a node that no longer fits its page into as few as hold it; returns each new node to its right, with
    /// its separator key (none if it fits).
    ///
    /// The pages are closest in size, except for a leaf whose new entries landed at its right edge (`appended`):
    /// then each page is filled as full as it fits, left to right, and the last takes the rest. Sorted inserts only
    /// ever append, so they would otherwise leave every page they split half empty (Graefe, 2011, §10.2).
    fn split(&mut self, id: PageId, appended: bool) -> Vec<(Vec<u8>, PageId)> {
        let node = self.dirty.get_mut(&id).unwrap();
        if node.size() <= PAGE_SIZE {
            node.settle();
            return Vec::new();
        }
        let is_branch = node.is_branch();
        let sizes: Vec<usize> = match node {
            Node::Leaf(entries) => entries.iter().map(|(k, v)| Node::entry_size_leaf(k, v)).collect(),
            Node::Branch { entries, .. } => entries.iter().map(|(k, _)| Node::entry_size_branch(k)).collect(),
            Node::Page(_) => unreachable!("a page holds what fits it"),
        };
        let cuts = cuts(&sizes, is_branch, appended && !is_branch);
        let mut pieces = Vec::with_capacity(cuts.len());
        for &cut in cuts.iter().rev() {
            pieces.push(match self.dirty.get_mut(&id).unwrap() {
                Node::Leaf(entries) => {
                    let right = entries.split_off(cut);
                    (right[0].0.clone(), Node::Leaf(right))
                }
                Node::Branch { entries, .. } => {
                    let mut right = entries.split_off(cut);
                    let (separator, middle) = right.remove(0);
                    (separator, Node::Branch { leftmost: middle, entries: right })
                }
                Node::Page(_) => unreachable!(),
            });
        }
        self.dirty.get_mut(&id).unwrap().settle();
        pieces
            .into_iter()
            .rev()
            .map(|(separator, mut node)| {
                debug_assert!(node.size() <= PAGE_SIZE);
                node.settle();
                let right = self.alloc();
                self.dirty.insert(right, node);
                (separator, right)
            })
            .collect()
    }

    /// Removes `key`; returns whether it was there.
    pub fn delete(&mut self, key: &[u8]) -> bool {
        if !self.get_exists(key) {
            return false;
        }
        let (mut path, leaf) = self.writable_path(key);
        let removed = self.dirty.get_mut(&leaf).unwrap().leaf_remove(key).unwrap();
        self.free_value(&removed);
        self.meta.entries -= 1;

        // empty nodes leave their parent; small leaves merge into a sibling
        let mut id = leaf;
        loop {
            let node = &self.dirty[&id];
            // a branch whose last child was removed is empty too (leftmost 0); one with a single child is "lone"
            let empty_leaf = node.len() == 0 && (!node.is_branch() || node.leftmost() == 0);
            let lone_branch = node.is_branch() && node.len() == 0 && node.leftmost() != 0;
            let Some(&(parent, slot)) = path.last() else {
                // the root
                if empty_leaf {
                    self.free(id);
                    self.meta.root = 0;
                } else if lone_branch {
                    let leftmost = self.dirty[&id].leftmost();
                    self.free(id);
                    self.meta.root = leftmost;
                }
                return true;
            };
            if empty_leaf {
                self.free(id);
                self.remove_child(parent, slot);
                path.pop();
                id = parent;
                continue;
            }
            if lone_branch {
                // replace the branch by its only child
                let leftmost = self.dirty[&id].leftmost();
                self.free(id);
                self.set_child(parent, slot, leftmost);
                return true;
            }
            if !node.is_branch() && node.size() < PAGE_SIZE / 4 {
                self.merge_leaf(parent, slot);
            }
            return true;
        }
    }

    fn get_exists(&self, key: &[u8]) -> bool {
        self.get(key).is_some()
    }

    fn set_child(&mut self, parent: PageId, slot: usize, child: PageId) {
        self.dirty.get_mut(&parent).unwrap().set_child(slot, child);
    }

    /// Removes a child slot. For slot 0, the next child becomes the leftmost (it already holds the keys from its
    /// separator up); with no next child, the branch is left empty.
    fn remove_child(&mut self, parent: PageId, slot: usize) {
        self.dirty.get_mut(&parent).unwrap().remove_child(slot);
    }

    /// Merges the small leaf in `parent`'s child `slot` with a neighbour, if the two fit one page.
    fn merge_leaf(&mut self, parent: PageId, slot: usize) {
        let parent_node = &self.dirty[&parent];
        if !parent_node.is_branch() {
            return;
        }
        let children = parent_node.len() + 1;
        if children < 2 {
            return;
        }
        // merge the right one of the pair into the left
        let (left_slot, right_slot) = if slot + 1 < children { (slot, slot + 1) } else { (slot - 1, slot) };
        let left = parent_node.child(left_slot);
        let right = parent_node.child(right_slot);
        let mut right_node = match self.dirty.get(&right) {
            Some(node) => node.clone(),
            None => Node::Page(copy_page(page_of(&self.map, right))),
        };
        let left_size = match self.dirty.get(&left) {
            Some(node) => node.size(),
            None => used(page_of(&self.map, left)),
        };
        if right_node.is_branch() || left_size + right_node.size() - HEADER > PAGE_SIZE {
            return;
        }
        let left = self.writable(left);
        self.set_child(parent, left_slot, left);
        right_node.widen();
        let Node::Leaf(right_entries) = right_node else { unreachable!() };
        let left_node = self.dirty.get_mut(&left).unwrap();
        left_node.widen();
        match left_node {
            Node::Leaf(entries) => entries.extend(right_entries),
            _ => return,
        }
        left_node.settle();
        self.free(right);
        self.remove_child(parent, right_slot);
    }

    /// Makes every change durable (as the database's [`Durability`] says) and visible to new readers.
    pub fn commit(mut self) -> io::Result<()> {
        let inner = &self.db.inner;
        let txn = self.meta.txn + 1;

        // the free list as of this commit: pages free to reuse now (no reader can see them), and pages that become
        // free once no reader predates this commit (released now, the old list's own pages) or that already waited
        let mut immediate: Vec<PageId> = self.reusable.drain(..).chain(self.recycled.drain(..)).collect();
        let mut deferred: Vec<(u64, PageId)> = std::mem::take(&mut self.kept);
        deferred.extend(self.released.drain(..).map(|p| (txn, p)));
        let mut old = self.meta.freelist;
        while old != 0 {
            let next = u64_at(page_of(&self.map, old), 8);
            deferred.push((txn, old));
            old = next;
        }
        // the list's own pages: free pages first (each one taken is one entry fewer), then the end of the file.
        // Taking them only from the end would grow the list by its own size every commit.
        let mut list_pages: Vec<PageId> = Vec::new();
        while list_pages.len() < (deferred.len() + immediate.len()).div_ceil(FREELIST_ENTRIES) {
            list_pages.push(immediate.pop().unwrap_or_else(|| {
                self.meta.page_count += 1;
                self.meta.page_count - 1
            }));
        }
        let mut free = deferred;
        free.extend(immediate.into_iter().map(|p| (0, p)));
        self.meta.freelist = list_pages.first().copied().unwrap_or(0);

        // grow the file if needed (then remap below)
        let needed = self.meta.page_count * PAGE_SIZE as u64;
        let length = inner.file.metadata()?.len();
        if needed > length {
            inner.file.set_len(needed.max(length * 2))?;
        }

        // the pages, in file order, each run of consecutive pages in one write (up to RUN pages): a transaction's
        // new pages mostly come from the end of the file, one after another
        const RUN: usize = 256;
        let mut ids: Vec<PageId> = self.dirty.keys().chain(self.overflow.keys()).copied().collect();
        ids.sort_unstable();
        let mut run = Vec::with_capacity(RUN * PAGE_SIZE);
        let mut first = 0;
        for (n, &id) in ids.iter().enumerate() {
            if run.is_empty() {
                first = id;
            }
            let at = run.len();
            run.resize(at + PAGE_SIZE, 0);
            match self.dirty.get(&id) {
                Some(node) => node.encode(&mut run[at..]),
                None => run[at..].copy_from_slice(&self.overflow[&id]),
            }
            let next = ids.get(n + 1).copied();
            if next != Some(id + 1) || run.len() == RUN * PAGE_SIZE {
                inner.file.write_all_at(&run, first * PAGE_SIZE as u64)?;
                run.clear();
            }
        }
        let mut buffer = vec![0u8; PAGE_SIZE];
        // every list page is written: taking a free page for the list can leave the last one empty
        for (n, &list_page) in list_pages.iter().enumerate() {
            let chunk = free.chunks(FREELIST_ENTRIES).nth(n).unwrap_or(&[]);
            buffer.fill(0);
            buffer[0] = KIND_FREELIST;
            buffer[2..4].copy_from_slice(&(chunk.len() as u16).to_le_bytes());
            let next = list_pages.get(n + 1).copied().unwrap_or(0);
            buffer[8..16].copy_from_slice(&next.to_le_bytes());
            for (i, &(freed_by, page)) in chunk.iter().enumerate() {
                buffer[HEADER + 16 * i..HEADER + 16 * i + 8].copy_from_slice(&freed_by.to_le_bytes());
                buffer[HEADER + 16 * i + 8..HEADER + 16 * i + 16].copy_from_slice(&page.to_le_bytes());
            }
            inner.file.write_all_at(&buffer, list_page * PAGE_SIZE as u64)?;
        }
        if inner.options.durability == Durability::Full {
            inner.file.sync_all()?;
        }

        // the commit point: the meta page in the slot the previous commit didn't use
        self.meta.txn = txn;
        self.meta.encode(&mut buffer);
        inner.file.write_all_at(&buffer, (txn % 2) * PAGE_SIZE as u64)?;
        if inner.options.durability == Durability::Full {
            inner.file.sync_all()?;
        }

        // publish: remap if the file grew, then the new meta and free list
        let length = inner.file.metadata()?.len();
        if inner.map.read().unwrap().len() < length as usize {
            let map = unsafe { Mmap::map(&inner.file)? };
            *inner.map.write().unwrap() = Arc::new(map);
        }
        let mut committed = inner.committed.lock().unwrap();
        committed.meta = self.meta;
        committed.free = free;
        Ok(())
    }
}

/// Where to cut a node's entries (their sizes, in order) so each part fits a page: the index each part after the
/// first starts at. A branch's entry at a cut moves up as the separator, so it counts in no part. `fill` fills each
/// part as full as it fits, left to right; otherwise the parts are as many as filling needs, closest in size.
fn cuts(sizes: &[usize], is_branch: bool, fill: bool) -> Vec<usize> {
    let capacity = PAGE_SIZE - HEADER;
    let fits = |cuts: &[usize]| {
        let mut starts = vec![0];
        starts.extend_from_slice(cuts);
        starts.push(sizes.len());
        starts.windows(2).enumerate().all(|(n, w)| {
            let first = if is_branch && n > 0 { w[0] + 1 } else { w[0] };
            w[0] < w[1] && sizes[first..w[1]].iter().sum::<usize>() <= capacity
        })
    };
    let mut filled = Vec::new();
    let (mut start, mut used) = (0, 0);
    for (i, &size) in sizes.iter().enumerate() {
        if used + size > capacity && i > start {
            filled.push(i);
            start = i;
            used = if is_branch { 0 } else { size };
        } else {
            used += size;
        }
    }
    if fill || filled.is_empty() {
        return filled;
    }
    // as many parts, each cut where the running size comes closest to its share of the total
    let parts = filled.len() + 1;
    let total: usize = sizes.iter().sum();
    let mut balanced = Vec::with_capacity(parts - 1);
    let mut prefix = 0;
    let mut i = 0;
    for part in 1..parts {
        let target = total * part / parts;
        while i < sizes.len() && prefix + sizes[i] <= target {
            prefix += sizes[i];
            i += 1;
        }
        // cut before entry i (running size `prefix`) or after it, whichever is nearer the target
        let cut = if i < sizes.len() && prefix + sizes[i] - target < target - prefix { i + 1 } else { i };
        balanced.push(cut);
    }
    if fits(&balanced) { balanced } else { filled }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_delete_commit() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db"), Options::default()).unwrap();
        let mut w = db.begin_write();
        w.put(b"b", b"2").unwrap();
        w.put(b"a", b"1").unwrap();
        w.put(b"c", &vec![7u8; 10_000]).unwrap();
        assert_eq!(w.get(b"a").as_deref(), Some(&b"1"[..]));
        w.commit().unwrap();

        let r = db.begin_read();
        assert_eq!(r.get(b"b").as_deref(), Some(&b"2"[..]));
        assert_eq!(r.get(b"c").unwrap().len(), 10_000);
        assert_eq!(r.iter().map(|(k, _)| k.to_vec()).collect::<Vec<_>>(), vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);

        let mut w = db.begin_write();
        assert!(w.delete(b"b"));
        assert!(!w.delete(b"zz"));
        w.commit().unwrap();
        assert_eq!(r.get(b"b").as_deref(), Some(&b"2"[..]), "an open snapshot keeps seeing its commit");
        assert_eq!(db.begin_read().get(b"b"), None);
    }
}
