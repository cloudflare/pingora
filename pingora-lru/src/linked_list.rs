// Copyright 2024 Cloudflare, Inc.
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

//! Doubly linked list
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
struct Node {
    pub(crate) prev: Index,
    pub(crate) next: Index,
    pub(crate) data: u64,
}

// Functionally the same as vec![head, tail, data_nodes...] where head & tail are fixed and
// the rest data nodes can expand. Both head and tail can be accessed faster than using index
struct Nodes {
    // we use these sentinel nodes to guard the head and tail of the list so that list
    // manipulation is simpler (fewer if-else)
    head: Node,
    tail: Node,
    data_nodes: Vec<Node>,
}

impl Nodes {
    fn with_capacity(capacity: usize) -> Self {
        Nodes {
            head: Node {
                prev: NULL,
                next: TAIL,
                data: 0,
            },
            tail: Node {
                prev: HEAD,
                next: NULL,
                data: 0,
            },
            data_nodes: Vec::with_capacity(capacity),
        }
    }

    fn new_node(&mut self, data: u64) -> Index {
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

    fn head(&self) -> &Node {
        &self.head
    }

    fn tail(&self) -> &Node {
        &self.tail
    }
}

impl std::ops::Index<usize> for Nodes {
    type Output = Node;

    fn index(&self, index: usize) -> &Self::Output {
        match index {
            HEAD => &self.head,
            TAIL => &self.tail,
            _ => &self.data_nodes[index - OFFSET],
        }
    }
}

impl std::ops::IndexMut<usize> for Nodes {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        match index {
            HEAD => &mut self.head,
            TAIL => &mut self.tail,
            _ => &mut self.data_nodes[index - OFFSET],
        }
    }
}

/// Doubly linked list
pub struct LinkedList {
    nodes: Nodes,
    free: Vec<Index>, // to keep track of freed node to be used again
}
// Panic when index used as parameters are invalid
// Index returned by push_* is always valid.
impl LinkedList {
    /// Create a [LinkedList] with the given predicted capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        LinkedList {
            nodes: Nodes::with_capacity(capacity),
            free: vec![],
        }
    }

    // Allocate a new node and return its index
    // NOTE: this node is leaked if not used by caller
    fn new_node(&mut self, data: u64) -> Index {
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

    fn node(&self, index: Index) -> Option<&Node> {
        if self.valid_index(index) {
            Some(&self.nodes[index])
        } else {
            None
        }
    }

    /// Peek into the list
    pub fn peek(&self, index: Index) -> Option<u64> {
        self.node(index).map(|n| n.data)
    }

    // safe because the index still needs to be in the range of the vec
    fn peek_unchecked(&self, index: Index) -> &u64 {
        &self.nodes[index].data
    }

    /// Whether the value exists closed (up to search_limit nodes) to the head of the list
    // It can be done via iter().take().find() but this is cheaper
    pub fn exist_near_head(&self, value: u64, search_limit: usize) -> bool {
        let mut current_node = HEAD;
        for _ in 0..search_limit {
            current_node = self.nodes[current_node].next;
            if current_node == TAIL {
                return false;
            }
            if self.nodes[current_node].data == value {
                return true;
            }
        }
        false
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
    pub fn push_head(&mut self, data: u64) -> Index {
        let new_node_index = self.new_node(data);
        self.insert_after(new_node_index, HEAD);
        new_node_index
    }

    /// Put the data at the tail of the list.
    pub fn push_tail(&mut self, data: u64) -> Index {
        let new_node_index = self.new_node(data);
        self.insert_after(new_node_index, self.nodes.tail().prev);
        new_node_index
    }

    // lift the node out of the linked list, to either delete it or insert to another place
    // NOTE: the node is leaked if not used by the caller
    fn lift(&mut self, index: Index) -> u64 {
        // can't touch the sentinels
        assert!(index != HEAD && index != TAIL);

        let node = &mut self.nodes[index];

        // zero out the pointers, useful in case we try to access a freed node
        let prev = replace(&mut node.prev, NULL);
        let next = replace(&mut node.next, NULL);
        let data = node.data;

        // make sure we are accessing a node in the list, not freed already
        assert!(prev != NULL && next != NULL);

        self.nodes[prev].next = next;
        self.nodes[next].prev = prev;

        data
    }

    /// Remove the node at the index, and return the value
    pub fn remove(&mut self, index: Index) -> u64 {
        self.free.push(index);
        self.lift(index)
    }

    /// Remove the tail of the list
    pub fn pop_tail(&mut self) -> Option<u64> {
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
        self.lift(index);
        self.insert_after(index, HEAD);
    }

    fn next(&self, index: Index) -> Index {
        self.nodes[index].next
    }

    fn prev(&self, index: Index) -> Index {
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
    pub fn iter(&self) -> LinkedListIter<'_> {
        LinkedListIter {
            list: self,
            head: HEAD,
            tail: TAIL,
            len: self.len(),
        }
    }
}

/// The iter over the list
pub struct LinkedListIter<'a> {
    list: &'a LinkedList,
    head: Index,
    tail: Index,
    len: usize,
}

impl<'a> Iterator for LinkedListIter<'a> {
    type Item = &'a u64;

    fn next(&mut self) -> Option<Self::Item> {
        let next_index = self.list.next(self.head);
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

impl<'a> DoubleEndedIterator for LinkedListIter<'a> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let prev_index = self.list.prev(self.tail);
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
    fn assert_list(list: &LinkedList, values: &[u64]) {
        let list_values: Vec<_> = list.iter().copied().collect();
        assert_eq!(values, &list_values)
    }

    fn assert_list_reverse(list: &LinkedList, values: &[u64]) {
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

        let index1 = list.push_head(2);
        assert_eq!(list.len(), 1);
        assert_eq!(list.peek(index1).unwrap(), 2);

        let index2 = list.push_head(3);
        assert_eq!(list.head(), Some(index2));
        assert_eq!(list.tail(), Some(index1));

        let index3 = list.push_tail(4);
        assert_eq!(list.head(), Some(index2));
        assert_eq!(list.tail(), Some(index3));

        assert_list(&list, &[3, 2, 4]);
        assert_list_reverse(&list, &[4, 2, 3]);
    }

    #[test]
    fn test_pop() {
        let mut list = LinkedList::with_capacity(10);
        list.push_head(2);
        list.push_head(3);
        list.push_tail(4);
        assert_list(&list, &[3, 2, 4]);
        assert_eq!(list.pop_tail(), Some(4));
        assert_eq!(list.pop_tail(), Some(2));
        assert_eq!(list.pop_tail(), Some(3));
        assert_eq!(list.pop_tail(), None);
    }

    #[test]
    fn test_promote() {
        let mut list = LinkedList::with_capacity(10);
        let index2 = list.push_head(2);
        let index3 = list.push_head(3);
        let index4 = list.push_tail(4);
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
        list.push_head(2);
        list.push_head(3);
        list.push_tail(4);
        assert_list(&list, &[3, 2, 4]);

        assert!(!list.exist_near_head(4, 1));
        assert!(!list.exist_near_head(4, 2));
        assert!(list.exist_near_head(4, 3));
        assert!(list.exist_near_head(4, 4));
        assert!(list.exist_near_head(4, 99999));
    }
}
