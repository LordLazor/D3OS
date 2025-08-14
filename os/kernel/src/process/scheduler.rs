use crate::built_info::FEATURES_LOWERCASE;
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
use alloc::sync::Arc;
use alloc::vec::Vec;
use log::info;
use core::ptr;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering::Relaxed;
use smallmap::Map;
use spin::{Mutex, MutexGuard};

use rbtree::RBTree;


// thread IDs
static THREAD_ID_COUNTER: AtomicUsize = AtomicUsize::new(1);

// Lazar Konstantinou:
// Constants for the CFS scheduler
static SCHED_NR_LATENCY: usize = 5; 
static SCHED_LATENCY: usize = 20_000_000;
static SCHED_MIN_GRANULARITY: usize = 4_000_000; 


pub fn next_thread_id() -> usize {
    THREAD_ID_COUNTER.fetch_add(1, Relaxed)
}

/// Everything related to the ready state in the scheduler
/// Lazar Konstantinou and David Schwabauer:
/// For CFS this also contains the sched_slice, sched_period, redblacktree and last switch time to ensure correctness of corresponding slice
struct ReadyState {
    sched_slice: usize,
    sched_period: usize,
    initialized: bool,
    last_switch_time: usize,
    rb_tree: RBTree<usize, Arc<SchedulingEntity>>,
    current: Option<Arc<SchedulingEntity>>,
}

/// Lazar Konstantinou and David Schwabauer:
// Contains all necessary information for a thread and the thread itself
// Manages virtual runtime, nice value, weight and last execution time of given thread
pub struct SchedulingEntity {
    vruntime: usize,
    nice: usize,
    weight: usize,
    last_exec_time: usize,
    thread: Arc<Thread>,
}

impl SchedulingEntity {
    /// Lazar Konstantinou and David Schwabauer:
    /// Creates a new SchedulingEntity instance for a given thread
    pub fn new(thread: Arc<Thread>, nice: i32) -> Self {
        // current system time in nanoseconds
        let current_time = timer().systime_ns();
        let nice = nice;
        let weight = Scheduler::nice_to_weight(nice) as usize; 
        Self {
            vruntime: 0,
            nice: nice as usize,
            last_exec_time: current_time,
            weight: weight,
            thread: thread,
        }
    }

    // Returns the current virtual runtime of the scheduling entity
    pub fn vruntime(&self) -> usize {
        self.vruntime
    }

    // sets the vruntime of a schedling entity
    pub fn set_vruntime(&mut self, vruntime: usize) {
        self.vruntime = vruntime;
    }

    // Returns the current scheduling entity so there are no issues with borrowing 
    pub fn thread(&self) -> Arc<Thread> {
        Arc::clone(&self.thread)
    }
}

impl ReadyState {
    pub fn new() -> Self {
        Self {
            sched_slice: 0, 
            sched_period: 0, 
            initialized: false,
            last_switch_time: 0,
            rb_tree: RBTree::new(),
            current: None,
        }
    }
}

/// Main struct of the scheduler
pub struct Scheduler {
    ready_state: Mutex<ReadyState>,
    sleep_list: Mutex<Vec<(Arc<SchedulingEntity>, usize)>>,
    join_map: Mutex<Map<usize, Vec<Arc<SchedulingEntity>>>>, // manage which threads are waiting for a thread-id to terminate
}

unsafe impl Send for Scheduler {}
unsafe impl Sync for Scheduler {}

/// Called from assembly code, after the thread has been switched
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unlock_scheduler() {
    unsafe { scheduler().ready_state.force_unlock(); }
}

impl Scheduler {

    /// Lazar Konstantinou:
    /// Creates a new instance of the CFS scheduler.
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

    /// Lazar Konstantinou:
    /// Returns all given thread ids that are currently active in the scheduler. (rb_tree and sleep_list)
    /// 
    /// Small changes inside this function to use the SchedulingEntity instead of the Thread directly inside of sleep map and cfs tree
    pub fn active_thread_ids(&self) -> Vec<usize> {
        let state = self.get_ready_state();

        let sleep_list = self.sleep_list.lock();

        state.rb_tree.iter()
            .map(|(_vruntime, entity)| entity.thread().id())
            .collect::<Vec<usize>>()
            .into_iter()
            .chain(sleep_list.iter().map(|(entity, _)| entity.thread().id()))
            .collect()
    }

    /// Description: Return reference to current thread
    /// 
    /// Lazar Konstantinou:
    /// Small changes to get a Thread inside of a SchedulingEntity
    pub fn current_thread(&self) -> Arc<Thread> {
        let state = self.get_ready_state();
        Scheduler::current_scheduling_entity(&state).thread()
    }

    /// Description: Return reference to thread for the given `thread_id`
    /// 
    /// David Schwabauer:
    /// Changes for the cfs scheduler as accessing the SchedulingEntity
    pub fn thread(&self, thread_id: usize) -> Option<Arc<Thread>> {
        self.get_ready_state().rb_tree
            .iter()
            .find(|(_, entity)| entity.thread().id() == thread_id)
            .map(|(_, entity)| Arc::clone(entity).thread())
    }

    /// Description: Start the scheduler, called only once from `boot.rs`
    /// 
    /// Lazar Konstantinou and David Schwabauer:
    /// Changes for the cfs scheduler as using the cfs tree correctly 
    pub fn start(&self) {
        // TODO: make sure this is actually called just once: This TODO was already inside this commit prob. by Fabian => We ignored this as we had no errors with this case
        let mut state = self.get_ready_state();
        state.current = state.rb_tree.pop_first().map(|(_, entity)| entity);

        self.update_sched_slice(&mut state);

        unsafe { 
            let entity = state.current.as_ref().expect("Failed to pop first thread from cfs tree!");
            Thread::start_first(entity.thread().as_ref());
        }
    }

    /// 
    /// Description: Insert a thread into the ready_queue
    /// 
    /// Parameters: `thread` thread to be inserted.
    /// 
    /// Lazar Konstantinou and David Schwabauer:
    /// Changes for the cfs scheduler as using the join map correctly
    /// Important: ready() gets a thread and creates a new SchedulingEntity for it.
    /// Also initialized the virtual runtime here
    pub fn ready(&self, thread: Arc<Thread>, nice: i32) {
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

         // We need to create a new SchedulingEntity wrapper for the thread when being inserted into the scheduler
        let mut entity_struct: SchedulingEntity = SchedulingEntity::new(thread, nice);

        if state.rb_tree.len() == 0 {
            // Nothing inside the scheduler init the first thread and set its vruntime to 1
            entity_struct.vruntime  = 0;
        } else {
            let weight = entity_struct.weight;
            let sum_weights: usize = state.rb_tree.iter()
                .map(|(_, entity)| entity.weight)
                .sum();
            let min_vruntime = state.rb_tree.get_first().map(|(key, _)| *key).unwrap_or(0);
            entity_struct.vruntime = self.calculated_sched_vslice(&state, weight, sum_weights, min_vruntime);
        }

        let entity = Arc::new(entity_struct);
        state.rb_tree.insert(entity.vruntime(), entity);
        join_map.insert(id, Vec::new());
    }

    // David Schwabauer:
    // Formula:
    // if nr_running <= sched_nr_latency => sched_latency
    // else sched_latency * nr_running / sched_nr_latency
    fn calculate_sched_period(&self, state: &ReadyState) -> usize {
        let sched_period: usize;
        let nr_running = state.rb_tree.len();
        if nr_running <= SCHED_NR_LATENCY {
            sched_period = SCHED_LATENCY;
        } else {
            sched_period = (SCHED_LATENCY * nr_running) / SCHED_NR_LATENCY;
        }

        sched_period
    }

    /// Lazar Konstantinou:
    // Formula: sched_period * current_weight / (current_weight+sum_rbtree_weights)
    fn update_sched_slice(&self, state: &mut ReadyState) {
        let sched_period = self.calculate_sched_period(state);

        let current_weight = state.current.as_ref().unwrap().weight;
        let sum_rbtree_weights: usize = state.rb_tree.iter()
            .map(|(_, entity)| entity.weight)
            .sum();

        state.sched_slice = (sched_period * current_weight) / (current_weight + sum_rbtree_weights);
    }

    // Lazar Konstantinou:
    // Formula: (current_period*NICE_0_LOAD)//entity_weight+sum(all_thread_weights))
    fn calculated_sched_vslice(&self, state: &ReadyState, entity_weight: usize, sum_entities_weight: usize, min_vruntime: usize) -> usize {
        let sched_period = self.calculate_sched_period(state);
        min_vruntime + (sched_period * 1024) / (entity_weight+sum_entities_weight)
    }

    /// Description: Put calling thread to sleep for `ms` milliseconds
    pub fn sleep(&self, ms: usize) {
        let mut state = self.get_ready_state();

        if !state.initialized {
            // Scheduler is not initialized yet, so this function has been called during the boot process
            // So we do active waiting
            timer().wait(ms);
        } 
        else {
            // Scheduler is initialized, so we can block the calling thread
            let thread = Scheduler::current_scheduling_entity(&state);
            let wakeup_time = timer().systime_ms() + ms;
            
            {
                // Execute in own block, so that the lock is released automatically (block() does not return)
                let mut sleep_list = self.sleep_list.lock();
                sleep_list.push((thread, wakeup_time));
            }

            self.block(&mut state);
        }
    }

    /// David Schwabauer:
    ///Checks, if there is a Thread with smaller vruntime then the current one
    fn check_smaller_vruntime(&self, state: &mut ReadyState) -> bool {
        let Some((_, next)) = state.rb_tree.pop_first() else {
            return false;
        };

        let current = Scheduler::current_scheduling_entity(state);
        let should_switch = current.vruntime() >= next.vruntime();

        // Current thread has a smaller vruntime than the next thread, so we do not switch
        state.rb_tree.insert(next.vruntime(), next);
        should_switch
    }


    /// David Schwabauer:
    /// Switch from current to next thread (from ready queue)
    /// 
    /// Description: Switch from current to next thread (from ready queue)
    /// 
    /// Parameters: `interrupt` true = called from ISR -> need to send EOI to APIC
    ///                         false = no EOI needed
    ///
    fn switch_thread(&self, state: &mut ReadyState, interrupt: bool) {
        let Some((_, next)) = state.rb_tree.pop_first() else {
            return;
        };

        let current = Scheduler::current_scheduling_entity(state);
        let current_ptr = ptr::from_ref(current.thread().as_ref());
        let next_ptr = ptr::from_ref(next.thread().as_ref());

        state.current = Some(next);
        state.rb_tree.insert(current.vruntime(), current);

        if interrupt {
            apic().end_of_interrupt();
        }

        unsafe {
            Thread::switch(current_ptr, next_ptr);
        }
    }


    /// Description: helper function, calling `switch_thread`
    /// this also checks if we even should switch threads right now with check_switch_thread and switch then with switch_thread
    /// David Schwabauer:
    pub fn switch_thread_no_interrupt(&self) {
        if let Some(mut state) = self.ready_state.try_lock() {
            if self.check_switch_thread(&mut state) && self.check_smaller_vruntime(&mut state){
                self.switch_thread(&mut state, false);
            }
        }
    }

    /// Description: helper function, calling `switch_thread`
    /// this also checks if we even should switch threads right now with check_switch_thread and switch then with switch_thread
    /// David Schwabauer:
    pub fn switch_thread_from_interrupt(&self) {
        if let Some(mut state) = self.ready_state.try_lock() {
            if self.check_switch_thread(&mut state) && self.check_smaller_vruntime(&mut state){
                self.switch_thread(&mut state, true);
            }
        }
    }

    // Lazar Konstantinou:
    // Helper function to print all thread ids and their virtual runtimes (for debugging purposes)
    fn print_thread_ids_plus_virtual_runtimes(&self, state: &mut ReadyState){
        for (vruntime, entity) in state.rb_tree.iter() {
            info!("Thread ID: {}, vruntime: {}", entity.thread().id(), vruntime);
        }
        // print current
        if let Some(current) = &state.current {
            info!("Current thread ID: {}, vruntime: {}", current.thread().id(), current.vruntime());
        } else {
            info!("No current thread set!");
        }
    }

    // Lazar Konstantinou:
    // Update current thread's vruntime
    // before inserting into the rb_tree check if current threads new vruntime is smaller than the min vruntime of rb tree
    // if true: return false, because the current thread should not be switched out
    // if false: return true and insert old current into tree, because the current thread should be switched out
    fn check_switch_thread(&self, mut state: &mut ReadyState) -> bool {

        if !state.initialized {
            return false;
        }

        // Get clone of the current thread
        let current = Scheduler::current_scheduling_entity(&state);

        // Current thread is initializing itself and may not be interrupted
        if current.thread().stacks_locked() || tss().is_locked() {
            return false;
        }

        if let Some(mut sleep_list) = self.sleep_list.try_lock() {
            Scheduler::check_sleep_list(&mut state, &mut sleep_list);
        }

        let now = timer().systime_ns();

        let sched_slice = state.sched_slice;
        let last_switch_time = state.last_switch_time;

        if now - last_switch_time < sched_slice {
            return false;
        }

        let rb_tree_len = state.rb_tree.len();
        if rb_tree_len == 0 {
            // No threads in the ready queue, so we dont need to switch
            return false;
        }
        drop(current);

        // Update the scheduling slice and current thread's vruntime
        self.update_sched_slice(state);
        self.update_current(state);

        true

    }

    // Lazar Konstantinou und David Schwabauer:
    // Updated die virtual Runtime des aktuellen Threads indem:
    // (deltaExecTime × NICE_0_WEIGHT)/weight_schedule_entity
    fn update_current(&self, state: &mut ReadyState) {
        //info!("Updating current entity vruntime");
        

        let Some(current_rc) = state.current.as_mut() else {
            // info!("update_current: No current_entity!");
            return;
        };

        let Some(current_entity) = Arc::get_mut(current_rc) else {
            // info!("update_current: No exclusive mutable!");
            return;
        };

        let now = timer().systime_ns();
        state.last_switch_time = now;
        let delta_exec = now.saturating_sub(current_entity.last_exec_time);
        if delta_exec <= 0 {
            return;
        }

        const NICE_0_LOAD: usize = 1024; // Standard Value for NICE_0_LOAD in CFS scheduler  
    
        // Virtual runtime update formula:
        // vruntime_new​=vruntime_old​+Δt*NICE_0_LOAD​/weight

        // Nice > 0 lower priority, Nice < 0 higher priority
        let weighted_delta = delta_exec * NICE_0_LOAD / current_entity.weight;
        
        current_entity.vruntime += weighted_delta; // vruntime_old + weighted_delta
        current_entity.last_exec_time = now;
        
    }

    /// 
    /// Description: Calling thread wants to wait for another thread to terminate
    /// 
    /// Parameters: `thread_id` thread to wait for
    /// 
    /// David Schwabauer:
    /// Changes for the cfs scheduler
    pub fn join(&self, thread_id: usize) {
        let mut state = self.get_ready_state();
        let thread = Scheduler::current_scheduling_entity(&state);

        {
            // Execute in own block, so that the lock is released automatically (block() does not return)
            let mut join_map = self.join_map.lock();
            if let Some(join_list) = join_map.get_mut(&thread_id) {
                join_list.push(thread);
            } else {
                // Joining on a non-existent thread has no effect (i.e. the thread has already finished running)
                return;
            }
        }

        self.block(&mut state);
    }

    /// Description: Exit calling thread.
    /// 
    /// David Schwabauer:
    /// Changes for the cfs scheduler as using the rb tree correct
    pub fn exit(&self) -> ! {
        let mut ready_state;
        let current;

        {
            // Execute in own block, so that join_map is released automatically (block() does not return)
            let state = self.get_ready_state_and_join_map();
            ready_state = state.0;
            let mut join_map = state.1;

            current = Scheduler::current_scheduling_entity(&ready_state);
            let join_list = join_map.get_mut(&current.thread().id()).expect("Missing join_map entry!");

            for entity in join_list {
                ready_state.rb_tree.insert(entity.vruntime(), Arc::clone(entity));
            }
            join_map.remove(&current.thread().id());
        }

        drop(current); // Decrease Arc manually, because block() does not return
        self.block(&mut ready_state);
        unreachable!()
    }

    /// 
    /// Description: Kill the thread with the  given id
    /// 
    /// Parameters: `thread_id` thread to be killed
    /// 
    /// Lazar Konstantinou:
    /// Changes for the cfs scheduler as using the rb tree correct and also the join map
    pub fn kill(&self, thread_id: usize) {
        {
            // Check if current thread tries to kill itself (illegal)
            let ready_state = self.get_ready_state();
            let current = Scheduler::current_scheduling_entity(&ready_state);

            if current.thread().id() == thread_id {
                panic!("A thread cannot kill itself!");
            }
        }

        let state = self.get_ready_state_and_join_map();
        let mut ready_state = state.0;
        let mut join_map = state.1;

        let join_list = join_map.get_mut(&thread_id).expect("Missing join map entry!");

        for entity in join_list {
            ready_state.rb_tree.insert(entity.vruntime(), Arc::clone(entity));
        }

        join_map.remove(&thread_id);

        /* Hier nochmal checken ob das richtig ist */
        // Alle vruntime-Keys sammeln, deren Entity die gewünschte Thread-ID hat
        let to_remove: Vec<usize> = ready_state.rb_tree
            .iter()
            .filter(|(_, entity)| entity.thread().id() == thread_id)
            .map(|(vruntime, _)| *vruntime)
            .collect();

        // Jetzt alle passenden Keys entfernen
        for vruntime in to_remove {
            ready_state.rb_tree.remove(&vruntime);
        }
    }

    /// 
    /// Description: Block calling thread
    /// 
    /// Parameters: `state` ReadyState of scheduler 
    /// MS -> why this param?
    /// 
    /// David Schwabauer:
    /// Changes for the cfs scheduler as using the rb tree correct and correct refs to the given objects inside
    fn block(&self, state: &mut ReadyState) {
        let mut first_node = state.rb_tree.pop_first();

        {
            // Execute in own block, so that the lock is released automatically (block() does not return)
            let mut sleep_list = self.sleep_list.lock();
            while first_node.is_none() {
                Scheduler::check_sleep_list(state, &mut sleep_list);
                first_node = state.rb_tree.pop_first();
            }
        }

        let current = Scheduler::current_scheduling_entity(&state);
        let next = first_node.unwrap();

        // Thread has enqueued itself into sleep list and waited so long, that it dequeued itself in the meantime
        if current.thread().id() == next.1.thread().id() {
            return;
        }

        let current_ptr = ptr::from_ref(current.thread().as_ref());
        let next_ptr = ptr::from_ref(next.1.thread().as_ref());

        state.current = Some(next.1);
        drop(current); // Decrease Arc manually, because Thread::switch does not return

        unsafe {
            Thread::switch(current_ptr, next_ptr);
        }
    }

    /// Description: Return current running thread
    /// Lazar Konstantinou:
    /// This function is used to access the current scheduling entity !not thread! in the scheduler.
    fn current_scheduling_entity(state: &ReadyState) -> Arc<SchedulingEntity> {
        Arc::clone(state.current.as_ref().expect("Trying to access current thread before initialization!"))
    }

    // Lazar Konstantinou:
    // Checking the sleep list for alle threads that are currently ready to be woken up
    // Try to insert the thread into the tree with a updated vruntime which check the 
    // maximum of old vruntime and and min vruntime - sched_latency
    fn check_sleep_list(state: &mut ReadyState, sleep_list: &mut Vec<(Arc<SchedulingEntity>, usize)>) {
        let time = timer().systime_ms();

        let mut to_reinsert = Vec::new();
        sleep_list.retain(|entry| {
            if time >= entry.1 {
                to_reinsert.push(Arc::clone(&entry.0));
                false
            } else {
                true
            }
        });

        // Check for each waking thread if its vruntime is smaller than the current minimum in the tree
        // If yes, set it to the minimum from the tree - SCHED_LATENCY
        // If his vruntime is greater than the minimum, it should be reinserted with its original vruntime
        for entity in to_reinsert {
            let min_vruntime = state.rb_tree.get_first().map(|(key, _)| *key).unwrap_or(0);
            
            let maximum_vruntime = entity.vruntime().max(min_vruntime - SCHED_LATENCY); 

            // Make a clone of the Arc to be able to transform it an mutable arc
            let mut entity_clone = Arc::clone(&entity);
            drop(entity); // Drop entity because otherwise we would have to Refs to an Arc (not possible) => no mutable ever!
            let Some(entity_arc) = Arc::get_mut(&mut entity_clone) else {
                //info!("Back to sleeplist");
                sleep_list.push((entity_clone, 1)); // Put 1 in so it will try to wake thread up almost instantly in next iteration
                continue;
            };

            entity_arc.set_vruntime(maximum_vruntime);

            //info!("Waked up in rbtree with vruntime: {} and maximum: {}", entity_clone.vruntime(), maximum_vruntime);

            state.rb_tree.insert(entity_clone.vruntime(), entity_clone);
        }

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
    fn get_ready_state_and_join_map(&self) -> (MutexGuard<ReadyState>, MutexGuard<Map<usize, Vec<Arc<SchedulingEntity>>>>) {
        loop {
            let ready_state = self.get_ready_state();
            if let Some(join_map) = self.join_map.try_lock() {
                return (ready_state, join_map);
            } else {
                self.switch_thread_no_interrupt();
            }
        }
    }

    // Array which contains the weights for the different nice values
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

    // Converts a nice value to the corresponding weight based on the PRIO_TO_WEIGHT array
    pub fn nice_to_weight(nice: i32) -> u32 {
        let idx = (nice + 20).clamp(0, 39) as usize; //Wandelt nice Wert in das passendes Gewicht um, indem er den korrekten Index aus dem Array aufruft, -20 ist Index 0, 19 ist Index 39...
        Scheduler::PRIO_TO_WEIGHT[idx]
    }
}
