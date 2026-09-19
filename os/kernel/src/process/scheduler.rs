/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: scheduler                                                       ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Implementation of a basic round-robin scheduler.                        ║
   ║                                                                         ║
   ║ Public functions                                                        ║
   ║   - active_thread_ids      get a list of all active thread IDs          ║
   ║   - current_thread         get the currently running thread             ║
   ║   - current_ids            get the (pid, tid) of the current thread     ║
   ║   - exit                   exit the calling thread                      ║
   ║   - join                   wait for a thread to finish                  ║
   ║   - kill                   kill a thread                                ║
   ║   - set_init               set the scheduler as initialized             ║
   ║   - thread                 get reference to a thread                    ║
   ║   - ready                  insert a thread in the ready queue           ║
   ║   - sleep                  put the caller into sleeping mode            ║
   ║   - start                  start the scheduler                          ║
   ║   - switch_thread_from_interrupt  switch thread, called from interrupt  ║
   ║   - switch_thread_no_interrupt    switch thread, not called from int.   ║
   ║   - current_ids            get the (pid, tid) of the current thread     ║
   ║   - block                  put the calling thread into blocked mode     ║
   ║   - deblock                wake up a blocked thread                     ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Fabian Ruhland, 05.09.2025, HHU                                 ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/
use crate::process::thread::{Thread, ThreadState};
use crate::{allocator, apic, per_cpu_ref, timer, tss};
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use syscall::return_vals::Errno;
use uuid::Uuid;
use core::{panic, ptr};
use core::arch::asm;
use core::cell::Cell;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use log::{debug, info};
use smallmap::Map;
use spin::{Mutex, MutexGuard, Once};
use thingbuf::mpsc::{Sender};
use crate::device::apic::get_apic_id;
use crate::device::cpu::{disable_int_nested, enable_int_nested};
use crate::process::core_local_storage::{cls, current_core_id, scheduler, tss_static};
use lockfree::lock_free_list_with_hp::{LockFreeList, Node};

#[derive(Clone)]
struct SleepEntry {
    wakeup_time: usize,
    thread_id: usize,
    thread: Arc<Thread>,
}

impl PartialEq for SleepEntry {
    fn eq(&self, other: &Self) -> bool {
        self.wakeup_time == other.wakeup_time && self.thread_id == other.thread_id
    }
}

impl PartialOrd for SleepEntry {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        (self.wakeup_time, self.thread_id).partial_cmp(&(other.wakeup_time, other.thread_id))
    }
}

#[derive(Clone)]
struct BlockedEntry {
    pid: Uuid,
    thread_id: usize,
    thread: Arc<Thread>,
}

impl PartialEq for BlockedEntry {
    fn eq(&self, other: &Self) -> bool {
        self.thread_id == other.thread_id
    }
}

impl PartialOrd for BlockedEntry {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        self.thread_id.partial_cmp(&other.thread_id)
    }
}

#[derive(Clone)]
struct Joiner {
    thread_id: usize,
    thread: Arc<Thread>,
}

impl PartialEq for Joiner {
    fn eq(&self, other: &Self) -> bool {
        self.thread_id == other.thread_id
    }
}

impl PartialOrd for Joiner {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        self.thread_id.partial_cmp(&other.thread_id)
    }
}

#[derive(Clone)]
struct JoinEntry {
    thread_id: usize,
    joiners: Arc<LockFreeList<Joiner, 2>>,
}

impl JoinEntry {
    fn new(thread_id: usize) -> Self {
        Self { thread_id, joiners: Arc::new(LockFreeList::new()) }
    }
}

impl PartialEq for JoinEntry {
    fn eq(&self, other: &Self) -> bool {
        self.thread_id == other.thread_id
    }
}

impl PartialOrd for JoinEntry {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        self.thread_id.partial_cmp(&other.thread_id)
    }
}

// thread IDs
pub static THREAD_ID_COUNTER: AtomicUsize = AtomicUsize::new(1);
static ACTIVE_CPUS: AtomicU32 = AtomicU32::new(1);  //BP automatically

/// Global set of "alive" thread IDs (across all cores).
/// Presence means: joining on this tid should block (unless it exits concurrently).
static ACTIVE_TIDS: Once<Mutex<Map<usize, ()>>> = Once::new();

#[inline]
pub fn active_tids() -> &'static Mutex<Map<usize, ()>> {
    ACTIVE_TIDS.call_once(|| Mutex::new(Map::new()))
}

#[inline]
fn mark_thread_alive(tid: usize) {
    let mut set = active_tids().lock();
    set.insert(tid, ());
}

#[inline]
fn mark_thread_dead(tid: usize) {
    let mut set = active_tids().lock();
    set.remove(&tid);
}

#[inline]
pub fn is_thread_alive(tid: usize) -> bool {
    active_tids().lock().contains_key(&tid)
}

#[inline]
pub fn next_thread_id() -> usize {
    THREAD_ID_COUNTER.fetch_add(1, Relaxed)
}

#[inline]
pub fn cpu_mark_online() {
    ACTIVE_CPUS.fetch_add(1, Relaxed);
}

#[inline]
pub fn cpu_count() -> u32 {
    ACTIVE_CPUS.load(Relaxed)
}

/// Main struct of the scheduler
pub struct Scheduler {
    current_thread: Cell<Option<Arc<Thread>>>,
    sleep_list: LockFreeList<SleepEntry>,
    blocked_list: LockFreeList<BlockedEntry>,
    join_map: LockFreeList<JoinEntry>, // manage which threads are waiting for a thread-id to terminate
    has_started: bool,

    // Fields from ReadyState migrated to Scheduler struct
    ready_queue: Mutex<VecDeque<Arc<Thread>>>,
    last_fpu_thread: Cell<Option<Arc<Thread>>>,
    initialized: AtomicBool,
    idle_thread: Arc<Thread>
}

unsafe impl Send for Scheduler {}
unsafe impl Sync for Scheduler {}

/// Called from assembly code, after the thread has been switched
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unlock_scheduler() {
    unsafe { scheduler().ready_queue.force_unlock(); }
}

impl Scheduler {

    /// Create and initialize the scheduler.
    pub fn new() -> Self {
        info!("Initializing scheduler for CPU {}", current_core_id());

        let ready_queue = Mutex::new(VecDeque::new());
        let sleep_list = LockFreeList::new();
        let blocked_list = LockFreeList::new();
        let join_map = LockFreeList::new();
        let has_started = false;


        // Fields from ReadyState migrated to Scheduler struct
        let initialized = AtomicBool::new(false); // Goes only from false to true, so no need for a Mutex
        let idle_thread = Thread::new_kernel_thread(idle_thread, "idle"); // No need for Mutex as this is only called once during initialization and only cloned during runtime (no state changes) 

        Self {
            current_thread: Cell::default(),
            sleep_list: sleep_list,
            blocked_list: blocked_list,
            join_map: join_map,
            has_started: has_started,
            ready_queue: ready_queue,
            last_fpu_thread: Cell::default(),
            initialized: initialized,
            idle_thread: idle_thread,
        }
    }

    /// Called after the scheduler has been fully initialized
    pub fn set_init(&self) {
        self.initialized.store(true, Release);
    }

    /// returns the number of threads that are currently actively running on this CPU
    /// does not count sleeping threads
    pub fn active_thread_count(&self) -> usize {
        let mut sum: u32 = 0;
        let active = ACTIVE_CPUS.load(Relaxed);
        for i in 0..active {
            let rq = per_cpu_ref(i as usize).rq_len.load(Ordering::Acquire);
            // Detect runaway counters without panicking the whole kernel in debug
            match sum.checked_add(rq) {
                Some(s) => sum = s,
                None => {
                    log::error!("active_thread_count overflow while adding cpu {} rq_len={}", i, rq);
                    return usize::MAX; // saturate to a sentinel
                }
            }
        }

        // Clamp to usize on 32-bit safely (and give a clear sentinel on 32-bit if it ever overflows)
        if sum > usize::MAX as u32 {
            log::error!("active_thread_count exceeds usize: {}", sum);
            usize::MAX
        } else {
            sum as usize
        }
    }


    /// Get all active thread IDs
    pub fn active_thread_ids(&self) -> Vec<usize> {
        Vec::from_iter(active_tids().lock().iter().cloned().map(|t | t.0))
    }

    /// Return the current running thread
    pub fn current_thread(&self) -> Arc<Thread> {
        self.try_current_thread()
            .expect("Trying to access current thread before initialization!")
    }

    /// Return reference to current thread, if possible.
    pub fn try_current_thread(&self) -> Option<Arc<Thread>> {
        self.current_thread
            .get_cloned()
            .take()
    }

    /// Try to return reference to current thread (called from interrupt dispatcher)
    pub fn try_get_current_thread(&self) -> Option<Arc<Thread>> {
        if allocator().is_locked() {
            return None;
        }
        self.try_current_thread()
    }

    /// Return reference to thread identified by `thread_id`
    pub fn thread(&self, thread_id: usize) -> Option<Arc<Thread>> {
        self.ready_queue.lock()
            .iter()
            .find(|thread| thread.id() == thread_id)
            .cloned()
    }

    /// Return (pid, tid) of current thread
    pub fn current_ids(&self) -> (Uuid, usize) {
        let tid = self.current_thread().id();
        let pid = self.current_thread().process().id();
        (pid, tid)
    }


    /// Start the scheduler, called only once from `boot.rs`
    pub fn start(&mut self) {
        if self.has_started {
            return;
        }
        self.has_started = true;
        let mut ready_queue = self.get_ready_queue();
        let next_thread = ready_queue.pop_back()
            .unwrap_or_else(|| self.idle_thread.clone());
        let old = self.current_thread.replace(Some(next_thread.clone()));
        assert!(old.is_none());

        unsafe { Thread::start_first(next_thread.as_ref()); }
    }

    /// Insert `thread` into the ready queue of the scheduler
    pub fn ready(&self, thread: Arc<Thread>) {
        let id = thread.id();
        mark_thread_alive(id);

        if let Some(current) = self.try_current_thread() {
            let myhprec = current.hp_record::<Node<JoinEntry>>();
            self.join_map.insert(JoinEntry::new(id), myhprec);
        }

        let mut ready_queue = self.get_ready_queue();
        inc_rq_len();
        ready_queue.push_front(thread);
    }

    /// Put calling thread to sleep for `ms` milliseconds
    pub fn sleep(&self, ms: usize) {
        let ready_queue = self.get_ready_queue();

        if !self.initialized.load(Acquire) {
            // Scheduler is not initialized yet, so this function has been called during the boot process
            // So we do active waiting
            timer().wait(ms);
        }
        else {
            // Scheduler is initialized, so we can block the calling thread
            let thread = self.current_thread();
            thread.set_state(ThreadState::Sleeping);

            let wakeup_time = timer().systime_ms() + ms;
            let thread_id = thread.id();
            let myhprec = thread.hp_record::<Node<SleepEntry>>();
            self.sleep_list.insert(SleepEntry { wakeup_time, thread_id, thread }, myhprec);

            dec_rq_len();
            self.block_and_switch(ready_queue);
        }
    }

    /// Put calling thread to block
    pub fn block(&self) {
        let ready_queue = self.get_ready_queue();

        if !self.initialized.load(Acquire) {
            // Scheduler is not initialized yet, so this function has been called during the boot process
            // We panic
            panic!("Scheduler: Cannot block thread before scheduler is initialized!");
        }
        else {
            // Scheduler is initialized, so we can block the calling thread
            let thread = self.current_thread();
            thread.set_state(ThreadState::Blocked);
            let pid = thread.process().id();
            let thread_id = thread.id();
            let myhprec = thread.hp_record::<Node<BlockedEntry>>();
            self.blocked_list.insert(BlockedEntry { pid, thread_id, thread }, myhprec);
            //info!("Scheduler::block: switch to next thread");
            dec_rq_len();
            self.block_and_switch(ready_queue);
        }
    }

    /// Requeue thread with `tid` from process with `pid` to the ready queue of the scheduler
    pub fn deblock(&self, pid: Uuid, tid: usize) {
        let mut ready_queue = self.ready_queue.lock();

        let myhprec = self.current_thread().hp_record::<Node<BlockedEntry>>();
        if let Some(entry) = self.blocked_list.find_and_remove(|e| e.thread_id == tid && e.pid == pid, myhprec) {
            entry.thread.set_state(ThreadState::Ready);
            ready_queue.push_front(entry.thread);
            inc_rq_len();
        } else {
            schedule_on_all_others(MessageItem::Cmd(MessageCmd::Deblock {pid, tid}))
        }
    }

    /// Helper function for switching a thread not caused by an interrupt
    pub fn switch_thread_no_interrupt(&self) {
        self.switch_thread(false);
    }

    /// Helper function for switching a thread caused by an interrupt
    pub fn switch_thread_from_interrupt(&self) {
        self.switch_thread(true);
    }

    /// Calling thread will block until thread with `thread_id` has terminated
    pub fn join(&self, thread_id: usize) -> Result<usize, Errno> {
        // Fast path => if it's already dead, don't block.
        if !is_thread_alive(thread_id) {
            return Err(Errno::ESRCH);
        }

        let ready_queue = self.get_ready_queue();
        let thread = self.current_thread();
        thread.set_state(ThreadState::Blocked);

        let myhprec = thread.hp_record::<Node<JoinEntry>>();

        self.join_map.find_or_insert_with(JoinEntry::new(thread_id), myhprec, |entry| {
            let joiner_hprec = thread.hp_record::<Node<Joiner>>();
            entry.joiners.insert(Joiner { thread_id: thread.id(), thread: Arc::clone(&thread) }, joiner_hprec);
        });

        dec_rq_len();
        self.block_and_switch(ready_queue);
        Ok(0)
    }

    fn unjoin(&self, thread_id: usize, ready_queue: &mut VecDeque<Arc<Thread>>) {
        let myhprec = self.current_thread().hp_record::<Node<JoinEntry>>();

        if let Some(entry) = self.join_map.find_and_remove(|e| e.thread_id == thread_id, myhprec) {
            let joiner_hprec = self.current_thread().hp_record::<Node<Joiner>>();

            while let Some(joiner) = entry.joiners.pop_front_if(
                Joiner { thread_id: 0, thread: self.idle_thread.clone() },
                |_| true,
                joiner_hprec,
            ) {
                joiner.thread.set_state(ThreadState::Running);
                ready_queue.push_front(joiner.thread);
                inc_rq_len();
            }
        }
        schedule_on_all_others(MessageItem::Cmd(MessageCmd::JoinTargetExited {tid: thread_id}));
    }

    /// Exit calling thread.
    pub fn exit(&self) -> ! {
        let mut ready_queue = self.get_ready_queue();
        let current = self.current_thread();
        current.set_state(ThreadState::Exited);

        // Mark dead globally *before* waking joiners, so joiners racing in will observe "dead"
        mark_thread_dead(current.id());
        self.unjoin(current.id(), &mut ready_queue);

        dec_rq_len();
        drop(current); // Decrease Rc manually, because block() does not return
        self.block_and_switch(ready_queue);
        unreachable!()
    }

    /// Kill the thread with the id `thread_id`, if it is on the same Core
    pub fn kill(&self, thread_id: usize) {
        let current = self.current_thread();

        // Check if current_thread tries to kill itself (illegal)
        if current.id() == thread_id {
            panic!("A thread cannot kill itself!");
        }

        if self.kill_locally(thread_id, &mut self.get_ready_queue()) == false {
            schedule_on_all_others(MessageItem::Cmd(MessageCmd::Kill {tid: thread_id}))
        }
    }

    /// Kill the thread with the id `thread_id`, if it is on the same Core
    /// goes through ready_queue, sleep_list, blocked_list, and join_map in this order
    /// returns true if a thread with the given id was found
    fn kill_locally(&self, thread_id: usize, ready_queue: &mut VecDeque<Arc<Thread>>) -> bool {
        if is_thread_alive(thread_id) == false { return true; }
        let mut changed = false;

        // check ready_queue
        let before = ready_queue.len();
        ready_queue.retain(|thread| thread.id() != thread_id);
        let after = ready_queue.len();
        if before != after {
            changed = true;
            dec_rq_len();
        }
        if !changed {
            let myhprec = self.current_thread().hp_record::<Node<SleepEntry>>();
            if self.sleep_list.find_and_remove(|entry| entry.thread_id == thread_id, myhprec).is_some() {
                changed = true;
            }
            if!changed {
                {   // check Block List
                    let myhprec = self.current_thread().hp_record::<Node<BlockedEntry>>();
                    if self.blocked_list.find_and_remove(|entry| entry.thread_id == thread_id, myhprec).is_some() {
                        changed = true;
                    }
                }
                if !changed {
                    // check all join_map's
                    let myhprec = self.current_thread().hp_record::<Node<JoinEntry>>();
                    self.join_map.for_each(|entry| {
                        let joiner_hprec = self.current_thread().hp_record::<Node<Joiner>>();
                        if entry.joiners.find_and_remove(|j| j.thread_id == thread_id, joiner_hprec).is_some() {
                            changed = true;
                        }
                    }, myhprec);
                }

            }
        }
        if changed {
            mark_thread_dead(thread_id);
            self.unjoin(thread_id, ready_queue);
        }
        changed
    }

    /// Gives out current thread id, then calls other debug methods
    pub fn debug_scheduler(&self) {
        let ready_queue = self.get_ready_queue();

        let nested = disable_int_nested();
        let id = current_core_id();
        let nbr_threads = self.active_thread_count() as u32;
        let own_threads = read_rq_len() as u32;
        let nbr_cpus = ACTIVE_CPUS.load(Relaxed);
        info!("Scheduler {}: Current thread: {}", id, self.current_thread().id());
        info!("Scheduler{}: total_threads: {}, own_threads: {}, cpus: {}",
                id, nbr_threads, own_threads, nbr_cpus);
        info!("Scheduler {}: Ready queue:", id);
        for thread in ready_queue.iter() {
            info!("  - {}", thread.id());
        }
        info!("Scheduler {}: Sleep list: (not iterable - LockFreeList has no iteration/dump support)", id);
        for i in 0..nbr_cpus {
            info!("Cpu {} has {} active threads running", i, read_rq_len_remote(i as usize));
        }
        enable_int_nested(nested);
    }

    /// Debugging function to print all threads in the ready queue.
    pub fn debug_ready_queue(&self) {
        let ready_queue = self.get_ready_queue();
        let id = current_core_id();
        info!("Scheduler {}: Ready queue:", id);
        for thread in ready_queue.iter() {
            info!("  - {}", thread.id());
        }
    }

    /// Debugging function to print all threads in the sleep list.
    pub fn debug_sleep_list(&self) {
        let id = current_core_id();
        info!("Scheduler {}: Sleep list: (not iterable - LockFreeList has no iteration/dump support)", id);
    }

    /// Block calling thread and switch to next ready thread.
    fn block_and_switch(&self, mut ready_queue: MutexGuard<VecDeque<Arc<Thread>>>) {
        let mut next_thread = ready_queue.pop_back();

        if next_thread.is_none() {
            self.check_sleep_list(&mut ready_queue);
            drain_inbox_into_ready(10, &mut ready_queue);
            next_thread = ready_queue.pop_back();
            if next_thread.is_none() {  //still no new thread => switch to idle
                next_thread = Some(Arc::clone(&self.idle_thread));
            }
        }

        let current = self.current_thread.take()
            .expect("failed to get current thread");
        let next = next_thread.unwrap();

        // Thread has enqueued itself into sleep list and waited so little,
        // that it dequeued itself in the meantime
        if current.id() == next.id() {
            // put it back in
            self.current_thread.set(Some(current));
            return;
        }

        let current_ptr = ptr::from_ref(current.as_ref());
        let next_ptr = ptr::from_ref(next.as_ref());

        self.current_thread.set(Some(next));
        drop(current); // Decrease Rc manually, because Thread::switch does not return

        unsafe {
            Thread::switch(current_ptr, next_ptr);
        }
    }

    /// Prepare to block the calling thread
    /// Used from wait_queue to prepare the thread for blocking and get its (pid, tid) for later `notify_one` and `notify_all` calls
    /// Returns (pid, tid)
    pub fn park_current(&self) -> (Uuid, usize) {
        let thread = self.current_thread();
        thread.set_state(ThreadState::Parking);
        (thread.process().id(), thread.id())
    }

    /// Block the calling thread, but only if it should still wait.
    pub fn block_if_parking<F>(&self, mut should_wait: F)
    where
        F: FnMut() -> bool,
    {
        let ready_queue = self.get_ready_queue();

        if !self.initialized.load(Acquire) {
            return;
        }

        let thread = self.current_thread();
        if thread.state() != ThreadState::Parking {
            // A wakeup raced in between registering and blocking; do not block.
            return;
        }

        if !should_wait() {
            // The condition was satisfied while we were registering; a racing
            // `unblock` may not have found us, so cancel the block ourselves.
            thread.set_state(ThreadState::Running);
            return;
        }

        thread.set_state(ThreadState::Blocked);

        let pid = thread.process().id();
        let thread_id = thread.id();
        let myhprec = thread.hp_record::<Node<BlockedEntry>>();
        self.blocked_list.insert(BlockedEntry { pid, thread_id, thread: Arc::clone(&thread) }, myhprec);

        dec_rq_len();
        self.block_and_switch(ready_queue);
    }

    /// Unblock thread with given (pid, tid). \
    /// Returns true if thread was found and unblocked, false otherwise.
    pub fn unblock(&self, pid: Uuid, tid: usize) -> bool {
       // info!("Unblock: Thread with PID={}, TID={}", pid, tid);

        // Synchronize against `thread_switch`
        let mut ready_queue = self.ready_queue.lock();

        // 1) Check if the given thread is in the blocked list -> need to be woken up
        let myhprec = self.current_thread().hp_record::<Node<BlockedEntry>>();
        let blocked_thread: Option<Arc<Thread>> = self.blocked_list
            .find_and_remove(|e| e.thread_id == tid && e.pid == pid, myhprec)
            .map(|entry| entry.thread);

        // If we found a blocked thread in the block_list, wake it up
        if let Some(thread) = blocked_thread {
            // let mut state = self.get_ready_state();
            thread.set_state(ThreadState::Ready);
            ready_queue.push_front(Arc::clone(&thread));
            inc_rq_len();
            return true;
        }

        // 2a) Check if the thread to be woken up is the current thread (it has not been blocked)
        {
            let curr_thread = self.current_thread();
            if curr_thread.id() == tid && curr_thread.process().id() == pid {
                curr_thread.set_state(ThreadState::Running);
                return true;
            }

        // 2b) Check if the thread to be woken up is in the ready queue
            if ready_queue.iter().any(|t| t.id() == tid && t.process().id() == pid) {
                // Already runnable (e.g. a previous wakeup raced in); nothing to do
                return true;
            }
        }

        // 3) Not found on this core. The thread may be blocked on the
        // `blocked_list` of another core (the scheduler is core-local), so ask
        // every other core to deblock it. The owning core's `Deblock` handler
        // does the matching `inc_rq_len()`. Only broadcast for live threads to
        // avoid waking stale (exited) waiters.
        if is_thread_alive(tid) {
            drop(ready_queue); // release ready_queue before cross-core scheduling
            schedule_on_all_others(MessageItem::Cmd(MessageCmd::Deblock { pid, tid }));
            return true;
        }

        false
    }

    /// Switch from current to next thread (from ready queue). \
    /// If `interrupt` is true, the function is called from an ISR and will send EOI to APIC otherwise not.
    fn switch_thread(&self, interrupt: bool) {
        if let Some(mut ready_queue) = self.ready_queue.try_lock() {
            if !self.initialized.load(Acquire) {
                if interrupt { apic().end_of_interrupt(); }
                return;
            }

            self.check_sleep_list(&mut ready_queue);
            drain_inbox_into_ready(10, &mut ready_queue);

            // Check if this core has too many threads running
            if read_resched_flag() || self.should_balance_now() {
                self.balance_once(&mut ready_queue);
            }

            // Get clone of the current thread
            let current = self.current_thread();
            let current_was_idle = current.id() == self.idle_thread.id();

            // Current thread is initializing itself and may not be interrupted
            if current.stacks_locked() || tss_static().is_locked() {
                if interrupt {
                    apic().end_of_interrupt();
                }
                return;
            }

            // Try to get the next thread from the ready queue
            let next = match ready_queue.pop_back() {
                Some(thread) => thread,
                None => {
                    if interrupt {
                        apic().end_of_interrupt();
                    }
                    //no new thread & idle thread already active => nothing to do
                    if current_was_idle {
                        return;
                    }
                    //no new thread & last!=idle => switch to idle
                    Arc::clone(&self.idle_thread)
                },
            };

            let current_ptr = ptr::from_ref(current.as_ref());
            let next_ptr = ptr::from_ref(next.as_ref());

            self.current_thread.set(Some(next));

            // last!=idle => we need to enqueue it back in the readyQueue
            if current_was_idle == false {
                ready_queue.push_front(current);
            }

            if interrupt {
                apic().end_of_interrupt();
            }

            unsafe {
                Thread::switch(current_ptr, next_ptr);
            }
        } else {
            if interrupt {
                apic().end_of_interrupt();
            }
        }
    }

    pub fn switch_fpu_context(&self) {
        let current = self.current_thread();

        unsafe { asm!("clts"); }

        if let Some(last) = self.last_fpu_thread.get_cloned().take() {
            last.store_fpu_context();

            if current.id() != last.id() {
                last.store_fpu_context();
            }

            current.restore_fpu_context();
        }

        self.last_fpu_thread.set(Some(current));
    }

    /// Checks whether the current core should balance its threads.
    /// returns own_threads > (nbr_threads /nbr_cpus +1)
    fn should_balance_now(&self) -> bool {
        let nbr_threads = self.active_thread_count() as u32;
        let own_threads = read_rq_len() as u32;
        let nbr_cpus = ACTIVE_CPUS.load(Relaxed);
        if own_threads > (nbr_threads /nbr_cpus +1){
            /*debug!("Scheduler{}: total_threads: {}, own_threads: {}, cpus: {}",
                current_core_id(), nbr_threads, own_threads, nbr_cpus);*/
            return true }
        false
    }

    /// Balances the threads on the current core by moving one thread from the tail to the target core.
    /// Target core is the core with the least number of threads.
    /// Returns the new state of the scheduler. (needed for mutable access)
    fn balance_once(&self, ready_queue: &mut VecDeque<Arc<Thread>>) {
        let own_load = read_rq_len() as usize;
        if own_load <= 1 {
            //debug!("Scheduler: Cannot balance, current load ({:?}) is too low!", own_load);
            return;
        }

        if let Some((target_core, target_load)) = self.find_less_loaded_core() {
            if own_load >= target_load + 2 {
                let amount = ((own_load-target_load)/4)+1;
                for _ in 0..amount {
                    // Move one thread from the tail to the target
                    let thread_opt = ready_queue.pop_front();
                    if let Some(thread) = thread_opt {
                        // If we can't migrate this thread, skip it.
                        // This makes us migrate one less thread than we wanted,
                        // but this should be okay in general.
                        if !thread.can_migrate() {
                            info!("cannot migrate {thread:?}, skipping");
                            ready_queue.push_back(thread);
                            continue;
                        }

                        // Check if the migrating thread is the last thread on this core that has used the FPU.
                        // If so, we need to reset `last_fpu_thread` to None.
                        // We do not need to store the FPU context of the migrating thread,
                        // as we always take a thread from the ready queue and never the current thread.
                        if let Some(last) = self.last_fpu_thread.get_cloned().take() {
                            if last.id() == thread.id() {
                                self.last_fpu_thread.set(None);
                            }
                        }
                        let _tid = thread.id();
                        let w = MessageItem::new_thread(thread);
                        dec_rq_len();
                        if let Ok(_r) = schedule_on(target_core, w) {
                            debug!(" Scheduler{}: Scheduled thread {} on core {}", current_core_id(), _tid, target_core);
                        }
                    }
                }
            }
        }
    }

    /// Finds the core with the least number of threads.
    /// returns (target_core, target_load)
    fn find_less_loaded_core(&self) -> Option<(usize, usize)> {
        // Inspect per-core exported metrics
        let mut curr: usize = 0;
        let mut min = read_rq_len_remote(0);
        for i in 1..ACTIVE_CPUS.load(Relaxed) as usize {
            if min > read_rq_len_remote(i) {
                min = read_rq_len_remote(i);
                curr = i;
            }
        }
        let own = read_rq_len();
        if min >= own { return None; }
        Some((curr, min as usize))
    }

    /// Finds the core with the most number of threads.
    /// returns the target's Core Id
    fn find_more_loaded_core(&self) -> Option<usize> {
        // Inspect per-core exported metrics
        let mut curr: usize = 0;
        let mut max = read_rq_len_remote(0);
        for i in 1..ACTIVE_CPUS.load(Relaxed) as usize {
            if max < read_rq_len_remote(i) {
                max = read_rq_len_remote(i);
                curr = i;
            };
        }
        let own = read_rq_len();
        if max <= own || max < 2 { return None; }
        Some(curr)
    }

    /// Forces a core with more than 2 threads to migrate one through a Reschedule IPI.
    /// (Sends a reschedule IPI that will result in a migration within switch_thread())
    pub fn look_for_overloaded_core(&self) {
        let overloaded_core = self.find_more_loaded_core();
        match overloaded_core {
            None => return,
            Some(target_id) => send_reschedule_ipi(target_id)
        }
    }

    fn check_sleep_list(&self, ready_queue: &mut VecDeque<Arc<Thread>>) {
        let time = timer().systime_ms();
        let myhprec = self.current_thread().hp_record::<Node<SleepEntry>>();

        while let Some(entry) = self.sleep_list.pop_front_if(
            SleepEntry { wakeup_time: 0, thread_id: 0, thread: self.idle_thread.clone() },
            |entry| entry.wakeup_time <= time,
            myhprec,
        ) {
            ready_queue.push_front(entry.thread);
            inc_rq_len();
        }
    }

    /// Helper function returning `ReadyState` of scheduler in a MutexGuard
    fn get_ready_queue(&self) -> MutexGuard<'_, VecDeque<Arc<Thread>>> {
        let rq;

        // We need to make sure, that both the kernel memory manager and the ready queue are currently not locked.
        // Otherwise, a deadlock may occur: Since we are holding the ready queue lock,
        // the scheduler won't switch threads anymore, and none of the locks will ever be released
        loop {
            let rq_tmp = self.ready_queue.lock();
            if allocator().is_locked() {    //allocator can be locked again, but only on other cores -> no deadlock, but bottleneck
                continue;
            }

            rq = rq_tmp;
            break;
        }

        rq
    }

    /// For ps command - get all processes & threads
    pub fn get_status(&self, buffer: &mut [u8]) -> Result<usize, Errno> {
        let mut out = String::new();

        // Current
        let cur = self.current_thread();
        let _ = writeln!(out, "PID: {}, TID: {}, State: {:?}, Name: {}", cur.process().id(), cur.id(), ThreadState::Running, cur.process().name());

        // Ready Queue
        let ready_queue = self.get_ready_queue();
        for thread in ready_queue.iter() {
            let _ = writeln!(out, "PID: {}, TID: {}, State: {:?}, Name: {}", thread.process().id(), thread.id(), thread.state(), thread.process().name());
        }

        // Sleep List
        let myhprec = self.current_thread().hp_record::<Node<SleepEntry>>();
        self.sleep_list.for_each(|entry| {
            let sleep_entry_thread_id = entry.thread_id;
            let sleep_entry_pid = entry.thread.process().id();
            let _ = writeln!(out, "PID: {}, TID: {}, State: {:?}, Name: {}", sleep_entry_pid, sleep_entry_thread_id, entry.thread.state(), entry.thread.process().name());

        }, myhprec);
        
        // Block list
        let myhprec = self.current_thread().hp_record::<Node<BlockedEntry>>();
        self.blocked_list.for_each(|entry| {
            let blocked_entry_thread_id = entry.thread_id;
            let blocked_entry_pid = entry.thread.process().id();
            let _ = writeln!(out, "PID: {}, TID: {}, State: {:?}, Name: {}", blocked_entry_pid, blocked_entry_thread_id, entry.thread.state(), entry.thread.process().name());
        }, myhprec);

        // Copy to caller buffer (truncate if needed)
        let bytes = out.as_bytes();
        let len = core::cmp::min(bytes.len(), buffer.len());
        buffer[..len].copy_from_slice(&bytes[..len]);
        Ok(len)
    }

    /// Handle a command received via inbox, using the already-held ready_state lock (`state`).
    fn handle_inbox_cmd(&self, cmd: MessageCmd, ready_queue: &mut VecDeque<Arc<Thread>>) {
        match cmd {
            // Wake local join-waiters and add to readyQueue
            MessageCmd::JoinTargetExited { tid } => {
                let myhprec = self.current_thread().hp_record::<Node<JoinEntry>>();

                if let Some(entry) = self.join_map.find_and_remove(|e| e.thread_id == tid, myhprec) {
                    let joiner_hprec = self.current_thread().hp_record::<Node<Joiner>>();
                    while let Some(waiter) = entry.joiners.pop_front_if(
                        Joiner { thread_id: 0, thread: self.idle_thread.clone() },
                        |_| true,
                        joiner_hprec,
                    ) {
                        waiter.thread.set_state(ThreadState::Running);
                        ready_queue.push_front(waiter.thread);
                        inc_rq_len();
                    }
                }
            }
            // If the thread is locally blocked, requeue it.
            MessageCmd::Deblock { pid, tid } => {
                if is_thread_alive(tid) == false { return; }

                let myhprec = self.current_thread().hp_record::<Node<BlockedEntry>>();
                if let Some(entry) = self.blocked_list.find_and_remove(|e| e.thread_id == tid && e.pid == pid, myhprec) {
                    entry.thread.set_state(ThreadState::Running);
                    ready_queue.push_front(entry.thread);
                    inc_rq_len();
                }
            }
            // if you have this thread, kill it
            MessageCmd::Kill { tid } => {
                if is_thread_alive(tid) == false { return; }
                let thread = self.current_thread();
                if thread.id() == tid { //cant kill itself, reschedule for other thread
                    let _ = schedule_on(current_core_id() as usize, MessageItem::Cmd(MessageCmd::Kill { tid }));
                    let _ = schedule_on_all_others(MessageItem::Cmd(MessageCmd::Kill { tid }));    //if target migrates until then
                    return;
                }
                self.kill_locally(tid, ready_queue);
            }
        }
    }

    
    /// Voluntarily yield the CPU to another runnable thread.
    ///
    /// Requirements / assumptions:
    /// - Must be called when it is safe to switch (no stack locks, tss not locked).
    /// - Does not change Parking/Blocked semantics; caller should set state beforehand if needed.
    pub fn yield_now(&self) {
        let mut ready_queue = self.get_ready_queue();

        if !self.initialized.load(Acquire) {
            return;
        }

        // Current thread
        let current = self.current_thread();

        // Same restrictions as your timer-driven switching path
        if current.stacks_locked() || tss().is_locked() {
            return;
        }

        // If there is nobody else runnable, don't bother.
        // (Note: ready_queue does NOT include the current thread yet.)
        if ready_queue.is_empty() {
            return;
        }

        // Requeue current as Ready
        current.set_state(ThreadState::Ready);
        ready_queue.push_front(Arc::clone(&current));

        // Pick next
        let next = match ready_queue.pop_back() {
            Some(t) => t,
            None => {
                // Shouldn't happen because we checked !empty, but be safe
                current.set_state(ThreadState::Running);
                return;
            }
        };

        next.set_state(ThreadState::Running);
        // If we ended up picking ourselves (possible if only ourselves was in queue),
        // return.
        if next.id() == current.id() {
            return;
        }

        // Switch to next
        let current_ptr = core::ptr::from_ref(current.as_ref());
        let next_ptr = core::ptr::from_ref(next.as_ref());
        self.current_thread.set(Some(next));


        // ready_state is unlocked in your asm trampoline via unlock_scheduler()
        // (Thread::switch ultimately calls unlock_scheduler after switch)
        drop(current);

        unsafe {
            Thread::switch(current_ptr, next_ptr);
        }
    }
}


//// Multicore support  ////

/// Helper struct to store shared public information of a core
///     rq_len: approximate runqueue length (owner updates, others read)
///     resched_flag: indicates whether another core requested a reschedule
///     tx: sender for other cores to send messages to this one
///     apic_id: stored at initialization, used to translate cpu_id to apic_id for IPI's
#[repr(align(64))]
pub struct PerCpuRef {
    rq_len: AtomicU32,
    resched_flag: AtomicBool,
    tx: Sender<Option<MessageItem>>,    // producers (remote cores)
    apic_id: AtomicU32,
}
unsafe impl Sync for PerCpuRef {}
impl PerCpuRef { pub fn new(tx: Sender<Option<MessageItem>>) -> Self {
    Self { rq_len: AtomicU32::new(0), resched_flag: AtomicBool::new(false),
        tx, apic_id: AtomicU32::new(0) } } }


/// Wrapper enum to store either a runnable thread or a small cross-core command.
/// (Should be kept "small" since it lives in per-core inboxes and is drained in scheduler paths)
#[derive(Clone)]
pub enum MessageItem {
    Thread(Arc<Thread>),
    Cmd(MessageCmd),
}

impl MessageItem {
    #[inline]
    pub fn new_thread(thread: Arc<Thread>) -> Self {
        MessageItem::Thread(thread)
    }
    #[inline]
    pub fn new_cmd(cmd: MessageCmd) -> Self {
        MessageItem::Cmd(cmd)
    }
}


/// Commands that a core can request another core to perform locally
#[derive(Clone)]
pub enum MessageCmd {
    /// "Thread with `tid` exited somewhere; if you have join-waiters for it locally, wake them."
    JoinTargetExited { tid: usize },

    /// "If you have this thread in your local blocked list, wake it."
    Deblock { pid: Uuid, tid: usize },

    /// "If you have this thread, kill it."
    Kill { tid: usize },
}

/// Called only once by each owner core during startup to set the PER_CPU_SCHED apic_id
pub fn set_inbox_apic_id(id: usize) {
    let curr_id = id;
    let apic_id = get_apic_id();
    info!("Setting inbox{} apic_id to {}", curr_id, apic_id);
    per_cpu_ref(curr_id).apic_id.store(apic_id, Ordering::Release);
}

/// Returns the apic_id of the core with the given id
pub fn per_cpu_apic_id(cpu_id: usize) -> u32 {
    per_cpu_ref(cpu_id).apic_id.load(Ordering::Acquire)
}

/// Returns a reference to the PerCpuSched struct of the current core
#[inline]
pub fn per_cpu_ref_curr() -> &'static PerCpuRef {
    per_cpu_ref(current_core_id() as usize)
}

/// Returns a reference to the sender out of the PerCpuSched struct of the core with the given id
#[inline]
pub fn per_cpu_sender(id: usize) -> &'static Sender<Option<MessageItem>> {
    &per_cpu_ref(id).tx
}

/// Schedules a thread (wrapped in a MessageItem) on a remote core with the target_id
/// Sends a reschedule IPI to wake that core up, if it was idle.
pub fn schedule_on(target_id: usize, item: MessageItem) -> Result<(), MessageItem> {
    let pc = per_cpu_ref(target_id);
    match pc.tx.try_send(Some(item)) {
        Ok(()) => {
            send_reschedule_ipi(target_id);
            Ok(())
        }
        Err(e) => Err(e.into_inner().unwrap()), // unwrap: we sent Some(_)
    }
}

/// Schedules a cmd (wrapped in a MessageItem) on ALL remote cores
/// Does not send reschedule IPI's since only one core needs to actually do something
pub fn schedule_on_all_others(item: MessageItem) {
    let own_id = current_core_id() as usize;
    for i in 0..ACTIVE_CPUS.load(Ordering::Acquire) as usize {
        if i != own_id {
            let pc = per_cpu_ref(i);
            pc.tx.try_send(Some(item.clone())).expect("Failed to send message to remote core!");
        }
    }
}

/// drains the inbox from the cls into the ready queue; 10 items max per call
/// automatically calls inc_rq_len()
pub fn drain_inbox_into_ready(max: usize, ready_queue: &mut VecDeque<Arc<Thread>>) {
    let mut drained_threads = 0usize;
    for _ in 0..max {
        match cls().try_recv() {
            Ok(Some(item)) => match item {
                MessageItem::Thread(thread) => {

                    ready_queue.push_front(thread);
                    inc_rq_len();
                    drained_threads += 1;
                }
                MessageItem::Cmd(cmd) => {
                    scheduler().handle_inbox_cmd(cmd, ready_queue);
                }
            },
            Ok(None) => {
                log::error!("cpu{}: inbox returned None (unexpected)", current_core_id());
                break;
            }
            Err(_) => break,
        }
    }
    if drained_threads > 0 {
        clear_resched_flag();
        debug!("cpu{}: drained {} thread(s) into ready_queue", current_core_id(), drained_threads);
    }
}

/// Sends a Reschedule IPI to wake the core with the given id up, if it was idle.
fn send_reschedule_ipi(target_id: usize) {
    apic().send_reschedule(per_cpu_apic_id(target_id))
}
/// Owner function to set the reschedule flag of the current core.
pub fn set_resched_flag() {
    per_cpu_ref(current_core_id() as usize).resched_flag.store(true, Ordering::Release);
}
/// Owner function to clear the reschedule flag of the current core.
pub fn clear_resched_flag() {
    per_cpu_ref(current_core_id() as usize).resched_flag.store(false, Ordering::Release);
}
/// Owner function to read the reschedule flag of the current core.
pub fn read_resched_flag() -> bool {
    per_cpu_ref(current_core_id() as usize).resched_flag.load(Ordering::Acquire)
}
/// Owner function to increase the runqueue length of the current core.
pub fn inc_rq_len() {
    let id = current_core_id() as usize;
    per_cpu_ref(id).rq_len.fetch_add(1, Ordering::Relaxed);
}
/// Owner function to decrease the runqueue length of the current core.
pub fn dec_rq_len() {
    let id = current_core_id() as usize;
    per_cpu_ref(id).rq_len.fetch_sub(1, Ordering::Release); // Release for stronger publication before donating
}
/// Owner function to read the runqueue length of the current core.
pub fn read_rq_len() -> u32 {
    per_cpu_ref(current_core_id() as usize).rq_len.load(Ordering::Acquire)
}
/// Remote Reader function to read the runqueue length of the given Core
pub fn read_rq_len_remote(target_id: usize) -> u32 {
    per_cpu_ref(target_id).rq_len.load(Ordering::Acquire)
}

/// Idle_thread thread that checks for available Threads on other Cores and then
/// halts the cpu until it gets woken up by interrupts. (Other Cpus need to send first)
extern "sysv64" fn idle_thread () -> () {   //should never return but new_kernel_thread requires it
    loop {
        scheduler().look_for_overloaded_core();
        unsafe {
            asm!("hlt");
        }
    }
}