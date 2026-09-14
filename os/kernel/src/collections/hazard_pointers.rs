/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: hazard_pointers.rs                                              ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ The Hazard Pointers are based on Maged M. Michael's paper               ║
   ║ "Hazard Pointers: Safe Memory Reclamation for Lock-Free Objects"        ║
   ║ https://dl.acm.org/doi/10.1109/TPDS.2004.8                              ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Lazar Konstantinou, 11.09.2025, HHU                             ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/

use core::{sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering::SeqCst}};

use alloc::{boxed::Box, vec::Vec};

// R: Threshold for reclamation => Needs to satisfy R(H) = H + Omega(H) (only important for runtime)
fn r_threshold() -> usize {
   2 * H.load(SeqCst) // R(H) = 2H always satisfies R(H) = H + Omega(H)
}

// K: the number of hazard pointers each thread uses
// Note: K is a constant that can be chosen at compile time
// For the Lock-Free List implementation, you only need K=2 hazard pointers per thread
// For the Lock-Free Map implementation, you need K=3 hazard pointers per thread
// I've therefore have chosen K=3 
// In the future you may adapt this to not waste memory for the lock-free list implementation
const K: usize = 3;

// Hazard pointer record
// structure HPRecType { HP[K]: *NodeType; Next: *HPRecType; }
#[repr(C)]
pub(crate) struct HPRecType<NodeType> {
    pub(crate) hp: [AtomicPtr<NodeType>; K], // HP[K]: array of K hazard pointers - pub(crate) so callers can set hp0/hp1 themselves (e.g. in search()'s Find translation)
    next: AtomicPtr<HPRecType<NodeType>>, // Next: pointer to the next hazard pointer record
    active: AtomicBool, // Active: Boolean - true while some thread owns/uses this record
    rlist: Vec<RetiredNode>, // rlist:  retired list of nodes to be freed - see RetiredNode for why this isn't just Vec<*mut NodeType>
    rcount: usize, // rcount: count of retired nodes
   }

pub(crate) struct RetiredNode {
    ptr: *mut (),
    drop_fn: unsafe fn(*mut ()),
}

unsafe fn drop_erased<NodeType>(ptr: *mut ()) {
   unsafe { drop(Box::from_raw(ptr as *mut NodeType)); }
}

// Shared variables
// HeadHPRec: *HPRecType; // initially null
static HEAD_HPREC: AtomicPtr<HPRecType<()>> = AtomicPtr::new(core::ptr::null_mut());
// H: integer; // initially 0
static H: AtomicUsize = AtomicUsize::new(0);

// AllocateHPRec() {
pub(crate) fn allocate_hprec() -> *mut HPRecType<()> {
   // First try to reuse a retired HP record
   // for (hprec = HeadHPRec; hprec != null; hprec = hprec^.Next) {
   let mut hprec = HEAD_HPREC.load(SeqCst);
   while !hprec.is_null() {
      // if (hprec^.Active) continue;
      if unsafe { (*hprec).active.load(SeqCst) } {
         hprec = unsafe { (*hprec).next.load(SeqCst) };
         continue;
      }

      // TAS(addr) = !CAS(addr, false, true)
      // if TAS(&hprec^.Active) continue;
      if unsafe { (*hprec).active.compare_exchange(false, true, SeqCst, SeqCst).is_err() } {
         hprec = unsafe { (*hprec).next.load(SeqCst) };
         continue;
      }

      // Succeeded in locking an inactive HP record
      // myhprec = hprec;
      // return;
      return hprec;
   }

   // No HP records abailable for reuse
   // Increment H, then allocate a new HP and push it
   // do { // wait-free - max. num. of threads is finite
   //    oldcount <- H;
   // } until CAS(&H, oldcount, oldcount+K);
   loop {
      let oldcount = H.load(SeqCst);
      if H.compare_exchange(oldcount, oldcount + K, SeqCst, SeqCst).is_ok() {
         break;
      }
   }

   // Alocate and push a new HP record
   // hprec = NewHPRec();
   // Initialize the fields of the new HP record
   let hprec: *mut HPRecType<()> = Box::into_raw(Box::new(HPRecType {
      hp: core::array::from_fn(|_| AtomicPtr::new(core::ptr::null_mut())),
      next: AtomicPtr::new(core::ptr::null_mut()),
      active: AtomicBool::new(true), // we're claiming it for ourselves right away
      rlist: Vec::new(),
      rcount: 0,
   }));

   // do { // wait-free - max. num. of threads is finite
   //   oldhead <- HeadHPRec;
   //   hprec^.Next = oldhead;
   // } until CAS(&HeadHPRec, oldhead, hprec);
   loop {
      let oldhead = HEAD_HPREC.load(SeqCst);
      unsafe { (*hprec).next.store(oldhead, SeqCst); }
      if HEAD_HPREC.compare_exchange(oldhead, hprec, SeqCst, SeqCst).is_ok() {
         break;
      }
   }

   // myhprec = hprec;
   return hprec;
}

// RetireHPRec() {
pub(crate) fn retire_hprec<NodeType>(myhprec: *mut HPRecType<NodeType>) {
   // for (i = 0 to K-1) myhprec^.HP[i] = null;
   for i in 0..K {
      unsafe { (*myhprec).hp[i].store(core::ptr::null_mut(), SeqCst); }
   }

   // myhprec^.Active = false;
   unsafe { (*myhprec).active.store(false, SeqCst); }
}

// Per-thread private variable (Fig. 4)
// myhprec: *HPRecType; // initially null
pub(crate) fn retire_node<NodeType>(node: *mut NodeType, myhprec: *mut HPRecType<NodeType>) {
   // myhprec^.rlist.push(node);
   unsafe {
      (*myhprec).rlist.push(RetiredNode {
         ptr: node as *mut (),
         drop_fn: drop_erased::<NodeType>,
      });
   }

   // myhprec^.rcount++;
   unsafe { (*myhprec).rcount += 1; }

   // head <- HeadHPRec;
   let head = HEAD_HPREC.load(SeqCst);

   // if (myhprec^.rcount >= R(H)) {
   if unsafe { (*myhprec).rcount } >= r_threshold() {
      // SAFETY: see the #[repr(C)] comment on HPRecType above.
      let myhprec_erased = myhprec as *mut HPRecType<()>;

      // Scan(head);
      scan(head, myhprec_erased);

      // HelpScan();
      help_scan(myhprec_erased);
   }

}

// Scan(head: *HPRecType) {
// Fig. 4: same as Figure 3, except rlist/rcount are now reached through
// *myhprec instead of being a separate ThreadPrivate argument.
pub fn scan(head: *mut HPRecType<()>, myhprec: *mut HPRecType<()>) {
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
   // tmplist = rlist.popAll();  (rlist = myhprec^.rlist)
   let mut tmplist: Vec<RetiredNode> = unsafe { core::mem::take(&mut (*myhprec).rlist) };

   // rcount = 0;  (rcount = myhprec^.rcount)
   unsafe { (*myhprec).rcount = 0; }

   // node = tmplist.pop();
   while let Some(node) = tmplist.pop() {
      // if (plist.lookup(node)) {
      if plist.contains(&node.ptr) {
         // rlist.push(node);
         unsafe { (*myhprec).rlist.push(node); }

         // rcount++;
         unsafe { (*myhprec).rcount += 1; }
      } else { // else {
         // PrepareForReuse(node);
         prepare_for_reuse(node);
      }
   }
   // plist.free();
   // Not needed

}

// PrepareForReuse(node: *NodeType)
fn prepare_for_reuse(node: RetiredNode) {
   unsafe { (node.drop_fn)(node.ptr); }
}


// TAS(addr) means !CAS(addr, false, true)
// HelpScan() {
fn help_scan(myhprec: *mut HPRecType<()>) {
   // for (hprec = HeadHPRec; hprec != null; hprec = hprec^.Next) {
   let mut hprec = HEAD_HPREC.load(SeqCst);
   while !hprec.is_null() {
      // if (hprec^.Active) continue;
      if unsafe { (*hprec).active.load(SeqCst) } {
         hprec = unsafe { (*hprec).next.load(SeqCst) };
         continue;
      }

      // if TAS(&hprec^.Active) continue;
      if unsafe { (*hprec).active.compare_exchange(false, true, SeqCst, SeqCst).is_err() } {
         hprec = unsafe { (*hprec).next.load(SeqCst) };
         continue;
      }

      // while (hprec^.rcount > 0) {
      while unsafe { (*hprec).rcount } > 0 {
         // node = hprec^.rlist.pop();
         let Some(node) = (unsafe { (*hprec).rlist.pop() }) else { break; };

         // hprec^.rcount--;
         unsafe { (*hprec).rcount -= 1; }

         // myhprec^.rlist.push(node);
         unsafe { (*myhprec).rlist.push(node); }

         // myhprec^.rcount++;
         unsafe { (*myhprec).rcount += 1; }

         // head = HeadHPRec;
         let head = HEAD_HPREC.load(SeqCst);

         // if (myhprec^.rcount >= R(H)) {
         if unsafe { (*myhprec).rcount } >= r_threshold() {
            // Scan(head);
            scan(head, myhprec);
         }
      }
      // hprec^.Active = false;
      unsafe { (*hprec).active.store(false, SeqCst); }

      // hprec = hprec^.Next; (for-loop's implicit per-iteration increment)
      hprec = unsafe { (*hprec).next.load(SeqCst) };
   }
}