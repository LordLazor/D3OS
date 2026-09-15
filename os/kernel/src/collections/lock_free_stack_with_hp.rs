/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: lock_free_stack_with_hp.rs                                      ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ This is the implementation of a lock-free stack with hazard pointers.   ║
   ║ The Lock-Free Stack implementation is based on Maged M. Michael's paper ║
   ║ "Hazard Pointers: Safe Memory Reclamation for Lock-Free Objects"        ║
   ║ https://dl.acm.org/doi/10.1109/TPDS.2004.8                              ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Lazar Konstantinou, 14.09.2025, HHU                             ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/

/*
This is used for the join_map in the Scheduler
The idea is to have a Lock-Free List and then Join Entries inside the List.
Each Join Entry will have a thread id and a stack of threads that are waiting for the thread
to end with that thread id.
*/

use core::{ptr, sync::atomic::{AtomicPtr, Ordering::SeqCst}};

use alloc::boxed::Box;

use crate::collections::hazard_pointers::{HPRecType, retire_node};

// structure NodeType { Data: DataType; Next: *NodeType; }
pub(crate) struct NodeType<DataType> {
    data: Option<DataType>,
    next: AtomicPtr<NodeType<DataType>>
}

pub(crate) struct LockFreeStack<DataType> {
    // Shared Variables
    // Top: *NodeType; // Initially null
    top: AtomicPtr<NodeType<DataType>>,
}

impl<DataType> LockFreeStack<DataType> {
    pub fn new() -> Self {
        Self {
            top: AtomicPtr::new(ptr::null_mut()),
        }
    }

    // Push(data: DataType) {
    pub fn push(&self, data: DataType) {
        // node = NewNode()
        // node^.Data = data
        let node = Box::into_raw(Box::new(NodeType {
            data: Some(data),
            next: AtomicPtr::new(ptr::null_mut()),
        }));

        // while true {...}
        loop {
            // t = Top;
            let t = self.top.load(SeqCst);

            // node^.Next = t;
            unsafe { (*node).next.store(t, SeqCst); }

            // if CAS(&Top, t, node) return;
            if self.top.compare_exchange(t, node, SeqCst, SeqCst).is_ok() {
                return;
            }
        }

    }

    // Pop() : DataType {
    pub fn pop(&self, myhprec: *mut HPRecType<NodeType<DataType>>) -> Option<DataType> {
        let mut t: *mut NodeType<DataType>;

        // while true {...}
        loop {
            // t = Top;
            t = self.top.load(SeqCst);

            // if (t=null) return EMPTY;
            if t.is_null() {
                return None;
            }

            // *hp = t;
            // Important:
            // We store the hazard pointer at index 2!
            // This is because the hazard pointer at index 0 and 1 are used for the Lock-Free List
            // Which we use for the outer part of the join_map (the list of Join Entries)
            unsafe { (*myhprec).hp[2].store(t, SeqCst); }

            // if (Top != t) continue;
            if self.top.load(SeqCst) != t {
                continue;
            }

            // next = t^.Next;
            let next = unsafe { (*t).next.load(SeqCst) };

            // if CAS(&Top, t, next) break;
            if self.top.compare_exchange(t, next, SeqCst, SeqCst).is_ok() {
                break;
            }

        }

        // data = t^.Data;
        let data = unsafe { (*t).data.take() }.expect("stack nodes data was already taken");

        // RetireNode(t);
        retire_node(t, myhprec);

        // return data;
        Some(data)
    }

}

