use alloc::rc::Rc;

pub fn spawn_demo_thread() {
    use log::info;

    use crate::process::thread::Thread;
    use crate::scheduler;
    use crate::process::scheduler::SchedulingEntity;
    // ent 1
    let thread = Thread::new_kernel_thread(
        || {
            
            for i in 0..5 {
                scheduler().sleep(5000);
                log::info!("Thread {} arbeitet: {}", scheduler().current_thread().id(), i);
            }
        },
        "test_thread1",
    );



    // ent 2
    let thread2 = Thread::new_kernel_thread(
        || {
            scheduler().sleep(5000);
            for i in 0..5 {
                log::info!("Thread {} arbeitet: {}", scheduler().current_thread().id(), i);
            }
        },
        "test_thread2",
    );


    // ent 3
    let thread3 = Thread::new_kernel_thread(
        || {
            scheduler().sleep(5000);
            for i in 0..5 {
                log::info!("Thread {} arbeitet: {}", scheduler().current_thread().id(), i);
            }

        },
        "test_thread3",
    );


    // ent 4
    let thread4 = Thread::new_kernel_thread(
        || {
            scheduler().sleep(5000);
            for i in 0..5 {
                log::info!("Thread {} arbeitet: {}", scheduler().current_thread().id(), i);
            }
        },
        "test_thread4",
    );




    let cfs = scheduler();
    cfs.ready(thread);
    //cfs.ready(thread2);
    //cfs.ready(thread3);
    //cfs.ready(thread4);


}