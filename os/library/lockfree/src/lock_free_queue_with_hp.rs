/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: lock_free_queue_with_hp.rs                                      ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ This is the implementation of a lock-free queue with hazard pointers.   ║
   ║ The Lock-Free Queue implementation is based on Maged M. Michael's paper ║
   ║ "Hazard Pointers: Safe Memory Reclamation for Lock-Free Objects"        ║
   ║ https://dl.acm.org/doi/10.1109/TPDS.2004.8                              ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Lazar Konstantinou, 16.09.2025, HHU                             ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/

use core::{ptr, sync::atomic::{AtomicPtr, Ordering::SeqCst}};

use alloc::boxed::Box;

use crate::hazard_pointers::{HPRecType, retire_node};

// structure NodeType { Data: DataType; Next: *NodeType; }
pub struct NodeType<DataType> {
    data: Option<DataType>,
    next: AtomicPtr<NodeType<DataType>>
}

pub struct LockFreeQueue<DataType, const HP_BASE: usize = 0> {
    // Shared Variables
    // Head, Tail, *NodeType;
    head: AtomicPtr<NodeType<DataType>>,
    tail: AtomicPtr<NodeType<DataType>>,
}

impl<DataType: Clone, const HP_BASE: usize> LockFreeQueue<DataType, HP_BASE> {
    pub fn new() -> Self {
        // Initially both Head and Tail point to a dummy node
        let dummy_node = Box::into_raw(Box::new(NodeType {
            data: None,
            next: AtomicPtr::new(core::ptr::null_mut()),
        }));

        Self {
            head: AtomicPtr::new(dummy_node),
            tail: AtomicPtr::new(dummy_node),
        }
    }

    // Enqueue(data: DataType) {
    pub fn enqueue(&self, data: DataType, myhprec: *mut HPRecType<NodeType<DataType>>) {
        // node = NewNode()
        // node^.Data = data;
        // node^.Next = null;
        let node = Box::into_raw(Box::new(NodeType {
            data: Some(data),
            next: AtomicPtr::new(ptr::null_mut()),
        }));

        let mut t: *mut NodeType<DataType>;

        // while true {...}
        loop {
            // t = Tail;
            t = self.tail.load(SeqCst);

            // *hp0 = t;
            unsafe { (*myhprec).hp[HP_BASE].store(t, SeqCst); }

            // if (Tail != t) continue;
            if self.tail.load(SeqCst) != t {
                continue;
            }

            // next = t^.Next;
            let next = unsafe { (*t).next.load(SeqCst) };

            // if (Tail != t) continue;
            if self.tail.load(SeqCst) != t {
                continue;
            }

            // if (next != null) { CAS(&Tail, t, next); continue; }
            if !next.is_null() {
                let _ = self.tail.compare_exchange(t, next, SeqCst, SeqCst);
                continue;
            }

            // if CAS(&t^.Next, null, node) break;
            if unsafe { (*t).next.compare_exchange(ptr::null_mut(), node, SeqCst, SeqCst).is_ok() } {
                break;
            }
     
        }

        // CAS(&Tail, t, node);
        let _ = self.tail.compare_exchange(t, node, SeqCst, SeqCst);

    }

    pub fn dequeue(&self, myhprec: *mut HPRecType<NodeType<DataType>>) -> Option<DataType> {
        let mut data: DataType;
        let mut h: *mut NodeType<DataType>;

        // while true {...}
        loop {
            // h = Head;
            h = self.head.load(SeqCst);

            // *hp0 = h;
            unsafe  { (*myhprec).hp[HP_BASE].store(h, SeqCst); }

            // if (Head != h) continue;
            if self.head.load(SeqCst) != h {
                continue;
            }

            // t = Tail;
            let t = self.tail.load(SeqCst);

            // next = h^.Next;
            let next = unsafe { (*h).next.load(SeqCst) };

            // *hp1 = next;
            unsafe { (*myhprec).hp[HP_BASE + 1].store(next, SeqCst); }

            // if (Head != h) continue;
            if self.head.load(SeqCst) != h {
                continue;
            }

            // if (next == nul) return EMPTY;
            if next.is_null() {
                return None;
            }

            // if (h == t) { CAS(&Tail, t, next); continue; }
            if h == t {
                let _ = self.tail.compare_exchange(t, next, SeqCst, SeqCst);
                continue;
            }

            // data = next^.Data;
            data = unsafe { (*next).data.clone() }.expect("next is never the initial dummy node here");

            // if CAS(&Head, h, next) break;
            if self.head.compare_exchange(h, next, SeqCst, SeqCst).is_ok() {
                break;
            }
        }

        // RetireNode(h)
        retire_node(h, myhprec);

        // return data;
        Some(data)

    }

 }