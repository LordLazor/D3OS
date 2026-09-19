/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: lock_free_single_writer_multiple_reader_doubly_linked_list_with_hp.rs ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ This is the implementation of a lock-free single-writer multiple-reader ║
   ║ doubly-linked list with hazard pointers.                                ║
   ║ The Lock-Free Queue implementation is based on Maged M. Michael's paper ║
   ║ "Hazard Pointers: Safe Memory Reclamation for Lock-Free Objects"        ║
   ║ https://dl.acm.org/doi/10.1109/TPDS.2004.8                              ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Lazar Konstantinou, 17.09.2025, HHU                             ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/

use core::{ptr, sync::atomic::{AtomicPtr, Ordering::SeqCst}};

use alloc::boxed::Box;

use crate::hazard_pointers::{HPRecType, retire_node};

// structure NodeType { Key: KeyType; Data: DataType; Prev: **NodeType; Next: *NodeType; }
pub struct NodeType<KeyType, DataType> {
    key: Option<KeyType>,
    data: Option<DataType>,
    prev: AtomicPtr<AtomicPtr<NodeType<KeyType, DataType>>>,
    next: AtomicPtr<NodeType<KeyType, DataType>>,
}

impl<KeyType, DataType> NodeType<KeyType, DataType> {
    fn new(key: KeyType, data: DataType) -> Self {
        Self {
            key: Some(key),
            data: Some(data),
            prev: AtomicPtr::new(ptr::null_mut()),
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

// SingleWriterInsertAfter(prev: **NodeType, node: *NodeType) {
fn single_writer_insert_after<KeyType, DataType>(prev: *mut AtomicPtr<NodeType<KeyType, DataType>>, node: *mut NodeType<KeyType, DataType>) {
    // next = *prev;
    let next = unsafe { (*prev).load(SeqCst) };

    // node^.Prev = prev;
    unsafe { (*node).prev.store(prev, SeqCst); }

    // node^.Next = next;
    unsafe { (*node).next.store(next, SeqCst); }

    // if (next != null) next^.Prev = &node^.Next;
    if !next.is_null() {
        unsafe { (*next).prev.store(&(*node).next as *const _ as *mut _, SeqCst); }
    }

    // *prev = node; // Inserted
    unsafe { (*prev).store(node, SeqCst); }
}

// SingleWriterDelete(node: *NodeType) {
fn single_writer_delete<KeyType, DataType>(node: *mut NodeType<KeyType, DataType>, myhprec: *mut HPRecType<NodeType<KeyType, DataType>>) {
    // prev = node^.Prev;
    let prev = unsafe { (*node).prev.load(SeqCst) };

    // next = node^.Next;
    let next = unsafe { (*node).next.load(SeqCst) };

    // if (next != null) next^.Prev = prev;
    if !next.is_null() {
        unsafe { (*next).prev.store(prev, SeqCst); }
    }

    // *prev = next; // Deleted
    unsafe { (*prev).store(next, SeqCst); }

    // node^.Next = null; // To alert readers not to proceed
    unsafe { (*node).next.store(core::ptr::null_mut(), SeqCst); }

    // RetireNode(node);
    retire_node(node, myhprec);
}

// ReaderSearch(head: **NodeType, key: KeyType) -> DataType {
fn reader_search<KeyType: PartialEq, DataType: Clone, const HP_BASE: usize>(head: *mut AtomicPtr<NodeType<KeyType, DataType>>, key: KeyType, myhprec: *mut HPRecType<NodeType<KeyType, DataType>>) -> Option<DataType> {
    // try_again:
    'try_again: loop {
        // prev = head;
        let mut prev = head;

        // cur = *prev;
        let mut cur = unsafe { (*prev).load(SeqCst) };

        // while (cur != null) {...}
        while !cur.is_null() {
            // *hp0 = cur;
            unsafe { (*myhprec).hp[HP_BASE].store(cur, SeqCst); }

            // if (*prev != cur) goto try_again;
            if unsafe { (*prev).load(SeqCst) } != cur {
                continue 'try_again;
            }

            // next = cur^.Next;
            let next = unsafe { (*cur).next.load(SeqCst) };

            // ckey = cur^.Key;
            // let ckey = unsafe { (*cur).key.as_ref() }; // Not needed as used directly in the next line

            // if (cur^.Key == key) {
            if unsafe { (*cur).key.as_ref() } == Some(&key) {
                // data = cur^.Data;
                let data = unsafe { (*cur).data.clone() };

                // if (*prev != cur) goto try_again;
                if unsafe { (*prev).load(SeqCst) } != cur {
                    continue 'try_again;
                }

                return data;
            }

            // if (*prev != cur) goto try_again;
            if unsafe { (*prev).load(SeqCst) } != cur {
                continue 'try_again;
            }

            // prev = &cur^.Next;
            prev = unsafe { &(*cur).next as *const _ as *mut _ };

            // tmp = hp0; hp0 = hp1; hp1 = tmp;
            unsafe {
                let tmp = (*myhprec).hp[HP_BASE].load(SeqCst);
                (*myhprec).hp[HP_BASE].store((*myhprec).hp[HP_BASE + 1].load(SeqCst), SeqCst);
                (*myhprec).hp[HP_BASE + 1].store(tmp, SeqCst);
            }

            // cur = next;
            cur = next;
        }

        // return NOTFOUND;
        return None;
    }
}

pub struct LockFreeSingleWriterMultipleReaderDoublyLinkedList<KeyType, DataType, const HP_BASE: usize = 0> {
    head: AtomicPtr<NodeType<KeyType, DataType>>,
}

impl<KeyType: PartialEq, DataType: Clone, const HP_BASE: usize> LockFreeSingleWriterMultipleReaderDoublyLinkedList<KeyType, DataType, HP_BASE> {
    pub fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
        }
    }

    pub fn insert_front(&self, key: KeyType, data: DataType) -> *mut NodeType<KeyType, DataType> {
        let node = Box::into_raw(Box::new(NodeType::new(key, data)));
        single_writer_insert_after(&self.head as *const _ as *mut _, node);
        node
    }

    pub fn insert_after(&self, prev_node: *mut NodeType<KeyType, DataType>, key: KeyType, data: DataType) -> *mut NodeType<KeyType, DataType> {
        let node = Box::into_raw(Box::new(NodeType::new(key, data)));
        single_writer_insert_after(unsafe { &(*prev_node).next as *const _ as *mut _ }, node);
        node
    }

    pub fn delete(&self, node: *mut NodeType<KeyType, DataType>, myhprec: *mut HPRecType<NodeType<KeyType, DataType>>) {
        single_writer_delete(node, myhprec);
    }

    pub fn search(&self, key: KeyType, myhprec: *mut HPRecType<NodeType<KeyType, DataType>>) -> Option<DataType> {
        reader_search::<KeyType, DataType, HP_BASE>(&self.head as *const _ as *mut _, key, myhprec)
    }
}
