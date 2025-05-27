use alloc::rc::Rc;

use crate::{cfs_scheduler, process::scheduler};



pub fn spawn_demo_thread() {
    use log::info;

    use crate::process::thread::Thread;
    use crate::scheduler;
    use crate::process::scheduler::SchedulingEntity;
    // ent 1
    let thread = Thread::new_kernel_thread(
        || {
            //scheduler().sleep(500);
            for i in 0..5 {
                log::info!("Thread {} arbeitet: {}", scheduler().current_thread().id(), i);
            }
        },
        "test_thread1",
    );

    let entity1 = Rc::new(SchedulingEntity::new(thread));

    // ent 2
    let thread2 = Thread::new_kernel_thread(
        || {
            //scheduler().sleep(500);
            for i in 0..5 {
                log::info!("Thread {} arbeitet: {}", scheduler().current_thread().id(), i);
            }
        },
        "test_thread2",
    );
    let entity2 = Rc::new(SchedulingEntity::new(thread2));

    // ent 3
    let thread3 = Thread::new_kernel_thread(
        || {
            //scheduler().sleep(500);
            for i in 0..5 {
                log::info!("Thread {} arbeitet: {}", scheduler().current_thread().id(), i);
            }
        },
        "test_thread3",
    );
    let entity3 = Rc::new(SchedulingEntity::new(thread3));

    // ent 4
    let thread4 = Thread::new_kernel_thread(
        || {
            //scheduler().sleep(500);
            for i in 0..5 {
                log::info!("Thread {} arbeitet: {}", scheduler().current_thread().id(), i);
            }
        },
        "test_thread4",
    );
    let entity4 = Rc::new(SchedulingEntity::new(thread4));



    let cfs = cfs_scheduler();
    cfs.ready(entity1);
    cfs.ready(entity2);
    cfs.ready(entity3);
    cfs.ready(entity4);

    cfs.kill(5);

    info!("Spawning thread:");
    //scheduler().ready(thread);
}