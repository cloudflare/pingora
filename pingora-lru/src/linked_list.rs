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

use std::mem::replace;

type Index = usize;
const NULL: Index = usize::MAX;
const HEAD: Index = 0;
const TAIL: Index = 1;
const OFFSET: usize = 2;

#[derive(Debug)]
struct Node<K> {
    pub(crate) prev: Index,
    pub(crate) next: Index,
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

    fn new_node(&mut self, data: K) -> Index {
        const VEC_EXP_GROWTH_CAP: usize = 65536;
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
        self.data_nodes.len() - 1 + OFFSET
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

impl<K> std::ops::Index<usize> for Nodes<K> {
    type Output = Node<K>;

    fn index(&self, index: usize) -> &Self::Output {
        match index {
            HEAD => &self.head,
            TAIL => &self.tail,
            _ => &self.data_nodes[index - OFFSET],
        }
    }
}

impl<K> std::ops::IndexMut<usize> for Nodes<K> {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        match index {
            HEAD => &mut self.head,
            TAIL => &mut self.tail,
            _ => &mut self.data_nodes[index - OFFSET],
        }
    }
}

/// Doubly linked list, generic over the stored data type `K`.
pub struct LinkedList<K = u64> {
    nodes: Nodes<K>,
    free: Vec<Index>, // to keep track of freed node to be used again
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

    // Allocate a new node and return its index
    // NOTE: this node is leaked if not used by caller
    fn new_node(&mut self, data: K) -> Index {
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
        index != HEAD && index != TAIL && index < self.nodes.len() + OFFSET
        // TODO: check node prev/next not NULL
        // TODO: debug_check index not in self.free
    }

    fn node(&self, index: Index) -> Option<&Node<K>> {
        if self.valid_index(index) {
            Some(&self.nodes[index])
        } else {
            None
        }
    }

    fn node_mut(&mut self, index: Index) -> Option<&mut Node<K>> {
        if self.valid_index(index) {
            Some(&mut self.nodes[index])
        } else {
            None
        }
    }

    /// Peek into the list, returning a reference to the data at the given index.
    pub fn peek(&self, index: Index) -> Option<&K> {
        self.node(index).map(|n| &n.data)
    }

    /// Peek into the list, returning a mutable reference to the data at the given index.
    pub fn peek_mut(&mut self, index: Index) -> Option<&mut K> {
        self.node_mut(index).map(|n| &mut n.data)
    }

    // safe because the index still needs to be in the range of the vec
    fn peek_unchecked(&self, index: Index) -> &K {
        &self.nodes[index].data
    }

    // put a node right after the node at `at`
    fn insert_after(&mut self, node_index: Index, at: Index) {
        assert!(at != TAIL && at != node_index); // can't insert after tail or to itself

        let next = replace(&mut self.nodes[at].next, node_index);

        let node = &mut self.nodes[node_index];
        node.next = next;
        node.prev = at;

        self.nodes[next].prev = node_index;
    }

    /// Put the data at the head of the list.
    pub fn push_head(&mut self, data: K) -> Index {
        let new_node_index = self.new_node(data);
        self.insert_after(new_node_index, HEAD);
        new_node_index
    }

    /// Put the data at the tail of the list.
    pub fn push_tail(&mut self, data: K) -> Index {
        let new_node_index = self.new_node(data);
        self.insert_after(new_node_index, self.nodes.tail().prev);
        new_node_index
    }

    /// Unlink a node from the list without touching its data.
    ///
    /// After this call the node's prev/next pointers are `NULL` and the
    /// surrounding nodes skip over it. The node can be re-inserted
    /// elsewhere (e.g. by [`promote`]) or freed.
    fn unlink(&mut self, index: Index) {
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

    /// Remove the node at the index, and return the value.
    ///
    /// Uses [`std::mem::take`] to move data out of the node without
    /// cloning, which avoids a heap allocation for types like `String`.
    pub fn remove(&mut self, index: Index) -> K {
        self.unlink(index);
        self.free.push(index);
        std::mem::take(&mut self.nodes[index].data)
    }

    /// Remove the tail of the list
    pub fn pop_tail(&mut self) -> Option<K> {
        let data_tail = self.nodes.tail().prev;
        if data_tail == HEAD {
            None // empty list
        } else {
            Some(self.remove(data_tail))
        }
    }

    /// Put the node at the index to the head
    pub fn promote(&mut self, index: Index) {
        if self.nodes.head().next == index {
            return; // already head
        }
        self.unlink(index);
        self.insert_after(index, HEAD);
    }

    /// Get the next index in the list (internal navigation).
    pub fn next_index(&self, index: Index) -> Index {
        self.nodes[index].next
    }

    /// Get the previous index in the list (internal navigation).
    pub fn prev_index(&self, index: Index) -> Index {
        self.nodes[index].prev
    }

    /// Get the head of the list
    pub fn head(&self) -> Option<Index> {
        let data_head = self.nodes.head().next;
        if data_head == TAIL {
            None
        } else {
            Some(data_head)
        }
    }

    /// Get the tail of the list
    pub fn tail(&self) -> Option<Index> {
        let data_tail = self.nodes.tail().prev;
        if data_tail == HEAD {
            None
        } else {
            Some(data_tail)
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
    head: Index,
    #[cfg_attr(not(test), allow(dead_code))]
    tail: Index,
    len: usize,
}

impl<'a, K: Default + Clone> Iterator for LinkedListIter<'a, K> {
    type Item = &'a K;

    fn next(&mut self) -> Option<Self::Item> {
        let next_index = self.list.next_index(self.head);
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
        let prev_index = self.list.prev_index(self.tail);
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
}
