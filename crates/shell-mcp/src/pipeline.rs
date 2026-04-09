//! Pipeline orchestration — run a DAG of jobs with dependency tracking.
//!
//! A pipeline is a set of named steps with optional `depends_on` edges.
//! Steps with no unmet dependencies are started immediately (in parallel).
//! When a step finishes, its dependents are evaluated for readiness.
//!
//! The coordinator runs as a background thread — pipeline_run returns instantly.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::job_store::JobStore;
use crate::tools::state::lock_mutex;

// ── Step state ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum StepStatus {
    Pending,
    Running,
    Done(i32),
    Failed(i32),
    /// Dependency failed and stop_on_failure is set.
    Skipped,
}

impl StepStatus {
    pub fn label(&self) -> &'static str {
        match self {
            StepStatus::Pending => "pending",
            StepStatus::Running => "running",
            StepStatus::Done(_) => "done",
            StepStatus::Failed(_) => "failed",
            StepStatus::Skipped => "skipped",
        }
    }
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            StepStatus::Done(c) | StepStatus::Failed(c) => Some(*c),
            _ => None,
        }
    }
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            StepStatus::Done(_) | StepStatus::Failed(_) | StepStatus::Skipped
        )
    }
    pub fn is_successful(&self) -> bool {
        matches!(self, StepStatus::Done(_))
    }
}

// ── PipelineStep ──────────────────────────────────────────────────────────────

pub struct PipelineStep {
    /// Caller-supplied ID (e.g. "build", "test").
    pub id: String,
    pub cmd: String,
    pub cwd: Option<PathBuf>,
    pub label: String,
    pub depends_on: Vec<String>,
    /// Set when the step spawns a bg job.
    pub job_id: Mutex<Option<String>>,
    pub status: Arc<Mutex<StepStatus>>,
}

// ── Pipeline ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum PipelineStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

impl PipelineStatus {
    pub fn label(&self) -> &'static str {
        match self {
            PipelineStatus::Running => "running",
            PipelineStatus::Done => "done",
            PipelineStatus::Failed => "failed",
            PipelineStatus::Cancelled => "cancelled",
        }
    }
}

pub struct Pipeline {
    pub pipeline_id: String,
    pub label: String,
    pub steps: Vec<Arc<PipelineStep>>,
    pub status: Arc<Mutex<PipelineStatus>>,
    pub stop_on_failure: bool,
    pub started_secs: u64,
}

impl Pipeline {
    pub fn elapsed_secs(&self) -> f64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        (now.saturating_sub(self.started_secs)) as f64
    }

    pub fn current_status(&self) -> PipelineStatus {
        lock_mutex(&self.status, "pipeline status").clone()
    }

    pub fn step_by_id(&self, id: &str) -> Option<&Arc<PipelineStep>> {
        self.steps.iter().find(|s| s.id == id)
    }
}

// ── PipelineStore ─────────────────────────────────────────────────────────────

pub struct PipelineStore {
    pipelines: Mutex<HashMap<String, Arc<Pipeline>>>,
    counter: AtomicUsize,
    job_store: Arc<JobStore>,
}

impl PipelineStore {
    pub fn new(job_store: Arc<JobStore>) -> Self {
        Self {
            pipelines: Mutex::new(HashMap::new()),
            counter: AtomicUsize::new(1),
            job_store,
        }
    }

    fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("p{n:04}")
    }

    pub fn get(&self, id: &str) -> Option<Arc<Pipeline>> {
        lock_mutex(&self.pipelines, "pipeline store")
            .get(id)
            .cloned()
    }

    #[allow(dead_code)]
    pub fn all(&self) -> Vec<Arc<Pipeline>> {
        let mut ps: Vec<Arc<Pipeline>> = lock_mutex(&self.pipelines, "pipeline store")
            .values()
            .cloned()
            .collect();
        ps.sort_by_key(|p| p.started_secs);
        ps
    }

    // ── create + run ──────────────────────────────────────────────────────────

    pub fn run(
        &self,
        step_defs: Vec<StepDef>,
        default_cwd: PathBuf,
        label: Option<&str>,
        stop_on_failure: bool,
    ) -> Result<Arc<Pipeline>, String> {
        // Validate: no unknown depends_on IDs.
        let ids: std::collections::HashSet<&str> =
            step_defs.iter().map(|s| s.id.as_str()).collect();
        for sd in &step_defs {
            for dep in &sd.depends_on {
                if !ids.contains(dep.as_str()) {
                    return Err(format!(
                        "pipeline step '{}' depends_on unknown step '{dep}'",
                        sd.id
                    ));
                }
            }
        }

        // Validate: no duplicate IDs.
        let mut seen = std::collections::HashSet::new();
        for sd in &step_defs {
            if !seen.insert(sd.id.clone()) {
                return Err(format!("duplicate step id: '{}'", sd.id));
            }
        }

        let pipeline_id = self.next_id();
        let label = label.unwrap_or(&pipeline_id).to_string();

        let steps: Vec<Arc<PipelineStep>> = step_defs
            .into_iter()
            .map(|sd| {
                let step_label = sd.label.unwrap_or_else(|| sd.id.clone());
                Arc::new(PipelineStep {
                    id: sd.id,
                    cmd: sd.cmd,
                    cwd: sd.cwd.map(PathBuf::from),
                    label: step_label,
                    depends_on: sd.depends_on,
                    job_id: Mutex::new(None),
                    status: Arc::new(Mutex::new(StepStatus::Pending)),
                })
            })
            .collect();

        let started_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let pipeline = Arc::new(Pipeline {
            pipeline_id: pipeline_id.clone(),
            label,
            steps,
            status: Arc::new(Mutex::new(PipelineStatus::Running)),
            stop_on_failure,
            started_secs,
        });

        lock_mutex(&self.pipelines, "pipeline store").insert(pipeline_id, Arc::clone(&pipeline));

        // Spawn coordinator thread.
        let pipeline_t = Arc::clone(&pipeline);
        let job_store_t = Arc::clone(&self.job_store);
        thread::spawn(move || coordinator(pipeline_t, job_store_t, default_cwd));

        Ok(pipeline)
    }

    // ── cancel ────────────────────────────────────────────────────────────────

    /// Cancel a running pipeline: kill all running step jobs, mark rest Skipped.
    pub fn cancel(&self, pipeline_id: &str) -> Result<Vec<String>, String> {
        let pipeline = self
            .get(pipeline_id)
            .ok_or_else(|| format!("pipeline '{pipeline_id}' not found"))?;

        *lock_mutex(&pipeline.status, "pipeline status") = PipelineStatus::Cancelled;

        let mut killed_jobs = Vec::new();
        for step in &pipeline.steps {
            let mut st = lock_mutex(&step.status, "pipeline step status");
            if matches!(*st, StepStatus::Running) {
                *st = StepStatus::Skipped;
                // Kill the associated job if any.
                let job_id = lock_mutex(&step.job_id, "pipeline step job id").clone();
                if let Some(jid) = job_id {
                    use nix::sys::signal::{kill, Signal};
                    use nix::unistd::Pid;
                    if let Some(job) = self.job_store.get(&jid) {
                        let _ = kill(Pid::from_raw(job.pid as i32), Signal::SIGTERM);
                    }
                    killed_jobs.push(jid);
                }
            } else if matches!(*st, StepStatus::Pending) {
                *st = StepStatus::Skipped;
            }
        }

        Ok(killed_jobs)
    }
}

// ── Coordinator thread ────────────────────────────────────────────────────────

fn coordinator(pipeline: Arc<Pipeline>, job_store: Arc<JobStore>, default_cwd: PathBuf) {
    loop {
        // ── 1. Update statuses of running steps from their jobs ───────────────
        for step in &pipeline.steps {
            let step_status = lock_mutex(&step.status, "pipeline step status").clone();
            if let StepStatus::Running = step_status {
                let job_id = lock_mutex(&step.job_id, "pipeline step job id").clone();
                if let Some(jid) = job_id {
                    if let Some(job) = job_store.get(&jid) {
                        let js = job.current_status();
                        use crate::job_store::JobStatus;
                        let new_step_status = match js {
                            JobStatus::Done(0) => Some(StepStatus::Done(0)),
                            JobStatus::Done(code) => Some(StepStatus::Failed(code)),
                            JobStatus::Killed => Some(StepStatus::Failed(-1)),
                            _ => None,
                        };
                        if let Some(nss) = new_step_status {
                            *lock_mutex(&step.status, "pipeline step status") = nss;
                        }
                    }
                }
            }
        }

        // ── 2. Check for failure propagation ─────────────────────────────────
        if pipeline.stop_on_failure {
            let any_failed = pipeline.steps.iter().any(|s| {
                matches!(
                    *lock_mutex(&s.status, "pipeline step status"),
                    StepStatus::Failed(_)
                )
            });
            if any_failed {
                for step in &pipeline.steps {
                    let mut st = lock_mutex(&step.status, "pipeline step status");
                    if matches!(*st, StepStatus::Pending) {
                        *st = StepStatus::Skipped;
                    }
                }
            }
        }

        // ── 3. Check pipeline cancellation ───────────────────────────────────
        if pipeline.current_status() == PipelineStatus::Cancelled {
            break;
        }

        // ── 4. Launch steps whose dependencies are all successfully Done ──────
        for step in &pipeline.steps {
            if !matches!(
                *lock_mutex(&step.status, "pipeline step status"),
                StepStatus::Pending
            ) {
                continue;
            }

            // Check that every dependency is Done (success) or skip if failed.
            let all_deps_ok = step.depends_on.iter().all(|dep_id| {
                pipeline
                    .step_by_id(dep_id)
                    .map(|s| lock_mutex(&s.status, "pipeline step status").is_successful())
                    .unwrap_or(true)
            });

            let any_dep_blocked = step.depends_on.iter().any(|dep_id| {
                pipeline
                    .step_by_id(dep_id)
                    .map(|s| {
                        let st = lock_mutex(&s.status, "pipeline step status");
                        matches!(*st, StepStatus::Failed(_) | StepStatus::Skipped)
                    })
                    .unwrap_or(false)
            });

            if any_dep_blocked {
                *lock_mutex(&step.status, "pipeline step status") = StepStatus::Skipped;
                continue;
            }

            if !all_deps_ok {
                continue;
            }

            // Launch.
            let cwd = step.cwd.clone().unwrap_or_else(|| default_cwd.clone());
            match job_store.spawn(&step.cmd, cwd, Some(&step.label), vec![]) {
                Ok(job) => {
                    *lock_mutex(&step.job_id, "pipeline step job id") = Some(job.job_id.clone());
                    *lock_mutex(&step.status, "pipeline step status") = StepStatus::Running;
                }
                Err(e) => {
                    eprintln!("ferrite pipeline: failed to spawn step '{}': {e}", step.id);
                    *lock_mutex(&step.status, "pipeline step status") = StepStatus::Failed(-1);
                }
            }
        }

        // ── 5. Check if all steps have reached a terminal state ───────────────
        let all_terminal = pipeline
            .steps
            .iter()
            .all(|s| lock_mutex(&s.status, "pipeline step status").is_terminal());

        if all_terminal {
            let any_failed = pipeline.steps.iter().any(|s| {
                matches!(
                    *lock_mutex(&s.status, "pipeline step status"),
                    StepStatus::Failed(_)
                )
            });
            *lock_mutex(&pipeline.status, "pipeline status") = if any_failed {
                PipelineStatus::Failed
            } else {
                PipelineStatus::Done
            };
            break;
        }

        thread::sleep(Duration::from_millis(200));
    }
}

// ── Public input type ─────────────────────────────────────────────────────────

/// Input definition for a single pipeline step, parsed from the MCP call.
pub struct StepDef {
    pub id: String,
    pub cmd: String,
    pub cwd: Option<String>,
    pub label: Option<String>,
    pub depends_on: Vec<String>,
}
