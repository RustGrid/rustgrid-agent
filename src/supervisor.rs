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
    lifecycle::WorkerStatus,
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
                let heartbeat = api.heartbeat_with_status(&worker_id, WorkerStatus::Busy);
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

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        path::PathBuf,
    };

    use super::*;
    use crate::config::{AppContext, Config};

    #[test]
    fn lease_loss_cancels_only_its_execution_token() {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("could not bind supervisor test server: {error}"),
        };
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for response in [
                (200, r#"{"id":"worker-1","status":"busy"}"#),
                (409, r#"{"error":"lease lost"}"#),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request).unwrap();
                let reason = if response.0 == 200 { "OK" } else { "Conflict" };
                write!(
                    stream,
                    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.0,
                    reason,
                    response.1.len(),
                    response.1
                )
                .unwrap();
            }
        });
        let context = AppContext {
            config: Config {
                project_id: Some("project-1".into()),
                project_key: None,
                repo: None,
                default_base_branch: "main".into(),
                quality_gate_command: None,
                codex_command: None,
                heartbeat_interval_seconds: 5,
                max_concurrency: 2,
                executor: crate::config::ExecutorConfig::DockerSandbox {
                    command: "sbx".into(),
                    template: "test".into(),
                    cpus: 1,
                    memory: "1g".into(),
                },
                lease_seconds: 30,
                workspace_root: None,
                command_timeout_seconds: 1800,
                run_timeout_seconds: 7200,
                failed_workspace_retention_hours: 72,
                max_command_output_bytes: 8 * 1024 * 1024,
                max_workspace_bytes: 5 * 1024 * 1024 * 1024,
                max_child_memory_bytes: 8 * 1024 * 1024 * 1024,
                max_child_file_bytes: 1024 * 1024 * 1024,
                max_child_open_files: 1024,
            },
            config_path: PathBuf::from("test.json"),
            api_url: format!("http://{address}"),
            api_key: Some("test-key".into()),
            workspace_root: PathBuf::from("/tmp/rustgrid-agent-supervisor-test"),
        };
        let execution = Arc::new(AtomicBool::new(true));
        let unrelated = Arc::new(AtomicBool::new(true));
        let supervisor = RunSupervisor::start(
            RustGridClient::new(&context).unwrap(),
            "worker-1".into(),
            "run-1".into(),
            Arc::new(AtomicI64::new(1)),
            Arc::clone(&execution),
            RunSupervisorConfig {
                heartbeat_interval: Duration::from_millis(10),
                lease_seconds: 30,
                run_timeout: Duration::from_secs(10),
            },
        );
        for _ in 0..100 {
            if !execution.load(Ordering::SeqCst) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(supervisor.lease_lost());
        assert!(!execution.load(Ordering::SeqCst));
        assert!(unrelated.load(Ordering::SeqCst));
        drop(supervisor);
        server.join().unwrap();
    }
}
