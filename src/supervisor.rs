use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::api::RustGridClient;

pub struct RunSupervisor {
    stop: Arc<AtomicBool>,
    healthy: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl RunSupervisor {
    pub fn start(
        api: RustGridClient,
        worker_id: String,
        run_id: String,
        row_version: Arc<AtomicI64>,
        heartbeat_interval: Duration,
        lease_seconds: u64,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let healthy = Arc::new(AtomicBool::new(true));
        let thread_stop = Arc::clone(&stop);
        let thread_healthy = Arc::clone(&healthy);
        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                let heartbeat = api.heartbeat_with_status(&worker_id, "busy");
                let lease = api.extend_lease(&run_id, &worker_id, lease_seconds);
                match (heartbeat, lease) {
                    (Ok(()), Ok(run)) => {
                        row_version.store(run.row_version, Ordering::SeqCst);
                        thread_healthy.store(true, Ordering::SeqCst);
                    }
                    (heartbeat, lease) => {
                        thread_healthy.store(false, Ordering::SeqCst);
                        if let Err(error) = heartbeat {
                            eprintln!("[warning] worker heartbeat failed: {error:#}");
                        }
                        if let Err(error) = lease {
                            eprintln!("[warning] run lease renewal failed: {error:#}");
                        }
                    }
                }
                let slices = heartbeat_interval.as_millis().div_ceil(250) as usize;
                for _ in 0..slices {
                    if thread_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(250));
                }
            }
        });
        Self {
            stop,
            healthy,
            handle: Some(handle),
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::SeqCst)
    }
}

impl Drop for RunSupervisor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
