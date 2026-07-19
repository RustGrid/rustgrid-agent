use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

use crate::config::{
    Config, ExecutorConfig, first_login_config, parse_config, save_config, user_config_path,
};

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;
const TARGET_CPUS_PER_JOB: u16 = 4;
const TARGET_MEMORY_PER_JOB: u64 = 8 * GIB;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostResources {
    pub logical_cpus: u16,
    pub memory_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResourcePlan {
    max_concurrency: usize,
    cpus_per_job: u16,
    memory_per_job_bytes: u64,
    capacity_cpus: u16,
    capacity_memory_bytes: u64,
}

pub fn setup_config_path(explicit: Option<&Path>) -> Result<PathBuf> {
    explicit
        .map(Path::to_path_buf)
        .map_or_else(user_config_path, Ok)
}

pub fn run(path: &Path, requested_concurrency: Option<usize>) -> Result<()> {
    let resources = detect_host_resources()?;
    let (mut config, imported_from) = load_base_config(path)?;
    let recommended = recommended_concurrency(resources);
    let default = config
        .max_concurrency
        .clamp(1, maximum_concurrency(resources));
    let default = if path.is_file() || imported_from.is_some() {
        default
    } else {
        recommended
    };

    println!(
        "Detected {} logical CPUs and {} of RAM.",
        resources.logical_cpus,
        format_bytes(resources.memory_bytes)
    );
    let max_concurrency = match requested_concurrency {
        Some(value) => value,
        None if io::stdin().is_terminal() => {
            prompt_concurrency(default, maximum_concurrency(resources))?
        }
        None => recommended,
    };
    let plan = resource_plan(resources, max_concurrency)?;

    config.max_concurrency = plan.max_concurrency;
    config.executor = ExecutorConfig::production_for_host(
        plan.cpus_per_job,
        format_mebibytes(plan.memory_per_job_bytes),
        plan.capacity_cpus,
        format_mebibytes(plan.capacity_memory_bytes),
    );
    if config.installation_id.is_none() {
        config.installation_id = Some(uuid::Uuid::new_v4().to_string());
    }
    config.validate()?;
    save_config(path, &config)?;

    if let Some(source) = imported_from {
        println!(
            "Imported existing worker settings from {}.",
            source.display()
        );
    }
    println!("\n[ complete] Configuration saved to {}", path.display());
    println!(
        "            {} concurrent jobs, each with {} CPU and {} RAM",
        plan.max_concurrency,
        plan.cpus_per_job,
        format_bytes(plan.memory_per_job_bytes)
    );
    if config.worker_id.is_some() {
        println!("\nNext: rustgrid-agent status\n      rustgrid-agent serve");
    } else {
        println!(
            "\nNext: rustgrid-agent login\n      rustgrid-agent status\n      rustgrid-agent serve"
        );
    }
    Ok(())
}

fn load_base_config(path: &Path) -> Result<(Config, Option<PathBuf>)> {
    match fs::read(path) {
        Ok(bytes) => return Ok((parse_config(path, &bytes)?, None)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", path.display()));
        }
    }

    let legacy = PathBuf::from(".rustgrid-agent.json");
    if path == user_config_path()?.as_path() && legacy != path && legacy.is_file() {
        let bytes =
            fs::read(&legacy).with_context(|| format!("could not read {}", legacy.display()))?;
        return Ok((parse_config(&legacy, &bytes)?, Some(legacy)));
    }
    Ok((first_login_config()?, None))
}

fn prompt_concurrency(default: usize, maximum: usize) -> Result<usize> {
    loop {
        print!("Concurrent jobs [{default}] (maximum {maximum}): ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let value = input.trim();
        if value.is_empty() {
            return Ok(default);
        }
        match value.parse::<usize>() {
            Ok(value) if (1..=maximum).contains(&value) => return Ok(value),
            _ => eprintln!("Enter a whole number between 1 and {maximum}."),
        }
    }
}

pub fn detect_host_resources() -> Result<HostResources> {
    let logical_cpus = std::thread::available_parallelism()
        .context("could not detect host CPU capacity")?
        .get()
        .try_into()
        .unwrap_or(u16::MAX);
    let memory_bytes = detect_memory_bytes()?;
    if memory_bytes < GIB {
        bail!("at least 1 GiB of host memory is required");
    }
    Ok(HostResources {
        logical_cpus,
        memory_bytes,
    })
}

#[cfg(target_os = "linux")]
fn detect_memory_bytes() -> Result<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo").context("could not read /proc/meminfo")?;
    let kibibytes = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|value| value.split_whitespace().next())
        .context("/proc/meminfo does not contain MemTotal")?
        .parse::<u64>()
        .context("MemTotal is not numeric")?;
    kibibytes.checked_mul(1024).context("host memory overflow")
}

#[cfg(target_os = "macos")]
fn detect_memory_bytes() -> Result<u64> {
    // SAFETY: `sysconf` takes no pointers and these constants are provided by
    // the target's libc. Negative values are handled as errors below.
    let (pages, page_size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    if pages <= 0 || page_size <= 0 {
        bail!("could not detect host memory with sysconf");
    }
    (pages as u64)
        .checked_mul(page_size as u64)
        .context("host memory overflow")
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn detect_memory_bytes() -> Result<u64> {
    bail!("automatic memory detection is supported on macOS and Linux")
}

fn maximum_concurrency(resources: HostResources) -> usize {
    usize::from(resources.logical_cpus)
        .min((resources.memory_bytes / GIB) as usize)
        .clamp(1, 100)
}

fn recommended_concurrency(resources: HostResources) -> usize {
    let by_cpu = usize::from(resources.logical_cpus / TARGET_CPUS_PER_JOB);
    let by_memory = (resources.memory_bytes / TARGET_MEMORY_PER_JOB) as usize;
    by_cpu.min(by_memory).clamp(1, 100)
}

fn resource_plan(resources: HostResources, max_concurrency: usize) -> Result<ResourcePlan> {
    if !(1..=maximum_concurrency(resources)).contains(&max_concurrency) {
        bail!(
            "this host can support at most {} concurrent jobs; choose a lower --max-concurrency value",
            maximum_concurrency(resources)
        );
    }
    let cpus_per_job =
        (resources.logical_cpus / max_concurrency as u16).clamp(1, TARGET_CPUS_PER_JOB);
    let memory_per_job_bytes =
        (resources.memory_bytes / max_concurrency as u64).min(TARGET_MEMORY_PER_JOB) / MIB * MIB;
    Ok(ResourcePlan {
        max_concurrency,
        cpus_per_job,
        memory_per_job_bytes,
        capacity_cpus: resources.logical_cpus.min(256),
        capacity_memory_bytes: resources.memory_bytes / MIB * MIB,
    })
}

fn format_mebibytes(bytes: u64) -> String {
    format!("{}m", bytes / MIB)
}

fn format_bytes(bytes: u64) -> String {
    if bytes.is_multiple_of(GIB) {
        format!("{} GiB", bytes / GIB)
    } else {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommends_four_balanced_jobs_on_a_sixteen_core_host() {
        let resources = HostResources {
            logical_cpus: 16,
            memory_bytes: 32 * GIB,
        };
        assert_eq!(recommended_concurrency(resources), 4);
        assert_eq!(
            resource_plan(resources, 4).unwrap(),
            ResourcePlan {
                max_concurrency: 4,
                cpus_per_job: 4,
                memory_per_job_bytes: 8 * GIB,
                capacity_cpus: 16,
                capacity_memory_bytes: 32 * GIB,
            }
        );
    }

    #[test]
    fn scales_each_sandbox_to_the_selected_concurrency() {
        let plan = resource_plan(
            HostResources {
                logical_cpus: 8,
                memory_bytes: 16 * GIB,
            },
            4,
        )
        .unwrap();
        assert_eq!(plan.cpus_per_job, 2);
        assert_eq!(plan.memory_per_job_bytes, 4 * GIB);
    }

    #[test]
    fn rejects_more_jobs_than_the_host_can_isolate() {
        let error = resource_plan(
            HostResources {
                logical_cpus: 2,
                memory_bytes: 2 * GIB,
            },
            3,
        )
        .unwrap_err();
        assert!(error.to_string().contains("at most 2"));
    }
}
