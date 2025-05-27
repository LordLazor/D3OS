/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: scheduler                                                       ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Descr.: Implementation of the scheduler.                                ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Fabian Ruhland, HHU                                             ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/
use crate::process::thread::Thread;
use crate::{allocator, apic, scheduler, timer, tss};
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::vec::Vec;
use log::info;
use rbtree::RBTree;

use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::sync::atomic::Ordering::Relaxed;
use smallmap::Map;
use spin::{Mutex, MutexGuard};

// thread IDs
static THREAD_ID_COUNTER: AtomicUsize = AtomicUsize::new(1);

pub fn next_thread_id() -> usize {
    THREAD_ID_COUNTER.fetch_add(1, Relaxed)
}

/// Everything related to the ready state in the scheduler
struct ReadyState {
    initialized: bool,
    current_thread: Option<Rc<Thread>>,
    ready_queue: VecDeque<Rc<Thread>>,
}

impl ReadyState {
    pub fn new() -> Self {
        Self {
            initialized: false,
            current_thread: None,
            ready_queue: VecDeque::new(),
        }
    }
}

/// Main struct of the scheduler
pub struct Scheduler {
    ready_state: Mutex<ReadyState>,
    sleep_list: Mutex<Vec<(Rc<Thread>, usize)>>,
    join_map: Mutex<Map<usize, Vec<Rc<Thread>>>>, // manage which threads are waiting for a thread-id to terminate
}

unsafe impl Send for Scheduler {}
unsafe impl Sync for Scheduler {}

/// Called from assembly code, after the thread has been switched
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unlock_scheduler() {
    unsafe { scheduler().ready_state.force_unlock(); }
}

impl Scheduler {

    /// Description: Create and init the scheduler.
    pub fn new() -> Self {
        Self {
            ready_state: Mutex::new(ReadyState::new()),
            sleep_list: Mutex::new(Vec::new()),
            join_map: Mutex::new(Map::new()),
        }
    }

    /// Description: Called during creation of threads
    pub fn set_init(&self) {
        self.get_ready_state().initialized = true;
    }

    pub fn active_thread_ids(&self) -> Vec<usize> {
        let state = self.get_ready_state();
        let sleep_list = self.sleep_list.lock();

        state.ready_queue.iter()
            .map(|thread| thread.id())
            .collect::<Vec<usize>>()
            .into_iter()
            .chain(sleep_list.iter().map(|entry| entry.0.id()))
            .collect()
    }

    /// Description: Return reference to current thread
    pub fn current_thread(&self) -> Rc<Thread> {
        let state = self.get_ready_state();
        Scheduler::current(&state)
    }

    /// Description: Return reference to thread for the given `thread_id`
    pub fn thread(&self, thread_id: usize) -> Option<Rc<Thread>> {
        self.ready_state.lock().ready_queue
            .iter()
            .find(|thread| thread.id() == thread_id)
            .cloned()
    }

    /// Description: Start the scheduler, called only once from `boot.rs` 
    pub fn start(&self) {
        let mut state = self.get_ready_state();
        state.current_thread = state.ready_queue.pop_back();

        unsafe { Thread::start_first(state.current_thread.as_ref().expect("Failed to dequeue first thread!").as_ref()); }
    }

    /// 
    /// Description: Insert a thread into the ready_queue
    /// 
    /// Parameters: `thread` thread to be inserted.
    /// 
    pub fn ready(&self, thread: Rc<Thread>) {
        let id = thread.id();
        let mut join_map;
        let mut state;

        // If we get the lock on 'self.state' but not on 'self.join_map' the system hangs.
        // The scheduler is not able to switch threads anymore, because of 'self.state' is locked,
        // and we will never be able to get the lock on 'self.join_map'.
        // To solve this, we need to release the lock on 'self.state' in case we do not get
        // the lock on 'self.join_map' and let the scheduler switch threads until we get both locks.
        loop {
            let state_mutex = self.get_ready_state();
            let join_map_option = self.join_map.try_lock();

            if join_map_option.is_some() {
                state = state_mutex;
                join_map = join_map_option.unwrap();
                break;
            } else {
                self.switch_thread_no_interrupt();
            }
        }

        state.ready_queue.push_front(thread);
        join_map.insert(id, Vec::new());
    }

    /// Description: Put calling thread to sleep for `ms` milliseconds
    pub fn sleep(&self, ms: usize) {
        let mut state = self.get_ready_state();

        if !state.initialized {
            timer().wait(ms);
        } else {
            let thread = Scheduler::current(&state);
            let wakeup_time = timer().systime_ms() + ms;
            
            {
                // Execute in own block, so that the lock is released automatically (block() does not return)
                let mut sleep_list = self.sleep_list.lock();
                sleep_list.push((thread, wakeup_time));
            }

            self.block(&mut state);
        }
    }

    /// 
    /// Description: Switch from current to next thread (from ready queue)
    /// 
    /// Parameters: `interrupt` true = called from ISR -> need to send EOI to APIC
    ///                         false = no EOI needed
    /// 
    fn switch_thread(&self, interrupt: bool) {
        if let Some(mut state) = self.ready_state.try_lock() {
            if !state.initialized {
                return;
            }

            if let Some(mut sleep_list) = self.sleep_list.try_lock() {
                Scheduler::check_sleep_list(&mut state, &mut sleep_list);
            }

            let current = Scheduler::current(&state);
            let next = match state.ready_queue.pop_back() {
                Some(thread) => thread,
                None => return,
            };

            // Current thread is initializing itself and may not be interrupted
            if current.stacks_locked() || tss().is_locked() {
                return;
            }

            let current_ptr = ptr::from_ref(current.as_ref());
            let next_ptr = ptr::from_ref(next.as_ref());

            state.current_thread = Some(next);
            state.ready_queue.push_front(current);

            if interrupt {
                apic().end_of_interrupt();
            }

            unsafe {
                Thread::switch(current_ptr, next_ptr);
            }
        }
    }

    /// Description: helper function, calling `switch_thread`
    pub fn switch_thread_no_interrupt(&self) {
        self.switch_thread(false);
    }

    /// Description: helper function, calling `switch_thread`
    pub fn switch_thread_from_interrupt(&self) {
        self.switch_thread(true);
    }

    /// 
    /// Description: Calling thread wants to wait for another thread to terminate
    /// 
    /// Parameters: `thread_id` thread to wait for
    /// 
    pub fn join(&self, thread_id: usize) {
        let mut state = self.get_ready_state();
        let thread = Scheduler::current(&state);

        {
            // Execute in own block, so that the lock is released automatically (block() does not return)
            let mut join_map = self.join_map.lock();
            let join_list = join_map.get_mut(&thread_id);
            if join_list.is_some() {
                join_list.unwrap().push(thread);
            } else {
                // Joining on a non-existent thread has no effect (i.e. the thread has already finished running)
                return;
            }
        }

        self.block(&mut state);
    }

    /// Description: Exit calling thread.
    pub fn exit(&self) {
        let mut ready_state;
        let current;

        {
            // Execute in own block, so that join_map is released automatically (block() does not return)
            let state = self.get_ready_state_and_join_map();
            ready_state = state.0;
            let mut join_map = state.1;

            current = Scheduler::current(&ready_state);
            let join_list = join_map.get_mut(&current.id()).expect("Missing join_map entry!");

            for thread in join_list {
                ready_state.ready_queue.push_front(Rc::clone(thread));
            }

            join_map.remove(&current.id());
        }

        drop(current); // Decrease Rc manually, because block() does not return
        self.block(&mut ready_state);
    }

    /// 
    /// Description: Kill the thread with the  given id
    /// 
    /// Parameters: `thread_id` thread to be killed
    /// 
    pub fn kill(&self, thread_id: usize) {
        {
            // Check if current thread tries to kill itself (illegal)
            let ready_state = self.get_ready_state();
            let current = Scheduler::current(&ready_state);

            if current.id() == thread_id {
                panic!("A thread cannot kill itself!");
            }
        }

        let state = self.get_ready_state_and_join_map();
        let mut ready_state = state.0;
        let mut join_map = state.1;

        let join_list = join_map.get_mut(&thread_id).expect("Missing join map entry!");

        for thread in join_list {
            ready_state.ready_queue.push_front(Rc::clone(thread));
        }

        join_map.remove(&thread_id);
        ready_state.ready_queue.retain(|thread| thread.id() != thread_id);
    }

    /// 
    /// Description: Block calling thread
    /// 
    /// Parameters: `state` ReadyState of scheduler 
    /// MS -> why this param?
    /// 
    fn block(&self, state: &mut ReadyState) {
        let mut next_thread = state.ready_queue.pop_back();

        {
            // Execute in own block, so that the lock is released automatically (block() does not return)
            let mut sleep_list = self.sleep_list.lock();
            while next_thread.is_none() {
                Scheduler::check_sleep_list(state, &mut sleep_list);
                next_thread = state.ready_queue.pop_back();
            }
        }

        let current = Scheduler::current(&state);
        let next = next_thread.unwrap();

        // Thread has enqueued itself into sleep list and waited so long, that it dequeued itself in the meantime
        if current.id() == next.id() {
            return;
        }

        let current_ptr = ptr::from_ref(current.as_ref());
        let next_ptr = ptr::from_ref(next.as_ref());

        state.current_thread = Some(next);
        drop(current); // Decrease Rc manually, because Thread::switch does not return

        unsafe {
            Thread::switch(current_ptr, next_ptr);
        }
    }

    /// Description: Return current running thread
    fn current(state: &ReadyState) -> Rc<Thread> {
        Rc::clone(state.current_thread.as_ref().expect("Trying to access current thread before initialization!"))
    }

    fn check_sleep_list(state: &mut ReadyState, sleep_list: &mut Vec<(Rc<Thread>, usize)>) {
        let time = timer().systime_ms();

        sleep_list.retain(|entry| {
            if time >= entry.1 {
                state.ready_queue.push_front(Rc::clone(&entry.0));
                return false;
            }

            return true;
        });
    }

    /// Description: Helper function returning `ReadyState` of scheduler in a MutexGuard
    fn get_ready_state(&self) -> MutexGuard<ReadyState> {
        let state;

        // We need to make sure, that both the kernel memory manager and the ready queue are currently not locked.
        // Otherwise, a deadlock may occur: Since we are holding the ready queue lock,
        // the scheduler won't switch threads anymore, and none of the locks will ever be released
        loop {
            let state_tmp = self.ready_state.lock();
            if allocator().is_locked() {
                continue;
            }

            state = state_tmp;
            break;
        }

        state
    }

    /// Description: Helper function returning `ReadyState` and `Map` of scheduler, each in a MutexGuard
    fn get_ready_state_and_join_map(&self) -> (MutexGuard<ReadyState>, MutexGuard<Map<usize, Vec<Rc<Thread>>>>) {
        loop {
            let ready_state = self.get_ready_state();
            let join_map = self.join_map.try_lock();

            if join_map.is_some() {
                return (ready_state, join_map.unwrap());
            } else {
                self.switch_thread_no_interrupt();
            }
        }
    }



}

/* 
Lazar Konstantinou:
Wrapper around each thread that is scheduled by the cfs scheduler 
which additionally stores scheduling parameters
*/
pub struct SchedulingEntity {
    vruntime: usize,
    weight: usize,
    nice: usize,
    last_exec_time: usize,
    thread: Rc<Thread>,
}
static GLOBAL_VRUNTIME_COUNTER: AtomicUsize = AtomicUsize::new(0);

impl SchedulingEntity {

    
    /*
        Lazar Konstantinou:
        Creates a new SchedulingEntity instance for a given thread    
     */
    pub fn new(thread: Rc<Thread>) -> Self {
        // current system time in nanoseconds
        let current_time = timer().systime_ns();
        let vruntime = GLOBAL_VRUNTIME_COUNTER.fetch_add(1, Ordering::Relaxed);
        Self {
            vruntime: vruntime,
            weight: 0,
            nice: 0 as usize,
            last_exec_time: current_time,
            thread,
        }
    }

    /*
        Lazar Konstantinou:
        Returns the current virtual runtime of the scheduling entity
    */
    pub fn vruntime(&self) -> usize {
        self.vruntime
    }

    /*
        Lazar Konstantinou:
        Returns the current scheduling entity so there are no issues with borrowing 
    */
    pub fn thread(&self) -> Rc<Thread> {
        Rc::clone(&self.thread)
    }
}


pub struct CfsScheduler {
    // Red-Black-Tree (z. B. via BTreeMap, echte RBT-Implementierungen sind ebenfalls möglich)
    cfs_tree: Mutex<RBTree<usize, Rc<SchedulingEntity>>>,

    // Aktuell laufende Scheduling-Entity (nicht im run_queue enthalten)
    current: Mutex<Option<Rc<SchedulingEntity>>>,

    // Globale Zeitscheibensteuerung
    // scheduling_latency: usize,
    // min_granularity: usize,

    // Time Zeitpunkt der letzten Aktualisierung (z. B. für `exec_start`)
    //last_update_time: usize,

}

unsafe impl Send for CfsScheduler {}
unsafe impl Sync for CfsScheduler {}

impl CfsScheduler {
    /* 
        Lazar Konstantinou:
        Merged from the Linux kernel 2.6.24
    */
    const MAX_RT_PRIO: i32 = 100;
    pub const fn nice_to_prio(nice: i32) -> i32 {
        CfsScheduler::MAX_RT_PRIO + nice + 20
    }
    pub const fn prio_to_nice(prio: i32) -> i32 {
        prio - CfsScheduler::MAX_RT_PRIO - 20
    }
    pub const PRIO_TO_WEIGHT: [u32; 40] = [
        88761, 71755, 56483, 46273, 36291,
        29154, 23254, 18705, 14949, 11916,
        9548,  7620,  6100,  4904,  3906,
        3121,  2501,  1991,  1586,  1277,
        1024,   820,   655,   526,   423,
        335,   272,   215,   172,   137,
        110,    87,    70,    56,    45,
        36,    29,    23,    18,    15,
    ];
    pub fn nice_to_weight(nice: i32) -> u32 {
        let idx = (nice + 20).clamp(0, 39) as usize;
        CfsScheduler::PRIO_TO_WEIGHT[idx]
    }
    pub const PRIO_TO_WMULT: [u32; 40] = [
        48388, 59856, 76040, 92818, 118348,
        147320, 184698, 229616, 287308, 360437,
        449829, 563644, 704093, 875809, 1099582,
        1376151, 1717300, 2157191, 2708050, 3363326,
        4194304, 5237765, 6557202, 8165337, 10153587,
        12820798, 15790321, 19976592, 24970740, 31350126,
        39045157, 49367440, 61356676, 76695844, 95443717,
        119304647, 148102320, 186737708, 238609294, 286331153,
    ];

    /* 
        Lazar Konstantinou:
        Creates a new CfsScheduler instance
        Initializes the rbtree and sets the current scheduling entity to None
    */
    pub fn new() -> Self {
        Self {
            cfs_tree: Mutex::new(RBTree::new()),
            current: Mutex::new(None),
        }
    }

    /*
        Lazar Konstantinou:
        Puts a scheduling entity into the rbtree
    */
    pub fn ready(&self, entity: Rc<SchedulingEntity>) {
        // Insert the scheduling entity into the rbtree
        let mut cfs_tree = self.cfs_tree.lock();

        let entity = Rc::clone(&entity);
        let vruntime = entity.vruntime();

        // Insert into the rbtree based on vruntime
        cfs_tree.insert(vruntime, entity);
    }

    /*
        Lazar Konstantinou:
        Take the current element on the rbtree
    */
    pub fn current(&self) -> Option<Rc<SchedulingEntity>> {
        let current = self.current.lock();
        current.as_ref().map(Rc::clone)
    }

    /*
        Lazar Konstantinou:
        Starts the cfs scheduler by taking the first element from rbtree
        and starting the thread associated with it.
    */
    pub fn start(&self) {
        // Start the scheduler by taking the first element from the rbtree
        let mut cfs_tree = self.cfs_tree.lock();
        if let Some((_, entity)) = cfs_tree.pop_first() {
            *self.current.lock() = Some(Rc::clone(&entity));

            unsafe {
                // Start the first thread
                Thread::start_first(entity.thread().as_ref());
            }
        } else {
            panic!("No scheduling entity available to start!");
        }
    }

    /* 
        Lazar Konstantinou:
        Switches the current thread to the next one in the rbtree
        and updates the current scheduling entity.
    */
    pub fn switch_thread(&self) {
        let mut cfs_tree = self.cfs_tree.lock();
        let mut current = self.current.lock();

        // If there is no current thread, we cannot switch
        if current.is_none() {
            return;
        }

        // Get the current scheduling entity and remove it from the rbtree
        let current_entity = current.as_ref().unwrap();
        cfs_tree.remove(&current_entity.vruntime());

        // Get the next entity from the rbtree
        if let Some((_, next_entity)) = cfs_tree.pop_first() {
            *self.current.lock() = Some(Rc::clone(&next_entity));

            unsafe {
                // Switch to the next thread
                Thread::switch(
                    ptr::from_ref(current_entity.thread().as_ref()),
                    ptr::from_ref(next_entity.thread().as_ref()),
                );
            }
        } else {
            // No more entities in the rbtree
            *current = None;
        }
    }

    /*
        Lazar Konstantinou:
        Kill the given thread id
    */
    pub fn kill(&self, thread_id: usize) {
        let mut cfs_tree = self.cfs_tree.lock();
        let current = self.current.lock();

        // current thread cannot kill itself
        let current_entity = current.as_ref().map(Rc::clone);
        if let Some(entity) = current_entity {
            if entity.thread().id() == thread_id {
                panic!("A thread cannot kill itself!");
            }
        }
    
        // for each key in the rbtree, check if the thread id matches
        let mut to_remove: Option<usize> = None;
        for (vruntime, entity) in cfs_tree.iter() {
            if entity.thread().id() == thread_id {
                to_remove = Some(vruntime.clone());
                break; // Found the thread, no need to continue
            }
        }

        // Remove the entity
        if to_remove.is_some() {
            let vruntime = to_remove.unwrap();
            cfs_tree.remove(&vruntime);
            info!("Thread with id {} and vruntime {} killed from CFS tree", thread_id, vruntime);
        } else {
            info!("Thread with id {} not found in CFS tree", thread_id);
        }
        
    }

    // David:
    // Updated die virtual Runtime des aktuellen Threads indem:
    // (deltaExecTime × NICE_0_WEIGHT)/weight_schedule_entity
    fn update_current(&self) {
        let now = timer().systime_ns(); // Aktuelle Zeit in Nanosekunden
        let mut current_lock = self.current.lock();

        let Some(current_entity) = current_lock.as_mut() else {
            return; // Wenn kein Thread aktiv ist, gibt es nichts zu aktualisieren
        };

        let mut_entity = Rc::get_mut(current_entity) //Muss der alleinige Zugriff auf den aktuellen Thread sein
            .expect("update_current: SchedulingEntity ist mehrfach geteilt!");

        let delta_exec = now.saturating_sub(mut_entity.last_exec_time); //Subtraktion, schließt jedoch negative Werte aus (z.B. 200 - 300 = 0)

        if delta_exec == 0 {
            return; // Keine Ausführungszeit vergangen, somit nichts zu aktualisieren
        }

        const NICE_0_LOAD: usize = 1024; // Standardwert

        let weight = if mut_entity.weight == 0 { // Wenn der Thread kein Gewicht hat, nehme NICE_0_LOAD um eine Division durch 0 zu verhindern
            NICE_0_LOAD
        } else {
            mut_entity.weight
        };

        let weighted_delta = delta_exec * NICE_0_LOAD / weight;

        mut_entity.vruntime += weighted_delta;

        mut_entity.last_exec_time = now;
    }
}