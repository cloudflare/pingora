// Copyright 2026 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Can't tell people you know Rust until you write a (doubly) linked list

//! Doubly linked list, generic over the stored data type `K`.
//!
//! Features
//! - Preallocate consecutive memory, no memory fragmentation.
//! - No shrink function: for Lru cache that grows to a certain size but never shrinks.
//! - Relatively fast and efficient.

// inspired by clru::FixedSizeList (Élie!)

use std::mem::{replace, size_of};

/// Public index type, kept as `usize` for compatibility.
type Index = usize;

/// Compact internal node and free-list index.
type CompactIndex = u32;

const NULL: CompactIndex = u32::MAX;
const HEAD: CompactIndex = 0;
const TAIL: CompactIndex = 1;
const OFFSET: CompactIndex = 2;

/// Number of non-NULL internal indices, including [`HEAD`] and [`TAIL`].
const MAX_SLOTS: usize = u32::MAX as usize;

// Ensure widening CompactIndex to Index is lossless.
const _: () = assert!(size_of::<usize>() >= size_of::<CompactIndex>());

/// Losslessly widen a compact index to the public index type.
#[inline]
fn to_index(index: CompactIndex) -> Index {
    index as Index
}

/// Narrow a public index, panicking if it is unrepresentable or [`NULL`].
#[track_caller]
#[inline]
fn to_compact(index: Index) -> CompactIndex {
    CompactIndex::try_from(index)
        .ok()
        .filter(|&compact| compact != NULL)
        .unwrap_or_else(|| {
            panic!(
                "linked list index {index} exceeds the maximum supported index {}",
                MAX_SLOTS - 1
            )
        })
}

/// Return whether `index` addresses one of `data_len` data slots.
#[inline]
fn is_valid_slot(index: Index, data_len: usize) -> bool {
    if index == to_index(NULL) {
        return false;
    }
    // Subtract to avoid overflowing `data_len + offset`.
    let offset = to_index(OFFSET);
    index >= offset && index - offset < data_len
}

#[derive(Debug)]
struct Node<K> {
    pub(crate) prev: CompactIndex,
    pub(crate) next: CompactIndex,
    pub(crate) data: K,
}

// Functionally the same as vec![head, tail, data_nodes...] where head & tail are fixed and
// the rest data nodes can expand. Both head and tail can be accessed faster than using index
struct Nodes<K> {
    // we use these sentinel nodes to guard the head and tail of the list so that list
    // manipulation is simpler (fewer if-else)
    head: Node<K>,
    tail: Node<K>,
    data_nodes: Vec<Node<K>>,
}

impl<K: Default> Nodes<K> {
    fn with_capacity(capacity: usize) -> Self {
        Nodes {
            head: Node {
                prev: NULL,
                next: TAIL,
                data: K::default(),
            },
            tail: Node {
                prev: HEAD,
                next: NULL,
                data: K::default(),
            },
            data_nodes: Vec::with_capacity(capacity),
        }
    }

    /// Reserve space for exactly `additional` more data nodes (no growth slack).
    fn reserve(&mut self, additional: usize) {
        self.data_nodes.reserve_exact(additional);
    }

    fn new_node(&mut self, data: K) -> CompactIndex {
        const VEC_EXP_GROWTH_CAP: usize = 65536;
        // Validate before mutation so a panic leaves `data_nodes` unchanged.
        let compact_index = to_compact(
            self.data_nodes
                .len()
                .checked_add(to_index(OFFSET))
                .expect("linked list length overflow"),
        );
        let node = Node {
            prev: NULL,
            next: NULL,
            data,
        };
        // Constrain the growth of vec: vec always double its capacity when it needs to grow.
        // It could waste too much memory when it is already very large.
        // Here we limit the memory waste to 10% once it grows beyond the cap.
        // The amortized growth cost is O(n) beyond the max of the initially reserved capacity and
        // the cap. But this list is for limited sized LRU and we recycle released node, so
        // hopefully insertions are rare beyond certain sizes
        if self.data_nodes.capacity() > VEC_EXP_GROWTH_CAP
            && self.data_nodes.capacity() - self.data_nodes.len() < 2
        {
            self.data_nodes
                .reserve_exact(self.data_nodes.capacity() / 10)
        }
        self.data_nodes.push(node);
        compact_index
    }

    fn len(&self) -> usize {
        self.data_nodes.len()
    }

    fn head(&self) -> &Node<K> {
        &self.head
    }

    fn tail(&self) -> &Node<K> {
        &self.tail
    }
}

impl<K> std::ops::Index<CompactIndex> for Nodes<K> {
    type Output = Node<K>;

    fn index(&self, index: CompactIndex) -> &Self::Output {
        match index {
            HEAD => &self.head,
            TAIL => &self.tail,
            // HEAD and TAIL are matched, so subtraction cannot underflow.
            _ => &self.data_nodes[to_index(index - OFFSET)],
        }
    }
}

impl<K> std::ops::IndexMut<CompactIndex> for Nodes<K> {
    fn index_mut(&mut self, index: CompactIndex) -> &mut Self::Output {
        match index {
            HEAD => &mut self.head,
            TAIL => &mut self.tail,
            // HEAD and TAIL are matched, so subtraction cannot underflow.
            _ => &mut self.data_nodes[to_index(index - OFFSET)],
        }
    }
}

/// Doubly linked list, generic over the stored data type `K`.
///
/// Each list supports up to `u32::MAX - 2` data slots.
pub struct LinkedList<K = u64> {
    nodes: Nodes<K>,
    free: Vec<CompactIndex>, // to keep track of freed node to be used again
}

/// Type alias preserving backward compatibility.
pub type LinkedListU64 = LinkedList<u64>;

// Panic when index used as parameters are invalid
// Index returned by push_* is always valid.
impl<K: Default + Clone> LinkedList<K> {
    /// Create a [LinkedList] with the given predicted capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        LinkedList {
            nodes: Nodes::with_capacity(capacity),
            free: vec![],
        }
    }

    /// Reserve space for `additional` more nodes up front, avoiding incremental
    /// reallocation when bulk-loading a list of known size.
    ///
    /// Only the node storage is reserved, not the free list: bulk loading pushes
    /// new nodes rather than reusing freed ones, so the free list stays empty and
    /// needs no pre-allocation.
    pub fn reserve(&mut self, additional: usize) {
        self.nodes.reserve(additional);
    }

    /// Capacity of the backing node storage (excludes the head/tail sentinels).
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.nodes.data_nodes.capacity()
    }

    // Allocate a new node and return its index.
    // NOTE: this node is leaked if not used by caller
    fn new_node(&mut self, data: K) -> CompactIndex {
        if let Some(index) = self.free.pop() {
            // have a free node, update its payload and return its index
            self.nodes[index].data = data;
            index
        } else {
            // create a new node
            self.nodes.new_node(data)
        }
    }

    /// How many nodes in the list
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        // exclude the 2 sentinels
        self.nodes.len() - self.free.len()
    }

    fn valid_index(&self, index: Index) -> bool {
        is_valid_slot(index, self.nodes.len())
        // TODO: check node prev/next not NULL
        // TODO: debug_check index not in self.free
    }

    fn node(&self, index: Index) -> Option<&Node<K>> {
        if self.valid_index(index) {
            Some(&self.nodes[to_compact(index)])
        } else {
            None
        }
    }

    fn node_mut(&mut self, index: Index) -> Option<&mut Node<K>> {
        if self.valid_index(index) {
            Some(&mut self.nodes[to_compact(index)])
        } else {
            None
        }
    }

    /// Peek into the list, returning a reference to the data at the given index.
    ///
    /// Returns `None` for sentinels and out-of-range indices. Removed slots
    /// remain addressable with their default value until reused.
    pub fn peek(&self, index: Index) -> Option<&K> {
        self.node(index).map(|n| &n.data)
    }

    /// Peek into the list, returning a mutable reference to the data at the given index.
    pub fn peek_mut(&mut self, index: Index) -> Option<&mut K> {
        self.node_mut(index).map(|n| &mut n.data)
    }

    // safe because the index still needs to be in the range of the vec
    fn peek_unchecked(&self, index: CompactIndex) -> &K {
        &self.nodes[index].data
    }

    // put a node right after the node at `at`
    fn insert_after(&mut self, node_index: CompactIndex, at: CompactIndex) {
        assert!(at != TAIL && at != node_index); // can't insert after tail or to itself

        let next = replace(&mut self.nodes[at].next, node_index);

        let node = &mut self.nodes[node_index];
        node.next = next;
        node.prev = at;

        self.nodes[next].prev = node_index;
    }

    /// Put the data at the head of the list.
    ///
    /// # Panics
    ///
    /// Panics if a new slot is needed after `u32::MAX - 2` slots have been
    /// allocated.
    pub fn push_head(&mut self, data: K) -> Index {
        let new_node_index = self.new_node(data);
        self.insert_after(new_node_index, HEAD);
        to_index(new_node_index)
    }

    /// Put the data at the tail of the list.
    ///
    /// # Panics
    ///
    /// Panics if a new slot is needed after `u32::MAX - 2` slots have been
    /// allocated.
    pub fn push_tail(&mut self, data: K) -> Index {
        let new_node_index = self.new_node(data);
        self.insert_after(new_node_index, self.nodes.tail().prev);
        to_index(new_node_index)
    }

    /// Unlink a node from the list without touching its data.
    ///
    /// After this call the node's prev/next pointers are `NULL` and the
    /// surrounding nodes skip over it. The node can be re-inserted
    /// elsewhere (e.g. by [`promote`]) or freed.
    fn unlink(&mut self, index: CompactIndex) {
        // can't touch the sentinels
        assert!(index != HEAD && index != TAIL);

        let node = &mut self.nodes[index];

        // zero out the pointers, useful in case we try to access a freed node
        let prev = replace(&mut node.prev, NULL);
        let next = replace(&mut node.next, NULL);

        // make sure we are accessing a node in the list, not freed already
        assert!(prev != NULL && next != NULL);

        self.nodes[prev].next = next;
        self.nodes[next].prev = prev;
    }

    /// Remove a node, return its value, and add its slot to the free list.
    fn remove_at(&mut self, index: CompactIndex) -> K {
        self.unlink(index);
        self.free.push(index);
        std::mem::take(&mut self.nodes[index].data)
    }

    /// Remove the node at the index, and return the value.
    ///
    /// Uses [`std::mem::take`] to move data out of the node without
    /// cloning, which avoids a heap allocation for types like `String`.
    ///
    /// # Panics
    ///
    /// Panics if `index` is invalid, already removed, or at least `u32::MAX`.
    pub fn remove(&mut self, index: Index) -> K {
        self.remove_at(to_compact(index))
    }

    /// Remove the tail of the list
    pub fn pop_tail(&mut self) -> Option<K> {
        let data_tail = self.nodes.tail().prev;
        if data_tail == HEAD {
            None // empty list
        } else {
            Some(self.remove_at(data_tail))
        }
    }

    /// Put the node at the index to the head.
    ///
    /// # Panics
    ///
    /// Panics if `index` is invalid, already removed, or at least `u32::MAX`.
    pub fn promote(&mut self, index: Index) {
        let index = to_compact(index);
        if self.nodes.head().next == index {
            return; // already head
        }
        self.unlink(index);
        self.insert_after(index, HEAD);
    }

    fn next_compact(&self, index: CompactIndex) -> CompactIndex {
        self.nodes[index].next
    }

    fn prev_compact(&self, index: CompactIndex) -> CompactIndex {
        self.nodes[index].prev
    }

    /// Get the next index in the list (internal navigation).
    ///
    /// # Panics
    ///
    /// Panics if `index` is out of range or at least `u32::MAX`.
    pub fn next_index(&self, index: Index) -> Index {
        to_index(self.next_compact(to_compact(index)))
    }

    /// Get the previous index in the list (internal navigation).
    ///
    /// # Panics
    ///
    /// Panics if `index` is out of range or at least `u32::MAX`.
    pub fn prev_index(&self, index: Index) -> Index {
        to_index(self.prev_compact(to_compact(index)))
    }

    /// Get the head of the list
    pub fn head(&self) -> Option<Index> {
        let data_head = self.nodes.head().next;
        if data_head == TAIL {
            None
        } else {
            Some(to_index(data_head))
        }
    }

    /// Get the tail of the list
    pub fn tail(&self) -> Option<Index> {
        let data_tail = self.nodes.tail().prev;
        if data_tail == HEAD {
            None
        } else {
            Some(to_index(data_tail))
        }
    }

    /// Iterate over the list
    pub fn iter(&self) -> LinkedListIter<'_, K> {
        LinkedListIter {
            list: self,
            head: HEAD,
            tail: TAIL,
            len: self.len(),
        }
    }
}

/// `exist_near_head` is only meaningful when K is comparable by equality.
impl<K: Default + Clone + PartialEq> LinkedList<K> {
    /// Whether the value exists close (up to search_limit nodes) to the head of the list
    pub fn exist_near_head(&self, value: &K, search_limit: usize) -> bool {
        let mut current_node = HEAD;
        for _ in 0..search_limit {
            current_node = self.nodes[current_node].next;
            if current_node == TAIL {
                return false;
            }
            if self.nodes[current_node].data == *value {
                return true;
            }
        }
        false
    }
}

/// The iter over the list
pub struct LinkedListIter<'a, K = u64> {
    list: &'a LinkedList<K>,
    head: CompactIndex,
    #[cfg_attr(not(test), allow(dead_code))]
    tail: CompactIndex,
    len: usize,
}

impl<'a, K: Default + Clone> Iterator for LinkedListIter<'a, K> {
    type Item = &'a K;

    fn next(&mut self) -> Option<Self::Item> {
        let next_index = self.list.next_compact(self.head);
        if next_index == TAIL || next_index == NULL {
            None
        } else {
            self.head = next_index;
            self.len -= 1;
            Some(self.list.peek_unchecked(next_index))
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }
}

impl<K: Default + Clone> DoubleEndedIterator for LinkedListIter<'_, K> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let prev_index = self.list.prev_compact(self.tail);
        if prev_index == HEAD || prev_index == NULL {
            None
        } else {
            self.tail = prev_index;
            self.len -= 1;
            Some(self.list.peek_unchecked(prev_index))
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // assert the list is the same as `values`
    fn assert_list(list: &LinkedList<u64>, values: &[u64]) {
        let list_values: Vec<_> = list.iter().copied().collect();
        assert_eq!(values, &list_values)
    }

    fn assert_list_reverse(list: &LinkedList<u64>, values: &[u64]) {
        let list_values: Vec<_> = list.iter().rev().copied().collect();
        assert_eq!(values, &list_values)
    }

    #[test]
    fn test_insert() {
        let mut list = LinkedList::with_capacity(10);
        assert_eq!(list.len(), 0);
        assert!(list.node(2).is_none());
        assert_eq!(list.head(), None);
        assert_eq!(list.tail(), None);

        let index1 = list.push_head(2u64);
        assert_eq!(list.len(), 1);
        assert_eq!(*list.peek(index1).unwrap(), 2);

        let index2 = list.push_head(3u64);
        assert_eq!(list.head(), Some(index2));
        assert_eq!(list.tail(), Some(index1));

        let index3 = list.push_tail(4u64);
        assert_eq!(list.head(), Some(index2));
        assert_eq!(list.tail(), Some(index3));

        assert_list(&list, &[3, 2, 4]);
        assert_list_reverse(&list, &[4, 2, 3]);
    }

    #[test]
    fn test_reserve() {
        let mut list = LinkedList::<u64>::with_capacity(0);
        assert_eq!(list.capacity(), 0);

        list.reserve(100);
        // `reserve_exact` from empty => no doubling slack.
        assert_eq!(list.capacity(), 100);

        for i in 0..100u64 {
            list.push_tail(i);
        }
        // The pre-sized capacity absorbed all pushes without reallocating.
        assert_eq!(list.len(), 100);
        assert_eq!(list.capacity(), 100);
    }

    #[test]
    fn test_pop() {
        let mut list = LinkedList::with_capacity(10);
        list.push_head(2u64);
        list.push_head(3u64);
        list.push_tail(4u64);
        assert_list(&list, &[3, 2, 4]);
        assert_eq!(list.pop_tail(), Some(4));
        assert_eq!(list.pop_tail(), Some(2));
        assert_eq!(list.pop_tail(), Some(3));
        assert_eq!(list.pop_tail(), None);
    }

    #[test]
    fn test_promote() {
        let mut list = LinkedList::with_capacity(10);
        let index2 = list.push_head(2u64);
        let index3 = list.push_head(3u64);
        let index4 = list.push_tail(4u64);
        assert_list(&list, &[3, 2, 4]);

        list.promote(index3);
        assert_list(&list, &[3, 2, 4]);

        list.promote(index2);
        assert_list(&list, &[2, 3, 4]);

        list.promote(index4);
        assert_list(&list, &[4, 2, 3]);
    }

    #[test]
    fn test_exist_near_head() {
        let mut list = LinkedList::with_capacity(10);
        list.push_head(2u64);
        list.push_head(3u64);
        list.push_tail(4u64);
        assert_list(&list, &[3, 2, 4]);

        assert!(!list.exist_near_head(&4, 1));
        assert!(!list.exist_near_head(&4, 2));
        assert!(list.exist_near_head(&4, 3));
        assert!(list.exist_near_head(&4, 4));
        assert!(list.exist_near_head(&4, 99999));
    }

    #[test]
    fn test_generic_string_keys() {
        let mut list: LinkedList<String> = LinkedList::with_capacity(10);

        let i1 = list.push_head("hello".to_string());
        let i2 = list.push_head("world".to_string());
        let _i3 = list.push_tail("foo".to_string());

        let values: Vec<_> = list.iter().cloned().collect();
        assert_eq!(values, vec!["world", "hello", "foo"]);

        list.promote(i1);
        let values: Vec<_> = list.iter().cloned().collect();
        assert_eq!(values, vec!["hello", "world", "foo"]);

        assert_eq!(list.pop_tail(), Some("foo".to_string()));
        assert_eq!(list.remove(i2), "world".to_string());
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn test_node_is_compact() {
        // Compact links reduce `Node<u64>` from 24 to 16 bytes on 64-bit targets.
        if size_of::<usize>() == 8 {
            assert_eq!(size_of::<Node<u64>>(), 16);
        }
        assert!(size_of::<Node<u64>>() <= 24);
    }

    #[test]
    fn test_peek_invalid_index_returns_none() {
        let mut list = LinkedList::<u64>::with_capacity(4);
        let valid = list.push_head(1u64);
        assert!(list.peek(valid).is_some());

        assert!(list.peek(100).is_none());
        assert!(list.peek(to_index(HEAD)).is_none());
        assert!(list.peek(to_index(TAIL)).is_none());
        assert!(list.peek(to_index(NULL)).is_none());
        assert!(list.peek(usize::MAX).is_none());
    }

    #[test]
    fn test_freed_index_reuse() {
        let mut list = LinkedList::with_capacity(4);
        let i1 = list.push_head(1u64);
        let i2 = list.push_head(2u64);
        let _i3 = list.push_head(3u64);
        assert_list(&list, &[3, 2, 1]);

        assert_eq!(list.remove(i2), 2);
        assert_eq!(list.len(), 2);
        // Removed slots remain addressable with their default value.
        assert_eq!(list.peek(i2), Some(&0));

        // The next insertion reuses the freed slot.
        let i4 = list.push_head(4u64);
        assert_eq!(i4, i2, "freed slot index should be reused");
        assert_eq!(*list.peek(i4).unwrap(), 4);
        assert_eq!(*list.peek(i1).unwrap(), 1);
        assert_list(&list, &[4, 3, 1]);
        assert_list_reverse(&list, &[1, 3, 4]);
    }

    #[test]
    fn test_to_compact_roundtrip() {
        for i in [0usize, 1, 2, 100, MAX_SLOTS - 1] {
            assert_eq!(to_index(to_compact(i)), i);
        }
        assert_eq!(to_compact(MAX_SLOTS - 1), NULL - 1);
    }

    #[test]
    fn test_to_compact_reserves_null() {
        assert!(std::panic::catch_unwind(|| to_compact(to_index(NULL))).is_err());
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn test_to_compact_rejects_overflow() {
        let too_big = u32::MAX as usize + 1;
        assert!(std::panic::catch_unwind(|| to_compact(too_big)).is_err());
    }

    #[test]
    fn test_is_valid_slot_sentinels_and_small_range() {
        assert!(!is_valid_slot(to_index(HEAD), 10));
        assert!(!is_valid_slot(to_index(TAIL), 10));

        assert!(!is_valid_slot(to_index(OFFSET), 0));

        let offset = to_index(OFFSET);
        assert!(is_valid_slot(offset, 10));
        assert!(is_valid_slot(offset + 9, 10));
        assert!(!is_valid_slot(offset + 10, 10));
    }

    #[test]
    fn test_is_valid_slot_max_length_rejects_null() {
        // Test the maximum length without allocating it.
        let max_data_len = MAX_SLOTS - to_index(OFFSET);

        let highest_valid = to_index(NULL) - 1;
        assert!(is_valid_slot(highest_valid, max_data_len));

        assert!(!is_valid_slot(to_index(NULL), max_data_len));
        assert!(!is_valid_slot(usize::MAX, max_data_len));
    }
}
