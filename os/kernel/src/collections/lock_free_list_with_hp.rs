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
   ║ Author: Lazar Konstantinou, 09.09.2025, HHU                             ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/

use core::{ptr, sync::atomic::{AtomicPtr, Ordering::SeqCst}};

use alloc::boxed::Box;

use crate::collections::hazard_pointers::{retire_node, HPRecType};

pub(crate) struct Node<KeyType> {
    key: Option<KeyType>,
    next: AtomicPtr<Node<KeyType>>
}

impl<KeyType> Node<KeyType> {
    pub fn new(key: KeyType) -> Self {
        Self {
            key: Some(key),
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }

    pub fn new_sentinel() -> Self {
        Self {
            key: None,
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

impl<KeyType: Clone + PartialEq + PartialOrd> LockFreeList<KeyType> {
    pub fn new() -> Self {
        // head = new Node<KeyType> ();
        let head = Box::into_raw(Box::new(Node::new_sentinel()));
        // tail = new Node<KeyType> ();
        let tail = Box::into_raw(Box::new(Node::new_sentinel()));
        // head.next = tail;
        unsafe { (*head).next.store(tail, SeqCst); }

        Self {
            head: AtomicPtr::new(head),
            tail: AtomicPtr::new(tail),
        }
    }

    pub fn insert(&self, search_key: KeyType, myhprec: *mut HPRecType<Node<KeyType>>) -> bool {
        let new_node = Box::into_raw(Box::new(Node::new(search_key.clone())));

        // Node *right_node, *left_node;
        let mut right_node: *mut Node<KeyType>;
        let mut left_node: *mut Node<KeyType> = ptr::null_mut();

        loop { /*B3: do {...} while (true);*/
            // right_node search(search_key, &left_node);
            right_node = self.search(search_key.clone(), &mut left_node, myhprec);

            // if ((right_node != tail) && (right_node.key == search_key)) return false; /*T1*/
            if right_node != self.tail.load(SeqCst) && unsafe { (*right_node).key.as_ref() } == Some(&search_key) {
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

    pub fn delete(&self, search_key: KeyType, myhprec: *mut HPRecType<Node<KeyType>>) -> bool {
        // Node *right_node, *right_node_next, *left_node;
        let mut right_node: *mut Node<KeyType>;
        let mut right_node_next: *mut Node<KeyType>;
        let mut left_node: *mut Node<KeyType> = ptr::null_mut();

        // do {...} while (true); /*B4*/
        loop {
            // right_node = search(search_key, &left_node);
            right_node = self.search(search_key.clone(), &mut left_node, myhprec);

            // if ((right_node == tail) || (right_node.key != search_key)) { return false; } /*T1*/
            if (right_node == self.tail.load(SeqCst)) || unsafe { (*right_node).key.as_ref() } != Some(&search_key) {
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

        // if CAS(prev,cur,next) RetireNode(cur); else Find(head,key); /*C4*/
        if unsafe { (*left_node).next.compare_exchange(right_node, right_node_next, SeqCst, SeqCst).is_ok() } {
            // RetireNode(cur);
            retire_node(right_node, myhprec);
        } else {
            // Find(head,key);
            _ = self.search(search_key, &mut left_node, myhprec);
        }

        // return true;
        true
    }

    pub fn find(&self, search_key: KeyType, myhprec: *mut HPRecType<Node<KeyType>>) -> bool {
        // Node *right_node, *left_node;
        let right_node: *mut Node<KeyType>;
        let mut left_node: *mut Node<KeyType> = ptr::null_mut();

        // right_node =  search(search_key, &left_node);
        right_node = self.search(search_key.clone(), &mut left_node, myhprec);

        // if ((right_node == tail) || (right_node.key != search_key)) { return false; }
        if right_node == self.tail.load(SeqCst) || unsafe { (*right_node).key.as_ref() } != Some(&search_key) {
            return false;
        } // else { return true; }
        else {
            return true;
        }
    }

    fn search(&self, search_key: KeyType, left_node: &mut *mut Node<KeyType>, myhprec: *mut HPRecType<Node<KeyType>>) -> *mut Node<KeyType> {
        // try_again: do { ... }
        'try_again: loop {
            // prev <- head;
            *left_node = self.head.load(SeqCst);

            // cur <- *prev;
            let mut cur: *mut Node<KeyType> = unsafe { (**left_node).next.load(SeqCst) };

            // while (cur != null) {
            while cur != self.tail.load(SeqCst) {
                // *hp0 <- cur;
                unsafe { (*myhprec).hp[0].store(cur, SeqCst); }

                // if (*prev != cur) goto try_again;
                if unsafe { (**left_node).next.load(SeqCst) } != cur {
                    continue 'try_again;
                }

                // next <- cur^.Next;
                let next = unsafe { (*cur).next.load(SeqCst) };

                // if (next & 1) { // bitwise AND
                if is_marked_reference(next) {
                    let unmarked_next = get_unmarked_reference(next);

                    // if !CAS(prev,cur,next-1) goto try_again;
                    if unsafe { (**left_node).next.compare_exchange(cur, unmarked_next, SeqCst, SeqCst).is_err() } {
                        continue 'try_again;
                    }

                    // RetireNode(cur);
                    retire_node(cur, myhprec);

                    // cur <- next-1;
                    cur = unmarked_next;
                } else {
                    // ckey <- cur^.Key;
                    let ckey = unsafe { (*cur).key.clone() }.expect("cur is never a sentinel here");

                    // if (*prev != cur) goto try_again;
                    if unsafe { (**left_node).next.load(SeqCst) } != cur {
                        continue 'try_again;
                    }

                    // if (ckey>=key) return (ckey = key);
                    if ckey >= search_key {
                        return cur;
                    }

                    // prev <- &cur^.Next;
                    *left_node = cur;

                    // tmp <- hp0; hp0 <- hp1; hp1 <- tmp; // all private
                    unsafe {
                        let tmp = (*myhprec).hp[0].load(SeqCst);
                        (*myhprec).hp[0].store((*myhprec).hp[1].load(SeqCst), SeqCst);
                        (*myhprec).hp[1].store(tmp, SeqCst);
                    }

                    // cur <- next;
                    cur = next;
                }
            }

            // return false;
            return self.tail.load(SeqCst);
        }
    }
    
    pub fn find_and_remove<F: Fn(&KeyType) -> bool>(&self, matches: F, myhprec: *mut HPRecType<Node<KeyType>>) -> Option<KeyType> {
        // try_again: do { ... }
        'try_again: loop {
            // prev <- head;
            let mut prev: *mut Node<KeyType> = self.head.load(SeqCst);

            // cur <- *prev;
            let mut cur: *mut Node<KeyType> = unsafe { (*prev).next.load(SeqCst) };

            // while (cur != null) {
            while cur != self.tail.load(SeqCst) {
                // *hp0 <- cur;
                unsafe { (*myhprec).hp[0].store(cur, SeqCst); }

                // if (*prev != cur) goto try_again;
                if unsafe { (*prev).next.load(SeqCst) } != cur {
                    continue 'try_again;
                }

                // next <- cur^.Next;
                let next = unsafe { (*cur).next.load(SeqCst) };

                // if (next & 1) { // bitwise AND
                if is_marked_reference(next) {
                    let unmarked_next = get_unmarked_reference(next);

                    // if !CAS(prev,cur,next-1) goto try_again;
                    if unsafe { (*prev).next.compare_exchange(cur, unmarked_next, SeqCst, SeqCst).is_err() } {
                        continue 'try_again;
                    }

                    // RetireNode(cur);
                    retire_node(cur, myhprec);

                    // cur <- next-1;
                    cur = unmarked_next;
                } else {
                    // ckey <- cur^.Key;
                    let ckey = unsafe { (*cur).key.clone() }.expect("cur is never a sentinel here");

                    // if (*prev != cur) goto try_again;
                    if unsafe { (*prev).next.load(SeqCst) } != cur {
                        continue 'try_again;
                    }

                    if matches(&ckey) {
                        return if self.delete(ckey.clone(), myhprec) {
                            Some(ckey)
                        } else {
                            None
                        };
                    }

                    // prev <- &cur^.Next;
                    prev = cur;

                    // tmp <- hp0; hp0 <- hp1; hp1 <- tmp; // all private
                    unsafe {
                        let tmp = (*myhprec).hp[0].load(SeqCst);
                        (*myhprec).hp[0].store((*myhprec).hp[1].load(SeqCst), SeqCst);
                        (*myhprec).hp[1].store(tmp, SeqCst);
                    }

                    // cur <- next;
                    cur = next;
                }
            }

            // return false; (i.e. no match found)
            return None;
        }
    }

    pub fn find_or_insert_with<F, R>(&self, key: KeyType, myhprec: *mut HPRecType<Node<KeyType>>, f: F) -> R 
    where F: FnOnce(&KeyType) -> R {
        loop {
            self.insert(key.clone(), myhprec);
            let mut left_node: *mut Node<KeyType> = ptr::null_mut();

            let found = self.search(key.clone(), &mut left_node, myhprec);
            
            if found != self.tail.load(SeqCst) && unsafe { (*found).key.as_ref() } == Some(&key) {
                let entry = unsafe { (*found).key.as_ref() }.expect("found is never a sentinel here");
                return f(entry);
            }
        }
    }

    pub fn pop_front_if<F: Fn(&KeyType) -> bool>(&self, min_query: KeyType, is_due: F, myhprec: *mut HPRecType<Node<KeyType>>) -> Option<KeyType> {
        let mut left_node: *mut Node<KeyType> = ptr::null_mut();
        let candidate = self.search(min_query, &mut left_node, myhprec);
        if candidate == self.tail.load(SeqCst) {
            return None;
        }

        let key = unsafe { (*candidate).key.clone() }.expect("candidate is never a sentinel here");
        if !is_due(&key) {
            return None;
        }

        if self.delete(key.clone(), myhprec) {
            Some(key)
        } else {
            None
        }
    }

}