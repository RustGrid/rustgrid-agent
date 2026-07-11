use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    api::{RustGridClient, is_lease_lost},
    shutdown,
};

pub struct RunSupervisor {
    stop: Arc<AtomicBool>,
    healthy: Arc<AtomicBool>,
    lease_lost: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

pub struct RunSupervisorConfig {
    pub heartbeat_interval: Duration,
    pub lease_seconds: u64,
    pub run_timeout: Duration,
}

impl RunSupervisor {
    pub fn start(
        api: RustGridClient,
        worker_id: String,
        run_id: String,
        row_version: Arc<AtomicI64>,
        execution_running: Arc<AtomicBool>,
        config: RunSupervisorConfig,
    ) -> Self {
        let heartbeat_interval = config.heartbeat_interval;
        let lease_seconds = config.lease_seconds;
        let run_timeout = config.run_timeout;
        let stop = Arc::new(AtomicBool::new(false));
        let healthy = Arc::new(AtomicBool::new(true));
        let lease_lost = Arc::new(AtomicBool::new(false));
        let timed_out = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_healthy = Arc::clone(&healthy);
        let thread_lease_lost = Arc::clone(&lease_lost);
        let thread_timed_out = Arc::clone(&timed_out);
        let handle = thread::spawn(move || {
            let run_started = Instant::now();
            let mut last_lease_success = Instant::now();
            let uncertainty_limit = Duration::from_secs(
                lease_seconds
                    .saturating_sub(heartbeat_interval.as_secs().saturating_mul(2))
                    .max(heartbeat_interval.as_secs()),
            );
            while !thread_stop.load(Ordering::SeqCst) {
                if shutdown::requested() {
                    execution_running.store(false, Ordering::SeqCst);
                    break;
                }
                if run_started.elapsed() >= run_timeout {
                    thread_timed_out.store(true, Ordering::SeqCst);
                    execution_running.store(false, Ordering::SeqCst);
                    break;
                }
                let heartbeat = api.heartbeat_with_status(&worker_id, "busy");
                let lease = api.extend_lease(&run_id, &worker_id, lease_seconds);
                let heartbeat_ok = heartbeat.is_ok();
                if let Err(error) = heartbeat {
                    eprintln!("[warning] worker heartbeat failed: {error:#}");
                }
                match lease {
                    Ok(run) => {
                        row_version.store(run.row_version, Ordering::SeqCst);
                        last_lease_success = Instant::now();
                        thread_healthy.store(heartbeat_ok, Ordering::SeqCst);
                    }
                    Err(error) => {
                        thread_healthy.store(false, Ordering::SeqCst);
                        eprintln!("[warning] run lease renewal failed: {error:#}");
                        if is_lease_lost(&error)
                            || last_lease_success.elapsed() >= uncertainty_limit
                        {
                            thread_lease_lost.store(true, Ordering::SeqCst);
                            execution_running.store(false, Ordering::SeqCst);
                            break;
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
            lease_lost,
            timed_out,
            handle: Some(handle),
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::SeqCst)
    }

    pub fn lease_lost(&self) -> bool {
        self.lease_lost.load(Ordering::SeqCst)
    }

    pub fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::SeqCst)
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
