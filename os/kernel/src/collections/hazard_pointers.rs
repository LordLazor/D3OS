/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: hazard_pointers.rs                                              ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ The Hazard Pointers are based on Maged M. Michael's paper               ║
   ║ "Hazard Pointers: Safe Memory Reclamation for Lock-Free Objects"        ║
   ║ https://dl.acm.org/doi/10.1109/TPDS.2004.8                              ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Lazar Konstantinou, 11.09.2025, HHU                                 ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/

use core::{sync::atomic::{AtomicPtr, Ordering::SeqCst}};

use alloc::vec::Vec;

// K: the number of hazard pointers each thread uses
const K: usize = 3;


// Fig. 1. Types and structures
// Hazard pointer record
// structure HPRecType { HP[K]: *NodeType; Next: *HPRecType; }
struct HPRecType<NodeType> {
    hp: [AtomicPtr<NodeType>; K], // HP[K]: array of K hazard pointers
    next: AtomicPtr<HPRecType<NodeType>>, // Next: pointer to the next hazard pointer record
   }

// The header of the HPRec list
// HeadHPRec: *HPRecType;
static HEAD_HPREC: AtomicPtr<HPRecType<()>> = AtomicPtr::new(core::ptr::null_mut());

// Per-thread private variables
// rlist: listType; // initially empty
// rcount: integer; // initially 0
struct ThreadPrivate<NodeType> {
    rlist: Vec<*mut NodeType>, // retired list of nodes to be freed
    rcount: usize, // count of retired nodes
}

// RetireNode(node: *mut NodeType) {
fn retire_node(node: *mut (), thread_private: &mut ThreadPrivate<()>) {
   // rlist.push(node);
   thread_private.rlist.push(node);

   // rcount++;
   thread_private.rcount += 1;

   // if (rcount >= R) {
   const R: usize = 10; // Threshold for reclamation => Needs to satisfy R = H + Omega(H) (only important for runtime)
   if thread_private.rcount >= R {
      // Scan(HeadHPRec);
      scan(HEAD_HPREC.load(SeqCst), thread_private);

   }

}

// Scan(head: *HPRecType) {
pub fn scan(head: *mut HPRecType<()>, thread_private: &mut ThreadPrivate<()>) {
   // Stage 1: Scan HP list and insert non-null values in plist
   // plist.init();
   let mut plist: Vec<*mut ()> = Vec::new();

   // hprec = head;
   let mut hprec = head;

   // while (hprec != null) {
   while !hprec.is_null() {
      // for (i = 0 to K-1) {
      for i in 0..K {
         // hptr = hprec^.HP[i];
         let hptr = unsafe { (*hprec).hp[i].load(SeqCst)};

         // if (hptr != null) {
         if !hptr.is_null() {
            // plist.insert(hptr);
            plist.push(hptr);
         }
      }
      // hprec = hprec^.Next;
      hprec = unsafe { (*hprec).next.load(SeqCst) };
   }

   // Stage 2: Search plist
   // tmplist = rlist.popAll();
   let mut tmplist: Vec<*mut ()> = core::mem::take(&mut thread_private.rlist);

   // rcount = 0;
   thread_private.rcount = 0;

   // node = tmplist.pop();
   let mut node = tmplist.pop().unwrap_or(core::ptr::null_mut());

   // while (node != null) 
   while !node.is_null() {
      // if (plist.lookup(node)) {
      if plist.contains(&node) {
         // rlist.push(node);
         thread_private.rlist.push(node);

         // rcount++;
         thread_private.rcount += 1;
      } else { // else {
         // PrepareForReuse(node);
         prepare_for_reuse(node);
      }
      // node = tmplist.pop();
      node = tmplist.pop().unwrap_or(core::ptr::null_mut());
   }
   // plist.free();
   // Not needed

}

// PrepareForReuse(node: *NodeType) 
// Is basically a placeholder, maybe return to this later if needed
fn prepare_for_reuse(node: *mut ()) {
   todo!();
}