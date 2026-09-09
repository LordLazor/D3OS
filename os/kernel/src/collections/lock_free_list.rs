/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: lock_free_list.rs                                               ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Implementation of Timothy L. Harris Lock-Free Linked-List               ║
   ║ This implementation is based on his paper                               ║
   ║ "A Pragmatic Implementation of Non-Blocking Linked-Lists"               ║
   ║ https://timharris.uk/papers/2001-disc.pdf                               ║
   ║                                                                         ║
   ║ This implementation is then being combined with Hazard Pointers         ║
   ║ to provide safe memory reclamation for the nodes of the list.           ║
   ║ The Hazard Pointers are based on Maged M. Michael's paper               ║
   ║ "Hazard Pointers: Safe Memory Reclamation for Lock-Free Objects"        ║
   ║ https://dl.acm.org/doi/10.1109/TPDS.2004.8                              ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Lazar Konstantinou, 09.09.2025, HHU                                 ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/

use core::{ptr, sync::atomic::{AtomicPtr, Ordering::SeqCst}};

use alloc::boxed::Box;
struct Node<KeyType> {
    key: KeyType,
    next: AtomicPtr<Node<KeyType>>
}

impl<KeyType> Node<KeyType> {
    pub fn new(key: KeyType) -> Self {
        Self {
            key,
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

pub struct LockFreeList<KeyType> {
    head: AtomicPtr<Node<KeyType>>,
    tail: AtomicPtr<Node<KeyType>>,
}

/*
IDEA BEHIND THE MARK BIT:
We need to use the mark bit to indicate that a node is logically deleted

If we would store the mark bit in a separate field, we would have to use a lock to update both the next pointer and the mark bit atomically
This would totally defeat the purpose of a lock-free list, as we would have to use a lock to update the next pointer and the mark bit atomically
*/

// The mark is always stored in the low-order bit of the next pointer itself
// Each node is at least as aligned as its AtomicPtr field (8 bytes on 64-bit targets)
// so a valid, unmarked pointer always has bit 0 equal to 0
// The mark bit is used to indicate that a node is logically deleted, and it is set to 1 when the node is marked for deleteion
const MARK_BIT: usize = 1;

// true if it is a marked reference, false otherwise
fn is_marked_reference<KeyType>(reference: *mut Node<KeyType>) -> bool {
    (reference as usize) & MARK_BIT == MARK_BIT
}

// gets the unmarked version of a reference, i.e., the reference with the mark bit cleared
fn get_unmarked_reference<KeyType>(reference: *mut Node<KeyType>) -> *mut Node<KeyType> {
    ((reference as usize) & !MARK_BIT) as *mut Node<KeyType>
}

// gets the marked version of a reference, i.e., the reference with the mark bit set
fn get_marked_reference<KeyType>(reference: *mut Node<KeyType>) -> *mut Node<KeyType> {
    ((reference as usize) | MARK_BIT) as *mut Node<KeyType>
}

impl<KeyType: Clone + PartialEq + PartialOrd + Default> LockFreeList<KeyType> {
    pub fn new() -> Self {
        // head = new Node<KeyType> ();
        let head = Box::into_raw(Box::new(Node::new(KeyType::default())));
        // tail = new Node<KeyType> ();
        let tail = Box::into_raw(Box::new(Node::new(KeyType::default())));
        // head.next = tail;
        unsafe { (*head).next.store(tail, SeqCst); }

        Self {
            head: AtomicPtr::new(head),
            tail: AtomicPtr::new(tail),
        }
    }

    pub fn insert(&self, search_key: KeyType) -> bool {
        let new_node = Box::into_raw(Box::new(Node::new(search_key.clone())));
        
        // Node *right_node, *left_node;
        let mut right_node: *mut Node<KeyType>;
        let mut left_node: *mut Node<KeyType> = ptr::null_mut();

        loop { /*B3: do {...} while (true);*/
            // right_node search(search_key, &left_node);
            right_node = self.search(search_key.clone(), &mut left_node);

            // if ((right_node != tail) && (right_node.key == search_key)) return false; /*T1*/
            if right_node != self.tail.load(SeqCst) && unsafe { (*right_node).key == search_key } {
                return false;
            }

            // new_node.next = right_node;
            unsafe { (*new_node).next.store(right_node, SeqCst); }

            // if (CAS (&(left_node.next), right_node, new_node)) { return true; } /*C2*/
            if unsafe { (*left_node).next.compare_exchange(right_node, new_node, SeqCst, SeqCst).is_ok() } {
                return true;
            }
        }

    }

    pub fn delete(&self, search_key: KeyType) -> bool {
        // Node *right_node, *right_node_next, *left_node;
        let mut right_node: *mut Node<KeyType>;
        let mut right_node_next: *mut Node<KeyType>;
        let mut left_node: *mut Node<KeyType> = ptr::null_mut();

        // do {...} while (true); /*B4*/
        loop {
            // right_node = search(search_key, &left_node);
            right_node = self.search(search_key.clone(), &mut left_node);

            // if ((right_node == tail) || (right_node.key != search_key)) { return false; } /*T1*/
            if (right_node == self.tail.load(SeqCst)) || unsafe { (*right_node).key != search_key } {
                return false;
            }

            // right_node_next = right_node.next;
            right_node_next = unsafe { (*right_node).next.load(SeqCst) };

            // if (!is_marked_reference(right_node_next)) {
            //    if (CAS(&(right_nodee.next), right_node_next, get_marked_reference(right_node_next))) { break; } /*C3 */
            // }
            if !is_marked_reference(right_node_next) {
                let marked_next = get_marked_reference(right_node_next);
                if unsafe { (*right_node).next.compare_exchange(right_node_next, marked_next, SeqCst, SeqCst).is_ok() } {
                    break;
                }
            }

        }

        // if (!CAS (&(left_node.next), right_node, right_node_next)) {...} /*C4*/
        if unsafe { (*left_node).next.compare_exchange(right_node, right_node_next, SeqCst, SeqCst).is_ok() } {
            _ = self.search(search_key, &mut left_node);
        }

        // return true;
        true
    }

    pub fn find(&self, search_key: KeyType) -> bool {
        // Node *right_node, *left_node;
        let right_node: *mut Node<KeyType>;
        let mut left_node: *mut Node<KeyType> = ptr::null_mut();

        // right_node =  search(search_key, &left_node);
        right_node = self.search(search_key.clone(), &mut left_node);

        // if ((right_node == tail) || (right_node.key != search_key)) { return false; }
        if right_node == self.tail.load(SeqCst) || unsafe { (*right_node).key != search_key } {
            return false;
        } // else { return true; }
        else {
            return true;
        }
    }

    fn search(&self, search_key: KeyType, left_node: &mut *mut Node<KeyType>) -> *mut Node<KeyType> {
        // Node *left_node_next, *right_node;
        let mut left_node_next: *mut Node<KeyType> = ptr::null_mut();
        let mut right_node: *mut Node<KeyType>;

        // search_again: do { ... } while (true); /*B2*/
        'search_again: loop {
            // Node *t = head;
            let mut t: *mut Node<KeyType> = self.head.load(SeqCst);

            // Node *t_next = head.next;
            let mut t_next: *mut Node<KeyType> = unsafe { (*t).next.load(SeqCst) };

            /* 1: Find left_node and right_node */
            // do { ... } while (is_marked_reference(t_next) || (t.key < search_key)); /*B1*/
            loop { 
                // if (!is_marked_reference(t_next)) { ... }
                if !is_marked_reference(t_next) {
                    // (*left_node) = t;
                    *left_node = t;

                    // left_node_next = t_next;
                    left_node_next = t_next;
                }

                // t = get_unmarked_reference(t_next);
                t = get_unmarked_reference(t_next);

                // if (t == tail) { break; }
                if t == self.tail.load(SeqCst) {
                    break;
                }

                t_next = unsafe { (*t).next.load(SeqCst)};

                // Inner Loop Condition: while (is_marked_reference(t_next) || (t.key < search_key)) (B1)
                if !(is_marked_reference(t_next) || unsafe { (*t).key < search_key }) {
                    break;
                }
            }

            // right_node = t;
            right_node = t;

            /* 2: Check nodes are adjacent */
            // if (left_node_next == right_node) 
            if left_node_next == right_node {
                // if ((right_node != tail) && is_marked_reference(right_node.next))
                if right_node != self.tail.load(SeqCst) && is_marked_reference(unsafe { (*right_node).next.load(SeqCst) }) {
                    // goto search_again; /*G1*/
                    continue 'search_again;
                } 
                // else { return right_node; } /*R1*/    
                else {
                    return right_node;
                }
            }

            /* 3: Remove one or more marked nodes */
            // if (CAS(&(left_node.next), left_node_next, right_node)) /*C1*/
            if unsafe { (**left_node).next.compare_exchange(left_node_next, right_node, SeqCst, SeqCst).is_ok() } {
                // if ((right_node != tail) && is_marked_reference(right_node.next)) 
                if right_node != self.tail.load(SeqCst) && is_marked_reference(unsafe { (*right_node).next.load(SeqCst)}) {
                    // goto search_again /*G2*/
                    continue 'search_again;
                } 
                // else { return right_node; } /*R2*/
                else {
                    return right_node;
                }
            }

        }

    }

}