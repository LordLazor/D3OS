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
use alloc::rc::Rc;
use alloc::vec::Vec;
use log::info;
use rbtree::RBTree;

use core::{panic, ptr};
use core::sync::atomic::{AtomicUsize};
use core::sync::atomic::Ordering::Relaxed;
use smallmap::Map;
use spin::{Mutex, MutexGuard};

// thread IDs
static THREAD_ID_COUNTER: AtomicUsize = AtomicUsize::new(1);

static sched_nr_latency: usize = 5; // initial value in Linux will not be touched
static sched_latency: usize = 20_000_000; // initial value in Linux will not be touched
static sched_min_granularity: usize = 4_000_000; // initial value in Linux will not be touched

pub fn next_thread_id() -> usize {
    THREAD_ID_COUNTER.fetch_add(1, Relaxed)
}


struct ReadyState {
    sched_slice: usize,
    sched_period: usize,
    initialized: bool,
    last_switch_time: usize,
    rb_tree: RBTree<usize, Rc<SchedulingEntity>>,
    current: Option<Rc<SchedulingEntity>>,
}

// TODO: Remove public and make private after testing
pub struct SchedulingEntity {
    vruntime: usize,
    nice: usize,
    weight: usize,
    last_exec_time: usize,
    thread: Rc<Thread>,
}

/// 
pub struct Scheduler {
    ready_state: Mutex<ReadyState>,
    sleep_list: Mutex<Vec<(Rc<SchedulingEntity>, usize)>>, // (Thread, Wakeup Time)
    join_map: Mutex<Map<usize, Vec<Rc<SchedulingEntity>>>>, // manage which threads are waiting for a thread-id to terminate
}

unsafe impl Send for Scheduler {}
unsafe impl Sync for Scheduler {}


/// Called from assembly code, after the thread has been switched
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unlock_scheduler() {
    unsafe { scheduler().ready_state.force_unlock(); }
}

static GLOBAL_VRUNTIME_COUNTER: AtomicUsize = AtomicUsize::new(0);

impl SchedulingEntity {
    /*
        Lazar Konstantinou and David Schwabauer:
        Creates a new CfsSchedulingEntity instance for a given thread    
     */
    pub fn new(thread: Rc<Thread>, nice: i32) -> Self {
        // current system time in nanoseconds
        let current_time = timer().systime_ns();
        let nice = nice; // Sinnvoll wäre (um die Funktionalität des CFS zu sehen), wenn man unterschiedliche nice Werte setzt oder sie zufällig bestimmt
        let weight = Scheduler::nice_to_weight(nice) as usize; //Gewicht richtig setzen um richtig damit rechnen zu können
        Self {
            vruntime: 0,
            nice: nice as usize,
            last_exec_time: current_time,
            weight: weight,
            thread: thread,
        }
    }

    /*
        Lazar Konstantinou:
        Returns the current virtual runtime of the scheduling entity
    */
    pub fn vruntime(&self) -> usize {
        self.vruntime
    }

    pub fn set_vruntime(&mut self, vruntime: usize) {
        self.vruntime = vruntime;
    }



    /*
        Lazar Konstantinou:
        Returns the current scheduling entity so there are no issues with borrowing 
    */
    pub fn thread(&self) -> Rc<Thread> {
        Rc::clone(&self.thread)
    }
}

impl Scheduler {
    /// Lazar Konstantinou:
    /// Creates a new instance of the CFS scheduler.
    pub fn new() -> Self {
        Self {
            ready_state: Mutex::new(ReadyState {
                sched_slice: 0, 
                sched_period: 0, 

                initialized: false,
                last_switch_time: 0,
                rb_tree: RBTree::new(),
                current: None,
            }),
            sleep_list: Mutex::new(Vec::new()),
            join_map: Mutex::new(Map::new()),
        }
    }

    pub fn set_init(&self) {
        // Set the initialized field for this instance
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
    pub fn current_thread(&self) -> Rc<Thread> {
        let state = self.get_ready_state();
        Scheduler::current(&state).thread()
    }

    /// Description: Return reference to thread for the given `thread_id`
    /// 
    /// Lazar Konstantinou:
    /// Changes for the cfs scheduler as accessing the SchedulingEntity
    pub fn thread(&self, thread_id: usize) -> Option<Rc<Thread>> {
        self.get_ready_state().rb_tree
            .iter()
            .find(|(_, entity)| entity.thread().id() == thread_id)
            .map(|(_, entity)| Rc::clone(entity).thread())
    }

    /// Description: Start the scheduler, called only once from `boot.rs`
    /// 
    /// Lazar Konstantinou:
    /// Changes for the cfs scheduler as using the cfs tree correctly 
    pub fn start(&self) {
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
    pub fn ready(&self, thread: Rc<Thread>, nice: i32) {
        let id = thread.id();
        // We need to create a new SchedulingEntity wrapper for the thread when being inserted into the scheduler
        let mut entity_struct: SchedulingEntity = SchedulingEntity::new(thread, nice);
        let (mut state, mut join_map) = self.get_ready_state_and_join_map();

        if state.rb_tree.len() == 0 {
            // Nothing inside the scheduler init the first thread and set its vruntime to 1
            entity_struct.vruntime  = 0;
        } else {
            let weight = entity_struct.weight;
            let sum_weights: usize = state.rb_tree.iter()
                .map(|(_, entity)| entity.weight)
                .sum();
            entity_struct.vruntime = self.calculated_sched_vslice(&state, weight, sum_weights);
        }

        let entity = Rc::new(entity_struct);
        state.rb_tree.insert(entity.vruntime(), entity);
        join_map.insert(id, Vec::new());
    }
    
    // Lazar Konstantinou:
    fn calculate_sched_period(&self, state: &mut MutexGuard<ReadyState>) -> usize {
        // Formula:
        // if nr_running <= sched_nr_latency => sched_latency
        // else sched_latency * nr_running / sched_nr_latency

        let sched_period: usize;
        let nr_running = state.rb_tree.len();
        if nr_running <= sched_nr_latency {
            sched_period = sched_latency;
        } else {
            sched_period = (sched_latency * nr_running) / sched_nr_latency;
        }
        
        state.sched_period = sched_period;

        sched_period
    }

    /// Lazar Konstantinou:
    fn update_sched_slice(&self, state: &mut MutexGuard<ReadyState>) {
        let sched_period = self.calculate_sched_period(state);

        let current_weight = state.current.as_ref().unwrap().weight;
        let sum_rbtree_weights: usize = state.rb_tree.iter()
            .map(|(_, entity)| entity.weight)
            .sum();
        
        // Formula for sched_slice:
        // sched_period * current_weight / (current_weight+sum_rbtree_weights)
        state.sched_slice = (sched_period * current_weight) / (current_weight + sum_rbtree_weights);
    }

    // Lazar Konstantinou:
    fn calculated_sched_vslice(&self, state: &MutexGuard<ReadyState>, entity_weight: usize, sum_entities_weight: usize) -> usize {
        // Formula: (current_period*thread_weight)//entity_weight+sum(all_thread_weights))
        (state.sched_period * entity_weight) / (entity_weight+sum_entities_weight)
    }

    /// Description: Put calling thread to sleep for `ms` milliseconds
    /// 
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
    /// Lazar Konstantinou:
    /// Changes for the cfs scheduler as using the rb tree correct
    fn switch_thread(&self, interrupt: bool) {
        if let Some(mut state) = self.ready_state.try_lock() {
            if !state.initialized {
                return;
            }

            if let Some(mut sleep_list) = self.sleep_list.try_lock() {
                Scheduler::check_sleep_list(&mut state, &mut sleep_list);
            }

            if let Some((current, next)) = self.prepare_context_switch(&mut state) {
                self.perform_context_switch(state, current, next, interrupt);
            }
        }
    }
    
    /// Checks if a context switch is needed and returns current and next thread
    /// David Schwabauer:
    fn prepare_context_switch(&self, state: &mut ReadyState) -> Option<(Rc<SchedulingEntity>, Rc<SchedulingEntity>)> {
        let current = Scheduler::current(&state);
        let next = match state.rb_tree.pop_first() {
            Some((_, entity)) => entity,
            None => return None,
        };

        if current.thread().stacks_locked() || tss().is_locked() {
            return None;
        }

        if current.vruntime() < next.vruntime() {
            // Current thread has a smaller vruntime than the next thread, so we do not switch
            state.rb_tree.insert(next.vruntime(), next);
            return None;
        }
        Some((current, next))
    }

    /// Performs the actual context switch between two threads
    /// David Schwabauer: 
    fn perform_context_switch(&self, mut state: MutexGuard<ReadyState>, current: Rc<SchedulingEntity>, next: Rc<SchedulingEntity>, interrupt: bool) {
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
    

    // Lazar Konstantinou:
    // Update current thread's vruntime
    // before inserting into the rb_tree check if current threads new vruntime is smaller than the min vruntime of rb tree
    // if true: return false, because the current thread should not be switched out
    // if false: return true and insert old current into tree, because the current thread should be switched out
    fn check_switch_thread(&self) -> bool {

        if let Some(mut state) = self.ready_state.try_lock() {

            if !state.initialized {
                return false;
            }

            let now = timer().systime_ns();
            let rb_tree_len = state.rb_tree.len();
            if rb_tree_len == 0 {
                // No threads in the ready queue, so we dont need to switch
                return false;
            }

            

            

            if now - state.last_switch_time < state.sched_slice {
                return false;
            }

            self.update_sched_slice(&mut state);

            self.update_current(&mut state);

            true
        }
        else {
            false
        }
    }

    /// Description: helper function, calling `switch_thread`
    pub fn switch_thread_no_interrupt(&self) {
        if self.check_switch_thread() {
            self.switch_thread(false);
        }
    }

    /// Description: helper function, calling `switch_thread`
    pub fn switch_thread_from_interrupt(&self) {
        if self.check_switch_thread() {
            self.switch_thread(true);
        }
    }

    /// 
    /// Description: Calling thread wants to wait for another thread to terminate
    /// 
    /// Parameters: `thread_id` thread to wait for
    /// 
    /// Lazar Konstantinou:
    /// Changes for the cfs scheduler
    pub fn join(&self, thread_id: usize) {
        let mut state = self.get_ready_state();
        let entity = Scheduler::current(&state);

        {
            // Execute in own block, so that the lock is released automatically (block() does not return)
            let mut join_map = self.join_map.lock();
            let join_list = join_map.get_mut(&thread_id);
            if join_list.is_some() {
                join_list.unwrap().push(entity);
            } else {
                // Joining on a non-existent thread has no effect (i.e. the thread has already finished running)
                return;
            }
        }

        self.block(&mut state);
    }

    /// Description: Exit calling thread.
    /// 
    /// Lazar Konstantinou:
    /// Changes for the cfs scheduler as using the rb tree correct
    pub fn exit(&self) {
        let mut ready_state;
        let current;
        {
            // Execute in own block, so that join_map is released automatically (block() does not return)
            let state = self.get_ready_state_and_join_map();
            ready_state = state.0;
            let mut join_map = state.1;

            current = Scheduler::current(&ready_state);

            self.reinsert_joined_threads(&mut ready_state, &mut join_map, current.thread().id());
        }

        drop(current); // Decrease Rc manually, because block() does not return
        self.block(&mut ready_state);
    }

    /// Reinserts threads waiting for a terminated thread back into the scheduler.
    /// David Schwabauer:
    fn reinsert_joined_threads(&self, ready_state: &mut ReadyState, join_map: &mut Map<usize, Vec<Rc<SchedulingEntity>>>, thread_id: usize, ) {
        let join_list = join_map.get_mut(&thread_id).expect("Missing join_map entry!");

        for entity in join_list {
            ready_state.rb_tree.insert(entity.vruntime(), Rc::clone(entity));
        }
        join_map.remove(&thread_id);
    }


    /// 
    /// Description: Kill the thread with the  given id
    /// 
    /// Parameters: `thread_id` thread to be killed
    /// 
    /// 
    /// Lazar Konstantinou:
    /// Changes for the cfs scheduler as using the rb tree correct and also the join map
    pub fn kill(&self, thread_id: usize) {
        {
            // Check if current thread tries to kill itself (illegal)
            let ready_state = self.get_ready_state();
            let current = Scheduler::current(&ready_state);

            if current.thread().id() == thread_id {
                panic!("A thread cannot kill itself!");
            }
        }

        let state = self.get_ready_state_and_join_map();
        let mut ready_state = state.0;
        let mut join_map = state.1;

        let join_list = join_map.get_mut(&thread_id).expect("Missing join map entry!");

        for entity in join_list {
            ready_state.rb_tree.insert(entity.vruntime(), Rc::clone(entity));
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
    /// 
    /// Lazar Konstantinou:
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

        let current = Scheduler::current(&state);
        let next = first_node.unwrap();

        // Thread has enqueued itself into sleep list and waited so long, that it dequeued itself in the meantime
        if current.thread().id() == next.1.thread().id() {
            return;
        }

        let current_ptr = ptr::from_ref(current.thread().as_ref());
        let next_ptr = ptr::from_ref(next.1.thread().as_ref());

        state.current = Some(next.1);
        drop(current); // Decrease Rc manually, because Thread::switch does not return

        unsafe {
            Thread::switch(current_ptr, next_ptr);
        }
    }

    /// Description: Return current running thread
    /// Lazar Konstantinou:
    /// This function is used to access the current scheduling entity !not thread! in the scheduler.
    fn current(state: &ReadyState) -> Rc<SchedulingEntity> {
        Rc::clone(state.current.as_ref().expect("Trying to access current thread before initialization!"))
    }


    /// Lazar Konstantinou and David Schwabauer:
    /// Checks the sleep list for threads that are ready to be woken up and inserts them into the CFS tree. 
    /// The vruntime should be set to the minimum of all vruntimes if it was the smallest
    fn check_sleep_list(state: &mut ReadyState, sleep_list: &mut Vec<(Rc<SchedulingEntity>, usize)>) {
        let time = timer().systime_ms();

        let mut to_reinsert = Vec::new();
        // Speichern alle Threads die aufwachen sollen
        sleep_list.retain(|(entity, wake_time)| {
            if time >= *wake_time {
                to_reinsert.push(Rc::clone(entity));
                false
            } else {
                true
            }
        });
        // Prüfen für jeden aufwachenden Thread, ob seine vruntime kleiner ist, als die aktuell kleinste im Baum. 
        // Falls ja, setzte sie auf die kleinste aus dem Baum und füge ihn dort ein
        // Also there need to be a small epsilon which is beeing subtracted (we use 1 here as epsilon)
        for entity in to_reinsert {
            let min_vruntime = state.rb_tree.get_first().map(|(key, _)| *key).unwrap_or(0);

            // Formula here is vruntime = max(vruntime, min_vruntime - epsilon)
            if entity.vruntime() < min_vruntime {
                if let Some(mut_entity) = Rc::get_mut(&mut Rc::clone(&entity)) {

                    if min_vruntime > 0 {
                        // Set the vruntime to the minimum vruntime minus 1, so that it is smaller than the minimum
                        // This is to ensure that the entity is scheduled before any other entity with the same vr as it slept unlike others in the tree
                        mut_entity.set_vruntime(min_vruntime - 1);
                    } else {
                        // Edge case so we can't get into negative vruntimes as it destroys the logic
                        mut_entity.set_vruntime(min_vruntime);
                    }
                } 
            }

            state.rb_tree.insert(entity.vruntime(), entity);
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
    fn get_ready_state_and_join_map(&self) -> (MutexGuard<ReadyState>, MutexGuard<Map<usize, Vec<Rc<SchedulingEntity>>>>) {
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

    /* CFS Logic */
    /// 
    /// Lazar Konstantinou:
    /// Merged from the Linux kernel 2.6.24
    ///
    const MAX_RT_PRIO: i32 = 100;
    pub const fn nice_to_prio(nice: i32) -> i32 {
        Scheduler::MAX_RT_PRIO + nice + 20
    }
    pub const fn prio_to_nice(prio: i32) -> i32 {
        prio - Scheduler::MAX_RT_PRIO - 20
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
        let idx = (nice + 20).clamp(0, 39) as usize; //Wandelt nice Wert in das passendes Gewicht um, indem er den korrekten Index aus dem Array aufruft, -20 ist Index 0, 19 ist Index 39...
        Scheduler::PRIO_TO_WEIGHT[idx]
    }


    // Lazar:
    // Updated die virtual Runtime des aktuellen Threads indem:
    // (deltaExecTime × NICE_0_WEIGHT)/weight_schedule_entity
    fn update_current(&self, state: &mut ReadyState) {
        //info!("Updating current entity vruntime");
        

        let Some(current_rc) = state.current.as_mut() else {
            info!("update_current: No current_entity!");
            return;
        };

        let Some(current_entity) = Rc::get_mut(current_rc) else {
            info!("update_current: No exclusive mutable!");
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
}
