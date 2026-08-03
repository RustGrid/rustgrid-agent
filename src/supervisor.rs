use std::{
    fmt,
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

/// Control-plane capabilities consumed by lease supervision.
pub trait LeaseControlPlane: Send + 'static {
    type Error: fmt::Display + Send + 'static;

    fn heartbeat(&self, worker_id: &str) -> Result<(), Self::Error>;
    fn extend_lease(
        &self,
        run_id: &str,
        worker_id: &str,
        lease_seconds: u64,
    ) -> Result<i64, LeaseRenewalError<Self::Error>>;
}

#[derive(Debug)]
pub enum LeaseRenewalError<E> {
    Lost(E),
    Unavailable(E),
}

impl<E: fmt::Display> fmt::Display for LeaseRenewalError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lost(error) | Self::Unavailable(error) => error.fmt(formatter),
        }
    }
}

struct RustGridLeaseControlPlane(RustGridClient);

impl LeaseControlPlane for RustGridLeaseControlPlane {
    type Error = anyhow::Error;

    fn heartbeat(&self, worker_id: &str) -> Result<(), Self::Error> {
        self.0
            .heartbeat_with_status(worker_id, WorkerStatus::Busy)
            .map(|_| ())
    }

    fn extend_lease(
        &self,
        run_id: &str,
        worker_id: &str,
        lease_seconds: u64,
    ) -> Result<i64, LeaseRenewalError<Self::Error>> {
        self.0
            .extend_lease(run_id, worker_id, lease_seconds)
            .map(|run| run.row_version)
            .map_err(|error| {
                if is_lease_lost(&error) {
                    LeaseRenewalError::Lost(error)
                } else {
                    LeaseRenewalError::Unavailable(error)
                }
            })
    }
}

/// Host lifecycle and monotonic-time capability used by the supervisor loop.
pub trait ExecutionEnvironment: Send + 'static {
    fn now(&self) -> Duration;
    fn shutdown_requested(&self) -> bool;
    fn sleep(&self, duration: Duration);
}

struct SystemExecutionEnvironment {
    origin: Instant,
}

impl SystemExecutionEnvironment {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl ExecutionEnvironment for SystemExecutionEnvironment {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn shutdown_requested(&self) -> bool {
        shutdown::requested()
    }

    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeaseDecision {
    Continue { healthy: bool, row_version: i64 },
    StopLeaseLost,
    StopTimedOut,
    StopShutdown,
}

struct LeaseMonitor {
    started_at: Duration,
    last_lease_success: Duration,
    uncertainty_limit: Duration,
    run_timeout: Duration,
}

impl LeaseMonitor {
    fn new(now: Duration, config: &RunSupervisorConfig) -> Self {
        Self {
            started_at: now,
            last_lease_success: now,
            uncertainty_limit: Duration::from_secs(
                config
                    .lease_seconds
                    .saturating_sub(config.heartbeat_interval.as_secs().saturating_mul(2))
                    .max(config.heartbeat_interval.as_secs()),
            ),
            run_timeout: config.run_timeout,
        }
    }

    fn before_renewal(&self, now: Duration, shutdown_requested: bool) -> Option<LeaseDecision> {
        if shutdown_requested {
            Some(LeaseDecision::StopShutdown)
        } else if now.saturating_sub(self.started_at) >= self.run_timeout {
            Some(LeaseDecision::StopTimedOut)
        } else {
            None
        }
    }

    fn observe<E>(
        &mut self,
        now: Duration,
        heartbeat_ok: bool,
        lease: &Result<i64, LeaseRenewalError<E>>,
    ) -> LeaseDecision {
        match lease {
            Ok(row_version) => {
                self.last_lease_success = now;
                LeaseDecision::Continue {
                    healthy: heartbeat_ok,
                    row_version: *row_version,
                }
            }
            Err(LeaseRenewalError::Lost(_)) => LeaseDecision::StopLeaseLost,
            Err(LeaseRenewalError::Unavailable(_))
                if now.saturating_sub(self.last_lease_success) >= self.uncertainty_limit =>
            {
                LeaseDecision::StopLeaseLost
            }
            Err(LeaseRenewalError::Unavailable(_)) => LeaseDecision::Continue {
                healthy: false,
                row_version: 0,
            },
        }
    }
}

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
        Self::start_with(
            RustGridLeaseControlPlane(api),
            SystemExecutionEnvironment::new(),
            worker_id,
            run_id,
            row_version,
            execution_running,
            config,
        )
    }

    fn start_with<C: LeaseControlPlane, E: ExecutionEnvironment>(
        api: C,
        environment: E,
        worker_id: String,
        run_id: String,
        row_version: Arc<AtomicI64>,
        execution_running: Arc<AtomicBool>,
        config: RunSupervisorConfig,
    ) -> Self {
        let heartbeat_interval = config.heartbeat_interval;
        let lease_seconds = config.lease_seconds;
        let stop = Arc::new(AtomicBool::new(false));
        let healthy = Arc::new(AtomicBool::new(true));
        let lease_lost = Arc::new(AtomicBool::new(false));
        let timed_out = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_healthy = Arc::clone(&healthy);
        let thread_lease_lost = Arc::clone(&lease_lost);
        let thread_timed_out = Arc::clone(&timed_out);
        let handle = thread::spawn(move || {
            let mut monitor = LeaseMonitor::new(environment.now(), &config);
            while !thread_stop.load(Ordering::SeqCst) {
                if let Some(decision) =
                    monitor.before_renewal(environment.now(), environment.shutdown_requested())
                {
                    if decision == LeaseDecision::StopTimedOut {
                        thread_timed_out.store(true, Ordering::SeqCst);
                    }
                    execution_running.store(false, Ordering::SeqCst);
                    break;
                }
                let heartbeat = api.heartbeat(&worker_id);
                let lease = api.extend_lease(&run_id, &worker_id, lease_seconds);
                let heartbeat_ok = heartbeat.is_ok();
                if let Err(error) = heartbeat {
                    eprintln!("[warning] worker heartbeat failed: {error:#}");
                }
                if let Err(error) = &lease {
                    eprintln!("[warning] run lease renewal failed: {error}");
                }
                match monitor.observe(environment.now(), heartbeat_ok, &lease) {
                    LeaseDecision::Continue {
                        healthy,
                        row_version: renewed_row_version,
                    } => {
                        thread_healthy.store(healthy, Ordering::SeqCst);
                        if lease.is_ok() {
                            row_version.store(renewed_row_version, Ordering::SeqCst);
                        }
                    }
                    LeaseDecision::StopLeaseLost => {
                        thread_healthy.store(false, Ordering::SeqCst);
                        thread_lease_lost.store(true, Ordering::SeqCst);
                        execution_running.store(false, Ordering::SeqCst);
                        break;
                    }
                    LeaseDecision::StopTimedOut | LeaseDecision::StopShutdown => unreachable!(),
                }
                let slices = heartbeat_interval.as_millis().div_ceil(250) as usize;
                for _ in 0..slices {
                    if thread_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    environment.sleep(Duration::from_millis(250));
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
    fn lease_loss_is_a_deterministic_supervision_decision() {
        let config = RunSupervisorConfig {
            heartbeat_interval: Duration::from_secs(5),
            lease_seconds: 30,
            run_timeout: Duration::from_secs(60),
        };
        let mut monitor = LeaseMonitor::new(Duration::ZERO, &config);
        let lease: Result<i64, LeaseRenewalError<&str>> =
            Err(LeaseRenewalError::Lost("ownership changed"));

        assert_eq!(
            monitor.observe(Duration::from_secs(5), true, &lease),
            LeaseDecision::StopLeaseLost
        );
    }

    #[test]
    fn transient_lease_errors_stop_only_after_the_uncertainty_window() {
        let config = RunSupervisorConfig {
            heartbeat_interval: Duration::from_secs(5),
            lease_seconds: 30,
            run_timeout: Duration::from_secs(60),
        };
        let mut monitor = LeaseMonitor::new(Duration::ZERO, &config);
        let lease: Result<i64, LeaseRenewalError<&str>> =
            Err(LeaseRenewalError::Unavailable("temporary outage"));

        assert_eq!(
            monitor.observe(Duration::from_secs(19), false, &lease),
            LeaseDecision::Continue {
                healthy: false,
                row_version: 0,
            }
        );
        assert_eq!(
            monitor.observe(Duration::from_secs(20), false, &lease),
            LeaseDecision::StopLeaseLost
        );
    }

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
                instance_url: Some(format!("http://{address}/api/v1")),
                installation_id: Some("00000000-0000-4000-8000-000000000099".into()),
                worker_id: Some("00000000-0000-4000-8000-000000000001".into()),
                tenant_id: Some("00000000-0000-4000-8000-000000000002".into()),
                worker_name: Some("test-worker".into()),
                credential_store: Some("private_file_fallback".into()),
                credential_expires_at_unix: None,
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
                    codex_version: "0.144.4".into(),
                    cpus: 1,
                    memory: "1g".into(),
                    capacity_cpus: 2,
                    capacity_memory: "2g".into(),
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
            instance_url: format!("http://{address}"),
            api_url: format!("http://{address}"),
            api_key: Some("test-key".into()),
            worker_id: Some("00000000-0000-4000-8000-000000000001".into()),
            tenant_id: Some("00000000-0000-4000-8000-000000000002".into()),
            worker_name: Some("test-worker".into()),
            installation_id: "00000000-0000-4000-8000-000000000099".into(),
            credential_source: crate::credentials::CredentialSource::FallbackFile,
            credential_expires_at_unix: None,
            credential_store: crate::credentials::CredentialStore::new(
                &format!("http://{address}"),
                "00000000-0000-4000-8000-000000000099",
            )
            .unwrap(),
            credentials_path: PathBuf::from("test.json.credentials"),
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
